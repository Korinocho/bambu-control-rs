//! Dark theme with green accent — port of the PySide6 palette.

use egui::Color32;

pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0d, 0x0c);
pub const CARD: Color32 = Color32::from_rgb(0x18, 0x1c, 0x1a);
pub const CARD_HOVER: Color32 = Color32::from_rgb(0x23, 0x28, 0x26);
pub const BORDER: Color32 = Color32::from_rgb(0x2b, 0x30, 0x2d);
pub const TEXT: Color32 = Color32::from_rgb(0xec, 0xee, 0xed);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x98, 0xa0, 0x9b);
pub const ACCENT: Color32 = Color32::from_rgb(0x22, 0xb1, 0x4c);
pub const ACCENT_DARK: Color32 = Color32::from_rgb(0x17, 0x70, 0x33);
pub const WARN: Color32 = Color32::from_rgb(0xd9, 0x8a, 0x00);
pub const WARN_BG: Color32 = Color32::from_rgb(0x33, 0x26, 0x0b);
pub const DANGER: Color32 = Color32::from_rgb(0xd6, 0x45, 0x41);
pub const DANGER_BG: Color32 = Color32::from_rgb(0x3a, 0x16, 0x14);
pub const BLUE: Color32 = Color32::from_rgb(0x3f, 0x8c, 0xff);

pub fn state_color(state: &str) -> Color32 {
    match state {
        "RUNNING" => ACCENT,
        "PAUSE" | "PREPARE" | "SLICING" => WARN,
        "FINISH" => BLUE,
        "FAILED" => DANGER,
        _ => TEXT_DIM,
    }
}

/// Apply the global dark style to the egui context.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles.insert(
        TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button, FontId::new(13.5, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Small, FontId::new(11.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional));

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = CARD_HOVER;
    v.faint_bg_color = CARD;
    // real text color per state (no global override → disabled greys out)
    v.override_text_color = None;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, TEXT);
    v.widgets.noninteractive.bg_fill = CARD;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.inactive.bg_fill = CARD_HOVER;
    v.widgets.inactive.weak_bg_fill = CARD_HOVER;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2b, 0x31, 0x2d);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2b, 0x31, 0x2d);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.active.bg_fill = Color32::from_rgb(0x14, 0x17, 0x1a);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x14, 0x17, 0x1a);
    v.widgets.inactive.corner_radius = 10.into();
    v.widgets.hovered.corner_radius = 10.into();
    v.widgets.active.corner_radius = 10.into();
    v.window_corner_radius = 14.into();
    v.window_stroke = egui::Stroke::new(1.0, BORDER);
    v.selection.bg_fill = ACCENT_DARK;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(16.0, 8.0);
    style.spacing.interact_size.y = 32.0;
    });
}

pub const BOLD_FAMILY: &str = "sbold";

/// Real-bold font id (Segoe UI Bold when available).
pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(BOLD_FAMILY.into()))
}

/// Windows system fonts: Segoe UI (+ Bold) for text and Segoe UI
/// Emoji/Symbol for the glyphs (⏸ ⏹ 💧 ⌂ …) missing from egui's
/// embedded fonts. Also registers the "sbold" family used by
/// [`bold`]; it falls back to the regular stack when the bold face
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
