//! Per-printer panel: camera + job card on the left, control cards +
//! AMS on the right. Port of `ui/panel.py`.

use egui::{
    Color32, CornerRadius, FontId, RichText, Sense, Stroke, Ui,
    vec2,
};
use serde_json::{Map, Value};

use crate::mqtt::speed_name;
use crate::theme;

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
}

fn card_frame(ui: &mut Ui, add: impl FnOnce(&mut Ui)) -> egui::Response {
    egui::Frame::new()
        .fill(theme::CARD)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| add(ui))
        .response
}

fn clickable_card(ui: &mut Ui, title: &str, value: &str) -> bool {
    let response = card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).color(theme::TEXT_DIM)
                .font(theme::bold(12.0)));
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("›").size(16.0)
                        .color(theme::TEXT_DIM).strong());
                });
        });
        ui.label(RichText::new(value).font(theme::bold(15.0)));
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
        ui.label(RichText::new(title).color(theme::TEXT_DIM)
            .font(theme::bold(12.0)));
        ui.horizontal(|ui| {
            let cur = current.map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "--".into());
            let tgt = target.map(|v| format!("/ {v:.0}°C"))
                .unwrap_or_else(|| "/ --°C".into());
            ui.label(RichText::new(cur).font(theme::bold(26.0)));
            ui.label(RichText::new(tgt).size(13.0)
                .color(theme::TEXT_DIM));
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
            && v.trim_matches('0') != ""
        {
            return true;
        }
    }
    false
}

fn ams_card(ui: &mut Ui, state: &Map<String, Value>, show_humidity: bool) {
    card_frame(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 3.0;
        let ams_root = state.get("ams").cloned().unwrap_or(Value::Null);
        let units = ams_root.get("ams").and_then(|v| v.as_array())
            .cloned().unwrap_or_default();
        let tray_now = ams_root.get("tray_now")
            .map(|v| v.as_str().map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string()))
            .unwrap_or_else(|| "255".into());

        ui.horizontal(|ui| {
            ui.label(RichText::new("FILAMENT").color(theme::TEXT_DIM)
                .font(theme::bold(12.0)));
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
        let mut slot_row = |ui: &mut Ui, slot: String, tray: &Value,
                            active: bool| {
            ui.horizontal(|ui| {
                let color_hex = tray.get("tray_color")
                    .and_then(|v| v.as_str()).unwrap_or("");
                let color = if color_hex.len() >= 6 {
                    let parse =
                        |i| u8::from_str_radix(&color_hex[i..i + 2], 16)
                            .unwrap_or(0x44);
                    Color32::from_rgb(parse(0), parse(2), parse(4))
                } else {
                    Color32::from_rgb(0x44, 0x44, 0x44)
                };
                let (rect, _) = ui.allocate_exact_size(
                    vec2(20.0, 20.0), Sense::hover());
                let ring = if active { theme::ACCENT } else { theme::BORDER };
                ui.painter().circle(rect.center(), 9.0, color,
                                    Stroke::new(1.5, ring));
                ui.painter().circle_filled(rect.center(), 3.0, theme::CARD);
                let ftype = tray.get("tray_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("empty");
                let mut text = RichText::new(format!("{slot}   {ftype}"))
                    .size(12.5);
                if active {
                    text = text.color(theme::ACCENT).font(theme::bold(12.5));
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
            let letter = (b'A' + uid as u8) as char;
            for tray in unit.get("tray").and_then(|v| v.as_array())
                .cloned().unwrap_or_default()
            {
                let tid = tray.get("id").and_then(|v| v.as_i64())
                    .or_else(|| tray.get("id").and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                let global = uid * 4 + tid;
                slot_row(ui, format!("{letter}{}", tid + 1), &tray,
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
        let left_w = (ui.available_width() * 0.58).floor();
        // ------------------------------------------------ left column
        ui.allocate_ui_with_layout(
            egui::vec2(left_w, 0.0),
            egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_width(left_w);

        // camera
        let cam_h = (ui.available_width() * 9.0 / 16.0).min(420.0);
        let (rect, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), cam_h), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(14),
                                 Color32::BLACK);
        if let Some(tex) = view.cam_texture {
            let size = tex.size_vec2();
            let scale =
                (rect.width() / size.x).min(rect.height() / size.y);
            let img_rect = egui::Rect::from_center_size(
                rect.center(), size * scale);
            egui::Image::new((tex.id(), size))
                .corner_radius(CornerRadius::same(10))
                .paint_at(ui, img_rect);
        } else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                              &view.cam_status,
                              FontId::proportional(13.0), theme::TEXT_DIM);
        }
        ui.add_space(10.0);

        // job card
        card_frame(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (thumb, _) = ui.allocate_exact_size(
                    vec2(88.0, 88.0), Sense::hover());
                ui.painter().rect_filled(
                    thumb, CornerRadius::same(10), theme::CARD_HOVER);
                if let Some(tex) = view.plate_texture {
                    let size = tex.size_vec2();
                    let scale = (thumb.width() / size.x)
                        .min(thumb.height() / size.y);
                    egui::Image::new((tex.id(), size))
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
                    ui.label(RichText::new(
                        if job.is_empty() { "—" } else { job })
                        .font(theme::bold(13.0)));
                    ui.horizontal(|ui| {
                        let display = if gcode_state.is_empty() {
                            "—"
                        } else {
                            &gcode_state
                        };
                        ui.label(RichText::new(display).font(theme::bold(14.0))
                            .color(theme::state_color(&gcode_state)));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let (ok, detail) = &view.connected;
                                ui.label(RichText::new(detail).color(
                                    if *ok { theme::ACCENT }
                                    else { theme::DANGER }));
                            });
                    });
                });
            });

            let pct = s_i64(state, "mc_percent").unwrap_or(0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{pct}%"))
                    .font(theme::bold(24.0)));
                let mins = s_i64(state, "mc_remaining_time").unwrap_or(0);
                if mins > 0 {
                    ui.label(RichText::new(format!(
                        "~{}h {:02}m left", mins / 60, mins % 60))
                        .color(theme::TEXT_DIM));
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let (Some(layer), Some(total)) = (
                            s_i64(state, "layer_num"),
                            s_i64(state, "total_layer_num"),
                        ) && total > 0
                        {
                            ui.label(RichText::new(format!(
                                "layer {layer}/{total}"))
                                .color(theme::TEXT_DIM));
                        }
                    });
            });
            let bar = egui::ProgressBar::new(pct as f32 / 100.0)
                .desired_height(8.0)
                .fill(theme::ACCENT);
            ui.add(bar);
            ui.add_space(6.0);

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
                    ui.label(RichText::new(format!("{p}%"))
                        .color(theme::TEXT_DIM).size(10.0));
                }
                let pause_label =
                    if paused { "▶ Resume" } else { "⏸ Pause" };
                if ui.add_enabled(can_act,
                                  egui::Button::new(pause_label)).clicked() {
                    actions.push(PanelAction::TogglePause);
                }
                let stop = egui::Button::new(
                    RichText::new("⏹ Stop").color(theme::DANGER))
                    .stroke(Stroke::new(1.0, theme::DANGER));
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
            ui.spacing_mut().item_spacing.y = 6.0;
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
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(8)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.label(RichText::new(format!(
                            "⚠ {} printer error(s)", hms.len()))
                            .color(theme::DANGER)
                            .font(theme::bold(14.0)));
                        for ecode in &ecodes {
                            // description only; the code stays in the
                            // popup (fallback when no description)
                            let line = crate::hms::lookup(ecode)
                                .unwrap_or_else(
                                    || crate::hms::dashed(ecode));
                            ui.add(egui::Label::new(
                                RichText::new(line)
                                    .color(theme::DANGER).size(12.0))
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
                ui.add_space(8.0);
            }

            // firmware banner
            if crate::firmware::is_newer(&view.fw_latest, &view.fw_current) {
                let response = egui::Frame::new()
                    .fill(theme::WARN_BG)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(8)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(format!(
                            "⬆ Firmware update available: {} → {}",
                            view.fw_current, view.fw_latest))
                            .color(theme::WARN).font(theme::bold(14.0)));
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
                ui.add_space(8.0);
            }

            ui.label(RichText::new("DEVICE CONTROL").color(theme::TEXT_DIM)
                .font(theme::bold(12.0)));
            ui.add_space(4.0);

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
                    ui.label(RichText::new("LIGHT").color(theme::TEXT_DIM)
                        .font(theme::bold(12.0)));
                    ui.horizontal(|ui| {
                        let mut on = view.light_shown_on;
                        ui.label(RichText::new(
                            if on { "On" } else { "Off" })
                            .font(theme::bold(15.0)));
                        ui.with_layout(egui::Layout::right_to_left(
                            egui::Align::Center), |ui| {
                            if super::widgets::toggle_switch(ui, &mut on) {
                                actions.push(PanelAction::SetLight(on));
                            }
                        });
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
            ams_card(ui, state, view.show_humidity);
        }
        });
    });
    actions
}
