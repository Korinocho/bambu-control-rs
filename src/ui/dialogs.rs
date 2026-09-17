//! Modal dialogs — ports of `ui/dialogs.py` on egui::Modal.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Stroke};

use crate::config::{self, PrinterCfg};
use crate::files::JobBundle;
use crate::firmware::is_newer;
use crate::mqtt::PrinterClient;
use crate::theme::{self, font, pad, radius, size, space, stroke};
use crate::ui::widgets;

/// How long the "New firmware … available" toast stays up.
const TOAST: Duration = Duration::from_millis(3500);

pub enum Dialog {
    None,
    AddPrinter(AddPrinterDlg),
    Temp(TempDlg),
    Speed,
    Fans,
    Move,
    Info(InfoDlg),
    Skip(SkipDlg),
    Hms,
    Maintenance(MaintenanceDlg),
    ConfirmStop,
    ConfirmRemove,
}

fn modal(ctx: &egui::Context, id: &str, width: f32,
         add: impl FnOnce(&mut egui::Ui) -> bool) -> bool {
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new(id))
        .frame(egui::Frame::new()
            .fill(theme::BG)
            .stroke(Stroke::new(stroke::HAIRLINE, theme::BORDER))
            .corner_radius(radius::CARD)
            .inner_margin(pad::MODAL))
        .show(ctx, |ui| {
            ui.set_width(width);
            close = add(ui);
        });
    close || response.should_close()
}

fn header(ui: &mut egui::Ui, text: &str) {
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).font(font::title()));
    });
    ui.add_space(space::L);
}

fn wide_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(egui::Button::new(text)
        .min_size(egui::vec2(ui.available_width(), size::BUTTON_H)))
        .clicked()
}

pub(crate) fn accent_button(ui: &mut egui::Ui, text: &str) -> bool {
    accent_button_response(ui, text,
                           egui::vec2(ui.available_width(), size::BUTTON_H))
        .clicked()
}

/// The same accent action at a given size, and with its response: the files
/// view puts its default button in a row and focuses it (section 6).
pub(crate) fn accent_button_response(ui: &mut egui::Ui, text: &str,
                                     min_size: egui::Vec2) -> egui::Response {
    let btn = egui::Button::new(
        RichText::new(text).color(theme::ON_ACCENT)
            .font(font::button_strong()))
        .fill(theme::ACCENT)
        .min_size(min_size);
    ui.add(btn)
}

// ------------------------------------------------------------ add/edit
pub struct AddPrinterDlg {
    pub draft: PrinterCfg,
    pub editing: Option<usize>,
    pub error: String,
}

pub enum AddResult {
    None,
    Save(PrinterCfg, Option<usize>),
}

pub fn show_add_printer(ctx: &egui::Context, dlg: &mut AddPrinterDlg)
                        -> (AddResult, bool) {
    let mut result = AddResult::None;
    let close = modal(ctx, "add-printer", size::MODAL_M, |ui| {
        let mut done = false;
        header(ui, if dlg.editing.is_some() { "Edit printer" }
                   else { "Add printer" });
        egui::Grid::new("printer-form").num_columns(2)
            .spacing([space::L, space::M]).show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut dlg.draft.name);
                ui.end_row();
                ui.label("IP address");
                ui.text_edit_singleline(&mut dlg.draft.ip);
                ui.end_row();
                ui.label("Serial");
                ui.text_edit_singleline(&mut dlg.draft.serial);
                ui.end_row();
                ui.label("Access code");
                ui.text_edit_singleline(&mut dlg.draft.access_code);
                ui.end_row();
            });
        ui.add_space(space::XS);
        ui.label(RichText::new(
            "Printer must be in LAN mode with Developer Mode ON.")
            .color(theme::TEXT_DIM).font(font::caption()));
        if !dlg.error.is_empty() {
            ui.label(RichText::new(&dlg.error).color(theme::DANGER));
        }
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                done = true;
            }
            if accent_button(ui, "Save") {
                let d = &mut dlg.draft;
                for field in [&mut d.name, &mut d.ip, &mut d.serial,
                              &mut d.access_code] {
                    *field = field.trim().to_string();
                }
                // the value the FTPS certificate check compares with the CN
                d.serial = config::normalize_serial(&d.serial);
                if d.ip.is_empty() || d.serial.is_empty()
                    || d.access_code.is_empty()
                {
                    dlg.error =
                        "IP, serial and access code are required.".into();
                } else {
                    if d.name.is_empty() {
                        d.name = d.ip.clone();
                    }
                    result = AddResult::Save(d.clone(), dlg.editing);
                    done = true;
                }
            }
        });
        done
    });
    (result, close)
}

// ----------------------------------------------------------------- temp
pub struct TempDlg {
    pub nozzle: bool,
    pub value: String,
}

pub fn show_temp(ctx: &egui::Context, dlg: &mut TempDlg,
                 client: &PrinterClient) -> bool {
    let (title, desc, presets, max): (_, _, &[i64], i64) = if dlg.nozzle {
        ("Nozzle temperature",
         "The nozzle melts the filament. Typical: PLA 220, PETG 250.",
         &[0, 180, 220, 250], 320)
    } else {
        ("Bed temperature",
         "The bed keeps the material adhered to the plate during the print.",
         &[0, 35, 45, 65, 80], 110)
    };
    modal(ctx, "temp", size::MODAL_M, |ui| {
        let mut done = false;
        header(ui, title);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut dlg.value)
                .font(font::metric())
                .desired_width(ui.available_width() - 40.0)
                .horizontal_align(egui::Align::Center));
            ui.label(RichText::new("°C").color(theme::TEXT_DIM));
        });
        ui.add_space(space::XS);
        ui.label(RichText::new(desc).color(theme::TEXT_DIM)
            .font(font::caption()));
        ui.add_space(space::S);
        ui.horizontal(|ui| {
            for t in presets {
                if ui.button(format!("{t}°C")).clicked() {
                    dlg.value = t.to_string();
                }
            }
        });
        ui.add_space(space::L);
        if accent_button(ui, "Set temperature") {
            match dlg.value.trim().replace(',', ".").parse::<f64>() {
                Ok(temp) if (0.0..=max as f64).contains(&temp) => {
                    if dlg.nozzle {
                        client.set_nozzle_temp(temp as i64);
                    } else {
                        client.set_bed_temp(temp as i64);
                    }
                    done = true;
                }
                _ => {}
            }
        }
        done
    })
}

// ---------------------------------------------------------------- speed
pub fn show_speed(ctx: &egui::Context, current: i64,
                  client: &PrinterClient) -> bool {
    modal(ctx, "speed", size::MODAL_S, |ui| {
        let mut done = false;
        header(ui, "Print speed");
        for (level, name, pct) in [(4, "Ludicrous", "166%"),
                                   (3, "Sport", "124%"),
                                   (2, "Standard", "100%"),
                                   (1, "Silent", "50%")] {
            let text = format!("{name}  ({pct})");
            let text = if level == current {
                RichText::new(text).color(theme::ACCENT)
                    .font(font::button_strong())
            } else {
                RichText::new(text)
            };
            if ui.add(egui::Button::new(text)
                .min_size(egui::vec2(ui.available_width(),
                                     size::BUTTON_H_LARGE)))
                .clicked()
            {
                client.set_speed(level);
                done = true;
            }
        }
        ui.add_space(space::XS);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Applies while printing.")
                .color(theme::TEXT_DIM).font(font::caption()));
        });
        done
    })
}

// ----------------------------------------------------------------- fans
pub fn show_fans(ctx: &egui::Context,
                 state: &serde_json::Map<String, serde_json::Value>,
                 client: &PrinterClient) -> bool {
    modal(ctx, "fans", size::MODAL_M, |ui| {
        header(ui, "Fans");
        let pct = |key: &str| -> Option<i64> {
            crate::ui::panel::s_i64(state, key).map(|v| v * 100 / 15)
        };
        let rows = [("Part", 1, pct("cooling_fan_speed")),
                    ("Aux", 2, pct("big_fan1_speed")),
                    ("Chamber", 3, pct("big_fan2_speed"))];
        let mut shown = 0;
        for (label, index, value) in rows {
            let Some(value) = value else { continue };
            shown += 1;
            egui::Frame::new().fill(theme::CARD_HOVER)
                .corner_radius(radius::CONTROL)
                .inner_margin(pad::CARD)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{label} ✤"))
                            .font(font::body_strong()));
                        ui.with_layout(egui::Layout::right_to_left(
                            egui::Align::Center), |ui| {
                            let mut on = value > 0;
                            if widgets::toggle_switch(ui, &mut on) {
                                client.set_fan(
                                    index, if on { 100 } else { 0 });
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{value}%"))
                            .font(font::metric()));
                        ui.with_layout(egui::Layout::right_to_left(
                            egui::Align::Center), |ui| {
                            let step = egui::vec2(size::STEP_BUTTON_W,
                                                  size::BUTTON_H);
                            if ui.add(egui::Button::new("+")
                                .min_size(step))
                                .clicked()
                            {
                                client.set_fan(
                                    index, (value + 10).min(100));
                            }
                            if ui.add(egui::Button::new("−")
                                .min_size(step))
                                .clicked()
                            {
                                client.set_fan(index, (value - 10).max(0));
                            }
                        });
                    });
                });
            ui.add_space(space::S);
        }
        if shown == 0 {
            ui.label("No fan telemetry from this printer.");
        }
        ui.add_space(space::XS);
        wide_button(ui, "Close")
    })
}

// ----------------------------------------------------------------- move
pub fn show_move(ctx: &egui::Context, client: &PrinterClient) -> bool {
    modal(ctx, "move", size::MODAL_L, |ui| {
        header(ui, "Movement");
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Toolhead").color(theme::TEXT_DIM)
                    .font(font::label()));
                if let Some(action) = widgets::jog_wheel(ui) {
                    match action {
                        widgets::JogAction::Home => client.home(),
                        widgets::JogAction::Jog(axis, dist) => {
                            client.jog(axis, dist, 3000);
                        }
                    }
                }
            });
            ui.add_space(space::L);
            ui.vertical(|ui| {
                ui.label(RichText::new("Bed (Z)").color(theme::TEXT_DIM)
                    .font(font::label()));
                for (label, dist) in [("⤒ 10", -10.0), ("⤒ 1", -1.0),
                                      ("⤓ 1", 1.0), ("⤓ 10", 10.0)] {
                    if ui.add(egui::Button::new(label)
                        .min_size(egui::vec2(size::Z_BUTTON_W,
                                             size::BUTTON_H_LARGE)))
                        .clicked()
                    {
                        client.jog("Z", dist, 900);
                    }
                }
            });
        });
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            if ui.button("Extrude 10 mm").clicked() {
                client.extrude(10.0, 300);
            }
            if ui.button("Retract 10 mm").clicked() {
                client.extrude(-10.0, 300);
            }
        });
        ui.add_space(space::S);
        ui.label(RichText::new(
            "Extruder needs a hot nozzle. Jog only while idle — the \
             wheel: inner ring 1 mm, outer 10 mm.")
            .color(theme::TEXT_DIM).font(font::caption()));
        ui.add_space(space::S);
        wide_button(ui, "Close")
    })
}

// ----------------------------------------------------------------- info
pub struct InfoDlg {
    pub check_slot: Arc<Mutex<Option<String>>>,
    pub checking: bool,
    pub status: String,
    pub status_color: Color32,
    pub toast_until: Option<Instant>,
    pub toast_text: String,
}

impl InfoDlg {
    pub fn new(fw_current: &str, fw_latest: &str) -> Self {
        let update = is_newer(fw_latest, fw_current);
        Self {
            check_slot: Arc::new(Mutex::new(None)),
            checking: false,
            status: String::new(),
            status_color: theme::TEXT_DIM,
            toast_until: if update {
                Some(Instant::now() + TOAST)
            } else {
                None
            },
            toast_text: if update {
                format!("New firmware {fw_latest} available")
            } else {
                String::new()
            },
        }
    }
}

pub struct InfoView<'a> {
    pub cfg: &'a PrinterCfg,
    pub device_info: &'a serde_json::Value,
    pub model: String,
    pub fw_current: String,
    pub fw_latest: String,
}

/// Returns (close, Some(new_latest_version)) when the manual check lands.
pub fn show_info(ctx: &egui::Context, dlg: &mut InfoDlg, view: &InfoView)
                 -> (bool, Option<String>) {
    let mut latest_update = None;

    // manual check finished?
    if dlg.checking
        && let Some(version) = dlg.check_slot.lock().unwrap().take()
    {
        dlg.checking = false;
        if version.is_empty() {
            dlg.status = "Couldn't reach the update server".into();
            dlg.status_color = theme::DANGER;
        } else if is_newer(&version, &view.fw_current) {
            dlg.status = "Update found".into();
            dlg.status_color = theme::WARN;
            dlg.toast_text = format!("New firmware {version} available");
            dlg.toast_until = Some(Instant::now() + TOAST);
            latest_update = Some(version);
        } else {
            dlg.status = "✓ Up to date".into();
            dlg.status_color = theme::ACCENT;
            latest_update = Some(version);
        }
    }

    let close = modal(ctx, "info", size::MODAL_M, |ui| {
        header(ui, &view.cfg.name);
        let fw_current = if view.fw_current.is_empty() {
            "—".to_string()
        } else {
            view.fw_current.clone()
        };
        egui::Grid::new("info-grid").num_columns(2)
            .spacing([space::XXL, space::S]).show(ui, |ui| {
                let dim = |ui: &mut egui::Ui, t: &str| {
                    ui.label(RichText::new(t).color(theme::TEXT_DIM));
                };
                dim(ui, "Model");
                ui.label(&view.model);
                ui.end_row();
                dim(ui, "Serial");
                ui.label(&view.cfg.serial);
                ui.end_row();
                dim(ui, "IP address");
                ui.label(&view.cfg.ip);
                ui.end_row();
                dim(ui, "Firmware");
                ui.horizontal(|ui| {
                    ui.label(&fw_current);
                    if is_newer(&view.fw_latest, &view.fw_current) {
                        ui.label(RichText::new(format!(
                            "●  {} available", view.fw_latest))
                            .color(theme::WARN).font(font::body_strong()))
                            .on_hover_text(
                                "Newer firmware released by Bambu Lab");
                    }
                });
                ui.end_row();
                dim(ui, "");
                ui.horizontal(|ui| {
                    let label = if dlg.checking {
                        "Checking…"
                    } else {
                        "Check for updates"
                    };
                    if ui.add_enabled(!dlg.checking,
                                      egui::Button::new(label)).clicked()
                    {
                        dlg.checking = true;
                        dlg.status.clear();
                        crate::firmware::spawn_check(
                            view.model.clone(), true,
                            dlg.check_slot.clone(), ctx.clone());
                    }
                    if !dlg.status.is_empty() {
                        ui.label(RichText::new(&dlg.status)
                            .color(dlg.status_color)
                            .font(font::body_strong()));
                    }
                });
                ui.end_row();
            });

        // module list
        if let Some(modules) = view.device_info.get("module")
            .and_then(|v| v.as_array())
        {
            let others: Vec<_> = modules.iter()
                .filter(|m| {
                    m.get("name").and_then(|v| v.as_str()) != Some("ota")
                        && m.get("sw_ver").and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                })
                .collect();
            if !others.is_empty() {
                ui.add_space(space::M);
                ui.label(RichText::new("MODULES").color(theme::TEXT_DIM)
                    .font(font::label()));
                egui::ScrollArea::vertical().max_height(size::MODULES_MAX_H)
                    .show(ui, |ui| {
                        egui::Grid::new("modules-grid").num_columns(2)
                            .spacing([space::XXL, space::XS]).show(ui, |ui| {
                                for m in others {
                                    let name = m.get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?");
                                    let ver = m.get("sw_ver")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let hw = m.get("hw_ver")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    ui.label(RichText::new(name)
                                        .color(theme::TEXT_DIM));
                                    ui.label(if hw.is_empty() {
                                        ver.to_string()
                                    } else {
                                        format!("{ver}  (hw {hw})")
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            }
        }

        ui.add_space(space::M);
        ui.label(RichText::new(
            "Firmware updates are not possible over LAN — use the \
             printer screen (cloud) or SD card package.")
            .color(theme::TEXT_DIM).font(font::caption()));
        ui.add_space(space::M);

        // toast bubble
        if let Some(until) = dlg.toast_until {
            if Instant::now() < until {
                let rect = ui.min_rect();
                let pos = egui::pos2(rect.center().x, rect.top() + space::XS);
                egui::Area::new(egui::Id::new("fw-toast"))
                    .fixed_pos(pos)
                    .pivot(egui::Align2::CENTER_TOP)
                    .show(ctx, |ui| {
                        egui::Frame::new()
                            .fill(theme::WARN_BG)
                            .stroke(Stroke::new(stroke::HAIRLINE, theme::WARN))
                            .corner_radius(radius::CONTROL)
                            .inner_margin(pad::CHIP)
                            .show(ui, |ui| {
                                ui.label(RichText::new(&dlg.toast_text)
                                    .color(theme::WARN)
                                    .font(font::body_strong()));
                            });
                    });
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                dlg.toast_until = None;
            }
        }

        wide_button(ui, "Close")
    });
    (close, latest_update)
}

// ----------------------------------------------------------------- skip
pub struct SkipDlg {
    pub selected: HashSet<i64>,
    pub confirm: bool,
}

pub fn show_skip(ctx: &egui::Context, dlg: &mut SkipDlg, bundle: &JobBundle,
                 live_skipped: &HashSet<i64>,
                 client: &PrinterClient) -> bool {
    let locked: HashSet<i64> =
        bundle.skipped.union(live_skipped).copied().collect();
    modal(ctx, "skip", size::MODAL_L, |ui| {
        let mut done = false;
        header(ui, "Skip objects");
        if !bundle.bboxes.is_empty() {
            ui.vertical_centered(|ui| {
                if let Some(id) = widgets::plate_map(
                    ui, &bundle.objects, &bundle.bboxes, &locked,
                    &dlg.selected)
                    && !dlg.selected.remove(&id)
                {
                    dlg.selected.insert(id);
                }
            });
            ui.add_space(space::S);
        }
        ui.label(format!(
            "Objects on plate ({}) — click the map or the list",
            bundle.objects.len()));
        egui::ScrollArea::vertical().max_height(size::SKIP_LIST_MAX_H)
            .show(ui, |ui| {
            for (index, (id, label)) in bundle.objects.iter().enumerate() {
                if locked.contains(id) {
                    ui.add_enabled(false, egui::Checkbox::new(
                        &mut true.clone(),
                        format!("{}.  {label}  (already skipped)",
                                index + 1)));
                } else {
                    let mut on = dlg.selected.contains(id);
                    if ui.checkbox(&mut on,
                                   format!("{}.  {label}", index + 1))
                        .changed()
                    {
                        if on {
                            dlg.selected.insert(*id);
                        } else {
                            dlg.selected.remove(id);
                        }
                    }
                }
            }
        });
        ui.label(RichText::new(format!("{} selected",
                                       dlg.selected.len()))
            .color(theme::TEXT_DIM));
        ui.add_space(space::S);

        if dlg.confirm {
            ui.label(RichText::new(format!(
                "Skip {} object(s)? Skipped objects cannot be resumed \
                 for this print.", dlg.selected.len()))
                .color(theme::WARN).font(font::body_strong()));
            ui.horizontal(|ui| {
                if ui.button("No").clicked() {
                    dlg.confirm = false;
                }
                if accent_button(ui, "Yes, skip") {
                    let ids: Vec<i64> =
                        dlg.selected.iter().copied().collect();
                    client.skip_objects(&ids);
                    done = true;
                }
            });
        } else {
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    done = true;
                }
                if ui.add_enabled(!dlg.selected.is_empty(),
                                  egui::Button::new("Skip selected"))
                    .clicked()
                {
                    dlg.confirm = true;
                }
            });
        }
        done
    })
}

// ------------------------------------------------------------------ hms
/// Printer-error popup: code + description per active HMS entry,
/// same style as Device information.
pub fn show_hms(ctx: &egui::Context, printer_name: &str,
                errors: &[(String, String)]) -> bool {
    modal(ctx, "hms", size::MODAL_L, |ui| {
        header(ui, &format!("{printer_name} — printer errors"));
        if errors.is_empty() {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No active errors ✓")
                    .color(theme::ACCENT).font(font::body_strong()));
            });
        }
        egui::ScrollArea::vertical().max_height(size::HMS_LIST_MAX_H)
            .show(ui, |ui| {
            for (code, intro) in errors {
                egui::Frame::new()
                    .fill(theme::DANGER_BG)
                    .corner_radius(radius::CONTROL)
                    .inner_margin(pad::CARD)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(code)
                            .color(theme::DANGER)
                            .font(font::body_strong()));
                        if intro.is_empty() {
                            ui.label(RichText::new(
                                "No description available for this code.")
                                .color(theme::TEXT_DIM)
                                .font(font::caption()));
                        } else {
                            ui.add(egui::Label::new(
                                RichText::new(intro).font(font::body()))
                                .wrap());
                        }
                    });
                ui.add_space(space::S);
            }
        });
        ui.add_space(space::S);
        wide_button(ui, "Close")
    })
}

// ------------------------------------------------------------ maintenance
/// Screen-menu replacement for printers with a broken display:
/// calibration, filament load/unload and nozzle type — all LAN MQTT.
pub struct MaintenanceDlg {
    pub cal_noise: bool,
    pub cal_bed: bool,
    pub cal_vibration: bool,
    pub cal_confirm: bool,
    pub fil_slot: Option<u8>,
    pub fil_temp: i64,
    pub nozzle_type: String,
    pub nozzle_diameter: f64,
    pub status: String,
}

impl MaintenanceDlg {
    pub fn new(current_type: &str, current_diameter: f64) -> Self {
        Self {
            cal_noise: false,
            cal_bed: true,
            cal_vibration: true,
            cal_confirm: false,
            fil_slot: None,
            fil_temp: 220,
            nozzle_type: if current_type.is_empty() {
                "stainless_steel".into()
            } else {
                current_type.into()
            },
            nozzle_diameter: if current_diameter > 0.0 {
                current_diameter
            } else {
                0.4
            },
            status: String::new(),
        }
    }
}

pub fn show_maintenance(ctx: &egui::Context, dlg: &mut MaintenanceDlg,
                        has_ams: bool,
                        client: &PrinterClient) -> bool {
    modal(ctx, "maintenance", size::MODAL_L, |ui| {
        let mut done = false;
        header(ui, "Maintenance (screen menu)");

        // ---- calibration
        ui.label(RichText::new("CALIBRATION").color(theme::TEXT_DIM)
            .font(font::label()));
        ui.horizontal(|ui| {
            ui.checkbox(&mut dlg.cal_bed, "Bed leveling");
            ui.checkbox(&mut dlg.cal_vibration, "Vibration");
            ui.checkbox(&mut dlg.cal_noise, "Motor noise");
        });
        if dlg.cal_confirm {
            ui.label(RichText::new(
                "The printer will move and heat. Clear the bed first!")
                .color(theme::WARN).font(font::body_strong()));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    dlg.cal_confirm = false;
                }
                if accent_button(ui, "Yes, start calibration") {
                    let option = (dlg.cal_noise as i64)
                        | ((dlg.cal_bed as i64) << 1)
                        | ((dlg.cal_vibration as i64) << 2);
                    client.calibrate(option);
                    dlg.status = "Calibration started".into();
                    dlg.cal_confirm = false;
                }
            });
        } else if ui.add_enabled(
            dlg.cal_bed || dlg.cal_vibration || dlg.cal_noise,
            egui::Button::new("Start calibration")).clicked()
        {
            dlg.cal_confirm = true;
        }
        ui.separator();

        // ---- filament
        ui.label(RichText::new("FILAMENT").color(theme::TEXT_DIM)
            .font(font::label()));
        ui.horizontal(|ui| {
            ui.label("Source:");
            ui.selectable_value(&mut dlg.fil_slot, None, "External");
            if has_ams {
                for s in 1..=4u8 {
                    ui.selectable_value(&mut dlg.fil_slot, Some(s),
                                        format!("Slot {s}"));
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Temp:");
            ui.add(egui::DragValue::new(&mut dlg.fil_temp)
                .range(180..=300).suffix("°C"));
            if ui.button("Load").clicked() {
                client.load_filament(dlg.fil_slot, dlg.fil_temp);
                dlg.status = "Load started (nozzle heating)".into();
            }
            if ui.button("Unload").clicked() {
                client.unload_filament(dlg.fil_temp);
                dlg.status = "Unload started (nozzle heating)".into();
            }
        });
        ui.separator();

        // ---- nozzle
        ui.label(RichText::new("NOZZLE").color(theme::TEXT_DIM)
            .font(font::label()));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("nozzle-type")
                .selected_text(dlg.nozzle_type.replace('_', " "))
                .show_ui(ui, |ui| {
                    for t in ["stainless_steel", "hardened_steel"] {
                        ui.selectable_value(&mut dlg.nozzle_type,
                                            t.to_string(),
                                            t.replace('_', " "));
                    }
                });
            egui::ComboBox::from_id_salt("nozzle-diam")
                .selected_text(format!("{} mm", dlg.nozzle_diameter))
                .show_ui(ui, |ui| {
                    for d in [0.2, 0.4, 0.6, 0.8] {
                        ui.selectable_value(&mut dlg.nozzle_diameter, d,
                                            format!("{d} mm"));
                    }
                });
            if ui.button("Apply").clicked() {
                client.set_nozzle(&dlg.nozzle_type, dlg.nozzle_diameter);
                dlg.status = format!(
                    "Nozzle set: {} {}mm",
                    dlg.nozzle_type.replace('_', " "),
                    dlg.nozzle_diameter);
            }
        });

        if !dlg.status.is_empty() {
            ui.add_space(space::XS);
            ui.label(RichText::new(&dlg.status)
                .color(theme::ACCENT).font(font::body_strong()));
        }
        ui.add_space(space::M);
        if wide_button(ui, "Close") {
            done = true;
        }
        done
    })
}

// ------------------------------------------------------------- confirms
pub fn show_confirm(ctx: &egui::Context, id: &str, text: &str,
                    yes_label: &str) -> (bool, bool) {
    let mut confirmed = false;
    let close = modal(ctx, id, size::MODAL_S, |ui| {
        let mut done = false;
        ui.label(RichText::new(text).font(font::body_strong()));
        ui.add_space(space::L);
        ui.horizontal(|ui| {
            if ui.button("No").clicked() {
                done = true;
            }
            let danger = egui::Button::new(
                RichText::new(yes_label).color(theme::DANGER))
                .stroke(Stroke::new(stroke::HAIRLINE, theme::DANGER));
            if ui.add(danger).clicked() {
                confirmed = true;
                done = true;
            }
        });
        done
    });
    (close, confirmed)
}
