//! Builds the projection shader's uniform block.

use fp_core::format::{Projection, StereoLayout, VideoFormat};
use fp_core::view::ViewSettings;
use fp_media::PixelLayout;
use fp_media::frame::{ColorInfo, Matrix};
use fp_media::info::Transfer;
use glam::{EulerRot, Mat4, Vec4};

/// Matches `struct Params` in shaders/scene.wgsl (std140-compatible: every
/// member is 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SceneParams {
    pub inv_view_proj: [Mat4; 2],
    pub correction: Mat4,
    pub screen_inv: Mat4,
    pub mode: [u32; 4],
    pub proj: Vec4,
    pub lens: Vec4,
    pub screen: Vec4,
    pub yuv_r: Vec4,
    pub yuv_g: Vec4,
    pub yuv_b: Vec4,
    pub yuv_offset: Vec4,
    pub picture: Vec4,
    pub extra: Vec4,
    pub tex_size: Vec4,
    pub bg_color: Vec4,
    pub key: Vec4,
    pub key2: Vec4,
}

/// Everything about the video that the projection pass needs, other than
/// the eye matrices.
#[derive(Clone, Copy, Debug)]
pub struct VideoParams {
    pub format: VideoFormat,
    pub settings: ViewSettings,
    /// Flat screens: where the screen's centre sits in the world (the screen
    /// faces +Z of this transform, i.e. towards a viewer looking down -Z).
    pub screen_pose: Mat4,
    /// Background around/behind the video, linear premultiplied RGBA.
    /// Alpha 0 lets passthrough show through.
    pub background: [f32; 4],
}

impl Default for VideoParams {
    fn default() -> Self {
        VideoParams {
            format: VideoFormat::default(),
            settings: ViewSettings::default(),
            screen_pose: Mat4::from_translation(glam::vec3(0.0, 0.0, -4.0)),
            background: [0.008, 0.008, 0.012, 1.0],
        }
    }
}

/// Description of the uploaded frame the parameters refer to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameDesc {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub color: ColorInfo,
}

/// YUV → RGB matrix with range expansion folded in, plus offsets.
pub(crate) fn yuv_matrix(color: &ColorInfo, layout: PixelLayout) -> ([Vec4; 3], Vec4) {
    let (kr, kb) = match color.matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let bits = match layout {
        PixelLayout::I420 { bits } => bits,
        PixelLayout::Nv12 => 8,
        PixelLayout::P010 | PixelLayout::DrmPrime => 10,
    };
    let max = ((1u32 << bits) - 1) as f32;
    // Sample scale: values are normalised by the texture format's range.
    let scale = match layout {
        PixelLayout::I420 { bits: 10 } => 65535.0 / 1023.0,
        PixelLayout::P010 => 65535.0 / 65472.0,
        _ => 1.0,
    };
    let (y_off, y_mul, c_off, c_mul) = if color.full_range {
        (0.0, 1.0, (1u32 << (bits - 1)) as f32 / max, 1.0)
    } else {
        let s = (1u32 << (bits - 8)) as f32;
        (
            16.0 * s / max,
            max / (219.0 * s),
            128.0 * s / max,
            max / (224.0 * s),
        )
    };
    // rgb = M * [Y', U, V] with U,V in [-0.5, 0.5].
    let m = [
        [1.0, 0.0, 2.0 * (1.0 - kr)],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0],
    ];
    let rows = m.map(|r| Vec4::new(r[0] * y_mul, r[1] * c_mul, r[2] * c_mul, 0.0));
    (rows, Vec4::new(y_off, c_off, c_off, scale))
}

pub(crate) fn build(
    video: &VideoParams,
    frame: Option<&FrameDesc>,
    inv_view_proj: [Mat4; 2],
) -> SceneParams {
    let s = &video.settings;
    let deg = |d: f32| d.to_radians();
    let (kind, h_fov, v_fov) = match video.format.projection {
        Projection::Flat => (0u32, 0.0, 0.0),
        Projection::Equirect { h_fov, v_fov } => (1, deg(h_fov), deg(v_fov)),
        Projection::Fisheye { fov } => (2, deg(fov), deg(fov)),
        Projection::Eac { h_fov } => (3, deg(h_fov), deg(180.0)),
    };
    let stereo = match video.format.stereo {
        StereoLayout::Mono => 0u32,
        StereoLayout::SideBySide => 1,
        StereoLayout::TopBottom => 2,
    };
    let swap = (video.format.eyes_swapped ^ s.swap_eyes) as u32;
    let (rows, offset, layout_code, tex) = match frame {
        Some(f) => {
            let (rows, off) = yuv_matrix(&f.color, f.layout);
            let lc = match f.layout {
                PixelLayout::I420 { .. } => 0u32,
                _ => 1,
            };
            (rows, off, lc, (f.width.max(1), f.height.max(1)))
        }
        None => (
            [Vec4::X, Vec4::Y, Vec4::Z],
            Vec4::new(0.0, 0.0, 0.0, 1.0),
            0,
            (1, 1),
        ),
    };
    // Per-eye aspect for flat screens.
    let (ew, eh) = match video.format.stereo {
        StereoLayout::SideBySide => (tex.0 as f32 / 2.0, tex.1 as f32),
        StereoLayout::TopBottom => (tex.0 as f32, tex.1 as f32 / 2.0),
        StereoLayout::Mono => (tex.0 as f32, tex.1 as f32),
    };
    let screen_w = s.screen_width * s.zoom;
    let screen_h = screen_w * eh / ew.max(1.0);
    let arc = s.screen_curvature.clamp(0.0, 1.0) * std::f32::consts::TAU / 3.0;
    let correction =
        Mat4::from_euler(EulerRot::YXZ, deg(s.yaw), deg(s.pitch), deg(s.roll)).inverse();
    let kc = chroma(s.key_color);
    let (transfer, wide) = match frame {
        Some(f) => (
            match f.color.transfer {
                Transfer::Sdr => 0.0,
                Transfer::Pq => 1.0,
                Transfer::Hlg => 2.0,
            },
            f.color.wide_gamut as u32 as f32,
        ),
        None => (0.0, 0.0),
    };
    SceneParams {
        inv_view_proj,
        correction,
        screen_inv: video.screen_pose.inverse(),
        mode: [kind, stereo, swap, layout_code],
        proj: Vec4::new(h_fov, v_fov, s.zoom.max(0.05), deg(s.ipd_offset)),
        lens: Vec4::new(
            s.lens_k1,
            s.lens_k2,
            deg(s.vertical_align),
            deg(s.rotation_align),
        ),
        screen: Vec4::new(screen_w, screen_h, arc, frame.is_some() as u32 as f32),
        yuv_r: rows[0],
        yuv_g: rows[1],
        yuv_b: rows[2],
        yuv_offset: offset,
        picture: Vec4::new(s.brightness, s.contrast, s.saturation, s.gamma),
        extra: Vec4::new(s.sharpen, transfer, wide, video.background[3]),
        tex_size: Vec4::new(
            tex.0 as f32,
            tex.1 as f32,
            1.0 / tex.0 as f32,
            1.0 / tex.1 as f32,
        ),
        bg_color: Vec4::from(video.background),
        key: Vec4::new(kc.x, kc.y, s.key_similarity, s.key_smoothness),
        key2: Vec4::new(
            s.key_spill,
            s.chroma_key as u32 as f32,
            video.format.alpha_pack_scale().unwrap_or(0.0),
            0.0,
        ),
    }
}

/// Chroma (BT.709 Cb, Cr) of an sRGB-encoded colour, as the shader computes it
/// from the video's samples.
pub(crate) fn chroma(rgb: [f32; 3]) -> glam::Vec2 {
    let [r, g, b] = rgb;
    glam::Vec2::new(
        -0.1146 * r - 0.3854 * g + 0.5 * b,
        0.5 * r - 0.4542 * g - 0.0458 * b,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(rows: &[Vec4; 3], off: Vec4, yuv: [f32; 3]) -> [f32; 3] {
        let v = glam::Vec3::new(yuv[0], yuv[1], yuv[2]) * off.w - off.truncate();
        [
            rows[0].truncate().dot(v),
            rows[1].truncate().dot(v),
            rows[2].truncate().dot(v),
        ]
    }

    #[test]
    fn limited_range_bt709_white_black_red() {
        let c = ColorInfo {
            matrix: Matrix::Bt709,
            full_range: false,
            transfer: Transfer::Sdr,
            wide_gamut: false,
        };
        let (rows, off) = yuv_matrix(&c, PixelLayout::I420 { bits: 8 });
        let near = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.01);
        assert!(near(
            apply(&rows, off, [16.0 / 255.0, 0.5019, 0.5019]),
            [0.0, 0.0, 0.0]
        ));
        assert!(near(
            apply(&rows, off, [235.0 / 255.0, 0.5019, 0.5019]),
            [1.0, 1.0, 1.0]
        ));
        // BT.709 limited red: Y 63, Cb 102, Cr 240.
        assert!(near(
            apply(&rows, off, [63.0 / 255.0, 102.0 / 255.0, 240.0 / 255.0]),
            [1.0, 0.0, 0.0]
        ));
    }

    #[test]
    fn ten_bit_low_bits_scale() {
        let c = ColorInfo {
            matrix: Matrix::Bt2020,
            full_range: false,
            transfer: Transfer::Pq,
            wide_gamut: true,
        };
        let (rows, off) = yuv_matrix(&c, PixelLayout::I420 { bits: 10 });
        // 10-bit white (940) stored in the low bits of a 16-bit texel.
        let s = 1.0 / 65535.0;
        let out = apply(&rows, off, [940.0 * s, 512.0 * s, 512.0 * s]);
        assert!(out.iter().all(|v| (v - 1.0).abs() < 0.01), "{out:?}");
    }
}
