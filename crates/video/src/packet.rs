//! Compressed packets and track descriptions shared by demuxers, decoders
//! and the playback engine.

use fp_core::{media::ColorTransfer, MediaTime, Projection, StereoMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

/// Codec of a track, as identified by the container.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CodecId {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Aac,
    Opus,
    Vorbis,
    Flac,
    Mp3,
    Ac3,
    Eac3,
    /// Uncompressed PCM.
    Pcm {
        bits: u8,
        float: bool,
        big_endian: bool,
    },
    /// Plain-text subtitles (SRT, Matroska `S_TEXT/UTF8`).
    SubRip,
    /// ASS/SSA (Matroska `S_TEXT/ASS|SSA`); `codec_private` holds the header.
    Ass,
    WebVtt,
    /// 3GPP timed text (MP4 `tx3g` / mov_text).
    MovText,
    /// Blu-ray presentation graphics.
    Pgs,
    Unknown(String),
}

impl CodecId {
    pub fn to_core(&self) -> fp_core::Codec {
        match self {
            CodecId::H264 => fp_core::Codec::H264,
            CodecId::Hevc => fp_core::Codec::Hevc,
            CodecId::Vp9 => fp_core::Codec::Vp9,
            CodecId::Av1 => fp_core::Codec::Av1,
            _ => fp_core::Codec::Other,
        }
    }

    /// Subtitle format string as used by [`fp_core::SubtitleTrackInfo::format`].
    pub fn subtitle_format(&self) -> Option<&'static str> {
        match self {
            CodecId::SubRip | CodecId::MovText => Some("srt"),
            CodecId::Ass => Some("ass"),
            CodecId::WebVtt => Some("webvtt"),
            CodecId::Pgs => Some("pgs"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub fps: f64,
    pub transfer: ColorTransfer,
    pub projection: Option<Projection>,
    pub stereo: Option<StereoMode>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// Ambisonic order and whether the layout is FuMa (false = AmbiX).
    pub ambisonic: Option<(u8, bool)>,
}

/// One elementary stream in a container.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackDesc {
    /// Container track id (MP4 `track_ID`, Matroska `TrackNumber`). Also used
    /// as `index` in [`fp_core::MediaInfo`] track lists.
    pub id: u32,
    pub kind: TrackKind,
    pub codec: CodecId,
    /// Codec configuration record: `avcC`/`hvcC`/`av1C`/`vpcC` payload,
    /// AAC AudioSpecificConfig, OpusHead, ASS header, …
    pub codec_private: Vec<u8>,
    pub language: Option<String>,
    pub name: Option<String>,
    pub default: bool,
    pub duration: Option<MediaTime>,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
}

impl TrackDesc {
    pub fn new(id: u32, kind: TrackKind, codec: CodecId) -> Self {
        TrackDesc {
            id,
            kind,
            codec,
            codec_private: Vec::new(),
            language: None,
            name: None,
            default: false,
            duration: None,
            video: None,
            audio: None,
        }
    }
}

/// One compressed access unit.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub track: u32,
    pub pts: MediaTime,
    /// Decode timestamp (equal to `pts` when the container has none).
    pub dts: MediaTime,
    pub duration: MediaTime,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Build a [`fp_core::MediaInfo`] from track descriptions.
pub fn media_info_from_tracks(
    container: &str,
    duration: Option<MediaTime>,
    tracks: &[TrackDesc],
    chapters: Vec<fp_core::media::Chapter>,
) -> fp_core::MediaInfo {
    let mut info = fp_core::MediaInfo {
        duration,
        container: container.into(),
        chapters,
        ..Default::default()
    };
    for t in tracks {
        match t.kind {
            TrackKind::Video => {
                let v = t.video.clone().unwrap_or_default();
                info.video.push(fp_core::VideoTrackInfo {
                    index: t.id,
                    codec: t.codec.to_core(),
                    width: v.width,
                    height: v.height,
                    fps: v.fps,
                    bit_depth: if v.bit_depth == 0 { 8 } else { v.bit_depth },
                    transfer: v.transfer,
                    signalled_projection: v.projection,
                    signalled_stereo: v.stereo,
                });
            }
            TrackKind::Audio => {
                let a = t.audio.clone().unwrap_or_default();
                info.audio.push(fp_core::AudioTrackInfo {
                    index: t.id,
                    channels: a.channels,
                    sample_rate: a.sample_rate,
                    language: t.language.clone(),
                    title: t.name.clone(),
                    ambisonic_order: a.ambisonic.map(|(o, _)| o),
                });
            }
            TrackKind::Subtitle => {
                if let Some(fmt) = t.codec.subtitle_format() {
                    info.subtitles.push(fp_core::SubtitleTrackInfo {
                        index: t.id,
                        language: t.language.clone(),
                        title: t.name.clone(),
                        format: fmt.into(),
                    });
                }
            }
            TrackKind::Other => {}
        }
    }
    info
}
