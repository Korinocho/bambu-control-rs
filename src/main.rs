//! Bambu Control — LAN control panel for Bambu Lab printers.
//! Rust + egui port of the PySide6 app: one printer visible at a time,
//! top selector chips with live state, white border on the active one.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod avi;
mod browser;
mod cache;
mod camera;
mod config;
mod files;
mod firmware;
mod ftp;
mod gcode;
mod hms;
mod instance;
mod mqtt;
mod player;
#[cfg(test)]
mod snapshots;
mod theme;
mod threemf;
mod tls;
mod ui;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{RichText, Sense, Stroke, StrokeKind};

use theme::{font, pad, radius, size, stroke};

use browser::{BrowserState, Cmd, Event, FtpWorker};
use config::{Config, PrinterCfg, model_from_serial};
use player::{MjpegPlayer, PlayerCmd};
use ui::dialogs::{self, Dialog};
use ui::files_view::{self, FilesUi};
use ui::panel::{self, PanelAction, PanelView};

/// What the central panel shows: the printer panel, or the files view of
/// design doc 6 for the selected printer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppView {
    Panel,
    Files,
}

struct PrinterUi {
    cfg: PrinterCfg,
    client: Arc<mqtt::PrinterClient>,
    camera: Option<Arc<camera::Camera>>,
    cam_texture: Option<egui::TextureHandle>,
    fw_latest_slot: Arc<Mutex<Option<String>>>,
    fw_latest: String,
    /// browse lane and transfer lane: listings, thumbnails and the running
    /// job's bundle on one session, user downloads on a second when one is
    /// running (design doc 4, rules 1 and 2)
    ftp: FtpWorker,
    /// the app's disk cache, shared by every printer (design doc 5.6)
    cache: Arc<cache::Cache>,
    /// what the files view shows
    browser: BrowserState,
    /// the files view's own state: tab, filter, selection and textures
    files: FilesUi,
    /// the open MJPEG player and the texture its frames go into
    /// (design doc 5.9); `None` when the grid is showing
    player: Option<Arc<MjpegPlayer>>,
    player_tex: Option<egui::TextureHandle>,
    /// why the last file could not be played, and the file it was about
    /// when the OS player is the way out of it (section 7, option B). An
    /// empty recording has nothing to open, so it carries no path.
    player_error: Option<(String, Option<PathBuf>)>,
    /// why "Open in player" or "Show in folder" last failed, shown in the
    /// files view until Back or the next one that works (C3)
    open_error: Option<String>,
    /// which local files may be handed to the Windows shell, answered by
    /// `player::openable_by_shell` and remembered here: the verdict reads
    /// the file's first bytes, and the view asks for it while painting
    /// (design doc 5.1 rule 5; stage 3 security review, F2)
    shell_openable: RefCell<HashMap<PathBuf, bool>>,
    /// Which remote entries already have a complete copy on disk. The
    /// answer is an `is_file()` on the cache's own key, which the detail
    /// pane asked for on every frame it painted (E30, D08). Both this and
    /// `shell_openable` are forgotten when a transfer of any file
    /// finishes, on Clear cache and on Back (E28).
    cached_copy: RefCell<HashMap<String, Option<PathBuf>>>,
    job_bundle: Option<files::JobBundle>,
    plate_texture: Option<egui::TextureHandle>,
    current_job: String,
    /// last job name and state seen on MQTT, to tell the worker when a job
    /// is starting (design doc 4, rule 5)
    last_job: String,
    last_gcode_state: String,
    light_pending: Option<(bool, Instant)>,
    /// the printer never agreed with the last light command (C18, D40)
    light_unconfirmed: bool,
    /// The HMS errors on screen, resolved once: a lookup takes the global
    /// table's lock per code, and the banner asked for three of them on
    /// every frame it painted (D38). Rebuilt in `sync` when the codes
    /// change, which is the only thing that changes the lines.
    hms_codes: Vec<String>,
    hms_lines: Vec<String>,
    /// a player being opened on a thread: the file, the speed it asked
    /// for, and the slot the thread leaves the result in (E30, D07)
    #[allow(clippy::type_complexity, reason = "one slot, read in sync")]
    opening: Option<(PathBuf, f32,
                     Arc<Mutex<Option<Result<Arc<MjpegPlayer>,
                                             player::PlayerError>>>>)>,
}

impl PrinterUi {
    fn new(cfg: PrinterCfg, ctx: &egui::Context,
           cache: Arc<cache::Cache>) -> Self {
        let ftp = FtpWorker::start(&cfg, ctx, cache.clone());
        Self::with_worker(cfg, ctx, ftp, cache)
    }

    /// The printer that replaces one whose connection was edited: its
    /// worker opens no session until the old lane threads have ended
    /// (design doc 4, rule 3).
    fn new_replacing(cfg: PrinterCfg, ctx: &egui::Context,
                     previous: &PrinterUi) -> Self {
        let cache = previous.cache.clone();
        let ftp = FtpWorker::start_replacing(&cfg, ctx, &previous.ftp,
                                             cache.clone());
        Self::with_worker(cfg, ctx, ftp, cache)
    }

    fn with_worker(cfg: PrinterCfg, ctx: &egui::Context, ftp: FtpWorker,
                   cache: Arc<cache::Cache>) -> Self {
        let client = mqtt::PrinterClient::start(
            &cfg.ip, &cfg.serial, &cfg.access_code, ctx.clone());
        let fw_latest_slot = Arc::new(Mutex::new(None));
        firmware::spawn_check(model_from_serial(&cfg.serial), false,
                              fw_latest_slot.clone(), ctx.clone());
        Self {
            cfg,
            client,
            camera: None,
            cam_texture: None,
            fw_latest_slot,
            fw_latest: String::new(),
            ftp,
            cache,
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

    fn set_active(&mut self, active: bool, ctx: &egui::Context) {
        // a background printer drops its prefetch and closes its session
        self.ftp.send(Cmd::SetBackground(!active));
        if active && self.camera.is_none() {
            // the serial is what the camera's certificate check binds to
            // (issue #2): without it nothing connects
            self.camera = Some(camera::Camera::start(
                self.cfg.ip.clone(), self.cfg.serial.clone(),
                self.cfg.access_code.clone(), ctx.clone()));
        } else if !active && let Some(cam) = self.camera.take() {
            cam.stop();
            self.cam_texture = None;
        }
    }

    /// Stops everything without waiting: the FTP session is cancelled and
    /// its lane thread ends on its own (design doc 5.1, rule 7).
    fn shutdown(&mut self) {
        if let Some(cam) = self.camera.take() {
            cam.stop();
        }
        self.close_player();
        self.client.stop();
        self.ftp.stop();
    }

    /// Opens a local file in the app's MJPEG player (5.8). A format it
    /// cannot decode is not a failure: the message and the OS player take
    /// its place in the view (section 7, option B).
    /// Opening walks the whole AVI to build its index, which on a long
    /// timelapse takes seconds: it runs on a thread and the view says
    /// "Opening…" until the result lands in `opening` (E30, E31, D07).
    fn open_player(&mut self, path: PathBuf, speed: f32,
                   ctx: &egui::Context) {
        self.close_player();
        let slot = Arc::new(Mutex::new(None));
        self.opening = Some((path.clone(), speed, slot.clone()));
        let (cache, ctx) = (self.cache.clone(), ctx.clone());
        std::thread::spawn(move || {
            let opened = MjpegPlayer::open(path, cache, ctx.clone());
            *slot.lock().unwrap() = Some(opened);
            ctx.request_repaint();
        });
    }

    /// The opening thread's result, taken in `sync`.
    fn take_opened(&mut self) {
        let Some((path, speed, slot)) = &self.opening else { return };
        let Some(opened) = slot.lock().unwrap().take() else { return };
        let (path, speed) = (path.clone(), *speed);
        self.opening = None;
        match opened {
            Ok(player) => {
                // a cached copy is named by a hash, so the speed comes
                // from the remote path the view resolved (section 7)
                player.send(PlayerCmd::Speed(speed));
                self.player = Some(player);
            }
            // only a format this app cannot decode has somewhere else to
            // go; an empty recording has nothing to open (5.10)
            Err(err) => {
                let path = err.offers_os_player().then_some(path);
                self.player_error = Some((err.text(), path));
            }
        }
    }

    /// The HMS lines the banner shows, resolved when the codes change and
    /// not while painting (D38). The table is fetched once, here, for the
    /// same reason.
    fn sync_hms(&mut self, ctx: &egui::Context) {
        let codes: Vec<String> = {
            let state = self.client.state.lock().unwrap();
            state.get("hms").and_then(|v| v.as_array())
                .map(|errors| errors.iter().take(3)
                    .map(|error| {
                        let field = |name: &str| error.get(name)
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        hms::ecode(field("attr"), field("code"))
                    })
                    .collect())
                .unwrap_or_default()
        };
        if codes == self.hms_codes {
            return;
        }
        if !codes.is_empty() {
            hms::ensure_loaded(ctx);
        }
        // description only; the code stays in the dialog
        self.hms_lines = codes.iter()
            .map(|ecode| hms::lookup(ecode)
                .unwrap_or_else(|| hms::dashed(ecode)))
            .collect();
        self.hms_codes = codes;
    }

    /// Forgets what this printer knows about the disk: which files have a
    /// copy, and which may be handed to the shell (E28).
    fn forget_disk_answers(&mut self) {
        self.cached_copy.borrow_mut().clear();
        self.shell_openable.borrow_mut().clear();
    }

    /// Ends the decode thread and releases the file in the cache. It never
    /// joins: the thread sees the flag and ends on its own (5.1, rule 7).
    fn close_player(&mut self) {
        if let Some(player) = self.player.take() {
            player.stop();
        }
        self.player_tex = None;
        self.player_error = None;
    }

    /// Per-frame sync of async results into UI state.
    fn sync(&mut self, ctx: &egui::Context) {
        // Textures are named by the hashed printer key, never by the
        // serial: egui lists texture ids in its own inspection UI, and
        // section 12 keeps the serial out of incidental strings.
        let tex = cache::Cache::printer_key(&self.cfg.serial);
        if let Some(version) = self.fw_latest_slot.lock().unwrap().take() {
            self.fw_latest = version;
        }
        if let Some(cam) = &self.camera
            && let Some(frame) = cam.frame.lock().unwrap().take()
        {
            match &mut self.cam_texture {
                Some(tex) => tex.set(frame, Default::default()),
                None => {
                    self.cam_texture = Some(ctx.load_texture(
                        format!("cam-{tex}"), frame, Default::default()));
                }
            }
        }
        // the decode thread's latest frame becomes the texture; the UI
        // thread only uploads it (design doc 5.1, rule 5)
        if let Some(player) = &self.player
            && let Some(frame) = player.frame.lock().unwrap().take()
        {
            match &mut self.player_tex {
                Some(tex) => tex.set(frame, Default::default()),
                None => {
                    self.player_tex = Some(ctx.load_texture(
                        format!("play-{tex}"), frame, Default::default()));
                }
            }
        }
        self.take_opened();
        // at most 64 worker events per frame (design doc 5.9)
        for _ in 0..64 {
            let Ok(event) = self.ftp.events.try_recv() else { break };
            let bundle = match event {
                Event::JobBundle { job, result } if job == self.current_job =>
                    result.unwrap_or_else(|e| files::JobBundle {
                        error: e.text(&self.cfg.serial),
                        ..Default::default()
                    }),
                // the files view (stage 2 part 2) shows the rest; a bundle
                // for another job is dropped, as JobFetcher dropped it
                event => {
                    // a transfer that landed changed the disk, so what
                    // this printer knows about it is forgotten (E28)
                    if matches!(event, Event::Done { .. }) {
                        self.forget_disk_answers();
                    }
                    for cmd in self.browser.apply(event) {
                        self.ftp.send(cmd);
                    }
                    continue;
                }
            };
            if let Some(png) = &bundle.plate_png
                && let Ok(img) = image::load_from_memory(png)
            {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    size, rgba.as_raw());
                // a job that follows another replaces the picture in the
                // texture it already has (E35)
                match &mut self.plate_texture {
                    Some(handle) => handle.set(image, Default::default()),
                    None => self.plate_texture = Some(ctx.load_texture(
                        format!("plate-{tex}"), image, Default::default())),
                }
            }
            self.job_bundle = Some(bundle);
        }
        // "Download & play": the file landed, so the player opens on it,
        // once (section 6). The remote path decides the starting speed.
        if let Some((path, remote)) = self.browser.take_play_request() {
            let speed = player::default_speed(&remote);
            self.open_player(path, speed, ctx);
        }

        self.sync_hms(ctx);
        // auto-fetch job data when a new print shows up. The four fields
        // this needs are read under the lock: cloning the whole map here
        // copied every printer's telemetry on every frame (E25, D35)
        let (gcode_state, job, file_name, print_type) = {
            let state = self.client.state.lock().unwrap();
            let job = match panel::s_str(&state, "subtask_name") {
                "" => panel::s_str(&state, "gcode_file"),
                name => name,
            };
            (panel::s_str(&state, "gcode_state").to_string(),
             job.to_string(),
             panel::s_str(&state, "gcode_file").to_string(),
             panel::s_str(&state, "print_type").to_string())
        };
        // a job preparing or starting closes an idle session at once
        // (design doc 4, rule 5)
        if gcode_state != self.last_gcode_state || job != self.last_job {
            let starting = gcode_state == "PREPARE"
                || (job != self.last_job && !job.is_empty()
                    && gcode_state != "RUNNING");
            if starting {
                self.ftp.send(Cmd::JobStarting);
            }
            let printing = matches!(gcode_state.as_str(), "RUNNING" | "PAUSE");
            let was_printing = matches!(self.last_gcode_state.as_str(),
                                        "RUNNING" | "PAUSE");
            if printing != was_printing {
                self.ftp.send(Cmd::SetPrinting(printing));
            }
            self.last_gcode_state = gcode_state.clone();
            self.last_job = job.clone();
        }
        if !job.is_empty() && job != self.current_job
            && matches!(gcode_state.as_str(), "RUNNING" | "PAUSE")
        {
            self.current_job = job.clone();
            self.job_bundle = None;
            self.plate_texture = None;
            // a fetch still running for the previous job name is cancelled,
            // and this one runs on the browse session (design doc 5.4)
            self.ftp.send(Cmd::JobBundle { job, file_name, print_type });
        }
    }
}

/// What one printer has in flight, for the confirmations of design doc 5.4:
/// the rows the view started, or the worker's own queue when that is
/// larger. The running job's 3mf over 1 MB and a big "Load preview" run on
/// the transfer lane under reserved ids and have no row at all, so counting
/// rows alone discarded an 86 MB download with no question asked.
fn active_transfers(printer: &PrinterUi) -> usize {
    printer.browser.active_transfers().max(printer.ftp.active_transfers())
}

enum ToolIcon {
    Edit,
    Add,
}

/// The tool buttons' glyphs, in points from the button's centre: geometry
/// private to that one painter. Segments are [x0, y0, x1, y1].
mod tool_icon {

    pub const STROKE: f32 = 1.7;
    pub const PENCIL_STROKE: f32 = 2.4;
    pub const PLUS_H: [f32; 4] = [-6.0, 0.0, 6.0, 0.0];
    pub const PLUS_V: [f32; 4] = [0.0, -6.0, 0.0, 6.0];
    pub const PENCIL_BODY: [f32; 4] = [-4.5, 5.5, 4.0, -3.0];
    pub const PENCIL_TIP: [[f32; 2]; 4] =
        [[4.9, -6.4], [6.4, -4.9], [3.2, -2.2], [2.2, -3.2]];

}

/// The chip drag feedback: how far the insertion marker sits outside the
/// chips, and where the ghost trails the pointer.
const MARKER_GAP: f32 = 4.0;
const MARKER_OVERHANG: f32 = 3.0;
const GHOST_OFFSET: egui::Vec2 = egui::vec2(14.0, 10.0);

/// How long a light command waits for telemetry to agree before the switch
/// goes back to what the printer reports.
const LIGHT_PENDING: Duration = Duration::from_secs(6);

struct App {
    cfg: Config,
    printers: Vec<PrinterUi>,
    selected: usize,
    dialog: Dialog,
    /// the printer panel, or the files view of the selected printer
    view: AppView,
    started: bool,
    /// config.toml; writes nothing after a failed load
    store: config::Store,
    /// the disk cache of design doc 5.6, one per app
    cache: Arc<cache::Cache>,
    /// The cache's used bytes, walked on a thread: the walk reads every
    /// file in the cache directory and must never run inside a frame
    /// (E30, D08). The figure the view shows is the last one that landed;
    /// `asked_at` keeps the 2 s rule, and Clear cache and a transfer that
    /// finishes clear the memo in `cache` itself (E28).
    cache_usage: Arc<Mutex<Option<u64>>>,
    cache_asked_at: Option<Instant>,
    /// Clear cache deletes files, so it runs on a thread too; while the
    /// flag is up the button says "Clearing…" (E30, E31).
    clearing: Arc<std::sync::atomic::AtomicBool>,
    /// a failed save, or an unreadable config.toml; shown above the panel
    config_error: Option<String>,
    /// the app was asked to close while transfers were running, so the
    /// close was cancelled and the question is on screen (design doc 5.4)
    pending_close: bool,
    /// the user confirmed the close: every worker is stopped and the app
    /// exits without waiting for a thread (5.1, rule 7)
    closing: bool,
    /// a connection edit waiting for its confirmation, because that
    /// printer still has transfers running (section 6)
    pending_edit: Option<(usize, PrinterCfg)>,
    /// `BAMBU_CONTROL_PLAY="<remote path>"` starts "Download & play" on
    /// that file as soon as the listing naming it arrives. Debug builds
    /// only, like `BAMBU_CONTROL_OPEN_FILES`: it exists so the stage's
    /// screenshots can be taken without driving the mouse, and nothing of
    /// it is in a release build.
    #[cfg(debug_assertions)]
    debug_play: Option<String>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        theme::install_fonts(ctx);
        theme::apply(ctx);
        let (store, loaded) = config::Store::load();
        let (cfg, migrated, config_error) = match loaded {
            Ok(loaded) => (loaded.cfg, loaded.migrated, None),
            Err(e) => (Config::default(), false, Some(format!(
                "config.toml can't be read; nothing is written until it is \
                 fixed ({e})"))),
        };
        // stale .part files from a previous run are deleted here: without
        // resume they are worthless (design doc 5.6)
        let cache = cache::Cache::open(cfg.files.cache_cap_bytes());
        // "Save to PC" writes outside the cache, so its folder is swept
        // too: a crash during one would otherwise leave the whole file's
        // bytes in the user's Downloads folder for ever (5.6)
        cache.sweep_save_parts(&cache::save_root());
        let printers: Vec<PrinterUi> = cfg.printers.iter()
            .map(|p| PrinterUi::new(p.clone(), ctx, cache.clone()))
            .collect();
        let dialog = if printers.is_empty() && !store.is_blocked() {
            Dialog::AddPrinter(dialogs::AddPrinterDlg {
                draft: PrinterCfg::default(),
                editing: None,
                error: String::new(),
            })
        } else {
            Dialog::None
        };
        let mut app = Self {
            cfg, printers, selected: 0, dialog, view: AppView::Panel,
            started: false, store, cache, config_error,
            cache_usage: Arc::new(Mutex::new(None)),
            cache_asked_at: None,
            clearing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_close: false, closing: false, pending_edit: None,
            #[cfg(debug_assertions)]
            debug_play: std::env::var("BAMBU_CONTROL_PLAY").ok(),
        };
        if migrated {
            app.save_config();
        }
        app
    }

    /// Ctrl+scroll and Ctrl+plus change egui's zoom; the level is saved so
    /// the next launch opens at it (decision O7). A zoom change is a user
    /// action, so the one small atomic write it costs is allowed (E30).
    fn save_zoom(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if (zoom - self.cfg.ui.zoom).abs() < 0.001 {
            return;
        }
        self.cfg.ui.zoom = zoom;
        self.save_config();
    }

    /// The cache's used bytes for this frame: the last figure a walk
    /// landed, and a new walk started at most once per
    /// `CACHE_USAGE_REFRESH` (E28, E30). A frame never walks the disk.
    fn cache_usage(&mut self, ctx: &egui::Context) -> u64 {
        /// The walk reads every file in the cache directory, so it runs
        /// at most this often (design doc 5.1, rule 5).
        const CACHE_USAGE_REFRESH: Duration = Duration::from_secs(2);

        let stale = self.cache_asked_at
            .is_none_or(|asked| asked.elapsed() >= CACHE_USAGE_REFRESH);
        if stale && !self.clearing.load(Ordering::SeqCst) {
            self.cache_asked_at = Some(Instant::now());
            let (cache, slot) =
                (self.cache.clone(), self.cache_usage.clone());
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let bytes = cache.usage_bytes();
                *slot.lock().unwrap() = Some(bytes);
                ctx.request_repaint();
            });
        }
        self.cache_usage.lock().unwrap().unwrap_or(0)
    }

    /// Saves atomically and shows a failure. While config.toml is unreadable
    /// the store writes nothing, and the load error stays on screen.
    fn save_config(&mut self) {
        self.cfg.printers =
            self.printers.iter().map(|p| p.cfg.clone()).collect();
        match self.store.save(&self.cfg) {
            Ok(()) => self.config_error = None,
            Err(_) if self.store.is_blocked() => {}
            Err(e) => self.config_error =
                Some(format!("settings couldn't be saved: {e}")),
        }
    }

    fn select(&mut self, index: usize, ctx: &egui::Context) {
        if index >= self.printers.len() {
            return;
        }
        self.selected = index;
        for (i, p) in self.printers.iter_mut().enumerate() {
            p.set_active(i == index, ctx);
        }
        // the files view follows the selected chip (design doc 6)
        if self.view == AppView::Files {
            self.open_files();
        }
    }

    /// Opens the files view for the selected printer and starts its first
    /// listing round. A model refused by name lists nothing and opens no
    /// socket (design doc 5.3, Models).
    fn open_files(&mut self) {
        /// A listing this old is taken again when the view is opened, so
        /// reopening never shows "updated 2 h ago" with nothing running
        /// (design doc 5.5).
        const STALE: Duration = Duration::from_secs(60);

        self.view = AppView::Files;
        let Some(printer) = self.printers.get_mut(self.selected) else {
            return;
        };
        if config::files_refused_by_name(&printer.cfg.serial).is_some() {
            return;
        }
        // nothing listed yet, a round that failed (no listing is Ready), or
        // one older than STALE — but never while a round is in flight
        let stale = printer.browser.updated_at()
            .is_none_or(|at| at.elapsed() >= STALE);
        if !printer.browser.is_listing()
            && (printer.browser.dirs.is_empty() || stale)
        {
            for cmd in printer.browser.refresh() {
                printer.ftp.send(cmd);
            }
        }
    }

    /// `BAMBU_CONTROL_OPEN_FILES="<printer index>[:timelapses|recordings|
    /// files]"` opens that view at start, for the G4 screenshot. The index
    /// is zero-based. Debug builds only: nothing of this exists in a
    /// release build.
    #[cfg(debug_assertions)]
    fn open_files_from_env(&mut self, ctx: &egui::Context) {
        let Ok(value) = std::env::var("BAMBU_CONTROL_OPEN_FILES") else {
            return;
        };
        let (index, tab) = match value.split_once(':') {
            Some((index, tab)) => (index, files_view::Tab::from_word(tab)),
            None => (value.as_str(), None),
        };
        let Ok(index) = index.trim().parse::<usize>() else { return };
        if index >= self.printers.len() {
            return;
        }
        self.select(index, ctx);
        if let Some(tab) = tab {
            self.printers[index].files.tab = tab;
        }
        self.open_files();
    }

    /// The files view of design doc 6, in place of the printer panel.
    fn show_files(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let selected = self.selected;
        // no repaint rate of its own: every counter and every gate in the
        // view asks for the moment it needs, and the earliest wins (E32,
        // D34). An idle view with a listing repaints once a second.
        // a dialog is painted over this view (Edit printer, for example):
        // it owns the keyboard while it is open (design doc 6)
        let dialog_open = !matches!(self.dialog, Dialog::None);
        let cache_usage = self.cache_usage(ctx);
        let cache_cap = self.cache.cap_bytes();
        let clearing = self.clearing.load(Ordering::SeqCst);
        let disk = self.cache.clone();
        let (actions, cmds) = {
            let printer = &mut self.printers[selected];
            let status = printer.ftp.status();
            let printing = {
                let state = printer.client.state.lock().unwrap();
                matches!(panel::s_str(&state, "gcode_state"),
                         "RUNNING" | "PAUSE")
            };
            // a complete, key-matching copy on disk is not a download: the
            // detail pane offers Play and the folder for it instead of an
            // ETA (design doc 5.6). The key is the worker's own, so the two
            // always name the same file.
            let printer_key = cache::Cache::printer_key(&printer.cfg.serial);
            let copies = &printer.cached_copy;
            let cached = move |entry: &ftp::RemoteEntry| -> Option<PathBuf> {
                if let Some(known) = copies.borrow().get(&entry.path) {
                    return known.clone();
                }
                let key = cache::Cache::key(&printer_key, entry, None);
                let ext = browser::extension_of(&entry.name);
                let hit = disk.get(&printer_key, cache::Kind::File, key,
                                   &ext);
                copies.borrow_mut().insert(entry.path.clone(), hit.clone());
                hit
            };
            // Whether a local file may be handed to the Windows shell: its
            // header and its extension must agree that it is media, because
            // the shell picks its program by extension and runs whatever it
            // finds, while a name off the card can lie about both (design
            // doc 5.8, section 7; stage 3 security review, F2). The answer
            // reads the file's first bytes, so it is taken once per path and
            // remembered: the view asks while it paints (5.1, rule 5).
            let verdicts = &printer.shell_openable;
            let shell_openable = move |path: &Path| -> bool {
                if let Some(known) = verdicts.borrow().get(path) {
                    return *known;
                }
                let verdict = player::openable_by_shell(path);
                verdicts.borrow_mut().insert(path.to_path_buf(), verdict);
                verdict
            };
            // the fields are split apart here so the view can change the
            // browser state while it reads the player next to it
            let PrinterUi { cfg, browser, files, player, player_tex,
                            player_error, open_error, .. } = printer;
            let player_view = player.as_ref().map(|player| {
                files_view::PlayerView {
                    title: player.path().file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("recording"),
                    texture: player_tex.as_ref(),
                    index: player.index.as_ref(),
                    pos: player.pos.load(Ordering::SeqCst),
                    playing: player.is_playing(),
                    speed: player.speed(),
                    path: player.path(),
                    skipped: player.skipped(),
                }
            });
            let view = files_view::View {
                name: &cfg.name,
                serial: &cfg.serial,
                profile: status.profile,
                open_sessions: status.open_sessions,
                printing,
                dialog_open,
                now: Instant::now(),
                cache_usage,
                cache_cap,
                clearing,
                opening: printer.opening.is_some(),
                cached: Some(&cached),
                shell_openable: Some(&shell_openable),
                player: player_view,
                player_error: player_error.as_ref()
                    .map(|(text, _)| text.as_str()),
                player_error_path: player_error.as_ref()
                    .and_then(|(_, path)| path.as_deref()),
                open_error: open_error.as_deref(),
            };
            let out = files_view::show(ui, browser, files, &view);
            (out.actions, out.cmds)
        };
        for cmd in cmds {
            self.printers[selected].ftp.send(cmd);
        }
        for action in actions {
            match action {
                files_view::Action::Back => {
                    self.printers[selected].forget_disk_answers();
                    self.printers[selected].open_error = None;
                    self.printers[selected].files.close();
                    self.printers[selected].close_player();
                    self.view = AppView::Panel;
                }
                files_view::Action::Play { path, speed } =>
                    self.printers[selected].open_player(path, speed, ctx),
                files_view::Action::ClosePlayer =>
                    self.printers[selected].close_player(),
                files_view::Action::Player(cmd) => {
                    if let Some(player) = &self.printers[selected].player {
                        player.send(cmd);
                    }
                }
                // the OS player and Explorer (design doc 7, option B)
                // a failure is said in the files view, where it happened,
                // and not in the settings-error slot above every page (C3)
                files_view::Action::OpenExternally(path) => {
                    self.printers[selected].open_error = opener::open(&path)
                        .err()
                        .map(|e| format!("couldn't open the file ({e})"));
                }
                files_view::Action::Reveal(path) => {
                    self.printers[selected].open_error = opener::reveal(&path)
                        .err()
                        .map(|e| format!("couldn't show the file ({e})"));
                }
                files_view::Action::ClearCache => {
                    // deleting every cached file is disk work: it runs on
                    // a thread, and the button says so until it is done
                    // (E30, E31, D08)
                    let (cache, clearing) =
                        (self.cache.clone(), self.clearing.clone());
                    let usage = self.cache_usage.clone();
                    let ctx = ctx.clone();
                    clearing.store(true, Ordering::SeqCst);
                    self.cache_asked_at = None;
                    for printer in &mut self.printers {
                        printer.forget_disk_answers();
                    }
                    std::thread::spawn(move || {
                        // it skips the file the player has open (5.6)
                        cache.clear();
                        *usage.lock().unwrap() = Some(cache.usage_bytes());
                        clearing.store(false, Ordering::SeqCst);
                        ctx.request_repaint();
                    });
                }
                files_view::Action::Refresh => {
                    let printer = &mut self.printers[selected];
                    for cmd in printer.browser.refresh() {
                        printer.ftp.send(cmd);
                    }
                }
                files_view::Action::Retry => {
                    let printer = &mut self.printers[selected];
                    // the lane stopped until the user acted (5.3)
                    printer.ftp.send(Cmd::Retry);
                    for cmd in printer.browser.refresh() {
                        printer.ftp.send(cmd);
                    }
                }
                files_view::Action::EditPrinter => {
                    self.dialog = Dialog::AddPrinter(dialogs::AddPrinterDlg {
                        draft: self.printers[selected].cfg.clone(),
                        editing: Some(selected),
                        error: String::new(),
                    });
                }
            }
        }
    }

    fn move_printer(&mut self, src: usize, slot: usize,
                    ctx: &egui::Context) {
        if src >= self.printers.len() {
            return;
        }
        let mut dst = if slot > src { slot - 1 } else { slot };
        dst = dst.min(self.printers.len() - 1);
        if dst == src {
            return;
        }
        let active_serial =
            self.printers[self.selected].cfg.serial.clone();
        let item = self.printers.remove(src);
        self.printers.insert(dst, item);
        self.save_config();
        let new_index = self.printers.iter()
            .position(|p| p.cfg.serial == active_serial)
            .unwrap_or(0);
        self.select(new_index, ctx);
    }

    // ------------------------------------------------------------ chips
    /// One icon button. With a `reason` it doesn't sense clicks at all: it
    /// draws in the dim colour and says why on hover, instead of looking
    /// live and swallowing the click (D11, D23, A11).
    fn tool_button(ui: &mut egui::Ui, icon: ToolIcon, tip: &str,
                   reason: Option<&str>) -> bool {
        let sense = match reason {
            Some(_) => Sense::hover(),
            None => Sense::click(),
        };
        let (rect, response) = ui.allocate_exact_size(
            size::ICON_BUTTON, sense);
        let visuals = ui.style().interact(&response);
        let color = match reason {
            Some(_) => theme::TEXT_DIM,
            None => theme::TEXT,
        };
        ui.painter().rect(rect, radius::CONTROL,
                          visuals.bg_fill, visuals.bg_stroke,
                          StrokeKind::Inside);
        let c = rect.center();
        let at = |[x, y]: [f32; 2]| c + egui::Vec2::new(x, y);
        let segment = |[x0, y0, x1, y1]: [f32; 4]| [at([x0, y0]), at([x1, y1])];
        let s = Stroke::new(tool_icon::STROKE, color);
        let p = ui.painter();
        match icon {
            ToolIcon::Add => {
                p.line_segment(segment(tool_icon::PLUS_H), s);
                p.line_segment(segment(tool_icon::PLUS_V), s);
            }
            ToolIcon::Edit => {
                // pencil: body + tip
                let body = Stroke::new(tool_icon::PENCIL_STROKE, color);
                p.line_segment(segment(tool_icon::PENCIL_BODY), body);
                p.add(egui::Shape::convex_polygon(
                    tool_icon::PENCIL_TIP.iter().map(|point| at(*point))
                        .collect(),
                    color, Stroke::NONE));
            }
        }
        // an icon button has no text, so its name is the one on its
        // tooltip (A9)
        let enabled = reason.is_none();
        let named = |response: egui::Response| {
            response.widget_info(|| egui::WidgetInfo::labeled(
                egui::WidgetType::Button, enabled, tip));
            response
        };
        let response = named(response);
        // the cursor changes only where the click acts (A11, E23)
        match reason {
            Some(reason) => {
                response.on_hover_text(reason);
                false
            }
            None => response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(tip)
                .clicked(),
        }
    }

    fn chips_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut clicked: Option<usize> = None;
        let mut dropped: Option<(usize, usize)> = None;
        let dragging: Option<usize> =
            egui::DragAndDrop::payload::<usize>(ctx).map(|p| *p);
        let mut chip_rects: Vec<egui::Rect> = Vec::new();

        ui.horizontal(|ui| {
            for (i, printer) in self.printers.iter().enumerate() {
                let state = printer.client.state.lock().unwrap();
                let gcode_state =
                    panel::s_str(&state, "gcode_state").to_string();
                let pct = panel::s_i64(&state, "mc_percent").unwrap_or(0);
                drop(state);
                // a printer the app cannot reach reads Offline, with no
                // percentage: its last telemetry is not news (C4, D10)
                let online = printer.client.conn.lock().unwrap().0;
                let (word, color) = theme::state_word(&gcode_state, online);
                let selected = i == self.selected;
                let is_dragged = dragging == Some(i);

                let frame_response = ui.scope(|ui| {
                    if is_dragged {
                        // fade the original while its ghost follows the
                        // cursor
                        ui.multiply_opacity(0.35);
                    }
                    // the chip is the widget, so hover, pressed and
                    // focus land on it and the drag starts on it (C2, C4)
                    ui::widgets::clickable_sense(
                        ui, ("chip", printer.cfg.serial.as_str()),
                        &format!("{} — {word}", printer.cfg.name),
                        ui::widgets::Surface::card()
                            .radius(radius::CONTROL)
                            .padding(pad::CHIP)
                            .selected(selected, false)
                            .ring(theme::CHIP_SELECTED),
                        Sense::click_and_drag(),
                        |ui| {
                            ui.horizontal(|ui| {
                                let label = font::label();
                                // baseline-aligned status dot
                                ui.label(RichText::new("●")
                                    .font(font::caption()).color(color));
                                // a long name is truncated, and egui shows
                                // it whole on hover (C4)
                                let name = &printer.cfg.name;
                                let strong = font::body_strong();
                                let name_w = theme::text_width(ui, name,
                                                               &strong)
                                    .min(size::CHIP_NAME_MAX_W);
                                ui.allocate_ui_with_layout(
                                    egui::vec2(name_w, size::CONTROL_H),
                                    egui::Layout::left_to_right(
                                        egui::Align::Center),
                                    |ui| {
                                        ui.set_width(name_w);
                                        ui.add(egui::Label::new(
                                            RichText::new(name).font(strong))
                                            .truncate());
                                    });
                                if !word.is_empty() {
                                    ui.label(RichText::new(&word)
                                        .color(color).font(label.clone()));
                                }
                                // numbers sit in slots for their widest
                                // value, so 9% becoming 10% moves no chip
                                // to the right of this one (C4)
                                if matches!(gcode_state.as_str(),
                                            "RUNNING" | "PAUSE")
                                    && pct > 0 && online
                                {
                                    ui::widgets::slot(ui, "100%",
                                        RichText::new(format!("{pct}%"))
                                            .color(color),
                                        &label, egui::Align::Min);
                                }
                                // the download badge of section 6: the
                                // previous printer's transfers keep running
                                // and stay visible on its chip
                                if let Some(pct) =
                                    printer.browser.running_percent()
                                {
                                    ui::widgets::slot(ui, "↓ 100%",
                                        RichText::new(format!("↓ {pct}%"))
                                            .color(theme::ACCENT),
                                        &label, egui::Align::Min);
                                }
                            });
                        }).response
                }).inner;

                let mut response = frame_response;
                chip_rects.push(response.rect);
                // while something is being dragged the grab cursor rules
                if dragging.is_none() {
                    response = response.on_hover_cursor(
                        egui::CursorIcon::PointingHand);
                }
                if response.clicked() {
                    clicked = Some(i);
                }
                if response.drag_started() {
                    egui::DragAndDrop::set_payload(ctx, i);
                }
            }
        });

        // ---- live drag feedback: ghost + insertion marker + drop ----
        if let Some(src) = dragging
            && src < self.printers.len()
        {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            let pointer = ctx.input(|i| i.pointer.latest_pos());
            if let Some(pos) = pointer
                && !chip_rects.is_empty()
            {
                // insertion slot = chips whose center is left of cursor
                let slot = chip_rects.iter()
                    .filter(|r| pos.x > r.center().x).count();
                // skip no-op slots (right before / after itself)
                let moves = slot != src && slot != src + 1;
                if moves {
                    let x = if slot == 0 {
                        chip_rects[0].left() - MARKER_GAP
                    } else if slot >= chip_rects.len() {
                        chip_rects.last().unwrap().right() + MARKER_GAP
                    } else {
                        (chip_rects[slot - 1].right()
                         + chip_rects[slot].left()) / 2.0
                    };
                    let top = chip_rects[0].top() - MARKER_OVERHANG;
                    let bottom = chip_rects[0].bottom() + MARKER_OVERHANG;
                    ui.painter().line_segment(
                        [egui::pos2(x, top), egui::pos2(x, bottom)],
                        Stroke::new(stroke::HEAVY, theme::ACCENT));
                }
                // ghost tile following the cursor
                let name = self.printers[src].cfg.name.clone();
                egui::Area::new(egui::Id::new("chip-ghost"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(pos + GHOST_OFFSET)
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(theme::CARD_HOVER)
                            .stroke(Stroke::new(stroke::MEDIUM,
                                                theme::CHIP_SELECTED))
                            .corner_radius(radius::CONTROL)
                            .inner_margin(pad::CHIP)
                            .show(ui, |ui| {
                                ui.label(RichText::new(name)
                                    .font(font::body_strong()));
                            });
                    });
                if ctx.input(|i| i.pointer.any_released()) {
                    if moves {
                        dropped = Some((src, slot));
                    }
                    egui::DragAndDrop::clear_payload(ctx);
                }
                ctx.request_repaint();
            }
        }

        if let Some(index) = clicked {
            self.select(index, ctx);
        }
        if let Some((src, slot)) = dropped {
            self.move_printer(src, slot, ctx);
        }
    }

    // ---------------------------------------------------------- dialogs
    fn show_dialog(&mut self, ctx: &egui::Context) {
        let dialog = std::mem::replace(&mut self.dialog, Dialog::None);
        match dialog {
            Dialog::None => {}
            Dialog::AddPrinter(mut dlg) => {
                let (result, close) =
                    dialogs::show_add_printer(ctx, &mut dlg);
                if let dialogs::AddResult::Remove(index) = result {
                    // the confirmation of 5.4 still asks, and it says
                    // what a running download would lose (O13)
                    self.dialog = Dialog::ConfirmRemove(index);
                }
                if let dialogs::AddResult::Save(new_cfg, editing) = result {
                    match editing {
                        Some(index) if index < self.printers.len() => {
                            let old = self.printers[index].cfg.clone();
                            let conn_changed = new_cfg.ip != old.ip
                                || new_cfg.serial != old.serial
                                || new_cfg.access_code != old.access_code;
                            let running =
                                active_transfers(&self.printers[index]);
                            match (conn_changed, running) {
                                // a connection edit replaces the worker, so
                                // its transfers are discarded: ask first
                                // (design doc 5.4)
                                (true, 1..) =>
                                    self.pending_edit = Some((index,
                                                              new_cfg)),
                                (true, 0) =>
                                    self.apply_edit(index, new_cfg, ctx),
                                (false, _) =>
                                    self.printers[index].cfg = new_cfg,
                            }
                        }
                        _ => {
                            self.printers.push(PrinterUi::new(
                                new_cfg, ctx, self.cache.clone()));
                            let last = self.printers.len() - 1;
                            self.select(last, ctx);
                        }
                    }
                    self.save_config();
                }
                if !close {
                    self.dialog = Dialog::AddPrinter(dlg);
                }
            }
            Dialog::Temp(mut dlg) => {
                let client = self.printers[self.selected].client.clone();
                if !dialogs::show_temp(ctx, &mut dlg, &client) {
                    self.dialog = Dialog::Temp(dlg);
                }
            }
            Dialog::Speed => {
                let printer = &self.printers[self.selected];
                let lvl = {
                    let state = printer.client.state.lock().unwrap();
                    panel::s_i64(&state, "spd_lvl").unwrap_or(2)
                };
                let client = printer.client.clone();
                if !dialogs::show_speed(ctx, lvl, &client) {
                    self.dialog = Dialog::Speed;
                }
            }
            Dialog::Fans => {
                let printer = &self.printers[self.selected];
                let state = printer.client.state.lock().unwrap().clone();
                let client = printer.client.clone();
                if !dialogs::show_fans(ctx, &state, &client) {
                    self.dialog = Dialog::Fans;
                }
            }
            Dialog::Move => {
                let client = self.printers[self.selected].client.clone();
                if !dialogs::show_move(ctx, &client) {
                    self.dialog = Dialog::Move;
                }
            }
            Dialog::Info(mut dlg) => {
                let printer = &self.printers[self.selected];
                let device_info =
                    printer.client.device_info.lock().unwrap().clone();
                let view = dialogs::InfoView {
                    cfg: &printer.cfg,
                    device_info: &device_info,
                    model: model_from_serial(&printer.cfg.serial),
                    fw_current: panel::ota_version(&device_info),
                    fw_latest: printer.fw_latest.clone(),
                };
                let (close, latest) =
                    dialogs::show_info(ctx, &mut dlg, &view);
                if let Some(version) = latest {
                    self.printers[self.selected].fw_latest = version;
                }
                if !close {
                    self.dialog = Dialog::Info(dlg);
                }
            }
            Dialog::Skip(mut dlg) => {
                let printer = &self.printers[self.selected];
                // a new job cleared the bundle while the dialog was open:
                // it says so and waits for Close instead of vanishing (D12)
                let Some(bundle) = printer.job_bundle.clone() else {
                    if !dialogs::show_skip_gone(ctx) {
                        self.dialog = Dialog::Skip(dlg);
                    }
                    return;
                };
                let live: HashSet<i64> = {
                    let state = printer.client.state.lock().unwrap();
                    state.get("s_obj").and_then(|v| v.as_array())
                        .map(|arr| arr.iter()
                            .filter_map(|v| v.as_i64()).collect())
                        .unwrap_or_default()
                };
                let client = printer.client.clone();
                if !dialogs::show_skip(ctx, &mut dlg, &bundle, &live,
                                       &client) {
                    self.dialog = Dialog::Skip(dlg);
                }
            }
            Dialog::Hms => {
                let printer = &self.printers[self.selected];
                let name = printer.cfg.name.clone();
                let errors: Vec<(String, String)> = {
                    let state = printer.client.state.lock().unwrap();
                    state.get("hms").and_then(|v| v.as_array())
                        .map(|arr| arr.iter().map(|h| {
                            let ecode = hms::ecode(
                                h.get("attr").and_then(|v| v.as_u64())
                                    .unwrap_or(0),
                                h.get("code").and_then(|v| v.as_u64())
                                    .unwrap_or(0));
                            (hms::dashed(&ecode),
                             hms::lookup(&ecode).unwrap_or_default())
                        }).collect())
                        .unwrap_or_default()
                };
                if !dialogs::show_hms(ctx, &name, &errors) {
                    self.dialog = Dialog::Hms;
                }
            }
            Dialog::Maintenance(mut dlg) => {
                let printer = &self.printers[self.selected];
                let has_ams = {
                    let state = printer.client.state.lock().unwrap();
                    state.get("ams")
                        .and_then(|v| v.get("ams"))
                        .and_then(|v| v.as_array())
                        .is_some_and(|arr| !arr.is_empty())
                };
                let client = printer.client.clone();
                if !dialogs::show_maintenance(ctx, &mut dlg, has_ams,
                                              &client) {
                    self.dialog = Dialog::Maintenance(dlg);
                }
            }
            Dialog::ConfirmStop => {
                let printer = &self.printers[self.selected];
                let (close, yes) = dialogs::show_confirm(
                    ctx, "confirm-stop",
                    &format!("Stop the current print on {}?\n\
                              This cannot be undone.", printer.cfg.name),
                    "Stop print");
                if yes {
                    printer.client.stop_print();
                }
                if !close {
                    self.dialog = Dialog::ConfirmStop;
                }
            }
            Dialog::ConfirmRemove(index) => {
                if index >= self.printers.len() {
                    return;
                }
                let name = self.printers[index].cfg.name.clone();
                // removing a printer stops its worker, so a running
                // download is discarded and the question says so (5.4)
                let running = active_transfers(&self.printers[index]);
                let mut text = format!("Remove {name} from the app?");
                if let Some(note) =
                    files_view::active_transfer_note(running)
                {
                    text.push('\n');
                    text.push_str(&note);
                }
                let (close, yes) = dialogs::show_confirm(
                    ctx, "confirm-remove", &text, "Remove");
                if yes {
                    let mut printer = self.printers.remove(index);
                    printer.shutdown();
                    self.save_config();
                    if !self.printers.is_empty() {
                        let next =
                            self.selected.min(self.printers.len() - 1);
                        self.select(next, ctx);
                    }
                }
                if !close {
                    self.dialog = Dialog::ConfirmRemove(index);
                }
            }
        }
    }

    /// Replaces a printer whose connection was edited. Its worker is
    /// stopped and the replacement opens no session until the old lane
    /// threads have ended (design doc 4, rule 3).
    fn apply_edit(&mut self, index: usize, new_cfg: PrinterCfg,
                  ctx: &egui::Context) {
        self.printers[index].shutdown();
        let replacement =
            PrinterUi::new_replacing(new_cfg, ctx, &self.printers[index]);
        self.printers[index] = replacement;
        if index == self.selected {
            self.printers[index].set_active(true, ctx);
        }
    }

    /// Starts the "Download & play" named by `BAMBU_CONTROL_PLAY`, once the
    /// listing that carries the file has arrived. Debug builds only; it
    /// goes through exactly the path the button does, so what the
    /// screenshots show is the real one.
    #[cfg(debug_assertions)]
    fn start_debug_play(&mut self) {
        let Some(wanted) = self.debug_play.clone() else { return };
        let Some(printer) = self.printers.get_mut(self.selected) else {
            return;
        };
        let found = printer.browser.timelapses.iter()
            .filter_map(|item| item.video.clone())
            .find(|video| video.path == wanted);
        let Some(video) = found else { return };
        printer.files.selected = Some(video.path.clone());
        if let Some(cmd) = printer.browser
            .download(&video, browser::Dest::Cache { open_after: true })
        {
            printer.ftp.send(cmd);
        }
        self.debug_play = None;
    }

    /// Transfers running across every printer, for the close confirmation.
    fn running_transfers(&self) -> usize {
        self.printers.iter().map(active_transfers).sum()
    }

    /// The confirmations that are not part of `Dialog`: closing the app and
    /// editing a connection, both of which discard running transfers
    /// (design doc 5.4, section 6).
    fn show_confirmations(&mut self, ctx: &egui::Context) {
        if self.pending_close {
            let note = files_view::active_transfer_note(
                self.running_transfers()).unwrap_or_default();
            let (close, yes) = dialogs::show_confirm(
                ctx, "confirm-close",
                &format!("Close Bambu Control?\n{note}"), "Close anyway");
            if yes {
                // stop everything and go; `.part` files left behind are
                // deleted at the next start (design doc 5.4)
                self.closing = true;
                for printer in &mut self.printers {
                    printer.shutdown();
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if close {
                self.pending_close = false;
            }
        }
        if let Some((index, cfg)) = self.pending_edit.clone() {
            let running = self.printers.get(index).map_or(0, active_transfers);
            let note = files_view::active_transfer_note(running)
                .unwrap_or_default();
            let (close, yes) = dialogs::show_confirm(
                ctx, "confirm-edit",
                &format!("Change this printer's connection?\n{note}"),
                "Change");
            if yes {
                self.apply_edit(index, cfg, ctx);
                self.save_config();
            }
            if close {
                self.pending_edit = None;
            }
        }
    }

    fn handle_action(&mut self, action: PanelAction) {
        let printer = &mut self.printers[self.selected];
        let state = printer.client.state.lock().unwrap().clone();
        match action {
            PanelAction::OpenNozzle => {
                let target = panel::s_f64(&state, "nozzle_target_temper")
                    .unwrap_or(0.0) as i64;
                self.dialog = Dialog::Temp(dialogs::TempDlg {
                    nozzle: true,
                    value: if target > 0 { target.to_string() }
                           else { String::new() },
                });
            }
            PanelAction::OpenBed => {
                let target = panel::s_f64(&state, "bed_target_temper")
                    .unwrap_or(0.0) as i64;
                self.dialog = Dialog::Temp(dialogs::TempDlg {
                    nozzle: false,
                    value: if target > 0 { target.to_string() }
                           else { String::new() },
                });
            }
            PanelAction::OpenSpeed => self.dialog = Dialog::Speed,
            PanelAction::OpenFans => self.dialog = Dialog::Fans,
            PanelAction::OpenMove => {
                let gcode_state = panel::s_str(&state, "gcode_state");
                if panel::IDLE_STATES.contains(&gcode_state) {
                    self.dialog = Dialog::Move;
                }
            }
            PanelAction::OpenInfo => {
                let device_info =
                    printer.client.device_info.lock().unwrap().clone();
                self.dialog = Dialog::Info(dialogs::InfoDlg::new(
                    &panel::ota_version(&device_info),
                    &printer.fw_latest));
            }
            PanelAction::OpenSkip => {
                if printer.job_bundle.as_ref()
                    .is_some_and(|b| !b.objects.is_empty())
                {
                    self.dialog = Dialog::Skip(dialogs::SkipDlg {
                        selected: HashSet::new(),
                        confirm: false,
                    });
                }
            }
            PanelAction::TogglePause => {
                if panel::s_str(&state, "gcode_state") == "PAUSE" {
                    printer.client.resume();
                } else {
                    printer.client.pause();
                }
            }
            PanelAction::AskStop => self.dialog = Dialog::ConfirmStop,
            PanelAction::OpenHmsDialog => self.dialog = Dialog::Hms,
            PanelAction::OpenMaintenance => {
                let nozzle_type =
                    panel::s_str(&state, "nozzle_type").to_string();
                let diameter =
                    panel::s_f64(&state, "nozzle_diameter").unwrap_or(0.4);
                self.dialog = Dialog::Maintenance(
                    dialogs::MaintenanceDlg::new(&nozzle_type, diameter));
            }
            PanelAction::SetLight(on) => {
                printer.light_pending = Some((on, Instant::now()));
                printer.light_unconfirmed = false;
                printer.client.set_light(on);
            }
            PanelAction::OpenFiles => self.open_files(),
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            // the zoom the window was left at (decision O7)
            ctx.set_zoom_factor(self.cfg.ui.zoom_factor());
            if !self.printers.is_empty() {
                self.select(0, ctx);
            }
            #[cfg(debug_assertions)]
            self.open_files_from_env(ctx);
        }
        for printer in &mut self.printers {
            printer.sync(ctx);
        }
        if self.selected >= self.printers.len() {
            self.selected = 0;
        }
        self.save_zoom(ctx);
        #[cfg(debug_assertions)]
        self.start_debug_play();
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;

        // closing with a transfer running asks first: the close is
        // cancelled and the question goes on screen (design doc 5.4)
        if ctx.input(|i| i.viewport().close_requested())
            && !self.closing
            && self.running_transfers() > 0
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_close = true;
        }

        egui::Panel::top("bar")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::BG)
                .inner_margin(pad::BAR))
            .show(root, |ui| {
                // the bar is a chip tall; the tool buttons take the right
                // end of it first and the chips get what is left, so a
                // long chip row is cut off rather than pushing Edit, Add
                // and Remove out of the window (C4, D09)
                let gap = ui.spacing().item_spacing.x;
                let chip_h = size::CONTROL_H + pad::CHIP.sum().y
                    + 2.0 * stroke::HAIRLINE;
                let (bar, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), chip_h), Sense::hover());
                // Edit and Add; Remove moved into the Edit dialog (O13)
                let tools_w = 2.0 * size::ICON_BUTTON.x + gap;
                let chips = egui::Rect::from_min_max(
                    bar.min,
                    egui::pos2((bar.max.x - tools_w - gap).max(bar.min.x),
                               bar.max.y));
                let mut chips_ui = ui.new_child(egui::UiBuilder::new()
                    .max_rect(chips)
                    .layout(egui::Layout::left_to_right(egui::Align::Min)));
                chips_ui.set_clip_rect(chips.intersect(ui.clip_rect()));
                self.chips_bar(&mut chips_ui, ctx);
                // the tools sit at the right end, laid out left to right
                // inside it, so the keyboard walks them in the order the
                // eye reads them (A8)
                let tools = egui::Rect::from_min_max(
                    egui::pos2((bar.max.x - tools_w).max(bar.min.x),
                               bar.min.y),
                    bar.max);
                let mut tools_ui = ui.new_child(egui::UiBuilder::new()
                    .max_rect(tools)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)));
                {
                    let ui = &mut tools_ui;
                        // with no printers there is nothing to edit or
                        // remove, and the buttons say so (D11)
                        let none = self.printers.is_empty()
                            .then_some("No printers yet");
                        if Self::tool_button(ui, ToolIcon::Edit,
                                             "Edit current printer", none)
                        {
                            self.dialog = Dialog::AddPrinter(
                                dialogs::AddPrinterDlg {
                                    draft: self.printers[self.selected]
                                        .cfg.clone(),
                                    editing: Some(self.selected),
                                    error: String::new(),
                                });
                        }
                        if Self::tool_button(ui, ToolIcon::Add,
                                             "Add printer", None)
                        {
                            self.dialog = Dialog::AddPrinter(
                                dialogs::AddPrinterDlg {
                                    draft: PrinterCfg::default(),
                                    editing: None,
                                    error: String::new(),
                                });
                        }
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG)
                .inner_margin(pad::PAGE))
            .show(root, |ui| {
                if let Some(error) = &self.config_error {
                    ui.push_id("settings-error", |ui| {
                        ui::widgets::banner(ui, ui::widgets::Tone::Danger,
                                            None, error);
                    });
                }
                // the files view is a full page, outside the panel's
                // ScrollArea: a nested show_rows inside it would break the
                // grid's virtualisation (design doc 6)
                if self.view == AppView::Files && !self.printers.is_empty() {
                    self.show_files(ui, ctx);
                    return;
                }
                // one scroll offset per printer (E1, D06)
                let panel_salt = ("panel", self.printers.get(self.selected)
                    .map(|printer| printer.cfg.serial.clone()));
                egui::ScrollArea::vertical().id_salt(panel_salt)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                if self.printers.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(
                            "Add a printer to get started")
                            .color(theme::TEXT_DIM).font(font::title()));
                    });
                    return;
                }
                let printer = &mut self.printers[self.selected];
                let state = printer.client.state.lock().unwrap().clone();
                let device_info =
                    printer.client.device_info.lock().unwrap().clone();

                // resolve pending light toggle vs telemetry
                let mut shown_on =
                    panel::light_on(&state).unwrap_or(false);
                if let Some((desired, ts)) = printer.light_pending {
                    if shown_on == desired {
                        printer.light_pending = None;
                        printer.light_unconfirmed = false;
                    } else if ts.elapsed() > LIGHT_PENDING {
                        // the command went and telemetry never agreed: say
                        // so rather than snapping the switch back (C18, D40)
                        printer.light_pending = None;
                        printer.light_unconfirmed = true;
                    } else {
                        shown_on = desired;
                        // the timeout is a deadline: without it nothing
                        // would repaint to notice it passed (E32)
                        ctx.request_repaint_after(
                            LIGHT_PENDING.saturating_sub(ts.elapsed()));
                    }
                }

                let model = model_from_serial(&printer.cfg.serial);
                // FILES card: counts from the listing taken this session
                let (timelapses, _, print_files) = printer.browser.counts();
                let files_summary = match printer.browser.updated_at() {
                    Some(_) => format!(
                        "{timelapses} timelapses  ·  {print_files} files"),
                    None =>
                        "Timelapses · Recordings · Print files".to_string(),
                };
                let view = PanelView {
                    state: &state,
                    connected: printer.client.conn.lock().unwrap()
                        .clone(),
                    cam_texture: printer.cam_texture.as_ref(),
                    cam_status: printer.camera.as_ref()
                        .map(|c| c.status.lock().unwrap().clone())
                        .unwrap_or_else(|| "camera paused".into()),
                    plate_texture: printer.plate_texture.as_ref(),
                    fetch_progress: printer.ftp
                        .job_progress(&printer.current_job),
                    object_count: printer.job_bundle.as_ref()
                        .map(|b| b.objects.len()).unwrap_or(0),
                    fw_current: panel::ota_version(&device_info),
                    fw_latest: printer.fw_latest.clone(),
                    show_humidity: !model.contains("A1"),
                    model,
                    hms: &printer.hms_lines,
                    light_shown_on: shown_on,
                    light_pending: printer.light_pending.is_some(),
                    light_unconfirmed: printer.light_unconfirmed,
                    files_summary,
                };
                let actions = panel::show(ui, &view);
                for action in actions {
                    self.handle_action(action);
                }
                    });
            });

        self.show_dialog(ctx);
        self.show_confirmations(ctx);
    }

    fn on_exit(&mut self) {
        for printer in &mut self.printers {
            printer.shutdown();
        }
    }
}

fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            egui::IconData {
                width: rgba.width(),
                height: rgba.height(),
                rgba: rgba.into_raw(),
            }
        }
        Err(_) => egui::IconData::default(),
    }
}

fn main() -> eframe::Result {
    // design doc 4, rule 6: a second instance would double every printer's
    // FTP session count, so it says so and exits without connecting
    let _instance: instance::Instance =
        match instance::acquire(instance::MUTEX_NAME) {
            Ok(instance) => instance,
            Err(instance::AlreadyRunning) => {
                instance::show_already_running();
                return Ok(());
            }
        };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size::WINDOW)
            .with_min_inner_size(size::WINDOW_MIN)
            .with_title("Bambu Control")
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native("Bambu Control", options,
                       Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}
