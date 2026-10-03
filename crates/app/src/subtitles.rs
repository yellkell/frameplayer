//! Subtitle overlay: active cues are drawn into a small dedicated UI panel
//! that the frame loop submits as its own quad layer *at the subtitle depth*
//! (`playback.subtitle_depth_m`). Because the layer really sits at that
//! distance, the compositor produces the correct stereo disparity for both
//! flat and immersive videos (HereSphere-style depth placement), and text
//! stays sharp under reprojection.

use crate::controller::SUBTITLE_KEY_BASE;
use fp_core::draw::TextureId;
use fp_ui::{Align, Color, FrameInput, Rect, TextParams, Ui, UiOutput, Vec2};
use fp_video::subtitle::{CueContent, SubtitleBitmap};
use fp_video::Cue;
use glam::{Quat, Vec3};

/// Subtitle panel resolution.
pub const PANEL_PX: Vec2 = Vec2::new(1600.0, 360.0);
/// Horizontal field of view the panel spans.
pub const PANEL_HFOV_DEG: f32 = 56.0;
/// Panel centre below the horizon.
pub const PANEL_DROP_DEG: f32 = 16.0;

/// Physical panel size for a depth.
pub fn panel_size_m(depth_m: f32) -> Vec2 {
    let w = 2.0 * depth_m.max(0.3) * (PANEL_HFOV_DEG.to_radians() / 2.0).tan();
    Vec2::new(w, w * PANEL_PX.y / PANEL_PX.x)
}

/// Panel pose in the app space: straight ahead at `depth_m`, tilted down,
/// rotated by `base` (lying-down orientation).
pub fn panel_pose(depth_m: f32, base: Quat) -> (Vec3, Quat) {
    let pitch = Quat::from_rotation_x(-PANEL_DROP_DEG.to_radians());
    let rot = base * pitch;
    (rot * Vec3::new(0.0, 0.0, -depth_m.max(0.3)), rot)
}

/// Image-cache key for a bitmap cue (stable while the cue is shown).
pub fn bitmap_key(b: &SubtitleBitmap) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    for v in [b.x, b.y, b.width, b.height] {
        mix(v as u64);
    }
    for chunk in b.rgba.chunks(997).take(64) {
        mix(chunk.iter().map(|&x| x as u64).sum());
    }
    SUBTITLE_KEY_BASE | (h & ((1 << 60) - 1))
}

/// Text of the active text cues, one cue per line block.
pub fn cue_text(cues: &[Cue]) -> String {
    cues.iter()
        .filter_map(|c| match &c.content {
            CueContent::Text(t) => Some(t.plain_text()),
            CueContent::Bitmap(_) => None,
        })
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Draw `cues` into `ui` for this frame. Returns `None` when nothing is shown.
pub fn draw(ui: &mut Ui, cues: &[Cue], dt: f32) -> Option<UiOutput> {
    if cues.is_empty() {
        return None;
    }
    ui.begin_frame(FrameInput {
        dt,
        ..Default::default()
    });
    let size = ui.size();
    let text = cue_text(cues);
    if !text.is_empty() {
        let font = size.y * 0.16;
        let layout = ui.layout_text(
            &text,
            TextParams::new(font)
                .width(size.x * 0.9)
                .wrap()
                .lines(4)
                .align(Align::Center),
        );
        let pad = font * 0.35;
        let x = (size.x - layout.size.x) / 2.0;
        let y = size.y - layout.size.y - pad * 2.0;
        let bg = Rect::new(
            x - pad,
            y - pad,
            layout.size.x + pad * 2.0,
            layout.size.y + pad * 2.0,
        );
        ui.painter().rect_rounded(bg, pad, Color::BLACK.alpha(0.55));
        ui.draw_text_at(Vec2::new(x, y), &layout, Color::WHITE);
    }
    for c in cues {
        if let CueContent::Bitmap(b) = &c.content {
            let scale = size.x / b.canvas_width.max(1) as f32;
            // Keep the bitmap's position relative to the bottom of the canvas.
            let bottom_gap = (b.canvas_height as f32 - (b.y + b.height) as f32).max(0.0) * scale;
            let r = Rect::new(
                b.x as f32 * scale,
                size.y - bottom_gap.min(size.y * 0.5) - b.height as f32 * scale,
                b.width as f32 * scale,
                b.height as f32 * scale,
            );
            ui.painter().image(
                r,
                TextureId::Image(bitmap_key(b)),
                [0.0, 0.0, 1.0, 1.0],
                Color::WHITE,
            );
        }
    }
    Some(ui.end_frame())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::MediaTime;
    use fp_video::subtitle::TextCue;

    #[test]
    fn geometry() {
        let s = panel_size_m(2.0);
        assert!((s.x - 2.0 * 2.0 * (28f32).to_radians().tan()).abs() < 1e-4);
        assert!((s.y / s.x - PANEL_PX.y / PANEL_PX.x).abs() < 1e-6);
        let (p, _) = panel_pose(2.0, Quat::IDENTITY);
        assert!((p.length() - 2.0).abs() < 1e-4);
        assert!(p.y < 0.0 && p.z < 0.0);
    }

    #[test]
    fn draws_text_and_bitmaps() {
        let mut ui = Ui::new(PANEL_PX, 800.0);
        assert!(draw(&mut ui, &[], 0.016).is_none());
        let cue = Cue::text(
            MediaTime::ZERO,
            MediaTime::from_millis(1000),
            TextCue::plain("Hello there"),
        );
        let out = draw(&mut ui, std::slice::from_ref(&cue), 0.016).unwrap();
        assert!(!out.draw_list.cmds.is_empty());
        assert_eq!(cue_text(&[cue.clone(), cue]), "Hello there\nHello there");
        let bmp = SubtitleBitmap {
            x: 100,
            y: 900,
            width: 200,
            height: 50,
            rgba: vec![255; 200 * 50 * 4],
            canvas_width: 1920,
            canvas_height: 1080,
        };
        let key = bitmap_key(&bmp);
        assert!(key & SUBTITLE_KEY_BASE == SUBTITLE_KEY_BASE);
        let cue = Cue {
            start: MediaTime::ZERO,
            end: MediaTime::from_millis(1000),
            content: CueContent::Bitmap(bmp),
        };
        let out = draw(&mut ui, &[cue], 0.016).unwrap();
        assert!(out
            .draw_list
            .cmds
            .iter()
            .any(|c| c.texture == TextureId::Image(key)));
    }
}
