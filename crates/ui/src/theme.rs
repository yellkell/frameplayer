//! Colours and the visual theme.
//!
//! Colours are stored linear and premultiplied, which is what
//! [`fp_core::draw::Vertex::color`] expects. Sizes are panel pixels and are
//! derived from the panel's pixels-per-metre and viewing distance so text
//! subtends a legible visual angle in the headset.

/// Linear-space, premultiplied RGBA.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Color(pub [f32; 4]);

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl Color {
    pub const TRANSPARENT: Color = Color([0.0; 4]);
    pub const WHITE: Color = Color([1.0; 4]);
    pub const BLACK: Color = Color([0.0, 0.0, 0.0, 1.0]);

    /// From 8-bit sRGB components with straight (non-premultiplied) alpha.
    pub fn from_srgba8(r: u8, g: u8, b: u8, a: u8) -> Color {
        let a = a as f32 / 255.0;
        Color([
            srgb_to_linear(r) * a,
            srgb_to_linear(g) * a,
            srgb_to_linear(b) * a,
            a,
        ])
    }

    pub fn from_srgb8(r: u8, g: u8, b: u8) -> Color {
        Color::from_srgba8(r, g, b, 255)
    }

    /// `0xRRGGBB` sRGB, opaque.
    pub fn hex(rgb: u32) -> Color {
        Color::from_srgb8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
    }

    /// Scales opacity (all channels, since premultiplied).
    pub fn alpha(self, a: f32) -> Color {
        let a = a.clamp(0.0, 1.0);
        Color(self.0.map(|c| c * a))
    }

    pub fn lerp(self, o: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        Color(std::array::from_fn(|i| {
            self.0[i] + (o.0[i] - self.0[i]) * t
        }))
    }

    pub fn a(&self) -> f32 {
        self.0[3]
    }
}

/// Smallest visual angle we let body text subtend: 20 px at 1000 px/m viewed
/// from 1.5 m (≈ 0.76°). [verify] tune on the Frame's 2160² panels and lenses.
pub const MIN_TEXT_ANGLE_RAD: f32 = 0.013_332;

/// Minimum legible text height in panel pixels for a panel of `pixels_per_meter`
/// viewed from `distance_m`.
pub fn min_text_px(pixels_per_meter: f32, distance_m: f32) -> f32 {
    pixels_per_meter * distance_m * MIN_TEXT_ANGLE_RAD.tan()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub panel_bg: Color,
    pub surface: Color,
    pub surface_hover: Color,
    pub surface_active: Color,
    pub border: Color,
    pub text: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub accent_hover: Color,
    pub on_accent: Color,
    pub danger: Color,
    pub success: Color,
    pub warning: Color,
    pub focus_ring: Color,
    pub backdrop: Color,
    pub track: Color,
    pub buffered: Color,

    pub text_size: f32,
    pub small_text_size: f32,
    pub title_text_size: f32,
    pub padding: f32,
    pub spacing: f32,
    pub corner_radius: f32,
    pub widget_height: f32,
    pub icon_size: f32,
    pub track_thickness: f32,
    pub focus_ring_width: f32,
    pub scrollbar_width: f32,
    /// Pointer travel (px) before a press inside a scroll area becomes a drag-scroll.
    pub drag_threshold: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

impl Theme {
    /// Dark theme sized for a 1000 px/m panel at ~1.5 m.
    pub fn dark() -> Theme {
        let accent = Color::hex(0x3d8bfd);
        Theme {
            panel_bg: Color::from_srgba8(18, 20, 26, 235),
            surface: Color::from_srgb8(38, 42, 52),
            surface_hover: Color::from_srgb8(54, 60, 74),
            surface_active: Color::from_srgb8(70, 78, 96),
            border: Color::from_srgb8(80, 86, 100),
            text: Color::from_srgb8(236, 238, 242),
            text_dim: Color::from_srgb8(150, 156, 170),
            accent,
            accent_hover: accent.lerp(Color::WHITE, 0.2),
            on_accent: Color::WHITE,
            danger: Color::hex(0xe5484d),
            success: Color::hex(0x30a46c),
            warning: Color::hex(0xf5a524),
            focus_ring: Color::hex(0xffd166),
            backdrop: Color::from_srgba8(0, 0, 0, 160),
            track: Color::from_srgb8(64, 68, 80),
            buffered: Color::from_srgb8(110, 116, 132),

            text_size: 24.0,
            small_text_size: 20.0,
            title_text_size: 34.0,
            padding: 16.0,
            spacing: 10.0,
            corner_radius: 10.0,
            widget_height: 56.0,
            icon_size: 32.0,
            track_thickness: 8.0,
            focus_ring_width: 3.0,
            scrollbar_width: 8.0,
            drag_threshold: 14.0,
        }
    }

    pub fn with_accent(mut self, accent: Color) -> Theme {
        self.accent = accent;
        self.accent_hover = accent.lerp(Color::WHITE, 0.2);
        self
    }

    /// Multiplies every size by `s` (colours unchanged).
    pub fn scaled(mut self, s: f32) -> Theme {
        for v in [
            &mut self.text_size,
            &mut self.small_text_size,
            &mut self.title_text_size,
            &mut self.padding,
            &mut self.spacing,
            &mut self.corner_radius,
            &mut self.widget_height,
            &mut self.icon_size,
            &mut self.track_thickness,
            &mut self.focus_ring_width,
            &mut self.scrollbar_width,
            &mut self.drag_threshold,
        ] {
            *v *= s;
        }
        self
    }

    /// Dark theme scaled for a given panel density and viewing distance, never
    /// letting small text drop below [`min_text_px`].
    pub fn for_panel(pixels_per_meter: f32, distance_m: f32) -> Theme {
        let base = Theme::dark();
        let min = min_text_px(pixels_per_meter, distance_m);
        Theme::dark().scaled((min / base.small_text_size).max(0.1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legibility_scaling() {
        assert!((min_text_px(1000.0, 1.5) - 20.0).abs() < 0.05);
        let t = Theme::for_panel(1000.0, 1.5);
        assert!((t.small_text_size - 20.0).abs() < 0.05);
        let far = Theme::for_panel(1000.0, 3.0);
        assert!((far.small_text_size - 40.0).abs() < 0.1);
        assert!(far.widget_height > t.widget_height);
    }

    #[test]
    fn colour_conversion() {
        let c = Color::from_srgba8(255, 255, 255, 128);
        assert!((c.0[0] - c.0[3]).abs() < 1e-6, "premultiplied");
        assert!((Color::hex(0x808080).0[0] - 0.2158).abs() < 1e-3);
        assert_eq!(Color::WHITE.alpha(0.5).0, [0.5; 4]);
    }
}
