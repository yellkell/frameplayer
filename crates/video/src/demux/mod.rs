//! Container demuxers.
//!
//! [`Demuxer`] is the interface the playback engine, thumbnailer and
//! library indexer use. Implementations:
//!
//! * [`mp4::Mp4Demuxer`]: pure-Rust ISO-BMFF / QuickTime (progressive and
//!   fragmented), with spherical v1/v2 and SA3D ambisonic metadata.
//! * [`mkv::MkvDemuxer`]: pure-Rust Matroska / WebM (EBML) with Cues-based
//!   seeking, lacing, header-stripping, Projection/StereoMode.
//! * `ffmpeg::FfmpegDemuxer` (feature `ffmpeg`): libavformat over a custom
//!   AVIO context for everything else (MPEG-TS, AVI, …).
//!
//! [`open_demuxer`] sniffs the first bytes and picks one.

pub mod mkv;
pub mod mp4;

#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;

use crate::error::Result;
use crate::input::MediaInput;
use crate::packet::{Packet, TrackDesc, TrackKind};
use fp_core::{MediaInfo, MediaTime};
use std::io::{Seek, SeekFrom};

/// A source of compressed packets from one container.
pub trait Demuxer: Send {
    /// Short container name ("mp4", "matroska", "webm", "ffmpeg:mpegts", …).
    fn format_name(&self) -> &str;
    fn tracks(&self) -> &[TrackDesc];
    fn media_info(&self) -> &MediaInfo;
    /// Next packet in decode order across enabled tracks; `None` at end.
    fn read_packet(&mut self) -> Result<Option<Packet>>;
    /// Reposition so the next video packet is the keyframe at or before
    /// `target` (other tracks are positioned at about the same time).
    /// Returns that keyframe's presentation time.
    fn seek(&mut self, target: MediaTime) -> Result<MediaTime>;
    /// Stop (or resume) returning packets for a track; demuxers that can
    /// avoid reading disabled tracks' data do so.
    fn set_track_enabled(&mut self, _track: u32, _enabled: bool) {}
    /// Presentation times of all keyframes of a track, when an index exists.
    fn keyframe_times(&self, _track: u32) -> Option<Vec<MediaTime>> {
        None
    }

    fn track(&self, id: u32) -> Option<&TrackDesc> {
        self.tracks().iter().find(|t| t.id == id)
    }
    /// First track of a kind (the default one if flagged).
    fn default_track(&self, kind: TrackKind) -> Option<&TrackDesc> {
        let mut it = self.tracks().iter().filter(|t| t.kind == kind);
        let first = it.clone().next();
        it.find(|t| t.default).or(first)
    }
}

/// Container families recognised by [`probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    Mp4,
    Matroska,
    Unknown,
}

/// Identify a container from its first bytes.
pub fn probe(head: &[u8]) -> ContainerKind {
    if head.len() >= 4 && head[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return ContainerKind::Matroska;
    }
    if head.len() >= 8 {
        let t = &head[4..8];
        if [
            b"ftyp", b"moov", b"mdat", b"free", b"wide", b"skip", b"styp", b"pnot", b"sidx",
        ]
        .iter()
        .any(|k| t == *k)
        {
            return ContainerKind::Mp4;
        }
    }
    ContainerKind::Unknown
}

/// Open the right demuxer for `input`.
pub fn open_demuxer(mut input: Box<dyn MediaInput>) -> Result<Box<dyn Demuxer>> {
    let mut head = [0u8; 16];
    input.seek(SeekFrom::Start(0))?;
    let n = read_up_to(&mut *input, &mut head)?;
    input.seek(SeekFrom::Start(0))?;
    match probe(&head[..n]) {
        ContainerKind::Mp4 => Ok(Box::new(mp4::Mp4Demuxer::open(input)?)),
        ContainerKind::Matroska => Ok(Box::new(mkv::MkvDemuxer::open(input)?)),
        ContainerKind::Unknown => {
            #[cfg(feature = "ffmpeg")]
            {
                Ok(Box::new(ffmpeg::FfmpegDemuxer::open(input)?))
            }
            #[cfg(not(feature = "ffmpeg"))]
            {
                Err(crate::error::VideoError::Unsupported(
                    "container not recognised (MPEG-TS/AVI need the `ffmpeg` feature)".into(),
                ))
            }
        }
    }
}

pub(crate) fn read_up_to(r: &mut dyn MediaInput, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_containers() {
        assert_eq!(
            probe(&[0, 0, 0, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm']),
            ContainerKind::Mp4
        );
        assert_eq!(
            probe(&[0x1a, 0x45, 0xdf, 0xa3, 0x9f]),
            ContainerKind::Matroska
        );
        assert_eq!(
            probe(&[0x47, 0x40, 0x00, 0x10, 0, 0, 0, 0]),
            ContainerKind::Unknown
        );
    }
}
