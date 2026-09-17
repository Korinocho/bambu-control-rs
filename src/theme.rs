//! Dark theme with green accent — port of the PySide6 palette.
//!
//! Every colour, font, spacing, padding, radius, stroke and widget size the
//! UI uses is declared here (docs/gui-polish-guidelines.md, section 1). The
//! scans in tests/source_rules.rs keep literals out of `src/ui` and
//! `src/main.rs`.

use egui::Color32;

// ------------------------------------------------------------ colour roles

/// Canvas: top bar, CentralPanel, modal fill, popup fill.
pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0d, 0x0c);
/// Cards, tiles, list rows, chips, transfer rows.
pub const CARD: Color32 = Color32::from_rgb(0x18, 0x1c, 0x1a);
/// Rest fill of buttons and inputs; hover fill of clickable surfaces; the
/// job thumbnail's well; skeletons; the jog wheel's inner ring.
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x23, 0x28, 0x26);
/// Hover fill of egui widgets; the jog wheel's home disc.
pub const HOVER_FILL: Color32 = Color32::from_rgb(0x2b, 0x31, 0x2d);
/// Pressed and keyboard-focused fill.
pub const PRESSED_FILL: Color32 = Color32::from_rgb(0x1d, 0x22, 0x20);
/// 1 px rest outline of cards, tiles, rows, chips, modal, inputs.
/// Decorative (A3), and light enough to read as an edge: 1.95:1 on CARD,
/// where the flat #2B302D it replaced was 1.28:1 (decision O11).
pub const BORDER: Color32 = Color32::from_rgb(0x45, 0x4c, 0x48);
pub const TEXT: Color32 = Color32::from_rgb(0xec, 0xee, 0xed);
/// Captions, units, section labels, placeholders. Never on ACCENT_DARK.
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x98, 0xa0, 0x9b);
pub const ACCENT: Color32 = Color32::from_rgb(0x22, 0xb1, 0x4c);
/// Selection fill: tabs, filters, text selection.
pub const ACCENT_DARK: Color32 = Color32::from_rgb(0x17, 0x70, 0x33);
/// The plate map's selected-object stroke.
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0x2c, 0xc9, 0x5a);
/// Text on an ACCENT fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x06, 0x13, 0x0a);
pub const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x00);
pub const WARN_BG: Color32 = Color32::from_rgb(0x33, 0x26, 0x0b);
pub const DANGER: Color32 = Color32::from_rgb(0xef, 0x71, 0x6c);
pub const DANGER_BG: Color32 = Color32::from_rgb(0x3a, 0x16, 0x14);
/// The Finished state. Never on HOVER_FILL.
pub const BLUE: Color32 = Color32::from_rgb(0x3f, 0x8c, 0xff);
/// The toggle's track when off.
pub const TRACK_OFF: Color32 = Color32::from_rgb(0x6b, 0x73, 0x6e);
pub const KNOB: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// The selected chip's ring.
pub const CHIP_SELECTED: Color32 = KNOB;
/// Camera, player and tile picture wells.
pub const MEDIA_WELL: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const PLATE_BG: Color32 = Color32::from_rgb(0x15, 0x18, 0x16);
pub const PLATE_GRID: Color32 =
    Color32::from_rgba_unmultiplied_const(0x20, 0x24, 0x22, 0x60);
pub const PLATE_OBJECT: Color32 = Color32::from_rgb(0x3a, 0x40, 0x3d);
/// An AMS swatch whose colour is missing or unparseable.
pub const SWATCH_UNKNOWN: Color32 = Color32::from_rgb(0x44, 0x44, 0x44);

/// A colour the printer reported (an AMS tray's filament), as opposed to a
/// role above.
pub fn reported(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// The one state vocabulary (C6): the word and the colour for a printer's
/// `gcode_state`, used by the chip and by the job card so they never read
/// differently. A printer the app cannot reach is Offline, whatever its
/// last telemetry said (D10, D28).
pub fn state_word(gcode_state: &str, online: bool) -> (String, Color32) {
    if !online {
        return ("Offline".to_string(), TEXT_DIM);
    }
    let (word, color) = match gcode_state {
        "RUNNING" => ("Running", ACCENT),
        "PAUSE" => ("Paused", WARN),
        "PREPARE" => ("Preparing", WARN),
        "SLICING" => ("Slicing", WARN),
        "FINISH" => ("Finished", BLUE),
        "FAILED" => ("Failed", DANGER),
        "IDLE" => ("Idle", TEXT_DIM),
        "" => ("—", TEXT_DIM),
        // anything the printer invents, in title case and no state colour
        other => return (title_case(other), TEXT_DIM),
    };
    (word.to_string(), color)
}

/// "SOME_STATE" as "Some state": a word the app does not know, said
/// plainly rather than shouted.
fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (at, ch) in text.chars().enumerate() {
        let ch = match ch {
            '_' => ' ',
            ch if at == 0 => ch.to_ascii_uppercase(),
            ch => ch.to_ascii_lowercase(),
        };
        out.push(ch);
    }
    out
}

// -------------------------------------------------------------------- type

pub const BOLD_FAMILY: &str = "sbold";

/// The six sizes the UI uses: 12, 13.5, 14, 16, 20 and 24.
pub mod font {
    use std::sync::LazyLock;

    use egui::{FontFamily, FontId};

    /// Built once, so a helper only clones an `Arc` and never allocates.
    static BOLD: LazyLock<FontFamily> =
        LazyLock::new(|| FontFamily::Name(super::BOLD_FAMILY.into()));

    fn regular(size: f32) -> FontId {
        FontId::new(size, FontFamily::Proportional)
    }

    /// Real bold (Segoe UI Bold when available).
    fn bold(size: f32) -> FontId {
        FontId::new(size, BOLD.clone())
    }

    /// Captions, metadata, units, notes, status and tile lines.
    pub fn caption() -> FontId {
        regular(12.0)
    }

    /// ALL-CAPS section and card titles, fact labels, badges, chip state.
    pub fn label() -> FontId {
        bold(12.0)
    }

    /// `TextStyle::Body`: row names, error and empty-state text.
    pub fn body() -> FontId {
        regular(14.0)
    }

    /// Chip, job and detail titles, banner titles, confirm questions.
    pub fn body_strong() -> FontId {
        bold(14.0)
    }

    /// `TextStyle::Button`.
    pub fn button() -> FontId {
        regular(13.5)
    }

    /// Accent button labels; the current option in the Speed dialog.
    pub fn button_strong() -> FontId {
        bold(13.5)
    }

    /// Dialog headers, the page title, card values.
    pub fn title() -> FontId {
        bold(16.0)
    }

    /// The one primary number on a card.
    pub fn metric() -> FontId {
        bold(24.0)
    }

    /// The jog wheel's home glyph.
    pub fn icon_large() -> FontId {
        regular(20.0)
    }
}

// ----------------------------------------------------------------- spacing

pub mod space {
    /// Label to value in one control; tight stacks; fact rows.
    pub const XS: f32 = 4.0;
    /// Between the parts of one block inside a card.
    pub const S: f32 = 6.0;
    /// The global item spacing, and the gap between cards.
    pub const M: f32 = 8.0;
    /// After a header; before a dialog's action row.
    pub const L: f32 = 12.0;
    /// The indent step; the gap before a card's buttons after prose.
    pub const XL: f32 = 16.0;
    /// Modal padding.
    pub const XXL: f32 = 20.0;
}

/// A tight vertical stack (AMS slots, HMS lines). Called on a Ui of its
/// own, so its siblings keep `space::M` (E21).
pub fn tight_stack(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing.y = space::XS;
}

/// One row of `font` as egui lays it out: derived heights are built from
/// this, never measured and fed back (1.8).
pub fn row_height(ui: &egui::Ui, font: &egui::FontId) -> f32 {
    ui.fonts_mut(|fonts| fonts.row_height(font))
}

/// The width `text` takes on one line in `font`: what a slot is sized for
/// (E10).
pub fn text_width(ui: &egui::Ui, text: &str, font: &egui::FontId) -> f32 {
    // rounded up, so a slot sized for a string never truncates it
    ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_string(), font.clone(),
                                              Color32::PLACEHOLDER).size().x)
        .ceil()
}

pub mod pad {
    use egui::Margin;

    pub const PAGE: Margin = Margin::same(12);
    pub const BAR: Margin = Margin::symmetric(12, 8);
    pub const CARD: Margin = Margin::same(12);
    pub const BANNER: Margin = Margin::same(8);
    pub const ROW: Margin = Margin::symmetric(12, 4);
    pub const CHIP: Margin = Margin::symmetric(12, 6);
    pub const TILE: Margin = Margin::same(6);
    pub const MODAL: Margin = Margin::same(20);
}

pub mod radius {
    use egui::CornerRadius;

    /// Cards, tiles, modals, the camera and player wells.
    pub const CARD: CornerRadius = CornerRadius::same(14);
    /// egui widgets, chips, rows, banners, the toast, inset frames.
    pub const CONTROL: CornerRadius = CornerRadius::same(10);
    /// Pictures inside a card or a tile.
    pub const MEDIA: CornerRadius = CornerRadius::same(8);
    /// Plate-map object boxes.
    pub const MARK: CornerRadius = CornerRadius::same(3);

    /// A pill: half the height of what it rounds.
    pub fn pill(height: f32) -> CornerRadius {
        CornerRadius::same((height / 2.0).round() as u8)
    }
}

pub mod stroke {
    /// Every rest outline and widget stroke.
    pub const HAIRLINE: f32 = 1.0;
    /// The selection ring on tiles, rows and chips.
    pub const SELECTED: f32 = 2.0;
    /// The focus ring.
    pub const FOCUS: f32 = 2.0;
    /// The AMS swatch ring; the chip drag ghost.
    pub const MEDIUM: f32 = 1.5;
    /// The drag insertion marker; jog dividers.
    pub const HEAVY: f32 = 3.0;
}

// ------------------------------------------------------------------- sizes

pub mod size {
    use egui::{Vec2, vec2};

    pub const CONTROL_H: f32 = 32.0;
    /// The smallest interact height an inline text button may have. The
    /// text keeps its own row height; only the interact rect grows (A5).
    pub const INLINE_TARGET_H: f32 = 24.0;
    /// What a default button paints: an 18 px text row plus 2 x 8 padding.
    pub const BUTTON_H: f32 = 34.0;
    pub const BUTTON_H_LARGE: f32 = 40.0;
    pub const STEP_BUTTON_W: f32 = 48.0;
    pub const Z_BUTTON_W: f32 = 92.0;
    pub const ICON_BUTTON: Vec2 = vec2(36.0, 34.0);
    pub const TOGGLE: Vec2 = vec2(46.0, 26.0);
    pub const TOGGLE_KNOB_R: f32 = 11.0;
    pub const PROGRESS_H: f32 = 8.0;
    pub const TRANSFER_BAR_W: f32 = 160.0;
    pub const FILTER_W: f32 = 180.0;
    pub const SPEED_COMBO_W: f32 = 72.0;
    pub const JOB_THUMB: f32 = 88.0;
    pub const SWATCH: f32 = 20.0;
    pub const SWATCH_RING_R: f32 = 9.0;
    pub const SWATCH_HOLE_R: f32 = 3.0;
    pub const PLATE_MAP_MAX: f32 = 360.0;
    pub const JOG: f32 = 240.0;
    /// A tile's outer width, stroke included.
    pub const TILE_W: f32 = 168.0;
    pub const TILE_IMAGE_H: f32 = 94.0;
    pub const DETAIL_W: f32 = 268.0;
    pub const LIST_MIN_W: f32 = 240.0;
    /// The fact label column, so fact values line up.
    pub const FACT_LABEL_W: f32 = 76.0;
    /// The panel's left column, as a share of the width.
    pub const LEFT_COLUMN: f32 = 0.58;
    /// What the panel's right column keeps, however narrow the window.
    pub const RIGHT_COLUMN_MIN: f32 = 280.0;
    pub const CAMERA_MAX_H: f32 = 420.0;
    /// A picture well's shape until its first frame arrives (C8).
    pub const VIDEO_ASPECT: Vec2 = vec2(16.0, 9.0);
    /// A chip's name is truncated past this.
    pub const CHIP_NAME_MAX_W: f32 = 160.0;
    pub const REFUSAL_MAX_W: f32 = 640.0;
    pub const SKELETON_MAX_W: f32 = 420.0;
    pub const MODAL_S: f32 = 320.0;
    pub const MODAL_M: f32 = 380.0;
    pub const MODAL_L: f32 = 440.0;
    pub const SKIP_LIST_MAX_H: f32 = 220.0;
    pub const MODULES_MAX_H: f32 = 150.0;
    pub const HMS_LIST_MAX_H: f32 = 320.0;
    /// Transfer rows beyond this many scroll.
    pub const TRANSFER_ROWS_VISIBLE: usize = 3;
    pub const WINDOW: [f32; 2] = [1080.0, 780.0];
    /// Raised from 700 x 480 (decision O2): the layout stays readable.
    pub const WINDOW_MIN: [f32; 2] = [960.0, 640.0];
}

/// The one card: cards, tiles (with `pad::TILE`), the detail pane, the
/// empty-state, error and refusal cards (C1).
pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(egui::Stroke::new(stroke::HAIRLINE, BORDER))
        .corner_radius(radius::CARD)
        .inner_margin(pad::CARD)
}

// ------------------------------------------------------------------ wiring

/// Apply the global dark style to the egui context.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
    use egui::{Stroke, TextStyle};
    style.text_styles.insert(TextStyle::Body, font::body());
    style.text_styles.insert(TextStyle::Button, font::button());
    style.text_styles.insert(TextStyle::Small, font::caption());
    style.text_styles.insert(TextStyle::Heading, font::title());

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = CARD_HOVER;
    v.faint_bg_color = CARD;
    // real text color per state (no global override → disabled greys out)
    v.override_text_color = None;
    let hairline = Stroke::new(stroke::HAIRLINE, BORDER);
    let text = Stroke::new(stroke::HAIRLINE, TEXT);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.bg_stroke = hairline;
    w.noninteractive.fg_stroke = text;
    w.noninteractive.corner_radius = radius::CONTROL;
    w.inactive.bg_fill = CARD_HOVER;
    w.inactive.weak_bg_fill = CARD_HOVER;
    w.inactive.bg_stroke = hairline;
    w.inactive.fg_stroke = text;
    w.inactive.corner_radius = radius::CONTROL;
    w.hovered.bg_fill = HOVER_FILL;
    w.hovered.weak_bg_fill = HOVER_FILL;
    w.hovered.bg_stroke = hairline;
    w.hovered.fg_stroke = text;
    w.hovered.corner_radius = radius::CONTROL;
    // pressed and keyboard focus: the 2 px focus outline
    w.active.bg_fill = PRESSED_FILL;
    w.active.weak_bg_fill = PRESSED_FILL;
    w.active.bg_stroke = Stroke::new(stroke::FOCUS, TEXT);
    w.active.fg_stroke = text;
    w.active.corner_radius = radius::CONTROL;
    // an open ComboBox keeps the palette instead of egui's greys
    w.open = w.inactive;
    v.window_corner_radius = radius::CARD;
    v.window_stroke = hairline;
    v.selection.bg_fill = ACCENT_DARK;
    v.selection.stroke = text;
    v.text_cursor.stroke = Stroke::new(stroke::SELECTED, ACCENT);
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.hyperlink_color = BLUE;
    style.spacing.item_spacing = egui::vec2(space::M, space::M);
    style.spacing.button_padding = egui::vec2(space::XL, space::M);
    style.spacing.interact_size.y = size::CONTROL_H;
    });
}

/// Windows system fonts: Segoe UI (+ Bold) for text and Segoe UI
/// Emoji/Symbol for the glyphs (⏸ ⏹ 💧 ⌂ …) missing from egui's
/// embedded fonts. Also registers the "sbold" family the bold `font`
/// helpers use; it falls back to the regular stack when the bold face
/// isn't found so the family name always resolves.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded: Vec<&str> = Vec::new();
    // (y_offset_factor, scale): the symbol/emoji faces sit too high on
    // the baseline next to Segoe text — nudge them down and shrink a
    // touch so ⏸ ⏹ 💧 line up with the text vertically.
    for (name, path, tweak) in [
        ("segoe", "C:\\Windows\\Fonts\\segoeui.ttf", (0.0, 1.0)),
        ("segoe-bold", "C:\\Windows\\Fonts\\segoeuib.ttf", (0.0, 1.0)),
        ("seguiemj", "C:\\Windows\\Fonts\\seguiemj.ttf", (0.12, 0.88)),
        ("seguisym", "C:\\Windows\\Fonts\\seguisym.ttf", (0.08, 0.95)),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            let mut data = egui::FontData::from_owned(bytes);
            data.tweak = egui::FontTweak {
                y_offset_factor: tweak.0,
                scale: tweak.1,
                ..Default::default()
            };
            fonts.font_data.insert(name.to_string(), data.into());
            loaded.push(name);
        }
    }
    // text fallback chain (regular first, then symbol fonts)
    let text_chain: Vec<String> = ["segoe", "seguiemj", "seguisym"]
        .iter()
        .filter(|n| loaded.contains(n))
        .map(|n| n.to_string())
        .collect();
    for family in [egui::FontFamily::Proportional,
                   egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for (pos, name) in text_chain.iter().enumerate() {
            list.insert(pos, name.clone());
        }
    }
    // bold family: bold face first, then the regular chain as fallback
    let mut bold_list = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if loaded.contains(&"segoe-bold") {
        bold_list.insert(0, "segoe-bold".to_string());
    }
    fonts.families.insert(
        egui::FontFamily::Name(BOLD_FAMILY.into()), bold_list);
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use egui::Color32;

    use super::*;

    /// WCAG 2 relative luminance of an sRGB colour.
    fn luminance(color: Color32) -> f64 {
        let channel = |value: u8| {
            let c = f64::from(value) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g())
            + 0.0722 * channel(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// Text at 4.5:1, `font::metric` and non-text indicators at 3:1 (A1, A2).
    const TEXT_MIN: f64 = 4.5;
    const LARGE_MIN: f64 = 3.0;

    /// Section 1.2's legal pairs: foreground, background, the ratio the
    /// table documents, and the floor it must clear.
    const LEGAL: &[(&str, Color32, Color32, f64, f64)] = &[
        ("TEXT on BG", TEXT, BG, 16.73, TEXT_MIN),
        ("TEXT_DIM on BG", TEXT_DIM, BG, 7.28, TEXT_MIN),
        ("TEXT on CARD", TEXT, CARD, 14.77, TEXT_MIN),
        ("TEXT_DIM on CARD", TEXT_DIM, CARD, 6.42, TEXT_MIN),
        ("TEXT on CARD_HOVER", TEXT, CARD_HOVER, 12.84, TEXT_MIN),
        ("TEXT_DIM on CARD_HOVER", TEXT_DIM, CARD_HOVER, 5.59, TEXT_MIN),
        ("TEXT on HOVER_FILL", TEXT, HOVER_FILL, 11.41, TEXT_MIN),
        ("TEXT_DIM on HOVER_FILL", TEXT_DIM, HOVER_FILL, 4.96, TEXT_MIN),
        ("DANGER on HOVER_FILL", DANGER, HOVER_FILL, 4.59, TEXT_MIN),
        ("TEXT on PRESSED_FILL", TEXT, PRESSED_FILL, 13.84, TEXT_MIN),
        ("TEXT_DIM on PRESSED_FILL", TEXT_DIM, PRESSED_FILL, 6.02, TEXT_MIN),
        ("DANGER on PRESSED_FILL", DANGER, PRESSED_FILL, 5.57, TEXT_MIN),
        ("ACCENT on CARD", ACCENT, CARD, 6.12, TEXT_MIN),
        ("ACCENT on HOVER_FILL", ACCENT, HOVER_FILL, 4.72, TEXT_MIN),
        ("TEXT on ACCENT_DARK", TEXT, ACCENT_DARK, 5.30, TEXT_MIN),
        ("ACCENT_DARK against BG", ACCENT_DARK, BG, 3.16, LARGE_MIN),
        ("ACCENT_BRIGHT against PLATE_BG", ACCENT_BRIGHT, PLATE_BG, 8.18,
         LARGE_MIN),
        ("ON_ACCENT on ACCENT", ON_ACCENT, ACCENT, 6.75, TEXT_MIN),
        ("WARN on WARN_BG", WARN, WARN_BG, 5.33, TEXT_MIN),
        ("WARN on CARD", WARN, CARD, 6.22, TEXT_MIN),
        ("WARN on HOVER_FILL", WARN, HOVER_FILL, 4.80, TEXT_MIN),
        ("DANGER on BG", DANGER, BG, 6.73, TEXT_MIN),
        ("DANGER on CARD", DANGER, CARD, 5.94, TEXT_MIN),
        ("DANGER on CARD_HOVER", DANGER, CARD_HOVER, 5.17, TEXT_MIN),
        ("DANGER on DANGER_BG", DANGER, DANGER_BG, 5.56, TEXT_MIN),
        ("DANGER on WARN_BG", DANGER, WARN_BG, 5.09, TEXT_MIN),
        ("BLUE on BG", BLUE, BG, 5.95, TEXT_MIN),
        ("BLUE on CARD", BLUE, CARD, 5.25, TEXT_MIN),
        ("BLUE on CARD_HOVER", BLUE, CARD_HOVER, 4.57, TEXT_MIN),
        ("TRACK_OFF against CARD", TRACK_OFF, CARD, 3.53, LARGE_MIN),
        ("TRACK_OFF against CARD_HOVER", TRACK_OFF, CARD_HOVER, 3.07,
         LARGE_MIN),
        ("KNOB on TRACK_OFF", KNOB, TRACK_OFF, 4.88, LARGE_MIN),
        ("TEXT_DIM on MEDIA_WELL", TEXT_DIM, MEDIA_WELL, 7.84, TEXT_MIN),
        ("TEXT on PLATE_OBJECT", TEXT, PLATE_OBJECT, 9.10, TEXT_MIN),
        // a dialog is a CARD surface since O11, so every pair its text
        // makes is the CARD pair, already above; what is new is the
        // outline that surrounds both, and it is decorative (A3)
    ];

    /// 1.2 and decision O11: the outline is decorative, so it has no
    /// floor to clear, but it has to read as an edge. These are the two
    /// surfaces it is drawn on.
    #[test]
    fn the_outline_reads_as_an_edge_on_both_surfaces() {
        for (name, under, documented) in [("CARD", CARD, 1.95),
                                          ("BG", BG, 2.21)] {
            let ratio = contrast(BORDER, under);
            assert!((ratio - documented).abs() < 0.01,
                    "BORDER on {name}: {ratio:.3}, the table says \
                     {documented}");
            // the flat #2B302D it replaced was 1.28:1 on CARD
            assert!(ratio > 1.5, "BORDER on {name} is flat: {ratio:.2}");
        }
    }

    /// A1 and A2: every pair section 1.2 allows clears its floor, and its
    /// ratio is the one the table documents.
    #[test]
    fn every_legal_pair_clears_its_contrast_floor() {
        for (name, fg, bg, documented, floor) in LEGAL {
            let ratio = contrast(*fg, *bg);
            assert!((ratio - documented).abs() < 0.01,
                    "{name}: {ratio:.3}, the table says {documented}");
            assert!(ratio >= *floor, "{name}: {ratio:.2} < {floor}");
        }
    }

    /// The control: the pairs 1.2 marks illegal are measured as failing by
    /// the same function, so a pass above is not a broken measurement.
    #[test]
    fn the_pairs_marked_illegal_fail_the_text_floor() {
        for (name, fg, bg, documented) in [
            ("TEXT_DIM on ACCENT_DARK", TEXT_DIM, ACCENT_DARK, 2.30),
            ("ACCENT on ACCENT_DARK", ACCENT, ACCENT_DARK, 2.19),
            ("BLUE on HOVER_FILL", BLUE, HOVER_FILL, 4.05),
        ] {
            let ratio = contrast(fg, bg);
            assert!((ratio - documented).abs() < 0.01,
                    "{name}: {ratio:.3}, the table says {documented}");
            assert!(ratio < TEXT_MIN, "{name}: {ratio:.2}");
        }
    }
}
