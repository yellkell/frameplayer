//! The library's metadata worker, implemented with FFmpeg through fp-media.

use crate::services::Opener;
use fp_library::{MediaProber, ProbeInfo, RgbaImage, ThumbnailCrop};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct FfmpegProber {
    pub opener: Arc<Opener>,
    /// Set while a video plays and when quitting: probes stop at the next
    /// read and the video stays pending. Thumbnails are decoded in
    /// software, and a 10-bit 4K file over SMB would otherwise take the
    /// CPU and the network from playback for minutes (and quitting waited
    /// for it).
    pub busy: Arc<AtomicBool>,
}

impl FfmpegProber {
    /// Maps an error, as cancelled when `busy` caused it.
    fn err(&self, e: impl std::fmt::Display) -> fp_library::Error {
        if self.busy.load(Ordering::Acquire) {
            fp_library::Error::Cancelled
        } else {
            fp_library::Error::Probe(e.to_string())
        }
    }

    fn check(&self) -> fp_library::Result<()> {
        if self.busy.load(Ordering::Acquire) {
            Err(fp_library::Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl MediaProber for FfmpegProber {
    fn probe(&self, location: &str) -> fp_library::Result<ProbeInfo> {
        self.check()?;
        let src = self.opener.open(location).map_err(|e| self.err(e))?;
        let info =
            fp_media::thumb::probe(src, location, self.busy.clone()).map_err(|e| self.err(e))?;
        let v = info.video_stream();
        Ok(ProbeInfo {
            duration: (info.duration > 0.0).then_some(info.duration),
            width: v.map(|v| v.width).unwrap_or(0),
            height: v.map(|v| v.height).unwrap_or(0),
            video_codec: v.map(|v| v.codec.clone()),
            hints: info.hints,
        })
    }

    fn thumbnail(
        &self,
        location: &str,
        at_seconds: f64,
        max_width: u32,
        max_height: u32,
        crop: ThumbnailCrop,
    ) -> fp_library::Result<RgbaImage> {
        self.check()?;
        let src = self.opener.open(location).map_err(|e| self.err(e))?;
        let crop = match crop {
            ThumbnailCrop::LeftEye => fp_media::thumb::Crop::LeftHalf,
            ThumbnailCrop::TopEye => fp_media::thumb::Crop::TopHalf,
            ThumbnailCrop::LeftEyeUnsqueezed => fp_media::thumb::Crop::LeftHalfUnsqueezed,
            ThumbnailCrop::TopEyeUnsqueezed => fp_media::thumb::Crop::TopHalfUnsqueezed,
            ThumbnailCrop::Full => fp_media::thumb::Crop::Full,
        };
        let img = fp_media::thumb::grab(
            src,
            location,
            at_seconds,
            max_width,
            max_height,
            crop,
            self.busy.clone(),
        )
        .map_err(|e| self.err(e))?;
        Ok(RgbaImage {
            width: img.width,
            height: img.height,
            pixels: img.pixels,
        })
    }
}
