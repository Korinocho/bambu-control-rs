//! Per-printer panel: camera + job card on the left, control cards +
//! AMS on the right. Port of `ui/panel.py`.

use egui::{RichText, Sense, Stroke, Ui, Vec2, vec2};
use serde_json::{Map, Value};

use crate::mqtt::speed_name;
use crate::theme::{self, font, pad, radius, size, space, stroke};
use crate::ui::widgets;

pub const IDLE_STATES: &[&str] = &["IDLE", "FINISH", "FAILED", ""];

// ---------------------------------------------------------- json helpers
pub fn s_str<'a>(state: &'a Map<String, Value>, key: &str) -> &'a str {
    state.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

pub fn s_i64(state: &Map<String, Value>, key: &str) -> Option<i64> {
    let v = state.get(key)?;
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

pub fn s_f64(state: &Map<String, Value>, key: &str) -> Option<f64> {
    let v = state.get(key)?;
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

pub fn light_on(state: &Map<String, Value>) -> Option<bool> {
    for entry in state.get("lights_report")?.as_array()? {
        if entry.get("node").and_then(|v| v.as_str()) == Some("chamber_light")
        {
            return Some(
                entry.get("mode").and_then(|v| v.as_str()) == Some("on"));
        }
    }
    None
}

pub fn ota_version(device_info: &Value) -> String {
    if let Some(modules) = device_info.get("module").and_then(|v| v.as_array())
    {
        for m in modules {
            if m.get("name").and_then(|v| v.as_str()) == Some("ota") {
                return m.get("sw_ver").and_then(|v| v.as_str())
                    .unwrap_or("").to_string();
            }
        }
    }
    String::new()
}

// ------------------------------------------------------------- actions
pub enum PanelAction {
    OpenNozzle,
    OpenBed,
    OpenSpeed,
    OpenFans,
    OpenMove,
    OpenInfo,
    OpenSkip,
    TogglePause,
    AskStop,
    SetLight(bool),
    OpenHmsDialog,
    OpenMaintenance,
    /// the FILES card: the files view of design doc 6
    OpenFiles,
}

pub struct PanelView<'a> {
    pub state: &'a Map<String, Value>,
    pub connected: (bool, String),
    pub cam_texture: Option<&'a egui::TextureHandle>,
    pub cam_status: String,
    pub plate_texture: Option<&'a egui::TextureHandle>,
    pub fetch_progress: Option<u8>,
    pub object_count: usize,
    pub fw_current: String,
    pub fw_latest: String,
    pub show_humidity: bool,
    pub model: String,
    pub light_shown_on: bool,
    /// FILES card value: counts from the listing taken this session, else
    /// the tab names (design doc 6)
    pub files_summary: String,
}

pub(crate) fn card_frame(ui: &mut Ui, add: impl FnOnce(&mut Ui))
                         -> egui::Response {
    theme::card_frame().show(ui, |ui| add(ui)).response
}

/// A control card's title row: the title on the left and, on a card that
/// opens something, the chevron on the right. Every card has the same row,
/// so two cards side by side start their values at the same height (C5).
fn card_title(ui: &mut Ui, title: &str, chevron: bool) {
    egui::Sides::new()
        .height(theme::row_height(ui, &font::title()))
        .show(ui,
            |ui| {
                ui.label(RichText::new(title).color(theme::TEXT_DIM)
                    .font(font::label()));
            },
            |ui| {
                if chevron {
                    ui.label(RichText::new("›").font(font::title())
                        .color(theme::TEXT_DIM));
                }
            });
}

/// A card's value row, `CONTROL_H` tall whatever it holds (C5).
fn value_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, size::CONTROL_H),
        egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(width, size::CONTROL_H));
        add(ui);
    });
}

fn clickable_card(ui: &mut Ui, title: &str, value: &str) -> bool {
    let response = card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        card_title(ui, title, true);
        value_row(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(value)
                .font(font::title())).truncate());
        });
    });
    let response = response.interact(Sense::click());
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

fn temp_card(ui: &mut Ui, title: &str, current: Option<f64>,
             target: Option<f64>) -> bool {
    let response = card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        card_title(ui, title, true);
        let metric = font::metric();
        let width = ui.available_width();
        let height = theme::row_height(ui, &metric);
        ui.allocate_ui_with_layout(vec2(width, height),
            egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_size(vec2(width, height));
            let cur = current.map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "—".into());
            let tgt = target.map(|v| format!("/ {v:.0}°C"))
                .unwrap_or_else(|| "/ —°C".into());
            // the reading changes every report: a slot keeps the target
            // from shifting with it (C6)
            widgets::slot(ui, "888", RichText::new(cur), &metric,
                          egui::Align::Min);
            ui.add(egui::Label::new(RichText::new(tgt)
                .font(font::caption()).color(theme::TEXT_DIM)).truncate());
        });
    });
    let response = response.interact(Sense::click());
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

fn has_rfid(tray: &Value) -> bool {
    for key in ["tag_uid", "tray_uuid"] {
        if let Some(v) = tray.get(key).and_then(|v| v.as_str())
            && !v.trim_matches('0').is_empty()
        {
            return true;
        }
    }
    false
}

fn ams_card(ui: &mut Ui, state: &Map<String, Value>, show_humidity: bool) {
    card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::tight_stack(ui);
        let ams_root = state.get("ams").cloned().unwrap_or(Value::Null);
        let units = ams_root.get("ams").and_then(|v| v.as_array())
            .cloned().unwrap_or_default();
        let tray_now = ams_root.get("tray_now")
            .map(|v| v.as_str().map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string()))
            .unwrap_or_else(|| "255".into());

        ui.horizontal(|ui| {
            ui.label(RichText::new("FILAMENT").color(theme::TEXT_DIM)
                .font(font::label()));
            if show_humidity {
                let hums: Vec<String> = units.iter()
                    .map(|u| format!(
                        "{}/5",
                        u.get("humidity").and_then(|v| v.as_str())
                            .unwrap_or("?")))
                    .collect();
                if !hums.is_empty() {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(RichText::new(
                                format!("💧 {}", hums.join("  ")))
                                .color(theme::TEXT_DIM));
                        });
                }
            }
        });

        let mut shown = false;
        let slot_row = |ui: &mut Ui, slot: String, tray: &Value,
                            active: bool| {
            ui.horizontal(|ui| {
                let color_hex = tray.get("tray_color")
                    .and_then(|v| v.as_str()).unwrap_or("");
                // the colour comes off the network: `get` never cuts a
                // character in half, and a colour is read whole or not at
                // all (E39, D16)
                let channel = |at: usize| color_hex.get(at..at + 2)
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok());
                let color = match (channel(0), channel(2), channel(4)) {
                    (Some(r), Some(g), Some(b)) => theme::reported([r, g, b]),
                    _ => theme::SWATCH_UNKNOWN,
                };
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::splat(size::SWATCH), Sense::hover());
                let ring = if active { theme::ACCENT } else { theme::BORDER };
                ui.painter().circle(rect.center(), size::SWATCH_RING_R, color,
                                    Stroke::new(stroke::MEDIUM, ring));
                ui.painter().circle_filled(rect.center(), size::SWATCH_HOLE_R,
                                           theme::CARD);
                let ftype = tray.get("tray_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("empty");
                let mut text = RichText::new(format!("{slot}   {ftype}"))
                    .font(font::body());
                if active {
                    text = text.color(theme::ACCENT).font(font::body_strong());
                }
                ui.label(text);
                let remain = tray.get("remain").and_then(|v| v.as_i64());
                if let Some(pct) = remain
                    && (0..=100).contains(&pct)
                    && has_rfid(tray)
                {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label(RichText::new(format!("{pct}%"))
                                .color(theme::TEXT_DIM));
                        });
                }
            });
        };

        for unit in &units {
            let uid = unit.get("id").and_then(|v| v.as_i64())
                .or_else(|| unit.get("id").and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            // a unit id past Z reads "?" instead of overflowing (E39)
            let letter = u8::try_from(uid).ok()
                .and_then(|id| b'A'.checked_add(id))
                .filter(u8::is_ascii_uppercase)
                .map_or('?', char::from);
            for tray in unit.get("tray").and_then(|v| v.as_array())
                .cloned().unwrap_or_default()
            {
                let tid = tray.get("id").and_then(|v| v.as_i64())
                    .or_else(|| tray.get("id").and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                let global = uid.saturating_mul(4).saturating_add(tid);
                slot_row(ui, format!("{letter}{}", tid.saturating_add(1)),
                         &tray,
                         tray_now == global.to_string());
                shown = true;
            }
        }
        if let Some(vt) = state.get("vt_tray")
            && vt.get("tray_type").and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
        {
            slot_row(ui, "EXT".into(), vt, tray_now == "254");
            shown = true;
        }
        if !shown {
            ui.label(RichText::new("No AMS detected")
                .color(theme::TEXT_DIM));
        }
    });
}

/// Renders the panel; returns requested actions.
pub fn show(ui: &mut Ui, view: &PanelView) -> Vec<PanelAction> {
    let mut actions = Vec::new();
    let state = view.state;
    let gcode_state = s_str(state, "gcode_state").to_string();
    let running = gcode_state == "RUNNING";
    let paused = gcode_state == "PAUSE";

    ui.horizontal_top(|ui| {
        // the right column keeps what its cards need, the left the rest
        // of its share (C5)
        let width = ui.available_width();
        let left_max = (width - size::RIGHT_COLUMN_MIN - space::M)
            .max(size::LIST_MIN_W);
        let left_w = (width * size::LEFT_COLUMN).floor()
            .clamp(size::LIST_MIN_W, left_max);
        // ------------------------------------------------ left column
        ui.allocate_ui_with_layout(
            egui::vec2(left_w, 0.0),
            egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(left_w);

        // camera: the well is the picture's own shape, 16:9 until the first
        // frame, fitted inside the column and centred in it (C8)
        let aspect = view.cam_texture.map_or(size::VIDEO_ASPECT,
                                             |tex| tex.size_vec2());
        let column = ui.available_width();
        let well = widgets::fit(aspect, vec2(column, size::CAMERA_MAX_H));
        let (row, _) = ui.allocate_exact_size(vec2(column, well.y),
                                              Sense::hover());
        let rect = egui::Rect::from_center_size(row.center(), well);
        ui.painter().rect_filled(rect, radius::CARD, theme::MEDIA_WELL);
        if let Some(tex) = view.cam_texture {
            egui::Image::new((tex.id(), tex.size_vec2()))
                .corner_radius(radius::CARD)
                .paint_at(ui, rect);
        } else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                              &view.cam_status,
                              font::body(), theme::TEXT_DIM);
        }
        ui.add_space(space::M);

        // job card
        card_frame(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (thumb, _) = ui.allocate_exact_size(
                    Vec2::splat(size::JOB_THUMB), Sense::hover());
                ui.painter().rect_filled(
                    thumb, radius::MEDIA, theme::CARD_HOVER);
                if let Some(tex) = view.plate_texture {
                    let size = tex.size_vec2();
                    let scale = (thumb.width() / size.x)
                        .min(thumb.height() / size.y);
                    egui::Image::new((tex.id(), size))
                        .corner_radius(radius::MEDIA)
                        .paint_at(ui, egui::Rect::from_center_size(
                            thumb.center(), size * scale));
                }
                ui.vertical(|ui| {
                    let job = s_str(state, "subtask_name");
                    let job = if job.is_empty() {
                        s_str(state, "gcode_file")
                    } else {
                        job
                    };
                    // a long name truncates instead of widening the column
                    ui.add(egui::Label::new(RichText::new(
                        if job.is_empty() { "—" } else { job })
                        .font(font::body_strong())).truncate());
                    let display = if gcode_state.is_empty() {
                        "—"
                    } else {
                        &gcode_state
                    };
                    // the state and the connection never overlap: the
                    // connection detail gives way (C8)
                    egui::Sides::new().shrink_right().truncate().show(ui,
                        |ui| {
                            ui.label(RichText::new(display)
                                .font(font::body_strong())
                                .color(theme::state_color(&gcode_state)));
                        },
                        |ui| {
                            let (ok, detail) = &view.connected;
                            ui.add(egui::Label::new(RichText::new(detail)
                                .color(if *ok { theme::ACCENT }
                                       else { theme::DANGER }))
                                .truncate());
                        });
                });
            });

            // each reading has a slot of its own, so a digit that changes
            // never moves the rest of the row (C6)
            let pct = s_i64(state, "mc_percent").unwrap_or(0);
            let caption = font::caption();
            egui::Sides::new().show(ui,
                |ui| {
                    widgets::slot(ui, "100%", RichText::new(format!("{pct}%")),
                                  &font::metric(), egui::Align::Min);
                    let mins = s_i64(state, "mc_remaining_time").unwrap_or(0);
                    let eta = match mins > 0 {
                        true => format!("~{}h {:02}m left", mins / 60,
                                        mins % 60),
                        false => String::new(),
                    };
                    widgets::slot(ui, "~99h 59m left",
                                  RichText::new(eta).color(theme::TEXT_DIM),
                                  &caption, egui::Align::Min);
                },
                |ui| {
                    let layer = match (s_i64(state, "layer_num"),
                                       s_i64(state, "total_layer_num")) {
                        (Some(layer), Some(total)) if total > 0 =>
                            format!("layer {layer}/{total}"),
                        _ => String::new(),
                    };
                    widgets::slot(ui, "layer 9999/9999",
                                  RichText::new(layer).color(theme::TEXT_DIM),
                                  &caption, egui::Align::Max);
                });
            let bar = egui::ProgressBar::new(pct as f32 / 100.0)
                .desired_height(size::PROGRESS_H)
                .fill(theme::ACCENT);
            ui.add(bar);
            ui.add_space(space::S);

            ui.horizontal(|ui| {
                let can_act = running || paused;
                let skip_label = if view.object_count > 0 {
                    format!("Skip objects ({})", view.object_count)
                } else {
                    "Skip objects".to_string()
                };
                if ui.add_enabled(can_act,
                                  egui::Button::new(skip_label)).clicked() {
                    actions.push(PanelAction::OpenSkip);
                }
                if let Some(p) = view.fetch_progress
                    && p < 100
                {
                    widgets::slot(ui, "100%", RichText::new(format!("{p}%"))
                        .color(theme::TEXT_DIM), &font::caption(),
                        egui::Align::Min);
                }
                let pause_label =
                    if paused { "▶ Resume" } else { "⏸ Pause" };
                if ui.add_enabled(can_act,
                                  egui::Button::new(pause_label)).clicked() {
                    actions.push(PanelAction::TogglePause);
                }
                let stop = egui::Button::new(
                    RichText::new("⏹ Stop").color(theme::DANGER))
                    .stroke(Stroke::new(stroke::HAIRLINE, theme::DANGER));
                if ui.add_enabled(can_act, stop).clicked() {
                    actions.push(PanelAction::AskStop);
                }
            });
        });

        });
        // ----------------------------------------------- right column
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 0.0),
            egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(ui.available_width());
        {
            // HMS banner — looks the errors up in Bambu's public DB
            // and opens the matching wiki page on click
            let hms = state.get("hms").and_then(|v| v.as_array())
                .cloned().unwrap_or_default();
            if !hms.is_empty() {
                crate::hms::ensure_loaded(ui.ctx());
                let ecodes: Vec<String> = hms.iter().take(3)
                    .map(|h| crate::hms::ecode(
                        h.get("attr").and_then(|v| v.as_u64()).unwrap_or(0),
                        h.get("code").and_then(|v| v.as_u64()).unwrap_or(0)))
                    .collect();
                let response = egui::Frame::new()
                    .fill(theme::DANGER_BG)
                    .corner_radius(radius::CONTROL)
                    .inner_margin(pad::BANNER)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        theme::tight_stack(ui);
                        ui.label(RichText::new(format!(
                            "⚠ {} printer error(s)", hms.len()))
                            .color(theme::DANGER)
                            .font(font::body_strong()));
                        for ecode in &ecodes {
                            // description only; the code stays in the
                            // popup (fallback when no description)
                            let line = crate::hms::lookup(ecode)
                                .unwrap_or_else(
                                    || crate::hms::dashed(ecode));
                            ui.add(egui::Label::new(
                                RichText::new(line)
                                    .color(theme::DANGER)
                                    .font(font::caption()))
                                .wrap());
                        }
                    })
                    .response.interact(Sense::click());
                if response.hovered() {
                    ui.output_mut(|o| {
                        o.cursor_icon = egui::CursorIcon::PointingHand;
                    });
                }
                if response.clicked() {
                    actions.push(PanelAction::OpenHmsDialog);
                }
                ui.add_space(space::M);
            }

            // firmware banner
            if crate::firmware::is_newer(&view.fw_latest, &view.fw_current) {
                let response = egui::Frame::new()
                    .fill(theme::WARN_BG)
                    .corner_radius(radius::CONTROL)
                    .inner_margin(pad::BANNER)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!(
                            "⬆ Firmware update available: {} → {}",
                            view.fw_current, view.fw_latest))
                            .color(theme::WARN).font(font::body_strong()));
                    })
                    .response.interact(Sense::click());
                if response.hovered() {
                    ui.output_mut(|o| {
                        o.cursor_icon = egui::CursorIcon::PointingHand;
                    });
                }
                if response.clicked() {
                    actions.push(PanelAction::OpenInfo);
                }
                ui.add_space(space::M);
            }

            ui.label(RichText::new("DEVICE CONTROL").color(theme::TEXT_DIM)
                .font(font::label()));
            ui.add_space(space::XS);

            ui.columns(2, |cards| {
                if temp_card(&mut cards[0], "NOZZLE",
                             s_f64(state, "nozzle_temper"),
                             s_f64(state, "nozzle_target_temper")) {
                    actions.push(PanelAction::OpenNozzle);
                }
                if temp_card(&mut cards[1], "BED",
                             s_f64(state, "bed_temper"),
                             s_f64(state, "bed_target_temper")) {
                    actions.push(PanelAction::OpenBed);
                }
            });
            ui.columns(2, |cards| {
                let lvl = s_i64(state, "spd_lvl").unwrap_or(2);
                if clickable_card(&mut cards[0], "SPEED", speed_name(lvl)) {
                    actions.push(PanelAction::OpenSpeed);
                }
                // light card with toggle
                card_frame(&mut cards[1], |ui| {
                    ui.set_width(ui.available_width());
                    card_title(ui, "LIGHT", false);
                    let mut on = view.light_shown_on;
                    let word = if on { "On" } else { "Off" };
                    egui::Sides::new().height(size::CONTROL_H).show(ui,
                        |ui| {
                            ui.label(RichText::new(word)
                                .font(font::title()));
                        },
                        |ui| {
                            if widgets::toggle_switch(ui, &mut on) {
                                actions.push(PanelAction::SetLight(on));
                            }
                        });
                });
            });
            ui.columns(2, |cards| {
                let mut fans_on = 0;
                for key in ["cooling_fan_speed", "big_fan1_speed",
                            "big_fan2_speed"] {
                    if s_i64(state, key).unwrap_or(0) > 0 {
                        fans_on += 1;
                    }
                }
                if clickable_card(&mut cards[0], "FANS",
                                  &format!("{fans_on} fan(s) on")) {
                    actions.push(PanelAction::OpenFans);
                }
                if clickable_card(&mut cards[1], "MOVEMENT", "XYZ") {
                    actions.push(PanelAction::OpenMove);
                }
            });
            let info_value = if view.fw_current.is_empty() {
                view.model.clone()
            } else {
                format!("{}  ·  {}", view.model, view.fw_current)
            };
            if clickable_card(ui, "DEVICE INFO", &info_value) {
                actions.push(PanelAction::OpenInfo);
            }
            // screen-menu replacement (calibration / filament / nozzle)
            let nozzle_type = s_str(state, "nozzle_type");
            let nozzle_diam = s_f64(state, "nozzle_diameter");
            let maint_value = match (nozzle_type.is_empty(), nozzle_diam) {
                (false, Some(d)) => format!(
                    "{}  ·  {d} mm",
                    if nozzle_type == "hardened_steel" { "Hardened" }
                    else { "Stainless" }),
                _ => "Calibration · Filament · Nozzle".to_string(),
            };
            if clickable_card(ui, "MAINTENANCE", &maint_value) {
                actions.push(PanelAction::OpenMaintenance);
            }
            // the printer's SD card: timelapses, recordings, print files
            if clickable_card(ui, "FILES", &view.files_summary) {
                actions.push(PanelAction::OpenFiles);
            }
            ams_card(ui, state, view.show_humidity);
        }
        });
    });
    actions
}

/// The panel against a hand-built view, rendered through `Context::run_ui`
/// like the files view's tests (E40): what is asserted is what it painted.
#[cfg(test)]
mod tests {
    use egui::{Pos2, Rect, Vec2};
    use serde_json::json;

    use super::*;

    /// Every piece of text painted, and every filled rect, with its fill.
    struct Painted {
        texts: Vec<(String, Rect)>,
        rects: Vec<(Rect, egui::Color32)>,
    }

    fn render(width: f32, state: &Map<String, Value>) -> Painted {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx);
        let view = PanelView {
            state,
            connected: (true, "online".to_string()),
            cam_texture: None,
            cam_status: "camera paused".to_string(),
            plate_texture: None,
            fetch_progress: None,
            object_count: 0,
            fw_current: "01.08.02.00".to_string(),
            fw_latest: String::new(),
            show_humidity: false,
            model: "Bambu Lab A1".to_string(),
            light_shown_on: true,
            files_summary: "Timelapses · Recordings · Print files".to_string(),
        };
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO,
                                                  Vec2::new(width, 900.0))),
            ..Default::default()
        };
        // the second frame, once egui has sized everything
        let _ = ctx.run_ui(input(), |ui| { show(ui, &view); });
        let full = ctx.run_ui(input(), |ui| { show(ui, &view); });
        let mut painted = Painted { texts: Vec::new(), rects: Vec::new() };
        fn walk(shape: &egui::Shape, painted: &mut Painted) {
            match shape {
                egui::Shape::Text(text) => painted.texts.push((
                    text.galley.text().to_string(),
                    Rect::from_min_size(text.pos, text.galley.rect.size()))),
                egui::Shape::Rect(rect) =>
                    painted.rects.push((rect.rect, rect.fill)),
                egui::Shape::Vec(shapes) =>
                    shapes.iter().for_each(|shape| walk(shape, painted)),
                _ => {}
            }
        }
        for clipped in &full.shapes {
            walk(&clipped.shape, &mut painted);
        }
        painted
    }

    fn idle() -> Map<String, Value> {
        let Value::Object(state) = json!({
            "gcode_state": "IDLE", "spd_lvl": 2,
            "nozzle_temper": 27.8, "nozzle_target_temper": 0,
            "bed_temper": 26.1, "bed_target_temper": 0,
            "lights_report": [{ "node": "chamber_light", "mode": "on" }],
        }) else {
            unreachable!("an object");
        };
        state
    }

    impl Painted {
        fn text(&self, label: &str) -> Rect {
            self.texts.iter().find(|(text, _)| text == label)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("{label:?} was not painted"))
        }

        /// The card behind a title: the smallest CARD rect around it.
        fn card(&self, label: &str) -> Rect {
            let title = self.text(label);
            self.rects.iter()
                .filter(|(rect, fill)| *fill == theme::CARD
                        && rect.contains_rect(title))
                .map(|(rect, _)| *rect)
                .min_by(|a, b| a.area().total_cmp(&b.area()))
                .unwrap_or_else(|| panic!("no card around {label:?}"))
        }
    }

    /// C5, D24: the two cards of a row start their titles on one line and
    /// end at one height, at 700 px as at a wide window.
    #[test]
    fn cards_in_a_row_line_up() {
        for width in [700.0, 1200.0] {
            let painted = render(width, &idle());
            for (left, right) in [("NOZZLE", "BED"), ("SPEED", "LIGHT"),
                                  ("FANS", "MOVEMENT")] {
                assert_eq!(painted.text(left).min.y, painted.text(right).min.y,
                           "{width}: {left} and {right} titles");
                let (a, b) = (painted.card(left), painted.card(right));
                assert_eq!((a.min.y, a.max.y), (b.min.y, b.max.y),
                           "{width}: {left} {a:?} and {right} {b:?}");
            }
        }
    }

    /// E39, D16: a filament colour that is not ASCII hex and a unit id past
    /// the alphabet render, and nothing panics (release aborts on a panic).
    #[test]
    fn hostile_ams_fields_render_without_panicking() {
        let mut state = idle();
        state.insert("ams".to_string(), json!({
            "ams": [{ "id": "255", "tray": [
                { "id": "0", "tray_type": "PLA", "tray_color": "aÁÉÍxx" },
                { "id": 9223372036854775807i64, "tray_type": "PETG",
                  "tray_color": "zz" },
            ]}],
            "tray_now": "1",
        }));
        let painted = render(900.0, &state);
        // unit 255 has no letter; the slots still render, and name their
        // filament
        assert!(painted.texts.iter().any(|(text, _)| text == "?1   PLA"),
                "{:?}", painted.texts.iter().map(|(t, _)| t)
                    .collect::<Vec<_>>());
        assert!(painted.texts.iter().any(|(text, _)| text.ends_with("PETG")));
    }
}
