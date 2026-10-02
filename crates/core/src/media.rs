//! Container / stream descriptions produced by the demuxer and stored by the library.

use crate::projection::{Projection, StereoMode};
use crate::time::MediaTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorTransfer {
    #[default]
    Sdr,
    /// SMPTE ST 2084 (HDR10 / HDR10+ / Dolby Vision base layer).
    Pq,
    /// ARIB STD-B67.
    Hlg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTrackInfo {
    pub index: u32,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bit_depth: u8,
    pub transfer: ColorTransfer,
    /// Projection/stereo signalled in container metadata (st3d/sv3d or
    /// Google spatial-media v1), if any. Filename detection fills the gaps.
    pub signalled_projection: Option<Projection>,
    pub signalled_stereo: Option<StereoMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrackInfo {
    pub index: u32,
    pub channels: u16,
    pub sample_rate: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    /// Ambisonic order if the track is ambisonic (1 = FOA).
    pub ambisonic_order: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrackInfo {
    pub index: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    /// "srt", "ass", "webvtt", "pgs".
    pub format: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start: MediaTime,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    pub duration: Option<MediaTime>,
    pub container: String,
    pub video: Vec<VideoTrackInfo>,
    pub audio: Vec<AudioTrackInfo>,
    pub subtitles: Vec<SubtitleTrackInfo>,
    pub chapters: Vec<Chapter>,
}

impl MediaInfo {
    pub fn primary_video(&self) -> Option<&VideoTrackInfo> {
        self.video.first()
    }
}
