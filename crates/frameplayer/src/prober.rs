//! The library's metadata worker, implemented with FFmpeg through fp-media.

use crate::services::Opener;
use fp_library::{MediaProber, ProbeInfo, RgbaImage, ThumbnailCrop};
use std::sync::Arc;

pub struct FfmpegProber {
    pub opener: Arc<Opener>,
}

fn err(e: impl std::fmt::Display) -> fp_library::Error {
    fp_library::Error::Probe(e.to_string())
}

impl MediaProber for FfmpegProber {
    fn probe(&self, location: &str) -> fp_library::Result<ProbeInfo> {
        let src = self.opener.open(location).map_err(err)?;
        let info = fp_media::thumb::probe(src, location).map_err(err)?;
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
        let src = self.opener.open(location).map_err(err)?;
        let crop = match crop {
            ThumbnailCrop::LeftEye => fp_media::thumb::Crop::LeftHalf,
            ThumbnailCrop::TopEye => fp_media::thumb::Crop::TopHalf,
            ThumbnailCrop::Full => fp_media::thumb::Crop::Full,
        };
        let img = fp_media::thumb::grab(src, location, at_seconds, max_width, max_height, crop)
            .map_err(err)?;
        Ok(RgbaImage {
            width: img.width,
            height: img.height,
            pixels: img.pixels,
        })
    }
}
