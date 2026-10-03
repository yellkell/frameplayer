//! Look and feel: large, high-contrast widgets sized for pointing with a
//! controller ray at arm's length.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(92, 160, 255);
pub const PANEL_BG: Color32 = Color32::from_rgb(14, 16, 22);
pub const CARD_BG: Color32 = Color32::from_rgb(28, 32, 42);
pub const MUTED: Color32 = Color32::from_rgb(150, 156, 170);
pub const WARN: Color32 = Color32::from_rgb(255, 190, 90);
pub const ERROR: Color32 = Color32::from_rgb(255, 110, 100);
pub const OK: Color32 = Color32::from_rgb(120, 220, 140);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(15.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(19.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(20.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(28.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(17.0, FontFamily::Monospace),
        ),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style.spacing.interact_size = egui::vec2(44.0, 40.0);
    style.spacing.slider_width = 260.0;
    style.spacing.icon_width = 24.0;
    style.spacing.scroll.bar_width = 14.0;
    style.spacing.scroll.floating = false;
    style.interaction.tooltip_delay = 0.6;
    let mut v = Visuals::dark();
    v.panel_fill = PANEL_BG;
    v.window_fill = Color32::from_rgb(22, 25, 33);
    v.extreme_bg_color = Color32::from_rgb(10, 11, 15);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.6);
    v.selection.stroke = Stroke::new(1.5_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    let r = CornerRadius::same(10);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.noninteractive,
        &mut v.widgets.open,
    ] {
        w.corner_radius = r;
    }
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 45, 58);
    v.widgets.inactive.bg_fill = Color32::from_rgb(40, 45, 58);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 66, 86);
    v.widgets.hovered.bg_stroke = Stroke::new(2.0_f32, ACCENT);
    v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.7);
    v.window_corner_radius = CornerRadius::same(16);
    style.visuals = v;
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    /// Every symbol the UI draws must exist in egui's bundled fonts.
    #[test]
    fn icons_have_glyphs() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let mut used = String::new();
        for src in [
            include_str!("library.rs"),
            include_str!("player.rs"),
            include_str!("sources.rs"),
            include_str!("settings.rs"),
            include_str!("keyboard.rs"),
        ] {
            used.extend(src.chars().filter(|c| !c.is_ascii()));
        }
        let missing: String = ctx.fonts(|f| {
            used.chars()
                .filter(|c| !f.has_glyph(&egui::FontId::proportional(20.0), *c))
                .collect()
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }
}
