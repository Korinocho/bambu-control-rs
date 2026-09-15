//! Custom-painted widgets: toggle switch, circular jog wheel and the
//! clickable plate map — ports of the PySide6 painted widgets.

use std::collections::{HashMap, HashSet};

use egui::{
    Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};

use crate::theme;

/// iOS-style pill switch, green when on. Returns true when toggled.
pub fn toggle_switch(ui: &mut Ui, on: &mut bool) -> bool {
    let (rect, mut response) =
        ui.allocate_exact_size(vec2(46.0, 26.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let painter = ui.painter();
    let track = if *on {
        theme::ACCENT
    } else {
        Color32::from_rgb(0x3a, 0x40, 0x3d)
    };
    painter.rect_filled(rect, CornerRadius::same(13), track);
    let x = if *on {
        rect.right() - 13.0
    } else {
        rect.left() + 13.0
    };
    painter.circle_filled(pos2(x, rect.center().y), 11.0, Color32::WHITE);
    response.changed()
}

// ------------------------------------------------------------------ jog
pub enum JogAction {
    Home,
    Jog(&'static str, f64),
}

const R_OUTER: f32 = 110.0;
const R_INNER: f32 = 72.0;
const R_HOME: f32 = 34.0;

fn jog_zone(center: Pos2, pos: Pos2) -> Option<JogAction> {
    let d = pos - center;
    let dist = d.length();
    if dist <= R_HOME {
        return Some(JogAction::Home);
    }
    if dist > R_OUTER {
        return None;
    }
    let angle = (-d.y).atan2(d.x).to_degrees().rem_euclid(360.0);
    let (axis, sign) = if (45.0..135.0).contains(&angle) {
        ("Y", 1.0)
    } else if (135.0..225.0).contains(&angle) {
        ("X", -1.0)
    } else if (225.0..315.0).contains(&angle) {
        ("Y", -1.0)
    } else {
        ("X", 1.0)
    };
    let ring = if dist <= R_INNER { 1.0 } else { 10.0 };
    Some(JogAction::Jog(axis, sign * ring))
}

/// Circular XY pad: outer ring = 10 mm, inner = 1 mm, home center.
pub fn jog_wheel(ui: &mut Ui) -> Option<JogAction> {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(240.0), Sense::click());
    let center = rect.center();
    let hover = response.hover_pos().and_then(|p| jog_zone(center, p));
    let painter = ui.painter();

    painter.circle_filled(center, R_OUTER,
                          Color32::from_rgb(0x1b, 0x1f, 0x1d));
    painter.circle_filled(center, R_INNER,
                          Color32::from_rgb(0x23, 0x28, 0x26));

    if let Some(JogAction::Jog(axis, dist)) = &hover {
        let start_deg: f32 = match (*axis, *dist > 0.0) {
            ("X", true) => -45.0,
            ("Y", true) => 45.0,
            ("X", false) => 135.0,
            _ => 225.0,
        };
        let (r_in, r_out) = if dist.abs() < 5.0 {
            (R_HOME, R_INNER)
        } else {
            (R_INNER, R_OUTER)
        };
        // filled wedge via triangle fan
        let steps = 24;
        let color = theme::ACCENT_DARK.gamma_multiply(0.45);
        let mut prev: Option<(Pos2, Pos2)> = None;
        for i in 0..=steps {
            let ang = (start_deg + 90.0 * i as f32 / steps as f32)
                .to_radians();
            let dir = vec2(ang.cos(), -ang.sin());
            let pair = (center + dir * r_in, center + dir * r_out);
            if let Some((pi, po)) = prev {
                painter.add(egui::Shape::convex_polygon(
                    vec![pi, po, pair.1, pair.0], color, Stroke::NONE));
            }
            prev = Some(pair);
        }
    }

    for angle in [45.0_f32, 135.0, 225.0, 315.0] {
        let rad = angle.to_radians();
        let dir = vec2(rad.cos(), -rad.sin());
        painter.line_segment(
            [center + dir * R_HOME, center + dir * R_OUTER],
            Stroke::new(3.0, theme::BG));
    }

    painter.circle_filled(center, R_HOME,
                          Color32::from_rgb(0x2c, 0x32, 0x2f));

    let dim = theme::TEXT_DIM;
    let bold = FontId::proportional(14.0);
    let small = FontId::proportional(11.0);
    painter.text(center + vec2(0.0, -102.0), egui::Align2::CENTER_CENTER,
                 "Y", bold.clone(), dim);
    painter.text(center + vec2(0.0, 102.0), egui::Align2::CENTER_CENTER,
                 "-Y", bold.clone(), dim);
    painter.text(center + vec2(102.0, 0.0), egui::Align2::CENTER_CENTER,
                 "X", bold.clone(), dim);
    painter.text(center + vec2(-102.0, 0.0), egui::Align2::CENTER_CENTER,
                 "-X", bold, dim);
    painter.text(center + vec2(52.0, -70.0), egui::Align2::CENTER_CENTER,
                 "+10", small.clone(), dim);
    painter.text(center + vec2(36.0, -39.0), egui::Align2::CENTER_CENTER,
                 "+1", small.clone(), dim);
    painter.text(center + vec2(-36.0, 39.0), egui::Align2::CENTER_CENTER,
                 "-1", small.clone(), dim);
    painter.text(center + vec2(-52.0, 70.0), egui::Align2::CENTER_CENTER,
                 "-10", small, dim);
    painter.text(center, egui::Align2::CENTER_CENTER, "⌂",
                 FontId::proportional(20.0), theme::ACCENT);

    if hover.is_some() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if response.clicked() {
        response.interact_pointer_pos()
            .and_then(|p| jog_zone(center, p))
    } else {
        None
    }
}

// ----------------------------------------------------------- plate map
/// Top-down plate view from per-object bounding boxes. Click toggles
/// selection. Returns the clicked identify_id, if any.
pub fn plate_map(ui: &mut Ui, objects: &[(i64, String)],
                 bboxes: &HashMap<i64, [f32; 4]>, locked: &HashSet<i64>,
                 selected: &HashSet<i64>) -> Option<i64> {
    let size = 360.0;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10),
                        Color32::from_rgb(0x15, 0x18, 0x16));

    let objs: Vec<&(i64, String)> =
        objects.iter().filter(|(i, _)| bboxes.contains_key(i)).collect();
    let bed = bboxes.values()
        .flat_map(|b| [b[2], b[3]])
        .fold(256.0_f32, f32::max);
    let scale = size / bed;
    let obj_rect = |bbox: &[f32; 4]| -> Rect {
        Rect::from_min_size(
            pos2(rect.left() + bbox[0] * scale,
                 rect.bottom() - bbox[3] * scale),
            vec2((bbox[2] - bbox[0]) * scale, (bbox[3] - bbox[1]) * scale))
    };

    // faint grid every 32 mm
    let step = scale * 32.0;
    let grid = Color32::from_rgba_unmultiplied(0x20, 0x24, 0x22, 0x60);
    let mut x = rect.left() + step;
    while x < rect.right() {
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())],
                             Stroke::new(1.0, grid));
        let y = rect.top() + (x - rect.left());
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)],
                             Stroke::new(1.0, grid));
        x += step;
    }

    let hover_pos = response.hover_pos();
    let hover_id = hover_pos.and_then(|p| {
        objs.iter().rev()
            .find(|(i, _)| obj_rect(&bboxes[i]).contains(p))
            .map(|(i, _)| *i)
    });

    let mut clicked = None;
    for (index, (id, label)) in objs.iter().enumerate() {
        let r = obj_rect(&bboxes[id]);
        let (fill, stroke) = if locked.contains(id) {
            (theme::DANGER.gamma_multiply(0.35),
             Stroke::new(1.0, theme::DANGER))
        } else if selected.contains(id) {
            (theme::ACCENT,
             Stroke::new(2.0, Color32::from_rgb(0x2c, 0xc9, 0x5a)))
        } else if hover_id == Some(*id) {
            (Color32::from_rgb(0x3a, 0x40, 0x3d),
             Stroke::new(2.0, Color32::from_rgb(0xe8, 0xeb, 0xe9)))
        } else {
            (Color32::from_rgb(0x3a, 0x40, 0x3d),
             Stroke::new(1.0, theme::BORDER))
        };
        painter.rect(r, CornerRadius::same(3), fill, stroke,
                     StrokeKind::Inside);
        if r.width() > 16.0 && r.height() > 12.0 {
            let text_color = if selected.contains(id) {
                Color32::from_rgb(0x06, 0x13, 0x0a)
            } else {
                theme::TEXT_DIM
            };
            painter.text(r.center(), egui::Align2::CENTER_CENTER,
                         format!("{}", index + 1),
                         FontId::proportional(11.0), text_color);
        }
        if hover_id == Some(*id) {
            response.clone().on_hover_text(label.clone());
        }
    }

    if let Some(id) = hover_id {
        if !locked.contains(&id) {
            ui.output_mut(|o| {
                o.cursor_icon = egui::CursorIcon::PointingHand;
            });
            if response.clicked() {
                clicked = Some(id);
            }
        }
    }
    clicked
}
