//! Video frame descriptors the renderer accepts. Deliberately independent of
//! the decoder crate; the app converts its decoded frames into these.

use crate::color::{ColorInfo, SampleStorage};
use std::os::fd::RawFd;

/// Two-plane 4:2:0 layouts the GPU path understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit Y plane + interleaved CbCr plane at half resolution.
    Nv12,
    /// 10-bit samples in the top bits of 16-bit words, NV12 layout.
    P010,
}

impl PixelFormat {
    pub fn storage(self) -> SampleStorage {
        match self {
            PixelFormat::Nv12 => SampleStorage::U8,
            PixelFormat::P010 => SampleStorage::MsbAligned16 { bits: 10 },
        }
    }

    /// Bytes per sample component.
    pub fn bytes_per_component(self) -> usize {
        match self {
            PixelFormat::Nv12 => 1,
            PixelFormat::P010 => 2,
        }
    }

    /// DRM fourcc of the whole image (`NV12` / `P010`).
    pub fn drm_fourcc(self) -> u32 {
        let f = |s: &[u8; 4]| u32::from_le_bytes(*s);
        match self {
            PixelFormat::Nv12 => f(b"NV12"),
            PixelFormat::P010 => f(b"P010"),
        }
    }

    /// Plane extent for a `width × height` image (`plane` 0 = luma, 1 = chroma).
    pub fn plane_extent(self, plane: usize, width: u32, height: u32) -> (u32, u32) {
        if plane == 0 {
            (width, height)
        } else {
            (width.div_ceil(2), height.div_ceil(2))
        }
    }

    /// Minimum tightly packed row size of a plane in bytes.
    pub fn plane_row_bytes(self, plane: usize, width: u32) -> usize {
        let (w, _) = self.plane_extent(plane, width, 1);
        let comps = if plane == 0 { 1 } else { 2 };
        w as usize * comps * self.bytes_per_component()
    }
}

/// One plane of a DMA-BUF frame. The fd is *borrowed*: the renderer dups it
/// for import and never closes the caller's descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufPlane {
    pub fd: RawFd,
    pub offset: u64,
    pub pitch: u64,
}

/// A hardware-decoded frame exported as DMA-BUF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DmaBufFrame {
    /// Stable identifier of the underlying decoder buffer. Imports are cached
    /// under this id, so pooled decoder buffers are imported once; call
    /// [`crate::Renderer::forget_dmabuf`] when the decoder frees the buffer.
    pub buffer_id: u64,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// DRM format modifier (`DRM_FORMAT_MOD_LINEAR` = 0).
    pub modifier: u64,
    pub planes: [DmaBufPlane; 2],
    pub color: ColorInfo,
}

/// A software-decoded frame in CPU memory.
#[derive(Debug, Clone, Copy)]
pub struct CpuFrame<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Luma and interleaved chroma planes.
    pub planes: [&'a [u8]; 2],
    /// Row strides in bytes.
    pub strides: [usize; 2],
    pub color: ColorInfo,
}

impl CpuFrame<'_> {
    /// Check plane sizes against the declared geometry.
    pub fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            return Err("empty frame".into());
        }
        for p in 0..2 {
            let (_, h) = self.format.plane_extent(p, self.width, self.height);
            let row = self.format.plane_row_bytes(p, self.width);
            if self.strides[p] < row {
                return Err(format!("plane {p}: stride {} < row {row}", self.strides[p]));
            }
            let need = self.strides[p] * (h as usize - 1) + row;
            if self.planes[p].len() < need {
                return Err(format!(
                    "plane {p}: {} bytes < {need}",
                    self.planes[p].len()
                ));
            }
        }
        Ok(())
    }
}

/// Either kind of frame.
#[derive(Debug, Clone, Copy)]
pub enum VideoFrame<'a> {
    DmaBuf(DmaBufFrame),
    Cpu(CpuFrame<'a>),
}

impl VideoFrame<'_> {
    pub fn size(&self) -> (u32, u32) {
        match self {
            VideoFrame::DmaBuf(f) => (f.width, f.height),
            VideoFrame::Cpu(f) => (f.width, f.height),
        }
    }
    pub fn format(&self) -> PixelFormat {
        match self {
            VideoFrame::DmaBuf(f) => f.format,
            VideoFrame::Cpu(f) => f.format,
        }
    }
    pub fn color(&self) -> ColorInfo {
        match self {
            VideoFrame::DmaBuf(f) => f.color,
            VideoFrame::Cpu(f) => f.color,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_geometry() {
        assert_eq!(PixelFormat::Nv12.plane_extent(1, 1921, 1081), (961, 541));
        assert_eq!(PixelFormat::Nv12.plane_row_bytes(1, 1920), 1920);
        assert_eq!(PixelFormat::P010.plane_row_bytes(0, 1920), 3840);
        assert_eq!(PixelFormat::P010.plane_row_bytes(1, 1920), 3840);
        assert_eq!(PixelFormat::Nv12.drm_fourcc(), 0x3231_564e);
    }

    #[test]
    fn cpu_frame_validation() {
        let y = vec![0u8; 64 * 32];
        let uv = vec![0u8; 64 * 16];
        let mut f = CpuFrame {
            width: 64,
            height: 32,
            format: PixelFormat::Nv12,
            planes: [&y, &uv],
            strides: [64, 64],
            color: ColorInfo::SDR_709,
        };
        assert!(f.validate().is_ok());
        f.strides[0] = 60;
        assert!(f.validate().is_err());
        f.strides[0] = 64;
        f.format = PixelFormat::P010;
        assert!(f.validate().is_err());
    }
}
