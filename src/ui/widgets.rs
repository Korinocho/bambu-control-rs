//! Custom-painted widgets: toggle switch, circular jog wheel and the
//! clickable plate map — ports of the PySide6 painted widgets.

use std::collections::{HashMap, HashSet};

use egui::{Align, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke,
           StrokeKind, Ui, Vec2, pos2, vec2};

use crate::theme::{self, font, radius, size, stroke};

/// A slot as wide as `widest` in `font`, holding `text` truncated to it: a
/// number that changes never moves what sits next to it (E10). `Max` puts
/// the text at the slot's right edge.
pub fn slot(ui: &mut Ui, widest: &str, text: RichText, font: &FontId,
            align: Align) -> egui::Response {
    let width = theme::text_width(ui, widest, font);
    let height = theme::row_height(ui, font);
    let layout = match align {
        Align::Max => Layout::right_to_left(Align::Center),
        _ => Layout::left_to_right(Align::Center),
    };
    ui.allocate_ui_with_layout(vec2(width, height), layout, |ui| {
        ui.set_width(width);
        // a slot with nothing in it is not content: an empty selectable
        // label would be a nameless node a screen reader walks into (A9)
        let empty = text.text().is_empty();
        ui.add(egui::Label::new(text.font(font.clone()))
            .selectable(!empty)
            .truncate());
    }).response
}

/// How a clickable surface is drawn (C2): its shape, its rest fill, and
/// whether it is the selected one.
#[derive(Clone, Copy)]
pub struct Surface {
    pub radius: egui::CornerRadius,
    pub padding: egui::Margin,
    /// the fill at rest; hover and pressed come from the palette
    pub fill: egui::Color32,
    /// the hairline outline, where the surface has one
    pub stroke: Option<egui::Color32>,
    /// the fill under the pointer, and the pressed fill with it. A banner
    /// has none: its tone must not turn grey while the pointer is on it,
    /// so it shows the hover with the cursor and the focus ring alone.
    pub hover: Option<egui::Color32>,
    /// whether this kind of surface can be selected at all: a row, a tile
    /// and a chip can, a card and a banner cannot. It decides what the
    /// surface tells a screen reader it is (A9).
    pub selectable: bool,
    /// a selected surface keeps a ring, and (a row, not a tile) a fill
    pub selected: bool,
    pub selected_fill: bool,
    pub ring: egui::Color32,
}

impl Surface {
    /// A card: the one card shape, filled `CARD`, with an accent ring when
    /// it is the selected one.
    pub fn card() -> Self {
        Self {
            radius: radius::CARD,
            padding: theme::pad::CARD,
            fill: theme::CARD,
            stroke: Some(theme::BORDER),
            hover: Some(theme::CARD_HOVER),
            selectable: false,
            selected: false,
            selected_fill: false,
            ring: theme::ACCENT,
        }
    }

    /// A banner (C3): the control shape, the banner padding, the tone's
    /// background and no outline.
    pub fn banner(tone: Tone) -> Self {
        Self {
            radius: radius::CONTROL,
            padding: theme::pad::BANNER,
            fill: banner_colors(tone).1,
            stroke: None,
            hover: None,
            selectable: false,
            selected: false,
            selected_fill: false,
            ring: theme::ACCENT,
        }
    }

    pub fn radius(mut self, radius: egui::CornerRadius) -> Self {
        self.radius = radius;
        self
    }

    pub fn padding(mut self, padding: egui::Margin) -> Self {
        self.padding = padding;
        self
    }

    /// Selected: a ring, and for a row also the hover fill (C11). A
    /// surface that can be selected says so to a screen reader (A9).
    pub fn selected(mut self, selected: bool, fill: bool) -> Self {
        self.selectable = true;
        self.selected = selected;
        self.selected_fill = fill;
        self
    }

    pub fn ring(mut self, ring: egui::Color32) -> Self {
        self.ring = ring;
        self
    }
}

/// One clickable surface — a card, a banner, a chip, a tile, a row (C2).
///
/// The surface itself is the widget: `UiBuilder::sense` makes the scope
/// clickable, rather than interacting with the frame's response afterwards
/// (E24). Hover, pressed and keyboard focus change only colours and painted
/// rings, never a size (E10), and the cursor is set through the response.
pub fn clickable<R>(ui: &mut Ui, id_salt: impl std::hash::Hash + std::fmt::Debug,
                    name: &str, surface: Surface,
                    add: impl FnOnce(&mut Ui) -> R)
                    -> egui::InnerResponse<R> {
    clickable_sense(ui, id_salt, name, surface, Sense::click(), add)
}

/// The same surface with a sense of its own: a chip is dragged as well as
/// clicked (C4), and nothing else needs more than a click.
/// `name` is what the surface is called: a screen reader reads it, and a
/// surface with a click and no name is a defect the A9 test fails on.
pub fn clickable_sense<R>(ui: &mut Ui,
                          id_salt: impl std::hash::Hash + std::fmt::Debug,
                          name: &str, surface: Surface, sense: Sense,
                          add: impl FnOnce(&mut Ui) -> R)
                          -> egui::InnerResponse<R> {
    let scope = ui.scope_builder(
        egui::UiBuilder::new().id_salt(id_salt).sense(sense),
        |ui| {
            // a selectable label senses click and drag, and being smaller
            // and painted later it takes the hit from the surface it sits
            // on; inside a row, a card or a tile the text is not a
            // selection, the surface is the target (C2, A11)
            ui.style_mut().interaction.selectable_labels = false;
            let response = ui.response();
            let fill = match surface.hover {
                Some(_) if response.is_pointer_button_down_on() =>
                    theme::PRESSED_FILL,
                Some(hover) if response.hovered()
                    || (surface.selected && surface.selected_fill) => hover,
                _ => surface.fill,
            };
            let outline = match surface.stroke {
                Some(color) => Stroke::new(stroke::HAIRLINE, color),
                None => Stroke::NONE,
            };
            let inner = egui::Frame::new()
                .fill(fill)
                .stroke(outline)
                .corner_radius(surface.radius)
                .inner_margin(surface.padding)
                .show(ui, add)
                .inner;
            let rect = ui.min_rect();
            if surface.selected {
                ui.painter().rect_stroke(
                    rect, surface.radius,
                    Stroke::new(stroke::SELECTED, surface.ring),
                    StrokeKind::Inside);
            }
            // the focus ring sits outside, a hairline clear of the surface
            if response.has_focus() {
                ui.painter().rect_stroke(
                    rect.expand(stroke::HAIRLINE + stroke::FOCUS),
                    surface.radius, Stroke::new(stroke::FOCUS, theme::TEXT),
                    StrokeKind::Outside);
            }
            inner
        });
    let enabled = scope.response.enabled();
    scope.response.widget_info(|| match surface.selectable {
        true => egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel, enabled, surface.selected,
            name),
        false => egui::WidgetInfo::labeled(egui::WidgetType::Button,
                                           enabled, name),
    });
    egui::InnerResponse::new(scope.inner, scope.response)
}

/// A button that says why it cannot act: disabled, with the reason on
/// hover, rather than live-looking and swallowing the click (D11, D23, A11).
pub fn button(ui: &mut Ui, button: egui::Button<'_>, reason: Option<&str>)
              -> bool {
    let Some(reason) = reason else {
        return ui.add(button).clicked();
    };
    ui.add_enabled(false, button).on_disabled_hover_text(reason);
    false
}

/// The tone of a banner (C3).
#[derive(Clone, Copy)]
pub enum Tone {
    Warn,
    Danger,
    Neutral,
}

/// A tone's text and background colour (C3).
pub fn banner_colors(tone: Tone) -> (egui::Color32, egui::Color32) {
    match tone {
        Tone::Warn => (theme::WARN, theme::WARN_BG),
        Tone::Danger => (theme::DANGER, theme::DANGER_BG),
        Tone::Neutral => (theme::TEXT_DIM, theme::CARD),
    }
}

/// The one banner (C3): full width, text wrapping top-down, an optional
/// title above it. The caller gives it an id scope of its own (E3).
pub fn banner(ui: &mut Ui, tone: Tone, title: Option<&str>, text: &str)
              -> egui::Response {
    let (color, fill) = banner_colors(tone);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(radius::CONTROL)
        .inner_margin(theme::pad::BANNER)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(title) = title {
                ui.add(egui::Label::new(RichText::new(title).color(color)
                    .font(font::body_strong())).wrap());
            }
            ui.add(egui::Label::new(RichText::new(text).color(color)
                .font(font::caption())).wrap());
        })
        .response
}

/// `content` scaled to fit inside `bounds`, keeping its aspect ratio.
pub fn fit(content: Vec2, bounds: Vec2) -> Vec2 {
    content * (bounds.x / content.x).min(bounds.y / content.y)
}

/// iOS-style pill switch, green when on. Returns true when toggled.
/// `pending`: the command was sent and the printer has not agreed yet, so
/// the knob sits where it was asked to and the track is half lit (C18).
pub fn toggle_switch(ui: &mut Ui, label: &str, on: &mut bool,
                     pending: bool) -> bool {
    let (rect, mut response) =
        ui.allocate_exact_size(size::TOGGLE, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    // a painted switch is a checkbox to a screen reader, and it says
    // which way it is (A9, A10)
    let (enabled, shown) = (response.enabled(), *on);
    response.widget_info(|| egui::WidgetInfo::selected(
        egui::WidgetType::Checkbox, enabled, shown, label));
    let painter = ui.painter();
    let track = if *on {
        theme::ACCENT
    } else {
        theme::TRACK_OFF
    };
    let track = match pending {
        true => track.gamma_multiply(0.5),
        false => track,
    };
    painter.rect_filled(rect, radius::pill(rect.height()), track);
    // the knob's centre sits half the track's height in from its end
    let inset = rect.height() / 2.0;
    let x = if *on {
        rect.right() - inset
    } else {
        rect.left() + inset
    };
    painter.circle_filled(pos2(x, rect.center().y), size::TOGGLE_KNOB_R,
                          theme::KNOB);
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
/// Where the axis letters sit, from the centre.
const AXIS_LABEL_R: f32 = 102.0;
/// The "+10" label, and "-10" mirrored through the centre.
const TEN_LABEL: Vec2 = vec2(52.0, -70.0);
/// The "+1" label, and "-1" mirrored through the centre.
const ONE_LABEL: Vec2 = vec2(36.0, -39.0);

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
        ui.allocate_exact_size(Vec2::splat(size::JOG), Sense::click());
    let center = rect.center();
    let hover = response.hover_pos().and_then(|p| jog_zone(center, p));
    let painter = ui.painter();

    painter.circle_filled(center, R_OUTER, theme::CARD);
    painter.circle_filled(center, R_INNER, theme::CARD_HOVER);

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
            Stroke::new(stroke::HEAVY, theme::BG));
    }

    painter.circle_filled(center, R_HOME, theme::HOVER_FILL);

    let dim = theme::TEXT_DIM;
    let axis = font::body();
    let step = font::caption();
    let up = Vec2::Y * -AXIS_LABEL_R;
    let right = Vec2::X * AXIS_LABEL_R;
    painter.text(center + up, egui::Align2::CENTER_CENTER,
                 "Y", axis.clone(), dim);
    painter.text(center - up, egui::Align2::CENTER_CENTER,
                 "-Y", axis.clone(), dim);
    painter.text(center + right, egui::Align2::CENTER_CENTER,
                 "X", axis.clone(), dim);
    painter.text(center - right, egui::Align2::CENTER_CENTER,
                 "-X", axis, dim);
    painter.text(center + TEN_LABEL, egui::Align2::CENTER_CENTER,
                 "+10", step.clone(), dim);
    painter.text(center + ONE_LABEL, egui::Align2::CENTER_CENTER,
                 "+1", step.clone(), dim);
    painter.text(center - ONE_LABEL, egui::Align2::CENTER_CENTER,
                 "-1", step.clone(), dim);
    painter.text(center - TEN_LABEL, egui::Align2::CENTER_CENTER,
                 "-10", step, dim);
    painter.text(center, egui::Align2::CENTER_CENTER, "⌂",
                 font::icon_large(), theme::ACCENT);

    if hover.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // one painted wheel with zones inside it: a screen reader is told what
    // the wheel is for, not one node per zone (A9)
    let enabled = response.enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(
        egui::WidgetType::Button, enabled,
        "Move the toolhead: X, Y and home"));
    if response.clicked() {
        response.interact_pointer_pos()
            .and_then(|p| jog_zone(center, p))
    } else {
        None
    }
}

// ----------------------------------------------------------- plate map

/// The bed the boxes are measured on: 256 units, 1 mm on a 256 mm bed.
const BED_UNITS: f32 = 256.0;
/// The faint grid's step, in bed units.
const GRID_UNITS: f32 = 32.0;
/// A box smaller than this has no room for its number.
const NUMBER_MIN: Vec2 = vec2(16.0, 12.0);
/// Objects that have a box, with their position in the skip list (the map
/// numbers them like the list), largest box first: a small object inside a
/// bigger object's box is then drawn on top and gets the pointer.
fn boxed_objects<'a>(objects: &'a [(i64, String)],
                     bboxes: &HashMap<i64, [f32; 4]>)
                     -> Vec<(usize, &'a (i64, String))> {
    let area = |id: &i64| bboxes.get(id)
        .map_or(0.0, |b| (b[2] - b[0]) * (b[3] - b[1]));
    let mut boxed: Vec<_> = objects.iter().enumerate()
        .filter(|(_, (i, _))| bboxes.contains_key(i))
        .collect();
    boxed.sort_by(|(_, (a, _)), (_, (b, _))| area(b).total_cmp(&area(a)));
    boxed
}

/// Top-down plate view from per-object bounding boxes. Click toggles
/// selection. Returns the clicked identify_id, if any.
pub fn plate_map(ui: &mut Ui, objects: &[(i64, String)],
                 bboxes: &HashMap<i64, [f32; 4]>, locked: &HashSet<i64>,
                 selected: &HashSet<i64>) -> Option<i64> {
    let side = size::PLATE_MAP_MAX.min(ui.available_width());
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, radius::CONTROL, theme::PLATE_BG);

    let objs = boxed_objects(objects, bboxes);
    let bed = bboxes.values()
        .flat_map(|b| [b[2], b[3]])
        .fold(BED_UNITS, f32::max);
    let scale = side / bed;
    let obj_rect = |bbox: &[f32; 4]| -> Rect {
        Rect::from_min_size(
            pos2(rect.left() + bbox[0] * scale,
                 rect.bottom() - bbox[3] * scale),
            vec2((bbox[2] - bbox[0]) * scale, (bbox[3] - bbox[1]) * scale))
    };

    // faint grid every 32 mm
    let step = scale * GRID_UNITS;
    let grid = Stroke::new(stroke::HAIRLINE, theme::PLATE_GRID);
    let mut x = rect.left() + step;
    while x < rect.right() {
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())],
                             grid);
        let y = rect.top() + (x - rect.left());
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)],
                             grid);
        x += step;
    }

    let hover_pos = response.hover_pos();
    let hover_id = hover_pos.and_then(|p| {
        objs.iter().rev()
            .find(|(_, (i, _))| obj_rect(&bboxes[i]).contains(p))
            .map(|(_, (i, _))| *i)
    });

    let mut clicked = None;
    for &(index, (id, label)) in &objs {
        let r = obj_rect(&bboxes[id]);
        let (fill, outline) = if locked.contains(id) {
            (theme::DANGER.gamma_multiply(0.35),
             Stroke::new(stroke::HAIRLINE, theme::DANGER))
        } else if selected.contains(id) {
            (theme::ACCENT,
             Stroke::new(stroke::SELECTED, theme::ACCENT_BRIGHT))
        } else if hover_id == Some(*id) {
            (theme::PLATE_OBJECT, Stroke::new(stroke::SELECTED, theme::TEXT))
        } else {
            (theme::PLATE_OBJECT,
             Stroke::new(stroke::HAIRLINE, theme::BORDER))
        };
        painter.rect(r, radius::MARK, fill, outline, StrokeKind::Inside);
        if r.width() > NUMBER_MIN.x && r.height() > NUMBER_MIN.y {
            let text_color = if selected.contains(id) {
                theme::ON_ACCENT
            } else {
                theme::TEXT
            };
            painter.text(r.center(), egui::Align2::CENTER_CENTER,
                         format!("{}", index + 1), font::caption(),
                         text_color);
        }
        if hover_id == Some(*id) {
            response.clone().on_hover_text(label.clone());
        }
    }

    if let Some(id) = hover_id
        && !locked.contains(&id)
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if response.clicked() {
            clicked = Some(id);
        }
    }
    // the map is one widget with the objects painted in it (A9)
    let enabled = response.enabled();
    let count = objects.len();
    response.widget_info(|| egui::WidgetInfo::labeled(
        egui::WidgetType::Button, enabled,
        format!("Plate map: {count} object(s); pick the ones to skip")));
    clicked
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::boxed_objects;

    #[test]
    fn map_numbers_follow_the_skip_list() {
        // copies without a box must not shift the numbers of the rest
        let objects = vec![(91, "Cube".to_string()),
                           (93, "Cube #2".to_string()),
                           (95, "Lid".to_string())];
        let bboxes = HashMap::from([(95, [1.0, 1.0, 2.0, 2.0])]);
        let boxed = boxed_objects(&objects, &bboxes);
        assert_eq!(boxed.len(), 1);
        assert_eq!(boxed[0].0 + 1, 3);
        assert_eq!(boxed[0].1.0, 95);
    }

    #[test]
    fn small_objects_inside_bigger_boxes_are_drawn_last() {
        // a peg standing in a ring's hole, listed first
        let objects = vec![(1, "peg".to_string()), (2, "ring".to_string())];
        let bboxes = HashMap::from([(1, [115.0, 120.0, 135.0, 141.0]),
                                    (2, [75.0, 80.0, 175.0, 181.0])]);
        let order: Vec<(usize, i64)> = boxed_objects(&objects, &bboxes)
            .iter().map(|(index, (id, _))| (*index, *id)).collect();
        assert_eq!(order, vec![(1, 2), (0, 1)]);
    }
}
