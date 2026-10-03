//! Look and feel: layered blue-black surfaces, one accent, Inter for text and
//! Phosphor for icons, sized for pointing with a controller ray at arm's
//! length. Colors and type come from here; components from `widgets`.

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    TextStyle, Visuals,
};
use std::sync::Arc;

// Surfaces, darkest first.
pub const BG: Color32 = Color32::from_rgb(13, 16, 21);
pub const SURFACE: Color32 = Color32::from_rgb(22, 27, 34);
pub const SURFACE_2: Color32 = Color32::from_rgb(31, 37, 46);
pub const SURFACE_3: Color32 = Color32::from_rgb(43, 51, 62);
/// Hairlines: dividers and card edges.
pub const STROKE: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);

// Text.
pub const TEXT: Color32 = Color32::from_rgb(236, 240, 244);
pub const TEXT_2: Color32 = Color32::from_rgb(150, 160, 174);
pub const TEXT_3: Color32 = Color32::from_rgb(104, 114, 128);

// Accent and status.
pub const ACCENT: Color32 = Color32::from_rgb(26, 159, 255);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(77, 182, 255);
pub const ACCENT_PRESSED: Color32 = Color32::from_rgb(16, 128, 214);
pub const OK: Color32 = Color32::from_rgb(92, 196, 110);
pub const WARN: Color32 = Color32::from_rgb(242, 182, 64);
pub const ERROR: Color32 = Color32::from_rgb(242, 88, 94);

// Older names, used across the screens.
pub const PANEL_BG: Color32 = BG;
pub const CARD_BG: Color32 = SURFACE;
pub const MUTED: Color32 = TEXT_2;

/// Text weights; each is Inter with Phosphor icons as a fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

impl Weight {
    fn family(self) -> FontFamily {
        match self {
            Weight::Regular => FontFamily::Proportional,
            Weight::Medium => FontFamily::Name("medium".into()),
            Weight::SemiBold => FontFamily::Name("semibold".into()),
            Weight::Bold => FontFamily::Name("bold".into()),
        }
    }
}

pub fn font(weight: Weight, size: f32) -> FontId {
    FontId::new(size, weight.family())
}

/// Phosphor's filled weight, for icons that should read as solid (play).
pub fn icon_fill(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icon-fill".into()))
}

/// Phosphor's regular (outline) weight.
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icon".into()))
}

pub fn title() -> TextStyle {
    TextStyle::Name("title".into())
}

pub fn subhead() -> TextStyle {
    TextStyle::Name("subhead".into())
}

fn fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let fallbacks: Vec<String> = defs.families[&FontFamily::Proportional].clone();
    let mut add = |name: &str, bytes: &'static [u8]| {
        defs.font_data
            .insert(name.into(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        "inter",
        include_bytes!("../../assets/fonts/Inter-Regular.ttf"),
    );
    add(
        "inter-medium",
        include_bytes!("../../assets/fonts/Inter-Medium.ttf"),
    );
    add(
        "inter-semibold",
        include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"),
    );
    add(
        "inter-bold",
        include_bytes!("../../assets/fonts/Inter-Bold.ttf"),
    );
    add(
        "phosphor",
        include_bytes!("../../assets/fonts/Phosphor.ttf"),
    );
    add(
        "phosphor-fill",
        include_bytes!("../../assets/fonts/Phosphor-Fill.ttf"),
    );
    let chain = |first: &str| {
        let mut v = vec![first.to_string(), "phosphor".to_string()];
        v.extend(fallbacks.iter().cloned());
        v
    };
    defs.families
        .insert(FontFamily::Proportional, chain("inter"));
    defs.families
        .insert(FontFamily::Name("medium".into()), chain("inter-medium"));
    defs.families
        .insert(FontFamily::Name("semibold".into()), chain("inter-semibold"));
    defs.families
        .insert(FontFamily::Name("bold".into()), chain("inter-bold"));
    defs.families
        .insert(FontFamily::Name("icon".into()), vec!["phosphor".into()]);
    defs.families.insert(
        FontFamily::Name("icon-fill".into()),
        vec!["phosphor-fill".into()],
    );
    if let Some(mono) = defs.families.get_mut(&FontFamily::Monospace) {
        mono.push("phosphor".into());
    }
    defs
}

pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Small, font(Weight::Regular, 15.0)),
        (TextStyle::Body, font(Weight::Regular, 18.0)),
        (TextStyle::Button, font(Weight::Medium, 18.0)),
        (TextStyle::Heading, font(Weight::SemiBold, 28.0)),
        (
            TextStyle::Monospace,
            FontId::new(16.0, FontFamily::Monospace),
        ),
        (title(), font(Weight::Bold, 40.0)),
        (subhead(), font(Weight::SemiBold, 22.0)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(10.0, 10.0);
    s.button_padding = egui::vec2(16.0, 10.0);
    s.interact_size = egui::vec2(44.0, 44.0);
    s.slider_width = 280.0;
    s.slider_rail_height = 6.0;
    s.icon_width = 22.0;
    s.icon_spacing = 10.0;
    s.menu_margin = egui::Margin::same(10);
    s.window_margin = egui::Margin::same(16);
    s.scroll.bar_width = 8.0;
    s.scroll.floating = true;
    s.scroll.floating_allocated_width = 0.0;
    style.interaction.tooltip_delay = 0.5;
    style.visuals = visuals();
    ctx.set_style(style);
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, STROKE);
    v.window_corner_radius = CornerRadius::same(16);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = v.window_shadow;
    v.menu_corner_radius = CornerRadius::same(12);
    v.extreme_bg_color = Color32::from_rgb(9, 11, 15);
    v.text_edit_bg_color = Some(SURFACE_2);
    v.faint_bg_color = SURFACE;
    v.code_bg_color = SURFACE_2;
    v.hyperlink_color = ACCENT_HOVER;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERROR;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.5, Color32::WHITE);
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.striped = false;

    let r = CornerRadius::same(10);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, STROKE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(214, 220, 228));
    w.inactive.bg_fill = SURFACE_3;
    w.inactive.weak_bg_fill = SURFACE_2;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.5, TEXT);
    w.hovered.bg_fill = SURFACE_3;
    w.hovered.weak_bg_fill = SURFACE_3;
    w.hovered.bg_stroke = Stroke::new(1.5, Color32::from_white_alpha(70));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.hovered.expansion = 1.0;
    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = ACCENT_PRESSED;
    w.active.bg_stroke = Stroke::new(1.5, ACCENT_HOVER);
    w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.active.expansion = 1.0;
    w.open = w.hovered;
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.corner_radius = r;
    }
    v
}

#[cfg(test)]
mod tests {
    /// Every symbol the UI draws must exist in the fonts.
    #[test]
    fn icons_have_glyphs() {
        let ctx = egui::Context::default();
        super::apply(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        let mut used = String::new();
        for src in [
            include_str!("library.rs"),
            include_str!("player.rs"),
            include_str!("sources.rs"),
            include_str!("settings.rs"),
            include_str!("keyboard.rs"),
            include_str!("widgets.rs"),
        ] {
            used.extend(src.chars().filter(|c| !c.is_ascii()));
        }
        let icons = include_str!("icons.rs");
        for line in icons.lines() {
            if let Some(hex) = line.split("\\u{").nth(1).and_then(|r| r.split('}').next())
                && let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            {
                used.push(c);
            }
        }
        let missing: String = ctx.fonts(|f| {
            used.chars()
                .filter(|c| !f.has_glyph(&egui::FontId::proportional(20.0), *c))
                .collect()
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
        // The filled weight has the icons the UI draws solid.
        for c in ['\u{E3D0}', '\u{E39E}', '\u{E46A}'] {
            assert!(ctx.fonts(|f| f.has_glyph(&super::icon_fill(20.0), c)));
        }
    }
}
