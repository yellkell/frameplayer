//! Decoded video frames in layouts the renderer can upload directly.

use crate::info::Transfer;
use crate::{Error, Result};
use fp_ffmpeg_sys as ff;

/// Memory layout of a decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Three planes Y, U, V at 4:2:0. `bits` is 8 (one byte per sample) or 10
    /// (two bytes, value in the low bits).
    I420 { bits: u32 },
    /// Y plane plus interleaved UV plane, 8 bits.
    Nv12,
    /// Y plane plus interleaved UV plane, 16 bits with 10 significant high bits.
    P010,
    /// GPU buffer shared as DMA-BUF file descriptors (zero copy).
    DrmPrime,
}

impl PixelLayout {
    pub fn bytes_per_sample(&self) -> u32 {
        match self {
            PixelLayout::I420 { bits: 8 } | PixelLayout::Nv12 => 1,
            _ => 2,
        }
    }
}

/// YUV to RGB matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorInfo {
    pub matrix: Matrix,
    pub full_range: bool,
    pub transfer: Transfer,
    /// BT.2020 primaries (needs gamut mapping to the display's BT.709).
    pub wide_gamut: bool,
}

/// One plane of a frame.
pub struct Plane<'a> {
    pub data: &'a [u8],
    /// Bytes per row.
    pub stride: usize,
    /// Size in samples (a UV pair counts once).
    pub width: u32,
    pub height: u32,
    pub bytes_per_sample: u32,
    /// 1 for Y/U/V planes, 2 for interleaved UV.
    pub components: u32,
}

/// A decoded frame. Holds a reference to FFmpeg's buffer, released on drop.
pub struct VideoFrame {
    /// Presentation time in seconds.
    pub pts: f64,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    pub color: ColorInfo,
    /// Seek generation this frame belongs to.
    pub serial: u64,
    /// Unique per decoded frame in this process, for upload caching.
    pub id: u64,
    frame: *mut ff::AVFrame,
}

static NEXT_FRAME_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_FRAME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// SAFETY: the AVFrame is immutable after construction and only freed on drop;
// FFmpeg frame buffers are reference counted and thread safe.
unsafe impl Send for VideoFrame {}
unsafe impl Sync for VideoFrame {}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        // SAFETY: we own this reference.
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

impl std::fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VideoFrame({:.3}s {}x{} {:?})",
            self.pts, self.width, self.height, self.layout
        )
    }
}

impl VideoFrame {
    pub fn planes(&self) -> Vec<Plane<'_>> {
        let (n, chroma_components) = match self.layout {
            PixelLayout::I420 { .. } => (3, 1),
            PixelLayout::Nv12 | PixelLayout::P010 => (2, 2),
            PixelLayout::DrmPrime => return Vec::new(),
        };
        let bps = self.layout.bytes_per_sample();
        // SAFETY: frame is a valid decoded frame of this layout; plane sizes
        // follow from the 4:2:0 subsampling.
        unsafe {
            let f = &*self.frame;
            (0..n)
                .map(|i| {
                    let (w, h, comps) = if i == 0 {
                        (self.width, self.height, 1)
                    } else {
                        (
                            self.width.div_ceil(2),
                            self.height.div_ceil(2),
                            chroma_components,
                        )
                    };
                    let stride = f.linesize[i].unsigned_abs() as usize;
                    let len = stride * (h as usize - 1) + (w * comps * bps) as usize;
                    Plane {
                        data: std::slice::from_raw_parts(f.data[i], len),
                        stride,
                        width: w,
                        height: h,
                        bytes_per_sample: bps,
                        components: comps,
                    }
                })
                .collect()
        }
    }

    /// Builds a frame from tightly packed RGBA pixels (photos decoded in Rust,
    /// test images). Converted to full-range BT.709 I420.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<VideoFrame> {
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return Err(Error::Unsupported(
                "RGBA buffer does not match its size".into(),
            ));
        }
        let (w, h) = (width as i32, height as i32);
        // SAFETY: a fresh frame is allocated with av_frame_get_buffer and
        // filled by swscale from the caller's buffer, which holds w*h*4 bytes.
        unsafe {
            let ctx = ff::sws_getContext(
                w,
                h,
                ff::AV_PIX_FMT_RGBA,
                w,
                h,
                ff::AV_PIX_FMT_YUV420P,
                (ff::SWS_BICUBIC | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INP) as i32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if ctx.is_null() {
                return Err(Error::Unsupported("swscale RGBA -> YUV".into()));
            }
            let coeffs = ff::sws_getCoefficients(ff::SWS_CS_ITU709 as i32);
            ff::sws_setColorspaceDetails(ctx, coeffs, 1, coeffs, 1, 0, 1 << 16, 1 << 16);
            let mut dst = ff::av_frame_alloc();
            (*dst).format = ff::AV_PIX_FMT_YUV420P;
            (*dst).width = w;
            (*dst).height = h;
            (*dst).colorspace = ff::AVCOL_SPC_BT709;
            (*dst).color_range = ff::AVCOL_RANGE_JPEG;
            (*dst).color_trc = ff::AVCOL_TRC_BT709;
            (*dst).color_primaries = ff::AVCOL_PRI_BT709;
            if ff::av_frame_get_buffer(dst, 0) < 0 {
                ff::av_frame_free(&mut dst);
                ff::sws_freeContext(ctx);
                return Err(Error::Unsupported("frame allocation failed".into()));
            }
            let src = [
                rgba.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            ];
            let stride = [w * 4, 0, 0, 0];
            ff::sws_scale(
                ctx,
                src.as_ptr(),
                stride.as_ptr(),
                0,
                h,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
            ff::sws_freeContext(ctx);
            let color = color_info(&*dst);
            Ok(VideoFrame {
                pts: 0.0,
                duration: 0.0,
                width,
                height,
                layout: PixelLayout::I420 { bits: 8 },
                color,
                serial: 0,
                id: next_id(),
                frame: dst,
            })
        }
    }

    /// DMA-BUF description for zero-copy import, when the layout is DrmPrime.
    pub fn drm_descriptor(&self) -> Option<&ff::AVDRMFrameDescriptor> {
        if self.layout != PixelLayout::DrmPrime {
            return None;
        }
        // SAFETY: for DRM_PRIME frames data[0] points at the descriptor.
        unsafe { ((*self.frame).data[0] as *const ff::AVDRMFrameDescriptor).as_ref() }
    }

    /// Copies the frame into tightly packed planes (for tests and thumbnails).
    pub fn packed_planes(&self) -> Vec<Vec<u8>> {
        self.planes()
            .iter()
            .map(|p| {
                let row = (p.width * p.components * p.bytes_per_sample) as usize;
                let mut out = Vec::with_capacity(row * p.height as usize);
                for y in 0..p.height as usize {
                    out.extend_from_slice(&p.data[y * p.stride..y * p.stride + row]);
                }
                out
            })
            .collect()
    }
}

fn color_info(f: &ff::AVFrame) -> ColorInfo {
    let matrix = match f.colorspace {
        ff::AVCOL_SPC_BT709 => Matrix::Bt709,
        ff::AVCOL_SPC_BT2020_NCL | ff::AVCOL_SPC_BT2020_CL => Matrix::Bt2020,
        ff::AVCOL_SPC_BT470BG | ff::AVCOL_SPC_SMPTE170M => Matrix::Bt601,
        // Unspecified: HD and larger is BT.709 by convention.
        _ => {
            if f.height >= 720 {
                Matrix::Bt709
            } else {
                Matrix::Bt601
            }
        }
    };
    let transfer = match f.color_trc {
        ff::AVCOL_TRC_SMPTE2084 => Transfer::Pq,
        ff::AVCOL_TRC_ARIB_STD_B67 => Transfer::Hlg,
        _ => Transfer::Sdr,
    };
    ColorInfo {
        matrix,
        full_range: f.color_range == ff::AVCOL_RANGE_JPEG || f.format == ff::AV_PIX_FMT_YUVJ420P,
        transfer,
        wide_gamut: f.color_primaries == ff::AVCOL_PRI_BT2020,
    }
}

/// Converts decoder output into a [`VideoFrame`], rescaling unusual pixel
/// formats with swscale.
pub(crate) struct FrameConverter {
    sws: *mut ff::SwsContext,
    sws_key: (i32, i32, i32, i32),
}

// SAFETY: used by one decode thread at a time.
unsafe impl Send for FrameConverter {}

impl Default for FrameConverter {
    fn default() -> Self {
        FrameConverter {
            sws: std::ptr::null_mut(),
            sws_key: (0, 0, 0, 0),
        }
    }
}

impl Drop for FrameConverter {
    fn drop(&mut self) {
        // SAFETY: frees our own context (null-safe).
        unsafe { ff::sws_freeContext(self.sws) };
    }
}

impl FrameConverter {
    /// Takes ownership of `src` (a decoded frame reference).
    pub(crate) fn convert(
        &mut self,
        src: *mut ff::AVFrame,
        pts: f64,
        duration: f64,
        serial: u64,
    ) -> Result<VideoFrame> {
        // SAFETY: src is a valid decoded frame we own.
        let f = unsafe { &*src };
        let layout = match f.format {
            ff::AV_PIX_FMT_YUV420P | ff::AV_PIX_FMT_YUVJ420P => Some(PixelLayout::I420 { bits: 8 }),
            ff::AV_PIX_FMT_YUV420P10LE => Some(PixelLayout::I420 { bits: 10 }),
            ff::AV_PIX_FMT_NV12 => Some(PixelLayout::Nv12),
            ff::AV_PIX_FMT_P010LE => Some(PixelLayout::P010),
            ff::AV_PIX_FMT_DRM_PRIME => Some(PixelLayout::DrmPrime),
            _ => None,
        };
        let color = color_info(f);
        let (width, height) = (f.width.max(0) as u32, f.height.max(0) as u32);
        if let Some(layout) = layout {
            return Ok(VideoFrame {
                pts,
                duration,
                width,
                height,
                layout,
                color,
                serial,
                id: next_id(),
                frame: src,
            });
        }
        // Anything else: convert to 8- or 10-bit I420.
        // SAFETY: pixdesc lookups and swscale on valid frames; the new frame is
        // allocated with av_frame_get_buffer before scaling into it.
        unsafe {
            let desc = ff::av_pix_fmt_desc_get(f.format);
            let deep = !desc.is_null() && (*desc).comp[0].depth > 8;
            let (dst_fmt, bits) = if deep {
                (ff::AV_PIX_FMT_YUV420P10LE, 10)
            } else {
                (ff::AV_PIX_FMT_YUV420P, 8)
            };
            let key = (f.width, f.height, f.format, dst_fmt);
            if self.sws.is_null() || self.sws_key != key {
                ff::sws_freeContext(self.sws);
                self.sws = ff::sws_getContext(
                    f.width,
                    f.height,
                    f.format,
                    f.width,
                    f.height,
                    dst_fmt,
                    // The fast paths produce wrong chroma in some builds;
                    // accurate rounding with full chroma input is exact.
                    (ff::SWS_BILINEAR | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INP) as i32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                self.sws_key = key;
                if self.sws.is_null() {
                    let mut s = src;
                    ff::av_frame_free(&mut s);
                    return Err(Error::Unsupported(format!("pixel format {}", f.format)));
                }
            }
            let mut dst = ff::av_frame_alloc();
            (*dst).format = dst_fmt;
            (*dst).width = f.width;
            (*dst).height = f.height;
            (*dst).colorspace = f.colorspace;
            (*dst).color_range = f.color_range;
            (*dst).color_trc = f.color_trc;
            (*dst).color_primaries = f.color_primaries;
            let r = ff::av_frame_get_buffer(dst, 0);
            if r < 0 {
                ff::av_frame_free(&mut dst);
                let mut s = src;
                ff::av_frame_free(&mut s);
                return Err(Error::Unsupported("frame allocation failed".into()));
            }
            ff::sws_scale(
                self.sws,
                f.data.as_ptr() as *const *const u8,
                f.linesize.as_ptr(),
                0,
                f.height,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
            let mut s = src;
            ff::av_frame_free(&mut s);
            Ok(VideoFrame {
                pts,
                duration,
                width,
                height,
                layout: PixelLayout::I420 { bits },
                color,
                serial,
                id: next_id(),
                frame: dst,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_rgba_grey_has_neutral_chroma() {
        let rgba: Vec<u8> = (0..64 * 32).flat_map(|_| [128u8, 128, 128, 255]).collect();
        let f = VideoFrame::from_rgba(64, 32, &rgba).unwrap();
        let p = f.packed_planes();
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].len(), p[1].len(), p[2].len()),
            (64 * 32, 32 * 16, 32 * 16)
        );
        let avg = |v: &[u8]| v.iter().map(|&x| x as u32).sum::<u32>() / v.len() as u32;
        assert!((avg(&p[0]) as i32 - 128).abs() <= 2, "Y {}", avg(&p[0]));
        assert!((avg(&p[1]) as i32 - 128).abs() <= 2, "U {}", avg(&p[1]));
        assert!((avg(&p[2]) as i32 - 128).abs() <= 2, "V {}", avg(&p[2]));
        assert!(f.color.full_range);
        // Saturated colours survive the round trip's chroma.
        let red: Vec<u8> = (0..16 * 16).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let r = VideoFrame::from_rgba(16, 16, &red).unwrap().packed_planes();
        // BT.709 full range red: Y ~54, Cb ~99, Cr 255.
        assert!(
            (r[0][0] as i32 - 54).abs() <= 3 && (r[1][0] as i32 - 99).abs() <= 3 && r[2][0] >= 250,
            "{} {} {}",
            r[0][0],
            r[1][0],
            r[2][0]
        );
    }
}
