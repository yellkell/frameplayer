//! Deterministic mock demuxer / decoder / backend for exercising the
//! playback engine without media files or hardware. Public so the app crate
//! can drive a [`Player`](crate::engine::Player) in its own tests.

use crate::audio_decode::{open_audio_decoder, AudioDecoder};
use crate::decode::{
    CpuFrame, DecodedFrame, DecoderPath, DecoderSelection, PixelFormat, VideoDecoder,
};
use crate::demux::Demuxer;
use crate::engine::MediaBackend;
use crate::error::{Result, VideoError};
use crate::input::MediaInput;
use crate::packet::{
    media_info_from_tracks, AudioParams, CodecId, Packet, TrackDesc, TrackKind, VideoParams,
};
use fp_audio::{AudioOutput, NullOutput, OutputConfig};
use fp_core::media::Chapter;
use fp_core::{MediaInfo, MediaTime};
use std::collections::VecDeque;
use std::time::Duration;

/// Shape of the synthetic media.
#[derive(Debug, Clone, PartialEq)]
pub struct MockMedia {
    pub fps: f64,
    pub duration: MediaTime,
    /// Keyframe every N frames.
    pub keyframe_interval: u64,
    /// Include a 48 kHz stereo s16 PCM audio track (track 2).
    pub audio: bool,
    pub chapters: Vec<Chapter>,
}

impl Default for MockMedia {
    fn default() -> Self {
        MockMedia {
            fps: 25.0,
            duration: MediaTime::from_secs_f64(10.0),
            keyframe_interval: 25,
            audio: true,
            chapters: Vec::new(),
        }
    }
}

const AUDIO_CHUNK: u64 = 960; // 20 ms at 48 kHz

pub struct MockDemuxer {
    media: MockMedia,
    tracks: Vec<TrackDesc>,
    info: MediaInfo,
    next_video: u64,
    next_audio: u64,
    video_enabled: bool,
    audio_enabled: bool,
}

impl MockDemuxer {
    pub fn new(media: MockMedia) -> Self {
        let mut v = TrackDesc::new(1, TrackKind::Video, CodecId::Hevc);
        v.video = Some(VideoParams {
            width: 64,
            height: 32,
            bit_depth: 8,
            fps: media.fps,
            ..Default::default()
        });
        v.duration = Some(media.duration);
        let mut tracks = vec![v];
        if media.audio {
            let mut a = TrackDesc::new(
                2,
                TrackKind::Audio,
                CodecId::Pcm {
                    bits: 16,
                    float: false,
                    big_endian: false,
                },
            );
            a.audio = Some(AudioParams {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 16,
                ambisonic: None,
            });
            tracks.push(a);
        }
        let info = media_info_from_tracks(
            "mock",
            Some(media.duration),
            &tracks,
            media.chapters.clone(),
        );
        MockDemuxer {
            media,
            tracks,
            info,
            next_video: 0,
            next_audio: 0,
            video_enabled: true,
            audio_enabled: true,
        }
    }

    fn frame_count(&self) -> u64 {
        (self.media.duration.as_secs_f64() * self.media.fps).round() as u64
    }

    fn frame_pts(&self, i: u64) -> MediaTime {
        MediaTime::from_secs_f64(i as f64 / self.media.fps)
    }

    fn audio_pts(&self, i: u64) -> MediaTime {
        MediaTime::from_secs_f64((i * AUDIO_CHUNK) as f64 / 48_000.0)
    }

    fn audio_count(&self) -> u64 {
        (self.media.duration.as_secs_f64() * 48_000.0 / AUDIO_CHUNK as f64).ceil() as u64
    }
}

impl Demuxer for MockDemuxer {
    fn format_name(&self) -> &str {
        "mock"
    }
    fn tracks(&self) -> &[TrackDesc] {
        &self.tracks
    }
    fn media_info(&self) -> &MediaInfo {
        &self.info
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let v_ok = self.video_enabled && self.next_video < self.frame_count();
        let a_ok = self.media.audio && self.audio_enabled && self.next_audio < self.audio_count();
        let take_video = match (v_ok, a_ok) {
            (false, false) => return Ok(None),
            (true, false) => true,
            (false, true) => false,
            (true, true) => self.frame_pts(self.next_video) <= self.audio_pts(self.next_audio),
        };
        if take_video {
            let i = self.next_video;
            self.next_video += 1;
            let pts = self.frame_pts(i);
            Ok(Some(Packet {
                track: 1,
                pts,
                dts: pts,
                duration: MediaTime::from_secs_f64(1.0 / self.media.fps),
                keyframe: i.is_multiple_of(self.media.keyframe_interval),
                data: i.to_le_bytes().to_vec(),
            }))
        } else {
            let i = self.next_audio;
            self.next_audio += 1;
            let pts = self.audio_pts(i);
            Ok(Some(Packet {
                track: 2,
                pts,
                dts: pts,
                duration: MediaTime::from_millis(20),
                keyframe: true,
                data: vec![0u8; AUDIO_CHUNK as usize * 4],
            }))
        }
    }

    fn seek(&mut self, target: MediaTime) -> Result<MediaTime> {
        let frame = ((target.as_secs_f64() * self.media.fps).floor().max(0.0) as u64)
            .min(self.frame_count().saturating_sub(1));
        let key = frame - frame % self.media.keyframe_interval;
        self.next_video = key;
        let kt = self.frame_pts(key);
        self.next_audio = (kt.as_secs_f64() * 48_000.0 / AUDIO_CHUNK as f64).floor() as u64;
        Ok(kt)
    }

    fn set_track_enabled(&mut self, track: u32, enabled: bool) {
        match track {
            1 => self.video_enabled = enabled,
            2 => self.audio_enabled = enabled,
            _ => {}
        }
    }

    fn keyframe_times(&self, track: u32) -> Option<Vec<MediaTime>> {
        (track == 1).then(|| {
            (0..self.frame_count())
                .step_by(self.media.keyframe_interval as usize)
                .map(|i| self.frame_pts(i))
                .collect()
        })
    }
}

/// Emits a tiny NV12 frame per packet, in order, with a bounded input queue.
/// Rejects decoding that does not start from a keyframe (catches seek bugs).
pub struct MockVideoDecoder {
    queue: VecDeque<Packet>,
    capacity: usize,
    draining: bool,
    need_key: bool,
    /// Packets decoded so far (for assertions).
    pub decoded: u64,
}

impl Default for MockVideoDecoder {
    fn default() -> Self {
        MockVideoDecoder {
            queue: VecDeque::new(),
            capacity: 3,
            draining: false,
            need_key: true,
            decoded: 0,
        }
    }
}

impl VideoDecoder for MockVideoDecoder {
    fn path(&self) -> DecoderPath {
        DecoderPath::Software {
            library: "mock".into(),
        }
    }
    fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
        if self.queue.len() >= self.capacity {
            return Ok(false);
        }
        if self.need_key {
            if !pkt.keyframe {
                return Err(VideoError::invalid(
                    "mock decoder: stream must start at a keyframe",
                ));
            }
            self.need_key = false;
        }
        self.draining = false;
        self.queue.push_back(pkt.clone());
        Ok(true)
    }
    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        let Some(p) = self.queue.pop_front() else {
            return Ok(None);
        };
        self.decoded += 1;
        Ok(Some(DecodedFrame::Cpu(CpuFrame {
            format: PixelFormat::Nv12,
            width: 2,
            height: 2,
            planes: vec![vec![p.data[0]; 4], vec![128; 2]],
            strides: vec![2, 2],
            pts: p.pts,
        })))
    }
    fn drain(&mut self) -> Result<()> {
        self.draining = true;
        Ok(())
    }
    fn is_drained(&self) -> bool {
        self.draining && self.queue.is_empty()
    }
    fn flush(&mut self) -> Result<()> {
        self.queue.clear();
        self.draining = false;
        self.need_key = true;
        Ok(())
    }
    fn wait(&mut self, timeout: Duration) {
        std::thread::sleep(timeout.min(Duration::from_micros(500)));
    }
}

/// [`MediaBackend`] producing mock media regardless of the input.
pub struct MockBackend {
    pub media: MockMedia,
}

impl MediaBackend for MockBackend {
    fn open_demuxer(&mut self, _input: Box<dyn MediaInput>) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(MockDemuxer::new(self.media.clone())))
    }
    fn open_video_decoder(&mut self, _track: &TrackDesc) -> Result<DecoderSelection> {
        Ok(DecoderSelection {
            decoder: Box::new(MockVideoDecoder::default()),
            path: DecoderPath::Software {
                library: "mock".into(),
            },
            warnings: Vec::new(),
            rejected: Vec::new(),
        })
    }
    fn open_audio_decoder(&mut self, track: &TrackDesc) -> Result<Box<dyn AudioDecoder>> {
        open_audio_decoder(track)
    }
    fn open_audio_output(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioOutput>> {
        Ok(Box::new(NullOutput::new(OutputConfig {
            sample_rate,
            channels,
            buffer: Duration::from_millis(200),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_demuxer_interleaves_and_seeks() {
        let mut d = MockDemuxer::new(MockMedia::default());
        let pkts: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).collect();
        assert_eq!(pkts.iter().filter(|p| p.track == 1).count(), 250);
        assert_eq!(pkts.iter().filter(|p| p.track == 2).count(), 500);
        assert!(pkts.windows(2).all(|w| w[0].pts <= w[1].pts));
        assert_eq!(
            d.seek(MediaTime::from_secs_f64(3.5)).unwrap(),
            MediaTime::from_secs_f64(3.0)
        );
        let p = d.read_packet().unwrap().unwrap();
        assert!(p.keyframe && p.track == 1);
    }

    #[test]
    fn mock_decoder_requires_keyframe() {
        let mut dec = MockVideoDecoder::default();
        let p = Packet {
            track: 1,
            pts: MediaTime::ZERO,
            dts: MediaTime::ZERO,
            duration: MediaTime::ZERO,
            keyframe: false,
            data: vec![0; 8],
        };
        assert!(dec.send_packet(&p).is_err());
    }
}
