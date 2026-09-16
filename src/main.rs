//! Bambu Control — LAN control panel for Bambu Lab printers.
//! Rust + egui port of the PySide6 app: one printer visible at a time,
//! top selector chips with live state, white border on the active one.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod camera;
mod config;
mod files;
mod firmware;
mod ftp;
mod hms;
mod instance;
mod mqtt;
mod theme;
mod tls;
mod ui;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, RichText, Sense, Stroke, StrokeKind};

use browser::{BrowserState, Cmd, Event, FtpWorker};
use config::{Config, PrinterCfg, model_from_serial};
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
    /// browse lane: listings, thumbnails and the running job's bundle, on
    /// one session at a time (design doc 4, rule 1)
    ftp: FtpWorker,
    /// what the files view shows
    browser: BrowserState,
    /// the files view's own state: tab, filter, selection and textures
    files: FilesUi,
    job_bundle: Option<files::JobBundle>,
    plate_texture: Option<egui::TextureHandle>,
    current_job: String,
    /// last job name and state seen on MQTT, to tell the worker when a job
    /// is starting (design doc 4, rule 5)
    last_job: String,
    last_gcode_state: String,
    light_pending: Option<(bool, Instant)>,
}

impl PrinterUi {
    fn new(cfg: PrinterCfg, ctx: &egui::Context) -> Self {
        Self::with_worker(cfg.clone(), ctx, FtpWorker::start(&cfg, ctx))
    }

    /// The printer that replaces one whose connection was edited: its
    /// worker opens no session until the old lane thread has ended
    /// (design doc 4, rule 3).
    fn new_replacing(cfg: PrinterCfg, ctx: &egui::Context,
                     previous: &PrinterUi) -> Self {
        let ftp = FtpWorker::start_replacing(&cfg, ctx, &previous.ftp);
        Self::with_worker(cfg, ctx, ftp)
    }

    fn with_worker(cfg: PrinterCfg, ctx: &egui::Context,
                   ftp: FtpWorker) -> Self {
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
            browser: BrowserState::default(),
            files: FilesUi::default(),
            job_bundle: None,
            plate_texture: None,
            current_job: String::new(),
            last_job: String::new(),
            last_gcode_state: String::new(),
            light_pending: None,
        }
    }

    fn set_active(&mut self, active: bool, ctx: &egui::Context) {
        // a background printer drops its prefetch and closes its session
        self.ftp.send(Cmd::SetBackground(!active));
        if active && self.camera.is_none() {
            self.camera = Some(camera::Camera::start(
                self.cfg.ip.clone(), self.cfg.access_code.clone(),
                ctx.clone()));
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
        self.client.stop();
        self.ftp.stop();
    }

    /// Per-frame sync of async results into UI state.
    fn sync(&mut self, ctx: &egui::Context) {
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
                        format!("cam-{}", self.cfg.serial), frame,
                        Default::default()));
                }
            }
        }
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
                self.plate_texture = Some(ctx.load_texture(
                    format!("plate-{}", self.cfg.serial),
                    egui::ColorImage::from_rgba_unmultiplied(
                        size, rgba.as_raw()),
                    Default::default()));
            }
            self.job_bundle = Some(bundle);
        }

        // auto-fetch job data when a new print shows up
        let state = self.client.state.lock().unwrap().clone();
        let gcode_state = panel::s_str(&state, "gcode_state").to_string();
        let job = {
            let s = panel::s_str(&state, "subtask_name");
            if s.is_empty() {
                panel::s_str(&state, "gcode_file")
            } else {
                s
            }
        }.to_string();
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
            self.ftp.send(Cmd::JobBundle {
                job,
                file_name: panel::s_str(&state, "gcode_file").to_string(),
                print_type: panel::s_str(&state, "print_type").to_string(),
            });
        }
    }
}

enum ToolIcon {
    Edit,
    Add,
    Trash,
}

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
    /// a failed save, or an unreadable config.toml; shown above the panel
    config_error: Option<String>,
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
        let printers: Vec<PrinterUi> = cfg.printers.iter()
            .map(|p| PrinterUi::new(p.clone(), ctx))
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
            started: false, store, config_error,
        };
        if migrated {
            app.save_config();
        }
        app
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
        // the session line ticks while a session connects or sits idle
        ctx.request_repaint_after(Duration::from_millis(100));
        // a dialog is painted over this view (Edit printer, for example):
        // it owns the keyboard while it is open (design doc 6)
        let dialog_open = !matches!(self.dialog, Dialog::None);
        let (actions, cmds) = {
            let printer = &mut self.printers[selected];
            let status = printer.ftp.status();
            let printing = {
                let state = printer.client.state.lock().unwrap();
                matches!(panel::s_str(&state, "gcode_state"),
                         "RUNNING" | "PAUSE")
            };
            let view = files_view::View {
                name: &printer.cfg.name,
                serial: &printer.cfg.serial,
                profile: status.profile,
                open_sessions: status.open_sessions,
                printing,
                dialog_open,
                now: Instant::now(),
            };
            let out = files_view::show(ui, &mut printer.browser,
                                       &mut printer.files, &view);
            (out.actions, out.cmds)
        };
        for cmd in cmds {
            self.printers[selected].ftp.send(cmd);
        }
        for action in actions {
            match action {
                files_view::Action::Back => {
                    self.printers[selected].files.close();
                    self.view = AppView::Panel;
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
    fn tool_button(ui: &mut egui::Ui, icon: ToolIcon,
                   tip: &str) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(36.0, 30.0), Sense::click());
        let visuals = ui.style().interact(&response);
        ui.painter().rect(rect, CornerRadius::same(10),
                          visuals.bg_fill, visuals.bg_stroke,
                          StrokeKind::Inside);
        let c = rect.center();
        let s = Stroke::new(1.7, theme::TEXT);
        let p = ui.painter();
        match icon {
            ToolIcon::Add => {
                p.line_segment([c + egui::vec2(-6.0, 0.0),
                                c + egui::vec2(6.0, 0.0)], s);
                p.line_segment([c + egui::vec2(0.0, -6.0),
                                c + egui::vec2(0.0, 6.0)], s);
            }
            ToolIcon::Edit => {
                // pencil: body + tip
                let body = Stroke::new(2.4, theme::TEXT);
                p.line_segment([c + egui::vec2(-4.5, 5.5),
                                c + egui::vec2(4.0, -3.0)], body);
                p.add(egui::Shape::convex_polygon(
                    vec![c + egui::vec2(4.9, -6.4),
                         c + egui::vec2(6.4, -4.9),
                         c + egui::vec2(3.2, -2.2),
                         c + egui::vec2(2.2, -3.2)],
                    theme::TEXT, Stroke::NONE));
            }
            ToolIcon::Trash => {
                // lid + handle
                p.line_segment([c + egui::vec2(-7.0, -5.0),
                                c + egui::vec2(7.0, -5.0)], s);
                p.line_segment([c + egui::vec2(-2.5, -7.5),
                                c + egui::vec2(2.5, -7.5)], s);
                // body
                let body = egui::Rect::from_min_max(
                    c + egui::vec2(-5.0, -3.0), c + egui::vec2(5.0, 7.0));
                p.rect(body, CornerRadius::same(2),
                       Color32::TRANSPARENT, s, StrokeKind::Inside);
                // inner lines
                p.line_segment([c + egui::vec2(-1.7, -1.0),
                                c + egui::vec2(-1.7, 5.0)],
                               Stroke::new(1.2, theme::TEXT));
                p.line_segment([c + egui::vec2(1.7, -1.0),
                                c + egui::vec2(1.7, 5.0)],
                               Stroke::new(1.2, theme::TEXT));
            }
        }
        if response.hovered() {
            ui.output_mut(|o| {
                o.cursor_icon = egui::CursorIcon::PointingHand;
            });
        }
        response.on_hover_text(tip).clicked()
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
                let color = theme::state_color(&gcode_state);
                let selected = i == self.selected;
                let is_dragged = dragging == Some(i);

                let frame_response = ui.scope(|ui| {
                    if is_dragged {
                        // fade the original while its ghost follows the
                        // cursor
                        ui.multiply_opacity(0.35);
                    }
                    egui::Frame::new()
                        .fill(theme::CARD)
                        .stroke(if selected {
                            Stroke::new(2.0, Color32::WHITE)
                        } else {
                            Stroke::new(1.0, theme::BORDER)
                        })
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(12, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // baseline-aligned status dot
                                ui.label(RichText::new("●")
                                    .size(10.0).color(color));
                                ui.label(RichText::new(&printer.cfg.name)
                                    .font(theme::bold(14.0)));
                                let mut parts = Vec::new();
                                if !gcode_state.is_empty() {
                                    let mut s = gcode_state.to_lowercase();
                                    if let Some(c) = s.get_mut(0..1) {
                                        c.make_ascii_uppercase();
                                    }
                                    parts.push(s);
                                }
                                if matches!(gcode_state.as_str(),
                                            "RUNNING" | "PAUSE")
                                    && pct > 0
                                {
                                    parts.push(format!("{pct}%"));
                                }
                                if !parts.is_empty() {
                                    ui.label(RichText::new(
                                        parts.join("  "))
                                        .color(color)
                                        .font(theme::bold(11.0)));
                                }
                            });
                        })
                        .response
                }).inner;

                let response = frame_response
                    .interact(Sense::click_and_drag());
                chip_rects.push(response.rect);
                if response.hovered() && dragging.is_none() {
                    ui.output_mut(|o| {
                        o.cursor_icon = egui::CursorIcon::PointingHand;
                    });
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
            ctx.output_mut(|o| {
                o.cursor_icon = egui::CursorIcon::Grabbing;
            });
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
                        chip_rects[0].left() - 4.0
                    } else if slot >= chip_rects.len() {
                        chip_rects.last().unwrap().right() + 4.0
                    } else {
                        (chip_rects[slot - 1].right()
                         + chip_rects[slot].left()) / 2.0
                    };
                    let top = chip_rects[0].top() - 3.0;
                    let bottom = chip_rects[0].bottom() + 3.0;
                    ui.painter().line_segment(
                        [egui::pos2(x, top), egui::pos2(x, bottom)],
                        Stroke::new(3.0, theme::ACCENT));
                }
                // ghost tile following the cursor
                let name = self.printers[src].cfg.name.clone();
                egui::Area::new(egui::Id::new("chip-ghost"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(pos + egui::vec2(14.0, 10.0))
                    .interactable(false)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(theme::CARD_HOVER)
                            .stroke(Stroke::new(1.5, Color32::WHITE))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(egui::Margin::symmetric(12, 6))
                            .show(ui, |ui| {
                                ui.label(RichText::new(name).font(theme::bold(14.0)));
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
                if let dialogs::AddResult::Save(new_cfg, editing) = result {
                    match editing {
                        Some(index) if index < self.printers.len() => {
                            let old = self.printers[index].cfg.clone();
                            let conn_changed = new_cfg.ip != old.ip
                                || new_cfg.serial != old.serial
                                || new_cfg.access_code != old.access_code;
                            if conn_changed {
                                self.printers[index].shutdown();
                                // the replacement opens no session until the
                                // old lane thread has ended (4, rule 3)
                                let replacement = PrinterUi::new_replacing(
                                    new_cfg, ctx, &self.printers[index]);
                                self.printers[index] = replacement;
                                if index == self.selected {
                                    self.printers[index]
                                        .set_active(true, ctx);
                                }
                            } else {
                                self.printers[index].cfg = new_cfg;
                            }
                        }
                        _ => {
                            self.printers
                                .push(PrinterUi::new(new_cfg, ctx));
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
                let Some(bundle) = printer.job_bundle.clone() else {
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
            Dialog::ConfirmRemove => {
                let name =
                    self.printers[self.selected].cfg.name.clone();
                let (close, yes) = dialogs::show_confirm(
                    ctx, "confirm-remove",
                    &format!("Remove {name} from the app?"), "Remove");
                if yes {
                    let mut printer = self.printers.remove(self.selected);
                    printer.shutdown();
                    self.save_config();
                    if !self.printers.is_empty() {
                        let index =
                            self.selected.min(self.printers.len() - 1);
                        self.select(index, ctx);
                    }
                }
                if !close {
                    self.dialog = Dialog::ConfirmRemove;
                }
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
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;

        egui::Panel::top("bar")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::BG)
                .inner_margin(egui::Margin::symmetric(10, 8)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    self.chips_bar(ui, ctx);
                    ui.with_layout(egui::Layout::right_to_left(
                        egui::Align::Center), |ui| {
                        if Self::tool_button(ui, ToolIcon::Trash,
                                             "Remove current printer")
                            && !self.printers.is_empty()
                        {
                            self.dialog = Dialog::ConfirmRemove;
                        }
                        if Self::tool_button(ui, ToolIcon::Add,
                                             "Add printer")
                        {
                            self.dialog = Dialog::AddPrinter(
                                dialogs::AddPrinterDlg {
                                    draft: PrinterCfg::default(),
                                    editing: None,
                                    error: String::new(),
                                });
                        }
                        if Self::tool_button(ui, ToolIcon::Edit,
                                             "Edit current printer")
                            && !self.printers.is_empty()
                        {
                            self.dialog = Dialog::AddPrinter(
                                dialogs::AddPrinterDlg {
                                    draft: self.printers[self.selected]
                                        .cfg.clone(),
                                    editing: Some(self.selected),
                                    error: String::new(),
                                });
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG)
                .inner_margin(10))
            .show(root, |ui| {
                if let Some(error) = &self.config_error {
                    ui.label(RichText::new(error).color(theme::DANGER));
                }
                // the files view is a full page, outside the panel's
                // ScrollArea: a nested show_rows inside it would break the
                // grid's virtualisation (design doc 6)
                if self.view == AppView::Files && !self.printers.is_empty() {
                    self.show_files(ui, ctx);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink(false)
                    .show(ui, |ui| {
                if self.printers.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(
                            "Add a printer to get started")
                            .color(theme::TEXT_DIM).size(15.0));
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
                    if shown_on == desired
                        || ts.elapsed().as_secs() > 6
                    {
                        printer.light_pending = None;
                    } else {
                        shown_on = desired;
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
                    light_shown_on: shown_on,
                    files_summary,
                };
                let actions = panel::show(ui, &view);
                for action in actions {
                    self.handle_action(action);
                }
                    });
            });

        self.show_dialog(ctx);
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
            .with_inner_size([1080.0, 780.0])
            .with_min_inner_size([700.0, 480.0])
            .with_title("Bambu Control")
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native("Bambu Control", options,
                       Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}
