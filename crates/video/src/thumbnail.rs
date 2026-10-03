//! Single-frame extraction to RGBA for the library thumbnailer and preview
//! scrubbing sprites. Uses a software decoder (the hardware path yields
//! DMA-BUFs, which would need a GPU readback); without the `dav1d` or
//! `ffmpeg` feature it fails with [`VideoError::NoSoftwareDecoder`].

use crate::decode::convert::{cpu_frame_to_rgba, RgbaImage, YuvMatrix};
use crate::decode::{
    open_software, software_decoders_for, DecodedFrame, DecoderRequest, VideoDecoder,
};
use crate::demux::{open_demuxer, Demuxer};
use crate::error::{Result, VideoError};
use crate::input::MediaInput;
use crate::packet::TrackKind;
use fp_core::{MediaTime, StereoMode};

#[derive(Debug, Clone, PartialEq)]
pub struct ThumbnailOptions {
    pub max_width: u32,
    pub max_height: u32,
    /// Decode up to the exact time (slower) instead of using the keyframe.
    pub precise: bool,
    /// Crop to the left eye for stereo content.
    pub left_eye_only: Option<StereoMode>,
    pub threads: usize,
}

impl Default for ThumbnailOptions {
    fn default() -> Self {
        ThumbnailOptions {
            max_width: 480,
            max_height: 270,
            precise: false,
            left_eye_only: None,
            threads: 2,
        }
    }
}

/// Decode the frame at `at` from `input` and return it as RGBA.
pub fn decode_frame_rgba(
    input: Box<dyn MediaInput>,
    at: MediaTime,
    opts: &ThumbnailOptions,
) -> Result<RgbaImage> {
    let demux = open_demuxer(input)?;
    let track = demux
        .default_track(TrackKind::Video)
        .cloned()
        .ok_or_else(|| VideoError::Unsupported("no video track".into()))?;
    let libs = software_decoders_for(&track.codec);
    let Some(lib) = libs.first() else {
        return Err(VideoError::NoSoftwareDecoder(format!("{:?}", track.codec)));
    };
    let decoder = open_software(lib, &DecoderRequest::from_track(&track), opts.threads)?;
    thumbnail_with(demux, decoder, at, opts)
}

/// [`decode_frame_rgba`] with an already-open demuxer and decoder.
pub fn thumbnail_with(
    mut demux: Box<dyn Demuxer>,
    mut decoder: Box<dyn VideoDecoder>,
    at: MediaTime,
    opts: &ThumbnailOptions,
) -> Result<RgbaImage> {
    let track = demux
        .default_track(TrackKind::Video)
        .cloned()
        .ok_or_else(|| VideoError::Unsupported("no video track".into()))?;
    for t in demux.tracks().to_vec() {
        demux.set_track_enabled(t.id, t.id == track.id);
    }
    let key = demux.seek(at)?;
    let target = if opts.precise { at } else { key };
    let fps = track
        .video
        .as_ref()
        .map(|v| v.fps)
        .filter(|f| *f > 0.0)
        .unwrap_or(30.0);
    let frame_dur = MediaTime::from_secs_f64(1.0 / fps);
    let bit_depth = track.video.as_ref().map_or(8, |v| v.bit_depth);
    let mut best: Option<DecodedFrame> = None;
    let mut pending = None;
    let mut eof = false;
    let mut idle = 0;
    loop {
        while let Some(f) = decoder.receive_frame()? {
            let done = f.pts() + frame_dur > target;
            best = Some(f);
            if done {
                return finish(best.unwrap(), bit_depth, opts);
            }
        }
        if eof && decoder.is_drained() {
            break;
        }
        if pending.is_none() && !eof {
            pending = demux.read_packet()?;
            if pending.is_none() {
                eof = true;
                decoder.drain()?;
                continue;
            }
        }
        if let Some(p) = pending.take() {
            if !decoder.send_packet(&p)? {
                pending = Some(p);
                decoder.wait(std::time::Duration::from_millis(5));
            }
            idle = 0;
        } else {
            idle += 1;
            decoder.wait(std::time::Duration::from_millis(5));
            if idle > 400 {
                break;
            }
        }
    }
    match best {
        Some(f) => finish(f, bit_depth, opts),
        None => Err(VideoError::invalid("no frame decoded")),
    }
}

fn finish(f: DecodedFrame, bit_depth: u8, opts: &ThumbnailOptions) -> Result<RgbaImage> {
    let DecodedFrame::Cpu(cpu) = f else {
        return Err(VideoError::Unsupported(
            "thumbnailing needs a CPU frame".into(),
        ));
    };
    let img = cpu_frame_to_rgba(&cpu, YuvMatrix::guess(cpu.width, cpu.height, bit_depth))?;
    let img = match opts.left_eye_only {
        Some(StereoMode::Sbs) => img.crop(0, 0, img.width / 2, img.height),
        Some(StereoMode::Ou) => img.crop(0, 0, img.width, img.height / 2),
        _ => img,
    };
    Ok(img.fit_within(opts.max_width, opts.max_height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDemuxer, MockMedia, MockVideoDecoder};
    use std::io::Cursor;

    #[test]
    fn thumbnail_from_mock_pipeline() {
        let demux = Box::new(MockDemuxer::new(MockMedia::default()));
        let img = thumbnail_with(
            demux,
            Box::new(MockVideoDecoder::default()),
            MediaTime::from_secs_f64(2.5),
            &ThumbnailOptions {
                precise: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        // Mock luma = frame index low byte: frame 62 (2.48 s) covers 2.5 s.
        let y = 62f32;
        let expect = ((y - 16.0) / 219.0 * 255.0).round() as u8;
        assert_eq!(img.pixel(0, 0)[1], expect);
    }

    #[test]
    fn clear_error_without_software_decoder() {
        let file = crate::demux::mp4::tests::sample_file();
        let r = decode_frame_rgba(
            Box::new(Cursor::new(file)),
            MediaTime::ZERO,
            &ThumbnailOptions::default(),
        );
        if cfg!(not(feature = "ffmpeg")) {
            assert!(matches!(r, Err(VideoError::NoSoftwareDecoder(_))), "{r:?}");
        }
    }
}
