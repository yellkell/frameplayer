//! Decoded frames (fp-video) → renderer frame descriptors (fp-gfx).
//!
//! The two crates deliberately don't know each other. Hardware frames map
//! one-to-one (DMA-BUF fds are borrowed, the decoder's buffer id keys the
//! renderer's import cache). Software frames in NV12/P010 are borrowed as
//! is; planar I420 / I420P10 output (dav1d, libavcodec) is interleaved into
//! a reusable NV12 / P010 scratch buffer, which is cheaper than a second GPU
//! upload path.
//!
//! Colour metadata does not travel with frames; it comes from the track
//! description ([`color_for_track`]).

use fp_core::{ColorTransfer, VideoTrackInfo};
use fp_gfx::{ColorInfo, ColorRange, Primaries, YuvMatrix};
use fp_video::decode::drm;
use fp_video::DecodedFrame;

/// Why a decoded frame cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    #[error("unsupported DMA-BUF format {0:#010x}")]
    UnsupportedFourcc(u32),
    #[error("DMA-BUF frame has {0} planes, need 2")]
    PlaneCount(usize),
    #[error("CPU frame is missing planes")]
    MissingPlanes,
    #[error("CPU frame plane too small")]
    ShortPlane,
}

/// Colour interpretation for frames of `track` (or a sane SDR default).
pub fn color_for_track(track: Option<&VideoTrackInfo>, width: u32, height: u32) -> ColorInfo {
    let transfer = track.map(|t| t.transfer).unwrap_or_default();
    let bit_depth = track.map(|t| t.bit_depth).filter(|b| *b > 0).unwrap_or(8);
    let hdr = transfer != ColorTransfer::Sdr;
    let (matrix, primaries) = if hdr || bit_depth > 8 {
        // 10-bit consumer video (and every HDR format) is BT.2020.
        (YuvMatrix::Bt2020, Primaries::Bt2020)
    } else if width >= 1280 || height >= 720 {
        (YuvMatrix::Bt709, Primaries::Bt709)
    } else {
        (YuvMatrix::Bt601, Primaries::Bt709)
    };
    ColorInfo {
        matrix,
        range: ColorRange::Limited,
        transfer,
        primaries,
        bit_depth,
        source_peak_nits: if hdr { 1000.0 } else { 0.0 },
    }
}

/// Converts frames, owning the scratch memory planar formats need.
#[derive(Default)]
pub struct FrameConverter {
    y: Vec<u8>,
    uv: Vec<u8>,
}

impl FrameConverter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Describe `frame` for [`fp_gfx::Renderer::upload_video_frame`].
    pub fn convert<'a>(
        &'a mut self,
        frame: &'a DecodedFrame,
        color: ColorInfo,
    ) -> Result<fp_gfx::VideoFrame<'a>, ConvertError> {
        match frame {
            DecodedFrame::DmaBuf(f) => {
                let format = match f.fourcc {
                    drm::FORMAT_NV12 => fp_gfx::PixelFormat::Nv12,
                    drm::FORMAT_P010 => fp_gfx::PixelFormat::P010,
                    other => return Err(ConvertError::UnsupportedFourcc(other)),
                };
                if f.planes.len() < 2 {
                    return Err(ConvertError::PlaneCount(f.planes.len()));
                }
                let plane = |i: usize| fp_gfx::DmaBufPlane {
                    fd: f.planes[i].fd,
                    offset: f.planes[i].offset as u64,
                    pitch: f.planes[i].pitch as u64,
                };
                Ok(fp_gfx::VideoFrame::DmaBuf(fp_gfx::DmaBufFrame {
                    buffer_id: f.buffer_id,
                    width: f.width,
                    height: f.height,
                    format,
                    modifier: f.modifier,
                    planes: [plane(0), plane(1)],
                    color: ColorInfo {
                        bit_depth: if format == fp_gfx::PixelFormat::P010 {
                            color.bit_depth.max(10)
                        } else {
                            8
                        },
                        ..color
                    },
                }))
            }
            DecodedFrame::Cpu(f) => {
                use fp_video::PixelFormat as P;
                let needed = match f.format {
                    P::Nv12 | P::P010 => 2,
                    P::I420 | P::I420P10 => 3,
                };
                if f.planes.len() < needed || f.strides.len() < needed {
                    return Err(ConvertError::MissingPlanes);
                }
                let (format, planes, strides) = match f.format {
                    P::Nv12 => (
                        fp_gfx::PixelFormat::Nv12,
                        [&f.planes[0][..], &f.planes[1][..]],
                        [f.strides[0], f.strides[1]],
                    ),
                    P::P010 => (
                        fp_gfx::PixelFormat::P010,
                        [&f.planes[0][..], &f.planes[1][..]],
                        [f.strides[0], f.strides[1]],
                    ),
                    P::I420 | P::I420P10 => {
                        let ten = f.format == P::I420P10;
                        planar_to_semi(f, ten, &mut self.y, &mut self.uv)?;
                        let bpc = if ten { 2 } else { 1 };
                        (
                            if ten {
                                fp_gfx::PixelFormat::P010
                            } else {
                                fp_gfx::PixelFormat::Nv12
                            },
                            [&self.y[..], &self.uv[..]],
                            [
                                f.width as usize * bpc,
                                f.width.div_ceil(2) as usize * 2 * bpc,
                            ],
                        )
                    }
                };
                let bit_depth = if format == fp_gfx::PixelFormat::P010 {
                    10
                } else {
                    8
                };
                let out = fp_gfx::CpuFrame {
                    width: f.width,
                    height: f.height,
                    format,
                    planes,
                    strides,
                    color: ColorInfo { bit_depth, ..color },
                };
                out.validate().map_err(|_| ConvertError::ShortPlane)?;
                Ok(fp_gfx::VideoFrame::Cpu(out))
            }
        }
    }
}

/// Interleave planar 4:2:0 into a tightly packed semi-planar layout. For
/// 10-bit input the low-bit-aligned samples are moved to the top bits (P010).
fn planar_to_semi(
    f: &fp_video::CpuFrame,
    ten_bit: bool,
    y_out: &mut Vec<u8>,
    uv_out: &mut Vec<u8>,
) -> Result<(), ConvertError> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bpc = if ten_bit { 2 } else { 1 };
    let row = |plane: usize, r: usize, len: usize| -> Result<&[u8], ConvertError> {
        let start = r * f.strides[plane];
        f.planes[plane]
            .get(start..start + len)
            .ok_or(ConvertError::ShortPlane)
    };
    y_out.clear();
    y_out.reserve(w * h * bpc);
    for r in 0..h {
        let src = row(0, r, w * bpc)?;
        if ten_bit {
            for s in src.chunks_exact(2) {
                let v = u16::from_le_bytes([s[0], s[1]]) << 6;
                y_out.extend_from_slice(&v.to_le_bytes());
            }
        } else {
            y_out.extend_from_slice(src);
        }
    }
    uv_out.clear();
    uv_out.reserve(cw * ch * 2 * bpc);
    for r in 0..ch {
        let u = row(1, r, cw * bpc)?;
        let v = row(2, r, cw * bpc)?;
        for x in 0..cw {
            if ten_bit {
                let uu = u16::from_le_bytes([u[2 * x], u[2 * x + 1]]) << 6;
                let vv = u16::from_le_bytes([v[2 * x], v[2 * x + 1]]) << 6;
                uv_out.extend_from_slice(&uu.to_le_bytes());
                uv_out.extend_from_slice(&vv.to_le_bytes());
            } else {
                uv_out.push(u[x]);
                uv_out.push(v[x]);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::{Codec, MediaTime};
    use fp_video::{CpuFrame, DmaBufFrame, DmaBufPlane, PixelFormat};

    fn track(transfer: ColorTransfer, bits: u8) -> VideoTrackInfo {
        VideoTrackInfo {
            index: 0,
            codec: Codec::Hevc,
            width: 3840,
            height: 2160,
            fps: 30.0,
            bit_depth: bits,
            transfer,
            signalled_projection: None,
            signalled_stereo: None,
        }
    }

    #[test]
    fn colour_from_track() {
        let c = color_for_track(Some(&track(ColorTransfer::Pq, 10)), 3840, 2160);
        assert_eq!(
            (c.matrix, c.primaries, c.bit_depth),
            (YuvMatrix::Bt2020, Primaries::Bt2020, 10)
        );
        assert_eq!(c.transfer, ColorTransfer::Pq);
        let c = color_for_track(Some(&track(ColorTransfer::Sdr, 8)), 1920, 1080);
        assert_eq!(c.matrix, YuvMatrix::Bt709);
        assert_eq!(color_for_track(None, 640, 480).matrix, YuvMatrix::Bt601);
    }

    #[test]
    fn dmabuf_maps_planes_and_id() {
        let f = DecodedFrame::DmaBuf(DmaBufFrame {
            buffer_id: 77,
            planes: vec![
                DmaBufPlane {
                    fd: 5,
                    offset: 0,
                    pitch: 4096,
                },
                DmaBufPlane {
                    fd: 5,
                    offset: 4096 * 2176,
                    pitch: 4096,
                },
            ],
            fourcc: drm::FORMAT_P010,
            modifier: drm::MOD_LINEAR,
            width: 3840,
            height: 2160,
            coded_width: 3840,
            coded_height: 2176,
            pts: MediaTime::ZERO,
            lease: None,
        });
        let mut conv = FrameConverter::new();
        let color = color_for_track(Some(&track(ColorTransfer::Hlg, 10)), 3840, 2160);
        match conv.convert(&f, color).unwrap() {
            fp_gfx::VideoFrame::DmaBuf(d) => {
                assert_eq!(d.buffer_id, 77);
                assert_eq!(d.format, fp_gfx::PixelFormat::P010);
                assert_eq!(d.planes[1].offset, 4096 * 2176);
                assert_eq!(d.planes[1].pitch, 4096);
                assert_eq!(d.planes[0].fd, 5);
                assert_eq!(d.color.transfer, ColorTransfer::Hlg);
                assert_eq!((d.width, d.height), (3840, 2160));
            }
            _ => panic!("expected dmabuf"),
        }
    }

    #[test]
    fn dmabuf_rejects_unknown_format() {
        let f = DecodedFrame::DmaBuf(DmaBufFrame {
            buffer_id: 1,
            planes: vec![DmaBufPlane {
                fd: 3,
                offset: 0,
                pitch: 64,
            }],
            fourcc: drm::fourcc(b'Y', b'U', b'Y', b'V'),
            modifier: 0,
            width: 16,
            height: 16,
            coded_width: 16,
            coded_height: 16,
            pts: MediaTime::ZERO,
            lease: None,
        });
        let mut conv = FrameConverter::new();
        assert!(matches!(
            conv.convert(&f, ColorInfo::SDR_709),
            Err(ConvertError::UnsupportedFourcc(_))
        ));
        if let DecodedFrame::DmaBuf(mut d) = f {
            d.fourcc = drm::FORMAT_NV12;
            let f = DecodedFrame::DmaBuf(d);
            assert_eq!(
                conv.convert(&f, ColorInfo::SDR_709).unwrap_err(),
                ConvertError::PlaneCount(1)
            );
        }
    }

    #[test]
    fn nv12_is_borrowed() {
        let f = DecodedFrame::Cpu(CpuFrame {
            format: PixelFormat::Nv12,
            width: 4,
            height: 2,
            planes: vec![vec![16; 8], vec![128; 4]],
            strides: vec![4, 4],
            pts: MediaTime::ZERO,
        });
        let mut conv = FrameConverter::new();
        match conv.convert(&f, ColorInfo::SDR_709).unwrap() {
            fp_gfx::VideoFrame::Cpu(c) => {
                assert_eq!(c.format, fp_gfx::PixelFormat::Nv12);
                assert_eq!(c.strides, [4, 4]);
                assert_eq!(c.planes[0].len(), 8);
            }
            _ => panic!("expected cpu frame"),
        }
    }

    #[test]
    fn i420_is_interleaved() {
        // 4x2: Y = 0..8, U = [10, 11], V = [20, 21].
        let f = DecodedFrame::Cpu(CpuFrame {
            format: PixelFormat::I420,
            width: 4,
            height: 2,
            planes: vec![(0..8).collect(), vec![10, 11], vec![20, 21]],
            strides: vec![4, 2, 2],
            pts: MediaTime::ZERO,
        });
        let mut conv = FrameConverter::new();
        match conv.convert(&f, ColorInfo::SDR_709).unwrap() {
            fp_gfx::VideoFrame::Cpu(c) => {
                assert_eq!(c.format, fp_gfx::PixelFormat::Nv12);
                assert_eq!(c.planes[0], &(0..8).collect::<Vec<u8>>()[..]);
                assert_eq!(c.planes[1], &[10, 20, 11, 21][..]);
                assert_eq!(c.strides, [4, 4]);
            }
            _ => panic!("expected cpu frame"),
        }
    }

    #[test]
    fn i420p10_becomes_msb_aligned_p010() {
        let le = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let f = DecodedFrame::Cpu(CpuFrame {
            format: PixelFormat::I420P10,
            width: 2,
            height: 2,
            planes: vec![le(&[64, 940, 512, 1023]), le(&[512]), le(&[300])],
            strides: vec![4, 2, 2],
            pts: MediaTime::ZERO,
        });
        let mut conv = FrameConverter::new();
        match conv.convert(&f, ColorInfo::HDR10).unwrap() {
            fp_gfx::VideoFrame::Cpu(c) => {
                assert_eq!(c.format, fp_gfx::PixelFormat::P010);
                assert_eq!(c.color.bit_depth, 10);
                assert_eq!(
                    c.planes[0],
                    &le(&[64 << 6, 940 << 6, 512 << 6, 1023 << 6])[..]
                );
                assert_eq!(c.planes[1], &le(&[512 << 6, 300 << 6])[..]);
            }
            _ => panic!("expected cpu frame"),
        }
    }

    #[test]
    fn short_planes_are_rejected() {
        let f = DecodedFrame::Cpu(CpuFrame {
            format: PixelFormat::I420,
            width: 4,
            height: 4,
            planes: vec![vec![0; 8], vec![0; 4], vec![0; 4]],
            strides: vec![4, 2, 2],
            pts: MediaTime::ZERO,
        });
        assert_eq!(
            FrameConverter::new()
                .convert(&f, ColorInfo::SDR_709)
                .unwrap_err(),
            ConvertError::ShortPlane
        );
    }
}
