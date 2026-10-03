//! CPU colour conversion and scaling for software-decoded frames
//! (thumbnails, previews). The real-time path converts on the GPU.

use super::{CpuFrame, PixelFormat};
use crate::error::{Result, VideoError};

/// 8-bit RGBA image, tightly packed.
#[derive(Debug, Clone, PartialEq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl RgbaImage {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        self.data[i..i + 4].try_into().unwrap()
    }

    /// Sub-rectangle (clamped to the image).
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> RgbaImage {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let w = w.clamp(1, self.width - x);
        let h = h.clamp(1, self.height - y);
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let o = ((row * self.width + x) * 4) as usize;
            data.extend_from_slice(&self.data[o..o + (w * 4) as usize]);
        }
        RgbaImage {
            width: w,
            height: h,
            data,
        }
    }

    /// Area-averaging downscale so the image fits in `max_w × max_h`
    /// (aspect preserved; never upscales).
    pub fn fit_within(&self, max_w: u32, max_h: u32) -> RgbaImage {
        let scale = (max_w as f64 / self.width as f64)
            .min(max_h as f64 / self.height as f64)
            .min(1.0);
        let w = ((self.width as f64 * scale).round() as u32).max(1);
        let h = ((self.height as f64 * scale).round() as u32).max(1);
        self.resize_box(w, h)
    }

    /// Box-filter resize to exactly `w × h`.
    pub fn resize_box(&self, w: u32, h: u32) -> RgbaImage {
        if w == self.width && h == self.height {
            return self.clone();
        }
        let mut out = vec![0u8; (w * h * 4) as usize];
        let sx = self.width as f64 / w as f64;
        let sy = self.height as f64 / h as f64;
        for oy in 0..h {
            let y0 = (oy as f64 * sy).floor() as u32;
            let y1 = (((oy + 1) as f64 * sy).ceil() as u32).clamp(y0 + 1, self.height);
            for ox in 0..w {
                let x0 = (ox as f64 * sx).floor() as u32;
                let x1 = (((ox + 1) as f64 * sx).ceil() as u32).clamp(x0 + 1, self.width);
                let mut acc = [0u32; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = self.pixel(x, y);
                        for c in 0..4 {
                            acc[c] += p[c] as u32;
                        }
                    }
                }
                let n = (y1 - y0) * (x1 - x0);
                let o = ((oy * w + ox) * 4) as usize;
                for c in 0..4 {
                    out[o + c] = ((acc[c] + n / 2) / n) as u8;
                }
            }
        }
        RgbaImage {
            width: w,
            height: h,
            data: out,
        }
    }
}

/// Matrix coefficients for YCbCr → RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl YuvMatrix {
    /// Usual default by resolution / bit depth.
    pub fn guess(width: u32, height: u32, bit_depth: u8) -> Self {
        if bit_depth > 8 {
            YuvMatrix::Bt2020
        } else if width >= 1280 || height >= 720 {
            YuvMatrix::Bt709
        } else {
            YuvMatrix::Bt601
        }
    }

    fn kr_kb(self) -> (f32, f32) {
        match self {
            YuvMatrix::Bt601 => (0.299, 0.114),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
            YuvMatrix::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// Limited-range YCbCr (8-bit scale) → RGB.
fn yuv_to_rgb(y: f32, u: f32, v: f32, m: YuvMatrix) -> [u8; 3] {
    let (kr, kb) = m.kr_kb();
    let kg = 1.0 - kr - kb;
    let yn = (y - 16.0) / 219.0;
    let pb = (u - 128.0) / 224.0;
    let pr = (v - 128.0) / 224.0;
    let r = yn + 2.0 * (1.0 - kr) * pr;
    let b = yn + 2.0 * (1.0 - kb) * pb;
    let g = (yn - kr * r - kb * b) / kg;
    let q = |c: f32| (c * 255.0).round().clamp(0.0, 255.0) as u8;
    [q(r), q(g), q(b)]
}

/// Convert a software-decoded frame to RGBA (nearest chroma sampling).
pub fn cpu_frame_to_rgba(f: &CpuFrame, matrix: YuvMatrix) -> Result<RgbaImage> {
    let (w, h) = (f.width as usize, f.height as usize);
    let need = match f.format {
        PixelFormat::Nv12 | PixelFormat::P010 => 2,
        PixelFormat::I420 | PixelFormat::I420P10 => 3,
    };
    if f.planes.len() < need || f.strides.len() < need {
        return Err(VideoError::invalid("frame is missing planes"));
    }
    let s16 = |p: &[u8], i: usize| u16::from_le_bytes([p[2 * i], p[2 * i + 1]]);
    let mut out = vec![0u8; w * h * 4];
    for yy in 0..h {
        for xx in 0..w {
            let (cy, cx) = (yy / 2, xx / 2);
            let (y, u, v) = match f.format {
                PixelFormat::Nv12 => {
                    let y = f.planes[0][yy * f.strides[0] + xx] as f32;
                    let o = cy * f.strides[1] + cx * 2;
                    (y, f.planes[1][o] as f32, f.planes[1][o + 1] as f32)
                }
                PixelFormat::I420 => (
                    f.planes[0][yy * f.strides[0] + xx] as f32,
                    f.planes[1][cy * f.strides[1] + cx] as f32,
                    f.planes[2][cy * f.strides[2] + cx] as f32,
                ),
                PixelFormat::P010 => {
                    let y = s16(&f.planes[0][yy * f.strides[0]..], xx) >> 6;
                    let row = &f.planes[1][cy * f.strides[1]..];
                    let u = s16(row, cx * 2) >> 6;
                    let v = s16(row, cx * 2 + 1) >> 6;
                    (y as f32 / 4.0, u as f32 / 4.0, v as f32 / 4.0)
                }
                PixelFormat::I420P10 => {
                    let y = s16(&f.planes[0][yy * f.strides[0]..], xx);
                    let u = s16(&f.planes[1][cy * f.strides[1]..], cx);
                    let v = s16(&f.planes[2][cy * f.strides[2]..], cx);
                    (y as f32 / 4.0, u as f32 / 4.0, v as f32 / 4.0)
                }
            };
            let rgb = yuv_to_rgb(y, u, v, matrix);
            let o = (yy * w + xx) * 4;
            out[o..o + 3].copy_from_slice(&rgb);
            out[o + 3] = 255;
        }
    }
    Ok(RgbaImage {
        width: f.width,
        height: f.height,
        data: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::MediaTime;

    pub fn solid_nv12(w: u32, h: u32, y: u8, u: u8, v: u8) -> CpuFrame {
        let (wu, hu) = (w as usize, h as usize);
        let mut uv = vec![0u8; wu * hu / 2];
        for c in uv.chunks_mut(2) {
            c[0] = u;
            c[1] = v;
        }
        CpuFrame {
            format: PixelFormat::Nv12,
            width: w,
            height: h,
            planes: vec![vec![y; wu * hu], uv],
            strides: vec![wu, wu],
            pts: MediaTime::ZERO,
        }
    }

    #[test]
    fn limited_range_white_black_red() {
        let img = cpu_frame_to_rgba(&solid_nv12(4, 4, 235, 128, 128), YuvMatrix::Bt709).unwrap();
        assert_eq!(img.pixel(1, 1), [255, 255, 255, 255]);
        let img = cpu_frame_to_rgba(&solid_nv12(4, 4, 16, 128, 128), YuvMatrix::Bt709).unwrap();
        assert_eq!(img.pixel(3, 3), [0, 0, 0, 255]);
        // BT.709 red: Y=63, Cb=102, Cr=240.
        let img = cpu_frame_to_rgba(&solid_nv12(2, 2, 63, 102, 240), YuvMatrix::Bt709).unwrap();
        let p = img.pixel(0, 0);
        assert!(p[0] > 250 && p[1] < 5 && p[2] < 5, "{p:?}");
    }

    #[test]
    fn p010_and_i420p10_match_nv12() {
        let w = 2usize;
        let y10 = (180u16 << 2) << 6;
        let u10 = (100u16 << 2) << 6;
        let v10 = (150u16 << 2) << 6;
        let p010 = CpuFrame {
            format: PixelFormat::P010,
            width: 2,
            height: 2,
            planes: vec![
                y10.to_le_bytes().repeat(4),
                [u10.to_le_bytes(), v10.to_le_bytes()].concat(),
            ],
            strides: vec![w * 2, w * 2],
            pts: MediaTime::ZERO,
        };
        let ref8 = cpu_frame_to_rgba(&solid_nv12(2, 2, 180, 100, 150), YuvMatrix::Bt709).unwrap();
        assert_eq!(cpu_frame_to_rgba(&p010, YuvMatrix::Bt709).unwrap(), ref8);
        let i10 = CpuFrame {
            format: PixelFormat::I420P10,
            width: 2,
            height: 2,
            planes: vec![
                (180u16 << 2).to_le_bytes().repeat(4),
                (100u16 << 2).to_le_bytes().to_vec(),
                (150u16 << 2).to_le_bytes().to_vec(),
            ],
            strides: vec![4, 2, 2],
            pts: MediaTime::ZERO,
        };
        assert_eq!(cpu_frame_to_rgba(&i10, YuvMatrix::Bt709).unwrap(), ref8);
    }

    #[test]
    fn box_downscale() {
        let mut data = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                let v = if (x < 2) == (y < 2) { 200 } else { 0 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let img = RgbaImage {
            width: 4,
            height: 4,
            data,
        };
        let small = img.resize_box(2, 2);
        assert_eq!(small.pixel(0, 0), [200, 200, 200, 255]);
        assert_eq!(small.pixel(1, 0), [0, 0, 0, 255]);
        let fit = img.fit_within(100, 1);
        assert_eq!((fit.width, fit.height), (1, 1));
        assert_eq!(fit.pixel(0, 0)[0], 100);
    }
}
