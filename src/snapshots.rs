//! Screenshots of the real app without a window, for the matrix of
//! docs/gui-polish-guidelines.md 0.6. Test-only.
//!
//! `App::ui` runs exactly as eframe runs it, on a context with the app's
//! fonts and theme, and egui's meshes are rasterised on the CPU into PNGs.
//! Every state is built by hand, so two runs of one scene differ only by what
//! the code changed, and a before/after pair can be compared pixel by pixel.
//!
//! Nothing here reaches a printer:
//! - every MQTT client and camera is started with an empty serial, which
//!   fails closed before a socket exists (`PrinterClient::start`,
//!   `Camera::run`);
//! - every configured address is in TEST-NET-1 and every access code is
//!   empty, so even a stray FTP command that started a lane would go nowhere
//!   and carry nothing;
//! - the scenes leave the view nothing to ask for: a thumbnail is always in
//!   flight, previews are already read, recordings already listed. The
//!   matrix asserts afterwards that no worker opened a session and that the
//!   view left no preview waiting on one.
//!
//! Matrix: `BAMBU_SNAPSHOT_DIR=<dir> cargo test snapshots::screenshot_matrix
//! -- --ignored` (`BAMBU_SNAPSHOT_ONLY=panel,dlg-skip` narrows it). Diff:
//! `BAMBU_SNAPSHOT_BEFORE=<dir> BAMBU_SNAPSHOT_AFTER=<dir>
//! BAMBU_SNAPSHOT_DIR=<out> cargo test snapshots::snapshot_diff -- --ignored`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::App as _;
use egui::{Color32, Pos2, Rect, TextureId, Vec2, pos2, vec2};
use serde_json::{Value, json};

use crate::browser::{BrowserState, Cmd, DetailState, Dest, Event, Picture,
                     ThreeMf, ThumbState};
use crate::ftp::{DateRule, FtpError, RemoteEntry, parse_list_line};
use crate::threemf::{Filament, ThreeMfInfo};
use crate::ui::dialogs::{self, Dialog};
use crate::ui::files_view::{FilesUi, Tab};
use crate::{App, AppView, PrinterUi, cache, camera, config, files, mqtt,
            player};

// ------------------------------------------------------------- rasteriser

/// A texture as egui uploaded it: premultiplied sRGBA, row by row.
struct Texture {
    size: [usize; 2],
    pixels: Vec<[f32; 4]>,
    linear: bool,
}

#[derive(Default)]
struct Raster {
    textures: HashMap<TextureId, Texture>,
}

fn unit(color: Color32) -> [f32; 4] {
    color.to_array().map(|c| f32::from(c) / 255.0)
}

impl Raster {
    fn apply(&mut self, delta: &egui::TexturesDelta) {
        for (id, change) in &delta.set {
            let egui::ImageData::Color(image) = &change.image;
            let pixels: Vec<[f32; 4]> =
                image.pixels.iter().map(|c| unit(*c)).collect();
            match change.pos {
                None => {
                    self.textures.insert(*id, Texture {
                        size: image.size,
                        pixels,
                        linear: change.options.magnification
                            == egui::TextureFilter::Linear,
                    });
                }
                Some([x, y]) => {
                    let Some(texture) = self.textures.get_mut(id) else {
                        continue;
                    };
                    let [w, h] = image.size;
                    for row in 0..h {
                        for col in 0..w {
                            let at = (y + row) * texture.size[0] + x + col;
                            texture.pixels[at] = pixels[row * w + col];
                        }
                    }
                }
            }
        }
    }

    fn free(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.textures.remove(id);
        }
    }
}

impl Texture {
    fn sample(&self, u: f64, v: f64) -> [f32; 4] {
        let [w, h] = self.size;
        let texel = |x: i64, y: i64| {
            let x = x.clamp(0, w as i64 - 1) as usize;
            let y = y.clamp(0, h as i64 - 1) as usize;
            self.pixels[y * w + x]
        };
        if !self.linear {
            return texel((u * w as f64).floor() as i64,
                         (v * h as f64).floor() as i64);
        }
        let x = u * w as f64 - 0.5;
        let y = v * h as f64 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let [a, b, c, d] = [texel(x0, y0), texel(x0 + 1, y0),
                            texel(x0, y0 + 1), texel(x0 + 1, y0 + 1)];
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            top + (bottom - top) * fy
        })
    }
}

/// Premultiplied sRGBA, blended in gamma space as egui's own renderers do.
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<[f32; 4]>,
}

fn cross(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self { width, height,
               pixels: vec![[0.0, 0.0, 0.0, 1.0]; width * height] }
    }

    fn paint(&mut self, raster: &Raster,
             primitives: &[egui::ClippedPrimitive], ppp: f32) {
        for clipped in primitives {
            let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive
            else {
                continue;
            };
            let texture = raster.textures.get(&mesh.texture_id)
                .unwrap_or_else(|| panic!(
                    "no texture {:?}: a context is shot once, because egui \
                     uploads its font atlas only on its first frame",
                    mesh.texture_id));
            let edge = |value: f32, max: usize| {
                ((value * ppp).round().max(0.0) as usize).min(max)
            };
            let clip = clipped.clip_rect;
            let clip = [edge(clip.min.x, self.width),
                        edge(clip.min.y, self.height),
                        edge(clip.max.x, self.width),
                        edge(clip.max.y, self.height)];
            for triangle in mesh.indices.chunks_exact(3) {
                let vertices = [0, 1, 2]
                    .map(|i| &mesh.vertices[triangle[i] as usize]);
                self.triangle(texture, vertices, ppp, clip);
            }
        }
    }

    /// Pixel centres inside the triangle, with the top-left rule, so two
    /// triangles sharing an edge never blend the same pixel twice.
    fn triangle(&mut self, texture: &Texture,
                vertices: [&egui::epaint::Vertex; 3], ppp: f32,
                clip: [usize; 4]) {
        let at = vertices.map(|v| [f64::from(v.pos.x * ppp),
                                   f64::from(v.pos.y * ppp)]);
        let mut order = [0, 1, 2];
        let mut area = cross(at[0], at[1], at[2]);
        if area == 0.0 {
            return;
        }
        if area < 0.0 {
            order.swap(1, 2);
            area = -area;
        }
        let [a, b, c] = order.map(|i| at[i]);
        let [va, vb, vc] = order.map(|i| vertices[i]);
        let top_left = |from: [f64; 2], to: [f64; 2]| {
            let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
            dy < 0.0 || (dy == 0.0 && dx > 0.0)
        };
        // the edge opposite each vertex, so its value weighs that vertex
        let edges = [(b, c, top_left(b, c)), (c, a, top_left(c, a)),
                     (a, b, top_left(a, b))];
        let lo = |i: usize, bound: usize| {
            (a[i].min(b[i]).min(c[i]).floor().max(0.0) as usize).max(bound)
        };
        let hi = |i: usize, bound: usize| {
            (a[i].max(b[i]).max(c[i]).ceil().max(0.0) as usize).min(bound)
        };
        let (x0, x1) = (lo(0, clip[0]), hi(0, clip[2]));
        let (y0, y1) = (lo(1, clip[1]), hi(1, clip[3]));
        let colors = [va, vb, vc].map(|v| unit(v.color));
        for py in y0..y1 {
            for px in x0..x1 {
                let q = [px as f64 + 0.5, py as f64 + 0.5];
                let mut weights = [0.0; 3];
                let mut inside = true;
                for (i, (from, to, tl)) in edges.iter().enumerate() {
                    let e = cross(*from, *to, q);
                    if e < 0.0 || (e == 0.0 && !tl) {
                        inside = false;
                        break;
                    }
                    weights[i] = e / area;
                }
                if !inside {
                    continue;
                }
                let u = weights[0] * f64::from(va.uv.x)
                    + weights[1] * f64::from(vb.uv.x)
                    + weights[2] * f64::from(vc.uv.x);
                let v = weights[0] * f64::from(va.uv.y)
                    + weights[1] * f64::from(vb.uv.y)
                    + weights[2] * f64::from(vc.uv.y);
                let texel = texture.sample(u, v);
                let w = weights.map(|w| w as f32);
                let src: [f32; 4] = std::array::from_fn(|i| {
                    (w[0] * colors[0][i] + w[1] * colors[1][i]
                        + w[2] * colors[2][i]) * texel[i]
                });
                let dst = &mut self.pixels[py * self.width + px];
                for i in 0..4 {
                    dst[i] = src[i] + dst[i] * (1.0 - src[3]);
                }
            }
        }
    }

    fn image(&self) -> image::RgbaImage {
        let bytes: Vec<u8> = self.pixels.iter()
            .flat_map(|p| {
                let [r, g, b, _] = p.map(|c| (c.clamp(0.0, 1.0) * 255.0)
                    .round() as u8);
                [r, g, b, 255]
            })
            .collect();
        image::RgbaImage::from_raw(self.width as u32, self.height as u32,
                                   bytes)
            .expect("a buffer of width x height x 4")
    }
}

// ----------------------------------------------------------------- driver

/// Frames run for one shot: the first lays out and sizes areas and modals,
/// the next ones settle them and finish any animation. The last is kept.
const FRAMES: usize = 4;

fn raw_input(points: Vec2, time: f64, events: Vec<egui::Event>)
             -> egui::RawInput {
    let mut raw = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, points)),
        time: Some(time),
        focused: true,
        events,
        ..Default::default()
    };
    raw.viewports.entry(egui::ViewportId::ROOT).or_default()
        .native_pixels_per_point = Some(1.0);
    raw
}

/// A fresh context with the app's fonts and theme, as `App::new` sets it up.
fn context(zoom: f32) -> egui::Context {
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    crate::theme::apply(&ctx);
    ctx.set_zoom_factor(zoom);
    ctx
}

/// Runs `draw` for `FRAMES` frames on a window of `pixels` at `zoom` and
/// rasterises the last one. `events` go into the last frame only.
fn shoot(ctx: &egui::Context, pixels: [usize; 2], zoom: f32,
         events: Vec<egui::Event>, mut draw: impl FnMut(&mut egui::Ui))
         -> image::RgbaImage {
    let points = vec2(pixels[0] as f32, pixels[1] as f32) / zoom;
    let mut raster = Raster::default();
    let mut canvas = Canvas::new(pixels[0], pixels[1]);
    let mut events = Some(events);
    for step in 0..FRAMES {
        let last = step + 1 == FRAMES;
        let input = match last {
            true => events.take().unwrap_or_default(),
            false => Vec::new(),
        };
        let out = ctx.run_ui(raw_input(points, step as f64, input),
                             &mut draw);
        raster.apply(&out.textures_delta);
        // egui paints an id clash as a "🔥" note in debug builds: a shot
        // that has one fails, so the matrix is the E6 check too
        let clash = clash_note(&out.shapes);
        assert!(clash.is_none(), "id clash in the shot: {clash:?}");
        if last {
            let primitives = ctx.tessellate(out.shapes,
                                            out.pixels_per_point);
            canvas.paint(&raster, &primitives, out.pixels_per_point);
        }
        raster.free(&out.textures_delta);
    }
    canvas.image()
}

/// The first "🔥" note egui painted, if any.
fn clash_note(shapes: &[egui::epaint::ClippedShape]) -> Option<String> {
    fn walk(shape: &egui::Shape) -> Option<String> {
        match shape {
            egui::Shape::Text(text) if text.galley.text().starts_with('🔥') =>
                Some(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => shapes.iter().find_map(walk),
            _ => None,
        }
    }
    shapes.iter().find_map(|clipped| walk(&clipped.shape))
}

// --------------------------------------------------------------- fixtures

/// A directory of this test process, removed when the scene is dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "bambu-snapshot-{}-{label}-{}", std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A deterministic picture: a lit bed under a dark chamber with a part on
/// it, tinted by `hue` so tiles tell apart.
fn picture(width: usize, height: usize, hue: u32) -> egui::ColorImage {
    let tint = [(hue * 67) % 256, (hue * 131 + 60) % 256,
                (hue * 29 + 120) % 256]
        .map(|c| c as f32 / 255.0);
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let (fx, fy) = (x as f32 / width as f32,
                            y as f32 / height as f32);
            let bed = fy > 0.62 && (fx - 0.5).abs() < 0.2 + 0.25 * fy;
            let part = (fx - 0.5).abs() < 0.08 && fy > 0.45 && fy < 0.7;
            let base = 0.08 + 0.18 * (1.0 - fy);
            let color: [f32; 3] = if part {
                tint
            } else if bed {
                [0.32, 0.34, 0.33]
            } else {
                [base, base * 1.05, base]
            };
            rgba.extend(color.map(|c| (c * 255.0) as u8));
            rgba.push(255);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

/// The sliced plate's picture: parts on a dark plate.
fn plate_picture() -> egui::ColorImage {
    let size = 256;
    let parts = [(40, 60, 90, 110), (130, 70, 200, 120), (80, 150, 140, 210)];
    let mut rgba = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let part = parts.iter().any(|&(x0, y0, x1, y1)|
                x >= x0 && x < x1 && y >= y0 && y < y1);
            rgba.extend_from_slice(match part {
                true => &[0x2c, 0xc9, 0x5a, 255],
                false => &[0x2a, 0x2e, 0x2c, 255],
            });
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([size, size], &rgba)
}

fn line(size: u64, date: &str, name: &str, dir: bool) -> String {
    let kind = match dir {
        true => 'd',
        false => '-',
    };
    format!("{kind}rw-rw-rw-   1 root  root  {size:>9} {date} {name}")
}

fn listed(state: &mut BrowserState, dir: &str, lines: &[String]) {
    let entries = lines.iter()
        .filter_map(|line| parse_list_line(dir, line, DateRule::CalendarYear,
                                           2026))
        .collect();
    let generation = state.generation();
    state.apply(Event::Listed { dir: dir.to_string(), generation,
                                result: Ok(entries) });
}

const TIMELAPSES: usize = 14;

/// Everything the three tabs show, for one printer. A thumbnail is always
/// in flight, so the view never asks for another (section 4).
fn browsed() -> BrowserState {
    let mut state = BrowserState::default();
    let _ = state.refresh();
    listed(&mut state, "/", &[
        line(0, "Oct 08 2025", "cache", true),
        line(0, "Jan 01 1980", "timelapse", true),
        line(0, "Jan 01 1980", "ipcam", true),
        line(0, "Oct 27 2025", "model", true),
        line(0, "Oct 27 2025", "spool", true),
        line(0, "Oct 27 2025", "calibration_data", true),
        line(7_234_567, "Sep 07 19:38",
             "Soporte-Altavoz-650-MT_v3.gcode.3mf", false),
        line(52_341, "Sep 11 23:07", "job.gcode.3mf", false),
        line(1_203_441, "Aug 30 10:12",
             "a1_manual_bed_screws_adjust_assist.gcode", false),
    ]);
    listed(&mut state, "/cache", &[
        line(41_123_456, "Sep 07 19:39",
             "Soporte-Altavoz-650-MT_v3_plate_1.gcode", false),
        line(11_234, "Sep 07 19:39",
             "1_Soporte-Altavoz-650-MT_v3.gcode.bbl", false),
        line(52_341, "Sep 11 23:08", "job.3mf", false),
    ]);
    listed(&mut state, "/model", &[
        line(3_112_004, "Oct 27 2025", "Benchy.gcode.3mf", false),
    ]);
    let mut videos = vec![line(0, "May 30 05:16", "thumbnail", true)];
    let mut thumbs = Vec::new();
    for index in 0..TIMELAPSES {
        let (month, number, day) = match index < 8 {
            true => ("Jul", 7, 25 - index),
            false => ("Jun", 6, 28 - index),
        };
        let stem =
            format!("video_2026-{number:02}-{day:02}_05-14-{index:02}");
        let date = format!("{month} {day:02} 14:05");
        // the orphan: its video was deleted, its thumbnail stayed
        if index != 5 {
            videos.push(line(4_411_548 + index as u64 * 7_300_000, &date,
                             &format!("{stem}.avi"), false));
        }
        thumbs.push(line(19_830, &date, &format!("{stem}.jpg"), false));
    }
    listed(&mut state, "/timelapse", &videos);
    listed(&mut state, "/timelapse/thumbnail", &thumbs);
    let _ = state.open_recordings();
    listed(&mut state, "/ipcam", &[
        line(13_421_772, "Jun 02 06:17", "ipcam-record.2026-06-02.1.avi",
             false),
        line(13_421_772, "Jun 01 06:17", "ipcam-record.2026-06-01.1.avi",
             false),
        line(9_812_331, "May 31 22:40", "ipcam-record.2026-05-31.3.avi",
             false),
    ]);
    let thumbs: Vec<String> = state.timelapses.iter()
        .filter_map(|item| item.thumb.as_ref().map(|t| t.path.clone()))
        .collect();
    for (index, path) in thumbs.into_iter().enumerate() {
        let thumb = match index {
            2 => ThumbState::Loading,
            4 => ThumbState::Failed(FtpError::Truncated { got: 1, want: 2 }),
            _ => ThumbState::Ready(picture(320, 180, index as u32)),
        };
        state.thumbs.insert(path, thumb);
    }
    state
}

fn ams(humidity: bool) -> Value {
    let tray = |id: u32, kind: &str, color: &str, remain: i64| json!({
        "id": id.to_string(), "tray_type": kind, "tray_color": color,
        "remain": remain, "tag_uid": "A1B2C3D4E5F60718",
    });
    let mut unit = json!({
        "id": "0",
        "tray": [tray(0, "PLA", "FF6A13FF", 82),
                 tray(1, "PETG", "161616FF", 40),
                 tray(2, "PLA", "F4EE2AFF", 7),
                 tray(3, "", "", -1)],
    });
    if humidity {
        unit["humidity"] = json!("4");
    }
    json!({ "ams": [unit], "tray_now": "1" })
}

fn telemetry(gcode_state: &str, humidity: bool) -> Value {
    let printing = matches!(gcode_state, "RUNNING" | "PAUSE");
    let mut state = json!({
        "gcode_state": gcode_state,
        "nozzle_temper": if printing { 219.6 } else { 27.8 },
        "nozzle_target_temper": if printing { 220 } else { 0 },
        "bed_temper": if printing { 64.9 } else { 26.1 },
        "bed_target_temper": if printing { 65 } else { 0 },
        "spd_lvl": 2,
        "cooling_fan_speed": if printing { "15" } else { "0" },
        "big_fan1_speed": "0",
        "big_fan2_speed": if printing { "10" } else { "0" },
        "lights_report": [{ "node": "chamber_light", "mode": "on" }],
        "nozzle_type": "stainless_steel",
        "nozzle_diameter": "0.4",
        "ams": ams(humidity),
        "vt_tray": { "tray_type": "" },
        "mc_percent": 0,
    });
    if printing {
        state["subtask_name"] = json!("Soporte-Altavoz-650-MT_v3");
        state["gcode_file"] = json!("Soporte-Altavoz-650-MT_v3.gcode.3mf");
        state["mc_percent"] = json!(42);
        state["mc_remaining_time"] = json!(83);
        state["layer_num"] = json!(120);
        state["total_layer_num"] = json!(310);
        state["print_type"] = json!("local");
    }
    state
}

fn device_info() -> Value {
    json!({ "module": [
        { "name": "ota", "sw_ver": "01.08.02.00" },
        { "name": "mc", "sw_ver": "00.00.35.64", "hw_ver": "MC07" },
        { "name": "ams_f1/0", "sw_ver": "00.00.07.97", "hw_ver": "AMS_F102" },
        { "name": "th", "sw_ver": "00.00.07.77", "hw_ver": "TH07" },
    ]})
}

struct PrinterFixture {
    name: &'static str,
    serial: &'static str,
    gcode_state: &'static str,
    online: bool,
}

/// One printer, with nothing that can connect (see the module comment).
fn printer_ui(ctx: &egui::Context, cache: &Arc<cache::Cache>,
              fixture: &PrinterFixture) -> PrinterUi {
    let cfg = config::PrinterCfg {
        name: fixture.name.to_string(),
        ip: "192.0.2.10".to_string(),
        serial: fixture.serial.to_string(),
        access_code: String::new(),
    };
    // the empty serial is what keeps the client from opening a socket
    let client = mqtt::PrinterClient::start("", "", "", ctx.clone());
    let humidity = !config::model_from_serial(fixture.serial).contains("A1");
    let Value::Object(state) = telemetry(fixture.gcode_state, humidity)
    else {
        unreachable!("telemetry is an object");
    };
    *client.state.lock().unwrap() = state;
    *client.device_info.lock().unwrap() = device_info();
    *client.conn.lock().unwrap() = match fixture.online {
        true => (true, "online".to_string()),
        false => (false, format!("offline: {}",
            mqtt::ProbeOutcome::Tls("handshake failed".into()).text())),
    };
    let ftp = crate::browser::FtpWorker::start(&cfg, ctx, cache.clone());
    PrinterUi {
        cfg,
        client,
        camera: None,
        cam_texture: None,
        fw_latest_slot: Arc::new(Mutex::new(None)),
        fw_latest: "01.08.02.00".to_string(),
        ftp,
        cache: cache.clone(),
        browser: BrowserState::default(),
        files: FilesUi::default(),
        player: None,
        player_tex: None,
        player_error: None,
        open_error: None,
        shell_openable: RefCell::new(HashMap::new()),
        cached_copy: RefCell::new(HashMap::new()),
        job_bundle: None,
        plate_texture: None,
        current_job: String::new(),
        last_job: String::new(),
        last_gcode_state: String::new(),
        hms_codes: Vec::new(),
        hms_lines: Vec::new(),
        light_pending: None,
        light_unconfirmed: false,
        opening: None,
    }
}

const A1: &str = "03919A0000000001";
const A1_COMBO: &str = "03919A0000000002";
const P1S: &str = "01P00A0000000003";

const PRINTERS: [PrinterFixture; 3] = [
    PrinterFixture { name: "A1 #1", serial: A1, gcode_state: "IDLE",
                     online: true },
    PrinterFixture { name: "A1 Combo #1", serial: A1_COMBO,
                     gcode_state: "RUNNING", online: true },
    PrinterFixture { name: "P1S", serial: P1S, gcode_state: "RUNNING",
                     online: false },
];

fn app(ctx: &egui::Context, printers: &[PrinterFixture], dir: &TempDir)
       -> App {
    let cache = cache::Cache::at(dir.0.join("cache"),
                                 5 * 1024 * 1024 * 1024);
    let printers = printers.iter()
        .map(|fixture| printer_ui(ctx, &cache, fixture))
        .collect();
    App {
        cfg: config::Config::default(),
        printers,
        selected: 0,
        dialog: Dialog::None,
        view: AppView::Panel,
        started: true,
        store: config::Store::load().0,
        cache,
        cache_usage: Arc::new(Mutex::new(None)),
        cache_asked_at: None,
        clearing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config_error: None,
        pending_close: false,
        closing: false,
        pending_edit: None,
        #[cfg(debug_assertions)]
        debug_play: None,
    }
}

/// The job a RUNNING printer is on: its objects, their boxes and the plate
/// picture, as the job bundle brings them.
fn job(printer: &mut PrinterUi, ctx: &egui::Context) {
    job_of(printer, ctx, 8);
}

/// The same job with `count` objects, up to twelve.
fn job_of(printer: &mut PrinterUi, ctx: &egui::Context, count: usize) {
    let names = ["Soporte base", "Soporte base #2", "Tapa", "Tapa #2",
                 "Clip", "Clip #2", "Clip #3", "Tornillo largo M3x40",
                 "Arandela", "Arandela #2", "Tuerca", "Tuerca #2"];
    let objects: Vec<(i64, String)> = names.iter().take(count).enumerate()
        .map(|(i, name)| (100 + i as i64 * 2, name.to_string()))
        .collect();
    let bboxes = objects.iter().enumerate()
        .map(|(i, (id, _))| {
            let (col, row) = ((i % 4) as f32, (i / 4) as f32);
            (*id, [30.0 + col * 52.0, 40.0 + row * 90.0,
                   70.0 + col * 52.0, 110.0 + row * 90.0])
        })
        .collect();
    printer.job_bundle = Some(files::JobBundle {
        objects,
        bboxes,
        skipped: HashSet::from([106]),
        label_objects: true,
        ..Default::default()
    });
    printer.plate_texture = Some(ctx.load_texture(
        "plate-snapshot", plate_picture(), Default::default()));
    printer.current_job = "Soporte-Altavoz-650-MT_v3".to_string();
    printer.last_job = printer.current_job.clone();
    printer.last_gcode_state = "RUNNING".to_string();
}

fn camera_frame(printer: &mut PrinterUi, ctx: &egui::Context) {
    printer.cam_texture = Some(ctx.load_texture(
        "cam-snapshot", picture(1280, 720, 3), Default::default()));
}

/// The scenes of the 0.6 matrix.
const MATRIX: [&str; 13] = [
    "panel-idle", "panel-printing", "panel-offline",
    "files-timelapses", "files-recordings", "files-printfiles",
    "files-3mf", "files-player", "files-transfers",
    "dlg-skip", "dlg-move", "dlg-info", "dlg-maintenance",
];

/// States the stages look at beyond the matrix, at the two smaller sizes.
const EXTRAS: [&str; 26] = [
    "chips-six", "panel-firmware", "files-loading", "files-empty",
    "files-error", "files-refusal", "files-folders", "files-gcode",
    "dlg-add", "dlg-temp", "dlg-speed", "dlg-fans", "dlg-confirm-stop",
    "dlg-confirm-remove", "dlg-hms", "no-printers", "dlg-skip-many",
    "dlg-skip-confirm", "files-failed-four", "files-long-name",
    "files-printing", "files-open-error", "files-companion",
    "panel-light-unconfirmed", "dlg-temp-invalid", "dlg-edit",
];

fn files_scene(app: &mut App, tab: Tab) -> &mut PrinterUi {
    app.view = AppView::Files;
    let printer = &mut app.printers[0];
    printer.browser = browsed();
    printer.files.tab = tab;
    printer
}

/// What a scene needs to stay alive while it is shot.
struct Scene {
    app: App,
    _dir: TempDir,
}

fn scene(name: &str, ctx: &egui::Context) -> Scene {
    let dir = TempDir::new(name);
    let mut app = app(ctx, &PRINTERS, &dir);
    for index in [0, 1] {
        camera_frame(&mut app.printers[index], ctx);
    }
    job(&mut app.printers[1], ctx);
    job(&mut app.printers[2], ctx);
    match name {
        "panel-idle" => {}
        "panel-printing" => app.selected = 1,
        "panel-offline" => {
            app.selected = 2;
            app.printers[2].camera = Some(lost_camera(ctx));
        }
        "panel-firmware" =>
            app.printers[0].fw_latest = "01.09.00.00".to_string(),
        // the light command went and telemetry never agreed (C18, D40)
        "panel-light-unconfirmed" =>
            app.printers[0].light_unconfirmed = true,
        "files-timelapses" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            printer.files.selected = printer.browser.timelapses[1].video
                .as_ref().map(|video| video.path.clone());
        }
        "files-recordings" => {
            let printer = files_scene(&mut app, Tab::Recordings);
            printer.files.selected =
                Some("/ipcam/ipcam-record.2026-06-01.1.avi".to_string());
        }
        "files-printfiles" => {
            let printer = files_scene(&mut app, Tab::Files);
            printer.files.selected = Some(
                "/a1_manual_bed_screws_adjust_assist.gcode".to_string());
        }
        "files-3mf" => {
            let printer = files_scene(&mut app, Tab::Files);
            let path = "/job.gcode.3mf";
            printer.files.selected = Some(path.to_string());
            printer.browser.details.insert(path.to_string(),
                DetailState::Ready(Box::new(three_mf())));
        }
        "files-gcode" => {
            let printer = files_scene(&mut app, Tab::Files);
            printer.files.selected = Some(
                "/cache/Soporte-Altavoz-650-MT_v3_plate_1.gcode".to_string());
        }
        "files-player" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            open_player(printer, ctx, &dir.0);
        }
        "files-transfers" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            transfers(&mut printer.browser);
        }
        "files-loading" => {
            app.view = AppView::Files;
            let _ = app.printers[0].browser.refresh();
        }
        "files-empty" => {
            app.view = AppView::Files;
            let state = &mut app.printers[0].browser;
            let _ = state.refresh();
            listed(state, "/", &[line(0, "Jan 01 1980", "timelapse", true)]);
            listed(state, "/timelapse", &[]);
        }
        "files-error" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            printer.browser.error = Some(FtpError::AuthRejected);
        }
        "files-refusal" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            printer.browser.cert_alert = Some(crate::tls::Refusal::Cert(
                crate::tls::PrinterCertError::SerialMismatch));
        }
        "files-folders" => {
            let printer = files_scene(&mut app, Tab::Files);
            printer.files.shown = crate::ui::files_view::Shown::Folders;
        }
        "chips-six" => {
            let six = [
                PrinterFixture { name: "A1 #1", serial: A1,
                                 gcode_state: "IDLE", online: true },
                PrinterFixture { name: "A1 Combo #1", serial: A1_COMBO,
                                 gcode_state: "RUNNING", online: true },
                PrinterFixture { name: "P1S", serial: P1S,
                                 gcode_state: "PAUSE", online: true },
                PrinterFixture { name: "Garage workshop X1 Carbon",
                                 serial: "00M00A0000000004",
                                 gcode_state: "FINISH", online: true },
                PrinterFixture { name: "Office A1 mini",
                                 serial: "03000A0000000005",
                                 gcode_state: "FAILED", online: true },
                PrinterFixture { name: "Spare P1P",
                                 serial: "01S00A0000000006",
                                 gcode_state: "", online: false },
            ];
            app = self::app(ctx, &six, &dir);
            camera_frame(&mut app.printers[0], ctx);
            let video = browsed().timelapses[0].video.clone()
                .expect("a video");
            let state = &mut app.printers[2].browser;
            let Some(Cmd::Download { id, .. }) =
                state.download(&video, Dest::SaveToPc)
            else {
                panic!("a download");
            };
            state.apply(Event::Progress { id, done: 1_000_000,
                                          total: 4_411_548,
                                          bytes_per_s: 205_000.0 });
        }
        "dlg-skip" => {
            app.selected = 1;
            app.dialog = Dialog::Skip(dialogs::SkipDlg {
                selected: HashSet::from([100, 104]),
                confirm: false,
            });
        }
        "dlg-move" => app.dialog = Dialog::Move,
        "dlg-info" => {
            app.printers[0].fw_latest = "01.09.00.00".to_string();
            app.dialog = Dialog::Info(dialogs::InfoDlg::new(
                "01.08.02.00", "01.09.00.00"));
        }
        "dlg-maintenance" => app.dialog = Dialog::Maintenance(
            dialogs::MaintenanceDlg::new("stainless_steel", 0.4)),
        "dlg-edit" => app.dialog = Dialog::AddPrinter(
            dialogs::AddPrinterDlg {
                draft: config::PrinterCfg {
                    name: "Garage P1S".to_string(),
                    ip: "192.0.2.44".to_string(),
                    serial: "01P00A000000001".to_string(),
                    access_code: "12345678".to_string(),
                },
                editing: Some(0),
                error: String::new(),
            }),
        "dlg-add" => app.dialog = Dialog::AddPrinter(
            dialogs::AddPrinterDlg {
                draft: config::PrinterCfg {
                    name: "Garage P1S".to_string(),
                    ip: "192.0.2.44".to_string(),
                    ..Default::default()
                },
                editing: None,
                error: "IP, serial and access code are required."
                    .to_string(),
            }),
        "dlg-temp" => app.dialog = Dialog::Temp(dialogs::TempDlg {
            nozzle: true, value: "220".to_string() }),
        // a value the printer would refuse: the button says the range
        "dlg-temp-invalid" => app.dialog = Dialog::Temp(dialogs::TempDlg {
            nozzle: true, value: "420".to_string() }),
        "dlg-speed" => {
            app.selected = 1;
            app.dialog = Dialog::Speed;
        }
        "dlg-fans" => {
            app.selected = 1;
            app.dialog = Dialog::Fans;
        }
        "dlg-confirm-stop" => {
            app.selected = 1;
            app.dialog = Dialog::ConfirmStop;
        }
        "dlg-confirm-remove" => app.dialog = Dialog::ConfirmRemove(0),
        "dlg-hms" => {
            app.selected = 1;
            app.printers[1].client.state.lock().unwrap().insert(
                "hms".to_string(),
                json!([{ "attr": 0x0700_2000u64, "code": 0x0002_0001u64 }]));
            // the banner's lines are resolved in `sync`, which a shot
            // does not run: the scene resolves them itself (D38)
            app.printers[1].sync_hms(ctx);
            app.dialog = Dialog::Hms;
        }
        "no-printers" => app.printers.clear(),
        "dlg-skip-many" | "dlg-skip-confirm" => {
            app.selected = 1;
            job_of(&mut app.printers[1], ctx, 12);
            app.dialog = Dialog::Skip(dialogs::SkipDlg {
                selected: HashSet::from([100, 104]),
                confirm: name == "dlg-skip-confirm",
            });
        }
        "files-failed-four" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            let videos: Vec<RemoteEntry> = printer.browser.timelapses.iter()
                .filter_map(|item| item.video.clone())
                .take(4)
                .collect();
            for video in &videos {
                let Some(Cmd::Download { id, .. }) = printer.browser
                    .download(video, Dest::SaveToPc)
                else {
                    panic!("a download");
                };
                printer.browser.apply(Event::Done {
                    id, result: Err(FtpError::SessionLost(
                        "connection reset".to_string())) });
            }
        }
        "files-long-name" => {
            let printer = files_scene(&mut app, Tab::Timelapses);
            printer.cfg.name =
                "Taller del fondo, impresora grande junto a la ventana 60c"
                    .to_string();
        }
        "files-printing" => {
            app.selected = 1;
            app.view = AppView::Files;
            let printer = &mut app.printers[1];
            printer.browser = browsed();
        }
        "files-open-error" => {
            files_scene(&mut app, Tab::Recordings).open_error = Some(
                "couldn't open the file (no program is associated with it)"
                    .to_string());
            app.config_error = Some(
                "settings couldn't be saved: access denied".to_string());
        }
        "files-companion" => {
            let printer = files_scene(&mut app, Tab::Files);
            let path = "/cache/job.3mf";
            printer.files.selected = Some(path.to_string());
            // already read, so the view has nothing to ask the worker for
            printer.browser.details.insert(path.to_string(),
                DetailState::Ready(Box::new(three_mf())));
        }
        other => panic!("no scene {other:?}"),
    }
    Scene { app, _dir: dir }
}

/// A camera that lost its printer. It is started on an empty serial, so it
/// fails closed at once; the status is then what a lost connection says.
fn lost_camera(ctx: &egui::Context) -> Arc<camera::Camera> {
    let camera = camera::Camera::start("192.0.2.10".into(), String::new(),
                                       String::new(), ctx.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    while camera.status.lock().unwrap().starts_with("connecting")
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    *camera.status.lock().unwrap() =
        "camera retry: connection timed out".to_string();
    camera
}

fn three_mf() -> ThreeMf {
    ThreeMf {
        info: ThreeMfInfo {
            plate: Some(1),
            has_gcode: true,
            printer_model: "Bambu Lab A1".to_string(),
            prediction_s: Some(5_978),
            weight_g: Some(38.42),
            layers: Some(212),
            max_z_mm: Some(42.4),
            bed_type: "Textured PEI Plate".to_string(),
            filaments: vec![
                Filament { kind: "PLA".to_string(),
                           color: "#FF6A13".to_string(),
                           used_g: Some(30.12), used_m: Some(9.9) },
                Filament { kind: "PETG".to_string(),
                           color: "#161616".to_string(),
                           used_g: Some(8.3), used_m: Some(2.7) },
            ],
            objects: vec![(1, "Soporte".to_string()),
                          (2, "Tapa".to_string())],
            warnings: vec!["not_support_traditional_timelapse".to_string()],
            ..Default::default()
        },
        plate: Some(Picture(plate_picture())),
    }
}

/// Two downloads running and one that failed, as the bar shows them.
fn transfers(state: &mut BrowserState) {
    let videos: Vec<RemoteEntry> = state.timelapses.iter()
        .filter_map(|item| item.video.clone())
        .take(3)
        .collect();
    let mut ids = Vec::new();
    for (index, video) in videos.iter().enumerate() {
        let dest = match index {
            1 => Dest::SaveToPc,
            _ => Dest::Cache { open_after: true },
        };
        let Some(Cmd::Download { id, .. }) = state.download(video, dest)
        else {
            panic!("a download");
        };
        ids.push((id, video.size));
    }
    for &(id, size) in &ids[..2] {
        state.apply(Event::Progress { id, done: size / 3, total: size,
                                      bytes_per_s: 205_000.0 });
    }
    state.apply(Event::Done {
        id: ids[2].0, result: Err(FtpError::Truncated { got: 1, want: 2 }) });
}

/// A real player on a synthetic AVI, paused on a known frame, with a
/// texture of its own: the decode thread's frames are never taken.
fn open_player(printer: &mut PrinterUi, ctx: &egui::Context, dir: &Path) {
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(16, 16))
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .expect("encode");
    let frames = vec![jpeg.into_inner(); 1128];
    let avi = crate::avi::test_avi(1280, 720, 41_667, &frames, b"MJPG");
    let path = dir.join("video_2026-07-24_05-14-01.avi");
    std::fs::write(&path, avi).expect("write the AVI");
    let player = player::MjpegPlayer::open(path, printer.cache.clone(),
                                           ctx.clone())
        .expect("a playable AVI");
    player.send(player::PlayerCmd::Pause);
    std::thread::sleep(Duration::from_millis(50));
    player.pos.store(504, Ordering::SeqCst);
    printer.player_tex = Some(ctx.load_texture(
        "play-snapshot", picture(1280, 720, 9), Default::default()));
    printer.player = Some(player);
}

// ------------------------------------------------------------------ tests

/// The default window, the minimum one (960x640 since O2) and a full HD
/// screen.
const SIZES: [[usize; 2]; 3] = [[1080, 780], [960, 640], [1920, 1080]];
const ZOOMS: [f32; 2] = [1.0, 1.5];

fn file_name(scene: &str, size: [usize; 2], zoom: f32) -> String {
    format!("{scene}-{}x{}@{zoom:.1}.png", size[0], size[1])
}

fn dir_from(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from)
        .unwrap_or_else(|| panic!("{var} is not set"))
}

/// Every matrix scene at every size and zoom, and the extras at the two
/// smaller sizes, into `BAMBU_SNAPSHOT_DIR`.
#[test]
#[ignore = "writes PNGs; run on demand for the 0.6 matrix"]
fn screenshot_matrix() {
    let dir = dir_from("BAMBU_SNAPSHOT_DIR");
    std::fs::create_dir_all(&dir).expect("output dir");
    let only = std::env::var("BAMBU_SNAPSHOT_ONLY").ok();
    let wanted = |name: &str| only.as_deref()
        .is_none_or(|only| only.split(',').any(|o| name.starts_with(o)));
    let mut shots: Vec<(&str, [usize; 2], f32)> = Vec::new();
    for name in MATRIX {
        for size in SIZES {
            for zoom in ZOOMS {
                shots.push((name, size, zoom));
            }
        }
    }
    for name in EXTRAS {
        shots.push((name, SIZES[0], 1.0));
        shots.push((name, SIZES[1], 1.0));
    }
    for (name, size, zoom) in shots {
        if !wanted(name) {
            continue;
        }
        let ctx = context(zoom);
        let mut scene = scene(name, &ctx);
        let mut frame = eframe::Frame::_new_kittest();
        let image = shoot(&ctx, size, zoom, Vec::new(),
                          |ui| scene.app.ui(ui, &mut frame));
        for printer in &scene.app.printers {
            assert_eq!(printer.ftp.status().sessions_opened, 0,
                       "{name}: a worker opened an FTP session");
            // a preview in Loading is one the view asked the worker for;
            // the session count alone passes when the connect is still
            // under way, so this is the check that holds
            assert!(!printer.browser.details.values()
                        .any(|detail| matches!(detail, DetailState::Loading)),
                    "{name}: the view asked the worker for a preview");
        }
        image.save(dir.join(file_name(name, size, zoom)))
            .expect("write the PNG");
    }
}

/// Pixel diff of two matrix runs: prints what changed per file and writes
/// the after image, dimmed, with the changed pixels in magenta.
#[test]
#[ignore = "reads two matrix runs; run on demand"]
fn snapshot_diff() {
    let before = dir_from("BAMBU_SNAPSHOT_BEFORE");
    let after = dir_from("BAMBU_SNAPSHOT_AFTER");
    let out = dir_from("BAMBU_SNAPSHOT_DIR");
    std::fs::create_dir_all(&out).expect("output dir");
    let mut names: Vec<String> = std::fs::read_dir(&after).expect("AFTER")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".png"))
        .collect();
    names.sort();
    for name in names {
        let Ok(old) = image::open(before.join(&name)) else {
            println!("{name}: new");
            continue;
        };
        let new = image::open(after.join(&name)).expect("after image");
        let (old, new) = (old.to_rgba8(), new.to_rgba8());
        if old.dimensions() != new.dimensions() {
            println!("{name}: size {:?} -> {:?}", old.dimensions(),
                     new.dimensions());
            continue;
        }
        let mut changed = 0u64;
        let mut bounds: Option<[u32; 4]> = None;
        let mut marked = new.clone();
        for (x, y, pixel) in marked.enumerate_pixels_mut() {
            if old.get_pixel(x, y) != new.get_pixel(x, y) {
                changed += 1;
                let b = bounds.get_or_insert([x, y, x, y]);
                *b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                *pixel = image::Rgba([255, 0, 255, 255]);
            } else {
                pixel.0[..3].iter_mut().for_each(|c| *c /= 3);
            }
        }
        match bounds {
            None => println!("{name}: identical"),
            Some(b) => {
                println!("{name}: {changed} px in {},{} - {},{}",
                         b[0], b[1], b[2], b[3]);
                marked.save(out.join(&name)).expect("write the diff");
            }
        }
    }
}

/// The rasteriser the matrix relies on. A rectangle lands where it was
/// painted and nowhere else, and a translucent rectangle — two triangles
/// sharing a diagonal — is blended once per pixel, the diagonal included.
#[test]
fn the_rasteriser_paints_each_pixel_once() {
    let ctx = context(1.0);
    let red = Color32::from_rgb(200, 30, 30);
    let veil = Color32::from_black_alpha(128);
    // the veil is square, so its diagonal runs through pixel centres
    let image = shoot(&ctx, [64, 32], 1.0, Vec::new(), |ui| {
        let painter = ui.painter();
        painter.rect_filled(Rect::from_min_size(Pos2::ZERO,
                                                vec2(64.0, 32.0)),
                            0.0, Color32::WHITE);
        painter.rect_filled(Rect::from_min_max(pos2(8.0, 8.0),
                                               pos2(24.0, 24.0)),
                            0.0, red);
        painter.rect_filled(Rect::from_min_max(pos2(32.0, 0.0),
                                               pos2(64.0, 32.0)),
                            0.0, veil);
    });
    let at = |x, y| image.get_pixel(x, y).0;
    // inside the red square, and a control outside it
    assert_eq!(at(16, 16), [200, 30, 30, 255]);
    assert_eq!(at(4, 4), [255, 255, 255, 255]);
    // the veil, once over white, on a pixel off both diagonals: 255 x
    // (1 - 128/255) = 127; and the same on every pixel, diagonals included
    let veiled = at(50, 5);
    assert_eq!(veiled, [127, 127, 127, 255]);
    for y in 0..32 {
        for x in 32..64 {
            assert_eq!(at(x, y), veiled, "blended twice at {x},{y}");
        }
    }
}

/// egui uploads the font atlas once per context, so a second shot on the
/// same context would find no texture for its text. It fails loudly rather
/// than rendering a picture with the meshes missing.
#[test]
#[should_panic(expected = "no texture")]
fn a_context_shot_twice_is_refused() {
    let ctx = context(1.0);
    let draw = |ui: &mut egui::Ui| {
        ui.painter().rect_filled(Rect::from_min_size(Pos2::ZERO,
                                                     vec2(8.0, 8.0)),
                                 0.0, Color32::WHITE);
    };
    // the first shot is fine: this is the control
    let first = shoot(&ctx, [8, 8], 1.0, Vec::new(), draw);
    assert_eq!(first.get_pixel(4, 4).0, [255, 255, 255, 255]);
    shoot(&ctx, [8, 8], 1.0, Vec::new(), draw);
}

/// O7: the app opens at the zoom it was left at, and a zoom change is
/// picked up so the next launch carries it.
#[test]
fn the_app_opens_at_the_saved_zoom_and_keeps_changes() {
    let ctx = context(1.0);
    let dir = TempDir::new("zoom-level");
    // no printers: the start path then opens no camera and no session,
    // and what is left of it is the zoom
    let mut app = app(&ctx, &[], &dir);
    app.started = false;
    app.cfg.ui.zoom = 1.25;
    let mut frame = eframe::Frame::_new_kittest();
    let points = vec2(1080.0, 780.0);
    let mut run = |app: &mut App, step: f64| {
        let _ = ctx.run_ui(raw_input(points, step, Vec::new()), |ui| {
            eframe::App::logic(app, ui.ctx(), &mut frame);
            app.ui(ui, &mut frame);
        });
    };
    // egui takes a new zoom at the start of the next pass
    run(&mut app, 0.0);
    run(&mut app, 1.0);
    assert!((ctx.zoom_factor() - 1.25).abs() < 0.001,
            "opened at {}", ctx.zoom_factor());
    // Ctrl+scroll is egui's own; what the app owes is to notice
    ctx.set_zoom_factor(1.5);
    run(&mut app, 2.0);
    run(&mut app, 3.0);
    assert!((app.cfg.ui.zoom - 1.5).abs() < 0.001,
            "the change was not picked up: {}", app.cfg.ui.zoom);
    // and it is not written again while nothing changes
    let before = app.cfg.ui.zoom;
    run(&mut app, 4.0);
    assert_eq!(app.cfg.ui.zoom, before);
}

/// O13: Remove printer lives in the Edit dialog, at the other end of the
/// row from Save, and nowhere else. The top bar holds Edit and Add.
#[test]
fn remove_printer_lives_in_the_edit_dialog() {
    use egui::accesskit::Action;

    /// The names of everything clickable, and every piece of text.
    fn shown(app: &mut App, ctx: &egui::Context) -> (Vec<String>,
                                                     Vec<String>) {
        let mut frame = eframe::Frame::_new_kittest();
        let points = vec2(1080.0, 780.0);
        let mut full = None;
        for step in 0..3 {
            full = Some(ctx.run_ui(
                raw_input(points, f64::from(step), Vec::new()),
                |ui| app.ui(ui, &mut frame)));
        }
        let full = full.expect("three frames");
        let update = full.platform_output.accesskit_update.clone()
            .expect("an AccessKit tree");
        let named = update.nodes.iter()
            .filter(|(_, node)| node.supports_action(Action::Click))
            .filter_map(|(_, node)| node.label().map(str::to_owned))
            .collect();
        let mut texts = Vec::new();
        fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) =>
                    found.push(text.galley.text().to_string()),
                egui::Shape::Vec(shapes) =>
                    shapes.iter().for_each(|shape| walk(shape, found)),
                _ => {}
            }
        }
        for clipped in &full.shapes {
            walk(&clipped.shape, &mut texts);
        }
        (named, texts)
    }

    let ctx = context(1.0);
    ctx.enable_accesskit();
    let dir = TempDir::new("remove-printer");
    let mut app = app(&ctx, &PRINTERS, &dir);
    // the panel: two tools, and no way to remove a printer from here
    let (named, _) = shown(&mut app, &ctx);
    assert!(named.iter().any(|name| name == "Edit current printer"),
            "{named:?}");
    assert!(named.iter().any(|name| name == "Add printer"), "{named:?}");
    assert!(!named.iter().any(|name| name.contains("Remove")),
            "the bar still offers a removal: {named:?}");

    // Add printer: a form with no printer to remove
    app.dialog = Dialog::AddPrinter(dialogs::AddPrinterDlg {
        draft: config::PrinterCfg::default(),
        editing: None,
        error: String::new(),
    });
    let (_, texts) = shown(&mut app, &ctx);
    assert!(texts.iter().any(|text| text == "Add printer"), "{texts:?}");
    assert!(!texts.iter().any(|text| text == "Remove printer"),
            "Add offers a removal: {texts:?}");

    // Edit printer: Remove is there, and the row keeps Save away from it
    app.dialog = Dialog::AddPrinter(dialogs::AddPrinterDlg {
        draft: app.printers[0].cfg.clone(),
        editing: Some(0),
        error: String::new(),
    });
    let (_, texts) = shown(&mut app, &ctx);
    for wanted in ["Edit printer", "Remove printer", "Save", "Cancel"] {
        assert!(texts.iter().any(|text| text == wanted),
                "{wanted} is missing: {texts:?}");
    }
}

/// O13 and 5.4: the removal still goes through its confirmation, and the
/// printer is gone only once that is answered.
#[test]
fn removing_a_printer_asks_first() {
    /// Where `label` was painted, if it was.
    fn spot(full: &egui::FullOutput, label: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::Shape, label: &str,
                found: &mut Option<egui::Pos2>) {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label =>
                    *found = Some(text.pos
                                  + text.galley.rect.size() * 0.5),
                egui::Shape::Vec(shapes) =>
                    shapes.iter().for_each(|shape| walk(shape, label,
                                                        found)),
                _ => {}
            }
        }
        let mut found = None;
        for clipped in &full.shapes {
            walk(&clipped.shape, label, &mut found);
        }
        found
    }

    let ctx = context(1.0);
    let dir = TempDir::new("remove-asks");
    let mut app = app(&ctx, &PRINTERS, &dir);
    let mut frame = eframe::Frame::_new_kittest();
    let points = vec2(1080.0, 780.0);
    let before = app.printers.len();
    assert!(before >= 2, "{before} printers");
    app.dialog = Dialog::AddPrinter(dialogs::AddPrinterDlg {
        draft: app.printers[0].cfg.clone(),
        editing: Some(0),
        error: String::new(),
    });
    // a click is a press and a release inside egui's click window
    // (`max_click_duration`, 0.6 s), so the frames are 50 ms apart
    let mut clock = 0.0;
    let mut run = |app: &mut App, events: Vec<egui::Event>| {
        clock += 0.05;
        ctx.run_ui(raw_input(points, clock, events),
                   |ui| app.ui(ui, &mut frame))
    };
    let mut click = |app: &mut App, label: &str| {
        // a modal's first frame is egui's invisible sizing pass, so the
        // label has a place only from the second one on
        run(app, Vec::new());
        let at = spot(&run(app, Vec::new()), label)
            .unwrap_or_else(|| panic!("{label} was not painted"));
        let button = |pressed| egui::Event::PointerButton {
            pos: at, button: egui::PointerButton::Primary, pressed,
            modifiers: egui::Modifiers::default(),
        };
        let moved = egui::Event::PointerMoved(at);
        run(app, vec![moved.clone()]);
        run(app, vec![moved.clone(), button(true)]);
        run(app, vec![moved, button(false)]);
    };

    click(&mut app, "Remove printer");
    assert_eq!(app.printers.len(), before,
               "the printer went without a question");
    assert!(matches!(app.dialog, Dialog::ConfirmRemove(0)),
            "no confirmation after Remove printer");
    // the question names the printer and the danger button answers it
    click(&mut app, "Remove");
    assert_eq!(app.printers.len(), before - 1,
               "the confirmation did not remove it");
    assert!(matches!(app.dialog, Dialog::None), "the dialog stayed open");
}

/// O11: a dialog is a card over the canvas. With the old `BG` fill its
/// only edge was the outline, and on the canvas that read as flat.
#[test]
fn a_dialog_is_filled_like_a_card() {
    let ctx = context(1.0);
    let dir = TempDir::new("modal-fill");
    let mut app = app(&ctx, &PRINTERS, &dir);
    app.dialog = Dialog::Temp(dialogs::TempDlg {
        nozzle: true, value: "220".to_string() });
    let mut frame = eframe::Frame::_new_kittest();
    let points = vec2(1080.0, 780.0);
    let mut rects: Vec<(egui::Rect, egui::Color32)> = Vec::new();
    fn walk(shape: &egui::Shape,
            found: &mut Vec<(egui::Rect, egui::Color32)>) {
        match shape {
            egui::Shape::Rect(rect) => found.push((rect.rect, rect.fill)),
            egui::Shape::Vec(shapes) =>
                shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    for step in 0..3 {
        let out = ctx.run_ui(raw_input(points, f64::from(step), Vec::new()),
                             |ui| app.ui(ui, &mut frame));
        if step == 2 {
            for clipped in &out.shapes {
                walk(&clipped.shape, &mut rects);
            }
        }
    }
    // the dialog is the widest thing under the middle of the window that
    // is not the canvas behind it
    let middle = (points / 2.0).to_pos2();
    // a modal is centred on the window and exactly as wide as its size
    // token plus its own margin
    // its own hairline sits outside the margin
    let wide = crate::theme::size::MODAL_M
        + crate::theme::pad::MODAL.sum().x
        + 2.0 * crate::theme::stroke::HAIRLINE;
    let (rect, fill) = rects.iter()
        .filter(|(rect, _)| rect.contains(middle)
                && (rect.center().x - middle.x).abs() < 2.0
                && (rect.width() - wide).abs() < 1.0)
        .max_by(|a, b| a.0.area().total_cmp(&b.0.area()))
        .copied()
        .expect("a dialog centred on the window");
    assert_eq!(fill, crate::theme::CARD,
               "the dialog at {rect:?} is not filled like a card");
    assert_ne!(fill, crate::theme::BG, "the dialog matches the canvas");
}

/// A8: the tool buttons take the keyboard in the order they are painted,
/// left to right. They sit at the right end of the bar, and laying them
/// out right to left would walk them backwards.
///
/// There are two of them since decision O13 moved Remove into the Edit
/// dialog; the bar holds Edit and Add.
#[test]
fn tab_walks_the_tool_buttons_left_to_right() {
    let ctx = context(1.0);
    let dir = TempDir::new("tab-order");
    let mut app = app(&ctx, &PRINTERS, &dir);
    let mut frame = eframe::Frame::_new_kittest();
    let points = vec2(1080.0, 780.0);
    let key = egui::Event::Key {
        key: egui::Key::Tab, physical_key: None, pressed: true,
        repeat: false, modifiers: egui::Modifiers::default(),
    };
    let mut walked: Vec<egui::Rect> = Vec::new();
    for step in 0..12 {
        let events = match step {
            0 => Vec::new(),
            _ => vec![key.clone()],
        };
        let _ = ctx.run_ui(raw_input(points, f64::from(step), events),
                           |ui| app.ui(ui, &mut frame));
        if let Some(id) = ctx.memory(|mem| mem.focused())
            && let Some(response) = ctx.read_response(id)
            && response.rect.size() == crate::theme::size::ICON_BUTTON
        {
            walked.push(response.rect);
        }
    }
    assert_eq!(walked.len(), 2, "Edit and Add: {walked:?}");
    assert!(walked.windows(2).all(|pair| pair[0].min.x < pair[1].min.x),
            "Tab walks the tools backwards: {walked:?}");
}

/// The control for the clash check in `shoot`: two widgets under one id
/// fail the shot.
#[test]
#[should_panic(expected = "id clash in the shot")]
fn an_id_clash_fails_the_shot() {
    let ctx = context(1.0);
    shoot(&ctx, [64, 64], 1.0, Vec::new(), |ui| {
        let id = egui::Id::new("twice");
        let _ = ui.interact(Rect::from_min_size(Pos2::ZERO, vec2(8.0, 8.0)),
                            id, egui::Sense::click());
        let _ = ui.interact(Rect::from_min_size(pos2(20.0, 20.0),
                                                vec2(8.0, 8.0)),
                            id, egui::Sense::click());
    });
}

/// D12: Skip objects stays open when a new job clears its bundle, and says
/// why, instead of vanishing.
#[test]
fn skip_objects_explains_a_bundle_that_went() {
    let ctx = context(1.0);
    let mut scene = scene("dlg-skip", &ctx);
    let mut frame = eframe::Frame::_new_kittest();
    let mut run = |scene: &mut Scene| {
        let mut painted = Vec::new();
        for step in 0..FRAMES {
            let out = ctx.run_ui(raw_input(vec2(1080.0, 780.0), step as f64,
                                           Vec::new()),
                                 |ui| scene.app.ui(ui, &mut frame));
            painted.clear();
            fn walk(shape: &egui::Shape, found: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(text) =>
                        found.push(text.galley.text().to_string()),
                    egui::Shape::Vec(shapes) =>
                        shapes.iter().for_each(|shape| walk(shape, found)),
                    _ => {}
                }
            }
            out.shapes.iter().for_each(|clipped| walk(&clipped.shape,
                                                      &mut painted));
        }
        painted
    };
    // the control: with its bundle, the dialog lists the objects
    let painted = run(&mut scene);
    assert!(painted.iter().any(|text| text.starts_with("Objects on plate")),
            "{painted:?}");
    scene.app.printers[1].job_bundle = None;
    let painted = run(&mut scene);
    assert!(painted.iter().any(|text|
                text == "The print changed; there are no objects to skip."),
            "{painted:?}");
    assert!(matches!(scene.app.dialog, Dialog::Skip(_)),
            "the dialog closed");
}
