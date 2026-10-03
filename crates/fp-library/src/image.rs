//! Minimal RGBA image handling for thumbnails: validation, area-average
//! downscaling, tiling and JPEG encoding (pure Rust).

use crate::error::{Error, Result};

/// An 8-bit RGBA image, rows top to bottom, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

/// Largest side a baseline JPEG can have.
const JPEG_MAX_SIDE: u32 = u16::MAX as u32;

impl RgbaImage {
    /// An image filled with one colour.
    pub fn filled(width: u32, height: u32, rgba: [u8; 4]) -> RgbaImage {
        let n = width as usize * height as usize;
        let mut pixels = Vec::with_capacity(n * 4);
        for _ in 0..n {
            pixels.extend_from_slice(&rgba);
        }
        RgbaImage {
            width,
            height,
            pixels,
        }
    }

    /// Checks that the buffer matches the dimensions and is not empty.
    pub fn validate(&self) -> Result<()> {
        let expected = self.width as usize * self.height as usize * 4;
        if self.width == 0 || self.height == 0 || self.pixels.len() != expected {
            return Err(Error::InvalidArgument(format!(
                "image {}x{} has {} bytes, expected {expected} (and non-zero size)",
                self.width,
                self.height,
                self.pixels.len()
            )));
        }
        Ok(())
    }

    /// Pixel at `(x, y)`.
    fn px(&self, x: u32, y: u32) -> &[u8] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        &self.pixels[i..i + 4]
    }

    /// Scales down (never up) to fit in `max_width` × `max_height`, keeping
    /// the aspect ratio, by averaging the source pixels under each target
    /// pixel. Fails when the buffer does not match the dimensions (see
    /// [`RgbaImage::validate`]).
    pub fn fit_within(&self, max_width: u32, max_height: u32) -> Result<RgbaImage> {
        self.validate()?;
        let (w, h) = fit_size(self.width, self.height, max_width, max_height);
        if (w, h) == (self.width, self.height) {
            return Ok(self.clone());
        }
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for ty in 0..h {
            let y0 = (ty as u64 * self.height as u64 / h as u64) as u32;
            let y1 = (((ty as u64 + 1) * self.height as u64).div_ceil(h as u64) as u32)
                .clamp(y0 + 1, self.height);
            for tx in 0..w {
                let x0 = (tx as u64 * self.width as u64 / w as u64) as u32;
                let x1 = (((tx as u64 + 1) * self.width as u64).div_ceil(w as u64) as u32)
                    .clamp(x0 + 1, self.width);
                let mut sum = [0u64; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        for (s, v) in sum.iter_mut().zip(self.px(x, y)) {
                            *s += *v as u64;
                        }
                    }
                }
                let n = ((y1 - y0) as u64 * (x1 - x0) as u64).max(1);
                out.extend(sum.iter().map(|s| ((s + n / 2) / n) as u8));
            }
        }
        Ok(RgbaImage {
            width: w,
            height: h,
            pixels: out,
        })
    }

    /// Copies `src` into this image with its top-left corner at `(x, y)`,
    /// clipping at the edges. Both images must be valid (callers validate
    /// frames before tiling them).
    pub(crate) fn blit(&mut self, src: &RgbaImage, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let w = src.width.min(self.width - x) as usize;
        let h = src.height.min(self.height - y);
        for row in 0..h {
            let s = (row as usize * src.width as usize) * 4;
            let d = ((y + row) as usize * self.width as usize + x as usize) * 4;
            self.pixels[d..d + w * 4].copy_from_slice(&src.pixels[s..s + w * 4]);
        }
    }

    /// Encodes as a baseline JPEG.
    pub fn to_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        self.validate()?;
        if self.width > JPEG_MAX_SIDE || self.height > JPEG_MAX_SIDE {
            return Err(Error::Jpeg(format!(
                "{}x{} exceeds the JPEG limit of {JPEG_MAX_SIDE}",
                self.width, self.height
            )));
        }
        let mut buf = Vec::new();
        jpeg_encoder::Encoder::new(&mut buf, quality.clamp(1, 100))
            .encode(
                &self.pixels,
                self.width as u16,
                self.height as u16,
                jpeg_encoder::ColorType::Rgba,
            )
            .map_err(|e| Error::Jpeg(e.to_string()))?;
        Ok(buf)
    }
}

/// Largest size with the aspect ratio of `w` × `h` that fits the box, never
/// larger than the original and at least 1×1.
pub(crate) fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let max_w = max_w.max(1);
    let max_h = max_h.max(1);
    if w <= max_w && h <= max_h {
        return (w.max(1), h.max(1));
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = ((w as f64 * scale).round() as u32).clamp(1, max_w);
    let nh = ((h as f64 * scale).round() as u32).clamp(1, max_h);
    (nw, nh)
}

/// Lays frames out in a grid of `columns` cells sized to the largest frame,
/// each frame centred on black. Returns the sheet and the cell size.
pub(crate) fn tile(frames: &[RgbaImage], columns: u32) -> Result<(RgbaImage, u32, u32)> {
    if frames.is_empty() {
        return Err(Error::InvalidArgument("no frames to tile".into()));
    }
    let columns = columns.clamp(1, frames.len() as u32);
    let rows = (frames.len() as u32).div_ceil(columns);
    let cell_w = frames.iter().map(|f| f.width).max().unwrap_or(1).max(1);
    let cell_h = frames.iter().map(|f| f.height).max().unwrap_or(1).max(1);
    let mut sheet = RgbaImage::filled(cell_w * columns, cell_h * rows, [0, 0, 0, 255]);
    for (i, f) in frames.iter().enumerate() {
        let i = i as u32;
        let x = (i % columns) * cell_w + (cell_w - f.width) / 2;
        let y = (i / columns) * cell_h + (cell_h - f.height) / 2;
        sheet.blit(f, x, y);
    }
    Ok((sheet, cell_w, cell_h))
}

/// Width and height of a JPEG from its first start-of-frame marker.
#[cfg(test)]
pub(crate) fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.get(..2) != Some(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if (0xC0..=0xC2).contains(&marker) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_sizes() {
        assert_eq!(fit_size(3840, 2160, 640, 640), (640, 360));
        assert_eq!(fit_size(2880, 2880, 640, 360), (360, 360));
        assert_eq!(fit_size(100, 50, 640, 640), (100, 50));
        assert_eq!(fit_size(10000, 1, 100, 100), (100, 1));
    }

    #[test]
    fn downscale_averages() {
        // Left half white, right half black: 4x2 -> 2x1 gives white, black.
        let mut img = RgbaImage::filled(4, 2, [0, 0, 0, 255]);
        for y in 0..2 {
            for x in 0..2 {
                let i = (y * 4 + x) * 4;
                img.pixels[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let small = img.fit_within(2, 2).unwrap();
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(small.pixels, vec![255, 255, 255, 255, 0, 0, 0, 255]);
        // Non-integer ratio still covers every source pixel.
        let odd = RgbaImage::filled(7, 5, [10, 20, 30, 255])
            .fit_within(3, 3)
            .unwrap();
        assert_eq!((odd.width, odd.height), (3, 2));
        assert!(odd.pixels.chunks(4).all(|p| p == [10, 20, 30, 255]));
    }

    #[test]
    fn tiles_and_encodes() {
        let frames = vec![
            RgbaImage::filled(10, 6, [255, 0, 0, 255]),
            RgbaImage::filled(8, 6, [0, 255, 0, 255]),
            RgbaImage::filled(10, 4, [0, 0, 255, 255]),
        ];
        let (sheet, cw, ch) = tile(&frames, 2).unwrap();
        assert_eq!((sheet.width, sheet.height, cw, ch), (20, 12, 10, 6));
        // Second frame is centred: column 0 of cell 1 is black padding.
        assert_eq!(&sheet.pixels[10 * 4..10 * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(&sheet.pixels[11 * 4..11 * 4 + 4], &[0, 255, 0, 255]);
        let jpg = sheet.to_jpeg(80).unwrap();
        assert_eq!(jpeg_size(&jpg), Some((20, 12)));
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9]);
        assert!(tile(&[], 1).is_err());
    }

    #[test]
    fn invalid_images_are_rejected() {
        let bad = RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![0; 3],
        };
        assert!(bad.validate().is_err());
        assert!(bad.to_jpeg(80).is_err());
        assert!(bad.fit_within(1, 1).is_err());
        assert!(RgbaImage::filled(0, 5, [0; 4]).validate().is_err());
    }
}
