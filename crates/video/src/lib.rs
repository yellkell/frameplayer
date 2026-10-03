//! FramePlayer video: everything from bytes to presentable frames.
//!
//! * [`input`]: [`MediaInput`] (blocking `Read + Seek + Send` + size) that
//!   `fp-sources` adapts local/SMB/WebDAV/HTTP to; [`BufferedInput`] read-ahead.
//! * [`demux`]: [`Demuxer`] trait; pure-Rust MP4/MOV (incl. fragmented) and
//!   Matroska/WebM demuxers, optional libavformat ([`demux::open_demuxer`]
//!   sniffs and picks). Spherical v1/v2 + Matroska projection metadata
//!   ([`spherical`]) lands in `fp_core::VideoTrackInfo::signalled_*`.
//! * [`codec`]: `avcC`/`hvcC`/`av1C`/`vpcC` parsing and Annex-B conversion.
//! * [`decode`]: [`VideoDecoder`] trait, [`DecodedFrame`] (DMA-BUF or CPU),
//!   V4L2 stateful M2M hardware decoding with DMA-BUF export, optional
//!   dav1d/ffmpeg software decoders, and [`decode::select_decoder`].
//! * [`audio_decode`]: PCM / AAC-LC (symphonia) / ffmpeg audio decoding.
//! * [`engine`]: [`Player`] (command channel, status, events), [`AvClock`]
//!   (audio master, A/V offset, 0.25–4×), [`FrameQueue`] decode-ahead with
//!   `frame_for(predicted_display_time)`, precise/keyframe seeking, frame
//!   step, A-B loop, chapters.
//! * [`subtitle`]: SRT / WebVTT / ASS / PGS, [`SubtitleTrack::active_at`],
//!   embedded-packet decoding, stereo depth placement.
//! * [`thumbnail`]: one frame → RGBA for the library.
//! * [`mock`]: deterministic mock demuxer/decoder/backend for tests.
//!
//! Audio output, the audio clock and DSP live in `fp-audio`; this crate
//! depends on it (never the reverse) and drives `fp_audio::AudioPipeline`.

pub mod audio_decode;
pub mod bytes;
pub mod codec;
pub mod decode;
pub mod demux;
pub mod engine;
pub mod error;
pub mod input;
pub mod mock;
pub mod packet;
pub mod spherical;
pub mod subtitle;
pub mod thumbnail;

pub use decode::{
    select_decoder, CpuFrame, DecodedFrame, DecoderOptions, DecoderPath, DecoderRequest,
    DecoderSelection, DmaBufFrame, DmaBufPlane, PixelFormat, VideoDecoder,
};
pub use demux::{open_demuxer, Demuxer};
pub use engine::{
    AvClock, DefaultBackend, FrameQueue, MediaBackend, OpenRequest, PlaybackState, Player,
    PlayerCommand, PlayerConfig, PlayerEvent, PlayerStatus, SeekMode, VideoFrame, VideoOutput,
};
pub use error::{Result, VideoError};
pub use input::{BufferedInput, MediaInput};
pub use packet::{CodecId, Packet, TrackDesc, TrackKind};
pub use subtitle::{Cue, CueContent, SubtitleTrack};
pub use thumbnail::{decode_frame_rgba, ThumbnailOptions};
