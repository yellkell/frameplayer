//! The playback engine: one thread owning demuxer, decoders and the audio
//! pipeline, driven by a command channel, publishing status, and feeding
//! the decode-ahead [`FrameQueue`] the render thread samples.
//!
//! Seek engine:
//! * **Precise** (default): demux from the previous keyframe, decode and
//!   discard every frame that ends before the target, so the first frame
//!   shown is exactly the one covering the target; audio is trimmed to the
//!   same instant.
//! * **Keyframe** (scrubbing): show the keyframe itself, no discard; while
//!   the user drags, consecutive seek commands are coalesced so only the
//!   latest is executed.
//!
//! While a seek (or the initial open) is in progress the clock is held;
//! it resumes from the first presentable frame so nothing is dropped.

use super::clock::AvClock;
use super::queue::{FrameQueue, VideoFrame};
use crate::audio_decode::{AudioDecoder, AudioFrame};
use crate::decode::{DecoderOptions, DecoderPath, DecoderRequest, DecoderSelection, VideoDecoder};
use crate::demux::Demuxer;
use crate::error::{Result, VideoError};
use crate::input::MediaInput;
use crate::packet::{Packet, TrackDesc, TrackKind};
use crate::subtitle::{Cue, EmbeddedSubtitleDecoder, SubtitleTrack};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use fp_audio::{
    AmbisonicNorm, AudioOutput, AudioPipeline, ChannelLayout, OutputConfig, PipelineConfig,
};
use fp_core::{MediaInfo, MediaTime};
use glam::Quat;
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Opens the pieces of a playback pipeline. The default implementation
/// uses the real demuxers, [`select_decoder`](crate::decode::select_decoder)
/// and the system audio output; tests substitute mocks.
pub trait MediaBackend: Send + 'static {
    fn open_demuxer(&mut self, input: Box<dyn MediaInput>) -> Result<Box<dyn Demuxer>>;
    fn open_video_decoder(&mut self, track: &TrackDesc) -> Result<DecoderSelection>;
    fn open_audio_decoder(&mut self, track: &TrackDesc) -> Result<Box<dyn AudioDecoder>>;
    fn open_audio_output(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioOutput>>;
}

/// Real demuxers + V4L2/software decoders + system audio output.
#[derive(Debug, Clone, Default)]
pub struct DefaultBackend {
    pub decoder: DecoderOptions,
    pub output_buffer: Option<Duration>,
}

impl MediaBackend for DefaultBackend {
    fn open_demuxer(&mut self, input: Box<dyn MediaInput>) -> Result<Box<dyn Demuxer>> {
        crate::demux::open_demuxer(input)
    }
    fn open_video_decoder(&mut self, track: &TrackDesc) -> Result<DecoderSelection> {
        crate::decode::select_decoder(&DecoderRequest::from_track(track), &self.decoder)
    }
    fn open_audio_decoder(&mut self, track: &TrackDesc) -> Result<Box<dyn AudioDecoder>> {
        crate::audio_decode::open_audio_decoder(track)
    }
    fn open_audio_output(
        &mut self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioOutput>> {
        let cfg = OutputConfig {
            sample_rate,
            channels,
            buffer: self.output_buffer.unwrap_or(Duration::from_millis(200)),
        };
        fp_audio::open_default_output(cfg).map_err(|e| VideoError::Device(e.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekMode {
    /// Frame-exact (decode and discard up to the target).
    Precise,
    /// Nearest previous keyframe (fast; for scrubbing).
    Keyframe,
}

/// What to open.
pub struct OpenRequest {
    pub input: Box<dyn MediaInput>,
    /// Display name / path (diagnostics).
    pub name: String,
    /// Resume position.
    pub start: Option<MediaTime>,
    pub start_paused: bool,
}

pub enum PlayerCommand {
    Open(OpenRequest),
    Play,
    Pause,
    TogglePause,
    Seek {
        target: MediaTime,
        mode: SeekMode,
    },
    /// Speed, clamped to 0.25–4×.
    SetSpeed(f64),
    /// Step `n` frames (positive forward, negative back); pauses playback.
    StepFrames(i32),
    /// A-B loop; `None` clears.
    SetLoop(Option<(MediaTime, MediaTime)>),
    /// Restart at the end instead of stopping.
    SetLoopFile(bool),
    SelectVideoTrack(u32),
    SelectAudioTrack(Option<u32>),
    SelectSubtitleTrack(Option<u32>),
    /// Replace subtitles with an external file's track (disables embedded).
    LoadExternalSubtitles(SubtitleTrack),
    SetAvOffset(MediaTime),
    NextChapter,
    PrevChapter,
    /// Head orientation (OpenXR pose quaternion) for binaural ambisonics.
    SetHeadOrientation(Quat),
    Stop,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Idle,
    Opening,
    Seeking,
    Playing,
    Paused,
    Ended,
    Error,
}

/// Snapshot of the player, refreshed every engine iteration.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerStatus {
    pub state: PlaybackState,
    pub position: MediaTime,
    pub duration: Option<MediaTime>,
    /// Furthest demuxed timestamp.
    pub buffered: MediaTime,
    pub speed: f64,
    pub av_offset: MediaTime,
    pub loop_ab: Option<(MediaTime, MediaTime)>,
    pub loop_file: bool,
    pub media: Option<MediaInfo>,
    pub name: Option<String>,
    pub video_track: Option<u32>,
    pub audio_track: Option<u32>,
    pub subtitle_track: Option<u32>,
    pub decoder: Option<DecoderPath>,
    pub audio_backend: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub late_frames: u64,
    pub current_chapter: Option<usize>,
}

impl Default for PlayerStatus {
    fn default() -> Self {
        PlayerStatus {
            state: PlaybackState::Idle,
            position: MediaTime::ZERO,
            duration: None,
            buffered: MediaTime::ZERO,
            speed: 1.0,
            av_offset: MediaTime::ZERO,
            loop_ab: None,
            loop_file: false,
            media: None,
            name: None,
            video_track: None,
            audio_track: None,
            subtitle_track: None,
            decoder: None,
            audio_backend: None,
            warnings: Vec::new(),
            error: None,
            late_frames: 0,
            current_chapter: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    Opened(Box<MediaInfo>),
    DecoderSelected {
        path: DecoderPath,
        warnings: Vec<String>,
    },
    StateChanged(PlaybackState),
    SeekCompleted(MediaTime),
    EndOfStream,
    Warning(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerConfig {
    /// Decoded frames kept ahead of display (3–4 recommended).
    pub decode_ahead: usize,
    /// Upper bound on demuxed-but-undecoded video packets.
    pub max_pending_packets: usize,
    pub output_sample_rate: u32,
    /// Render ambisonic audio binaurally.
    pub binaural: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        PlayerConfig {
            decode_ahead: 4,
            max_pending_packets: 240,
            output_sample_rate: 48_000,
            binaural: true,
        }
    }
}

/// What the render thread needs: frames, the clock, and subtitles.
#[derive(Clone)]
pub struct VideoOutput {
    queue: Arc<FrameQueue>,
    clock: Arc<AvClock>,
    subtitles: Arc<Mutex<SubtitleTrack>>,
}

impl VideoOutput {
    /// Frame to show at the predicted display instant (e.g. converted from
    /// `XrFrameState::predicted_display_time`).
    pub fn frame_for_display(&self, display_at: Instant) -> Option<Arc<VideoFrame>> {
        self.queue.frame_for(self.clock.video_time_at(display_at))
    }
    /// Frame for an explicit media time.
    pub fn frame_for(&self, t: MediaTime) -> Option<Arc<VideoFrame>> {
        self.queue.frame_for(t)
    }
    pub fn media_time_at(&self, at: Instant) -> MediaTime {
        self.clock.video_time_at(at)
    }
    pub fn clock(&self) -> &Arc<AvClock> {
        &self.clock
    }
    pub fn queue(&self) -> &Arc<FrameQueue> {
        &self.queue
    }
    /// Subtitle cues visible at `t`.
    pub fn subtitles_at(&self, t: MediaTime) -> Vec<Cue> {
        self.subtitles
            .lock()
            .active_at(t)
            .into_iter()
            .cloned()
            .collect()
    }
}

/// Handle to the playback thread.
pub struct Player {
    tx: Sender<PlayerCommand>,
    status: Arc<RwLock<PlayerStatus>>,
    events: Receiver<PlayerEvent>,
    output: VideoOutput,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    pub fn spawn(backend: Box<dyn MediaBackend>, config: PlayerConfig) -> Player {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (etx, erx) = crossbeam_channel::unbounded();
        let queue = Arc::new(FrameQueue::new(config.decode_ahead));
        let clock = Arc::new(AvClock::new());
        let subtitles = Arc::new(Mutex::new(SubtitleTrack::default()));
        let status = Arc::new(RwLock::new(PlayerStatus::default()));
        let engine = Engine {
            backend,
            cfg: config,
            queue: queue.clone(),
            clock: clock.clone(),
            subtitles: subtitles.clone(),
            status: status.clone(),
            events: etx,
            output: None,
            media: None,
            user_paused: false,
            speed: 1.0,
            loop_ab: None,
            loop_file: false,
            head: Quat::IDENTITY,
            last_state: PlaybackState::Idle,
            warnings: Vec::new(),
            error: None,
        };
        let thread = std::thread::Builder::new()
            .name("fp-playback".into())
            .spawn(move || engine.run(rx))
            .expect("spawn playback thread");
        Player {
            tx,
            status,
            events: erx,
            output: VideoOutput {
                queue,
                clock,
                subtitles,
            },
            thread: Some(thread),
        }
    }

    pub fn send(&self, cmd: PlayerCommand) -> Result<()> {
        self.tx.send(cmd).map_err(|_| VideoError::Closed)
    }

    pub fn open(
        &self,
        input: Box<dyn MediaInput>,
        name: impl Into<String>,
        start: Option<MediaTime>,
    ) -> Result<()> {
        self.send(PlayerCommand::Open(OpenRequest {
            input,
            name: name.into(),
            start,
            start_paused: false,
        }))
    }
    pub fn play(&self) -> Result<()> {
        self.send(PlayerCommand::Play)
    }
    pub fn pause(&self) -> Result<()> {
        self.send(PlayerCommand::Pause)
    }
    pub fn seek(&self, target: MediaTime, mode: SeekMode) -> Result<()> {
        self.send(PlayerCommand::Seek { target, mode })
    }
    pub fn set_speed(&self, speed: f64) -> Result<()> {
        self.send(PlayerCommand::SetSpeed(speed))
    }
    pub fn step(&self, frames: i32) -> Result<()> {
        self.send(PlayerCommand::StepFrames(frames))
    }
    pub fn set_loop(&self, ab: Option<(MediaTime, MediaTime)>) -> Result<()> {
        self.send(PlayerCommand::SetLoop(ab))
    }
    pub fn set_av_offset(&self, offset: MediaTime) -> Result<()> {
        self.send(PlayerCommand::SetAvOffset(offset))
    }

    pub fn status(&self) -> PlayerStatus {
        self.status.read().clone()
    }
    pub fn events(&self) -> &Receiver<PlayerEvent> {
        &self.events
    }
    pub fn video_output(&self) -> VideoOutput {
        self.output.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(PlayerCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct VideoState {
    track: u32,
    dec: Box<dyn VideoDecoder>,
    pending: VecDeque<Packet>,
    held: Option<VideoFrame>,
    drain_sent: bool,
    last_pts: Option<MediaTime>,
}

struct AudioState {
    track: u32,
    dec: Box<dyn AudioDecoder>,
    pipeline: Option<AudioPipeline>,
    ambisonic: Option<(u8, bool)>,
    discard_before: Option<MediaTime>,
}

struct SeekState {
    target: MediaTime,
    started: Instant,
}

struct Media {
    demux: Box<dyn Demuxer>,
    info: MediaInfo,
    name: String,
    video: Option<VideoState>,
    audio: Option<AudioState>,
    subtitle: Option<(u32, EmbeddedSubtitleDecoder)>,
    demux_eof: bool,
    seek: Option<SeekState>,
    pending_step: i32,
    buffered: MediaTime,
    ended: bool,
    /// Last frame discarded by a precise seek (shown if the target is past the end).
    last_discarded: Option<VideoFrame>,
    frame_dur: MediaTime,
    decoder_path: Option<DecoderPath>,
}

struct Engine {
    backend: Box<dyn MediaBackend>,
    cfg: PlayerConfig,
    queue: Arc<FrameQueue>,
    clock: Arc<AvClock>,
    subtitles: Arc<Mutex<SubtitleTrack>>,
    status: Arc<RwLock<PlayerStatus>>,
    events: Sender<PlayerEvent>,
    output: Option<Box<dyn AudioOutput>>,
    media: Option<Media>,
    user_paused: bool,
    speed: f64,
    loop_ab: Option<(MediaTime, MediaTime)>,
    loop_file: bool,
    head: Quat,
    last_state: PlaybackState,
    warnings: Vec<String>,
    error: Option<String>,
}

impl Engine {
    fn emit(&self, e: PlayerEvent) {
        let _ = self.events.send(e);
    }

    fn warn(&mut self, msg: String) {
        tracing::warn!("{msg}");
        self.warnings.push(msg.clone());
        self.emit(PlayerEvent::Warning(msg));
    }

    fn fail(&mut self, msg: String) {
        tracing::error!("{msg}");
        self.error = Some(msg.clone());
        self.emit(PlayerEvent::Error(msg));
    }

    fn run(mut self, rx: Receiver<PlayerCommand>) {
        let mut progressed = false;
        loop {
            let mut cmds = Vec::new();
            if !progressed {
                let busy = self.media.as_ref().is_some_and(|m| !m.ended);
                let timeout = if busy {
                    Duration::from_millis(2)
                } else {
                    Duration::from_millis(50)
                };
                match rx.recv_timeout(timeout) {
                    Ok(c) => cmds.push(c),
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            while let Ok(c) = rx.try_recv() {
                cmds.push(c);
            }
            // Coalesce bursts of seeks (scrubbing): only the last one matters.
            let last_seek = cmds
                .iter()
                .rposition(|c| matches!(c, PlayerCommand::Seek { .. }));
            for (i, c) in cmds.into_iter().enumerate() {
                if matches!(c, PlayerCommand::Seek { .. }) && Some(i) != last_seek {
                    continue;
                }
                if !self.handle(c) {
                    self.close();
                    return;
                }
            }
            progressed = self.pump();
            self.publish();
        }
    }

    // ---- commands -------------------------------------------------------

    fn handle(&mut self, cmd: PlayerCommand) -> bool {
        match cmd {
            PlayerCommand::Open(req) => {
                if let Err(e) = self.open(req) {
                    self.close();
                    self.fail(format!("open failed: {e}"));
                }
            }
            PlayerCommand::Play => self.set_paused(false),
            PlayerCommand::Pause => self.set_paused(true),
            PlayerCommand::TogglePause => self.set_paused(!self.user_paused),
            PlayerCommand::Seek { target, mode } => self.seek(target, mode),
            PlayerCommand::SetSpeed(s) => {
                self.speed = s.clamp(super::clock::MIN_SPEED, super::clock::MAX_SPEED);
                self.clock.set_speed(self.speed);
                if let Some(p) = self
                    .media
                    .as_mut()
                    .and_then(|m| m.audio.as_mut())
                    .and_then(|a| a.pipeline.as_mut())
                {
                    p.set_speed(self.speed);
                }
            }
            PlayerCommand::StepFrames(n) => self.step(n),
            PlayerCommand::SetLoop(ab) => self.loop_ab = ab.filter(|(a, b)| b > a),
            PlayerCommand::SetLoopFile(on) => self.loop_file = on,
            PlayerCommand::SelectVideoTrack(id) => self.select_video(id),
            PlayerCommand::SelectAudioTrack(id) => self.select_audio(id),
            PlayerCommand::SelectSubtitleTrack(id) => self.select_subtitle(id),
            PlayerCommand::LoadExternalSubtitles(track) => {
                if let Some(m) = &mut self.media {
                    if let Some((old, _)) = m.subtitle.take() {
                        m.demux.set_track_enabled(old, false);
                    }
                }
                *self.subtitles.lock() = track;
            }
            PlayerCommand::SetAvOffset(o) => self.clock.set_av_offset(o),
            PlayerCommand::NextChapter => self.chapter_nav(true),
            PlayerCommand::PrevChapter => self.chapter_nav(false),
            PlayerCommand::SetHeadOrientation(q) => {
                self.head = q;
                if let Some(p) = self
                    .media
                    .as_mut()
                    .and_then(|m| m.audio.as_mut())
                    .and_then(|a| a.pipeline.as_mut())
                {
                    p.set_head_orientation(q);
                }
            }
            PlayerCommand::Stop => self.close(),
            PlayerCommand::Shutdown => return false,
        }
        true
    }

    fn close(&mut self) {
        self.media = None;
        self.queue.clear();
        if let Some(o) = &mut self.output {
            o.flush();
        }
        self.clock.set_paused(true);
        self.clock.set_position(MediaTime::ZERO);
        *self.subtitles.lock() = SubtitleTrack::default();
        self.warnings.clear();
        self.error = None;
    }

    fn open(&mut self, req: OpenRequest) -> Result<()> {
        self.close();
        self.set_state(PlaybackState::Opening);
        let mut demux = self.backend.open_demuxer(req.input)?;
        let info = demux.media_info().clone();
        let tracks: Vec<TrackDesc> = demux.tracks().to_vec();
        let mut video = None;
        let mut decoder_path = None;
        if let Some(t) = demux.default_track(TrackKind::Video).cloned() {
            match self.backend.open_video_decoder(&t) {
                Ok(sel) => {
                    for w in &sel.warnings {
                        self.warnings.push(w.clone());
                    }
                    self.emit(PlayerEvent::DecoderSelected {
                        path: sel.path.clone(),
                        warnings: sel.warnings.clone(),
                    });
                    decoder_path = Some(sel.path.clone());
                    video = Some(VideoState::new(t.id, sel.decoder));
                }
                Err(e) => self.warn(format!("video track {} cannot be decoded: {e}", t.id)),
            }
        }
        let mut audio = None;
        if let Some(t) = demux.default_track(TrackKind::Audio).cloned() {
            match self.open_audio_track(&t) {
                Ok(a) => audio = Some(a),
                Err(e) => self.warn(format!("audio track {} cannot be decoded: {e}", t.id)),
            }
        }
        if video.is_none() && audio.is_none() {
            return Err(VideoError::NoDecoder(
                "no playable video or audio track".into(),
            ));
        }
        // Only read what we play.
        for t in &tracks {
            let keep = video.as_ref().is_some_and(|v: &VideoState| v.track == t.id)
                || audio.as_ref().is_some_and(|a: &AudioState| a.track == t.id);
            demux.set_track_enabled(t.id, keep);
        }
        let fps = info
            .primary_video()
            .map(|v| v.fps)
            .filter(|f| *f > 1.0)
            .unwrap_or(30.0);
        self.clock.set_audio_master(match (&audio, &self.output) {
            (Some(_), Some(o)) => Some(o.clock() as Arc<dyn fp_audio::AudioClock>),
            _ => None,
        });
        self.media = Some(Media {
            demux,
            info: info.clone(),
            name: req.name,
            video,
            audio,
            subtitle: None,
            demux_eof: false,
            seek: None,
            pending_step: 0,
            buffered: MediaTime::ZERO,
            ended: false,
            last_discarded: None,
            frame_dur: MediaTime::from_secs_f64(1.0 / fps),
            decoder_path,
        });
        self.user_paused = req.start_paused;
        self.emit(PlayerEvent::Opened(Box::new(info)));
        let start = req.start.unwrap_or(MediaTime::ZERO);
        if start > MediaTime::ZERO {
            self.seek(start, SeekMode::Precise);
        } else {
            self.hold_for_seek(MediaTime::ZERO);
        }
        Ok(())
    }

    fn open_audio_track(&mut self, t: &TrackDesc) -> Result<AudioState> {
        let dec = self.backend.open_audio_decoder(t)?;
        if self.output.is_none() {
            let o = self
                .backend
                .open_audio_output(self.cfg.output_sample_rate, 2)?;
            self.status.write().audio_backend = Some(o.backend_name().to_string());
            self.output = Some(o);
        }
        Ok(AudioState {
            track: t.id,
            dec,
            pipeline: None,
            ambisonic: t.audio.as_ref().and_then(|a| a.ambisonic),
            discard_before: None,
        })
    }

    fn set_paused(&mut self, paused: bool) {
        if !paused && self.media.as_ref().is_some_and(|m| m.ended) {
            self.user_paused = false;
            self.seek(MediaTime::ZERO, SeekMode::Precise);
            return;
        }
        self.user_paused = paused;
        let seeking = self.media.as_ref().is_some_and(|m| m.seek.is_some());
        if !seeking {
            self.clock.set_paused(paused);
            if let Some(o) = &mut self.output {
                o.set_paused(paused);
            }
        }
    }

    /// Freeze the clock at `t` until the first frame after a seek is ready.
    fn hold_for_seek(&mut self, t: MediaTime) {
        self.clock.set_paused(true);
        self.clock.set_position(t);
        if let Some(o) = &mut self.output {
            o.set_paused(true);
        }
        if let Some(m) = &mut self.media {
            m.seek = Some(SeekState {
                target: t,
                started: Instant::now(),
            });
        }
    }

    fn seek(&mut self, target: MediaTime, mode: SeekMode) {
        let Some(m) = &mut self.media else { return };
        let max = m.info.duration.unwrap_or(MediaTime(i64::MAX / 2));
        let target = target.clamp_to(MediaTime::ZERO, max);
        let key = match m.demux.seek(target) {
            Ok(k) => k,
            Err(e) => {
                let msg = format!("seek failed: {e}");
                self.warn(msg);
                return;
            }
        };
        let effective = match mode {
            SeekMode::Precise => target,
            SeekMode::Keyframe => key,
        };
        if let Some(v) = &mut m.video {
            if let Err(e) = v.dec.flush() {
                tracing::warn!("decoder flush: {e}");
            }
            v.pending.clear();
            v.held = None;
            v.drain_sent = false;
            v.last_pts = None;
        }
        if let Some(a) = &mut m.audio {
            a.dec.flush();
            if let Some(p) = &mut a.pipeline {
                p.reset();
            }
            a.discard_before = Some(effective);
        }
        if let Some(o) = &mut self.output {
            o.flush();
        }
        self.queue.flush();
        m.demux_eof = false;
        m.ended = false;
        m.last_discarded = None;
        m.buffered = key;
        self.hold_for_seek(effective);
    }

    fn step(&mut self, n: i32) {
        if self.media.is_none() || n == 0 {
            return;
        }
        if !self.user_paused {
            self.set_paused(true);
        }
        let m = self.media.as_mut().unwrap();
        if n > 0 {
            m.pending_step += n;
        } else {
            let cur = self
                .queue
                .current()
                .map(|f| f.pts)
                .unwrap_or_else(|| self.clock.position());
            let target = cur - MediaTime(m.frame_dur.0 * (-n) as i64);
            self.seek(
                target.clamp_to(MediaTime::ZERO, MediaTime(i64::MAX)),
                SeekMode::Precise,
            );
        }
    }

    fn chapter_nav(&mut self, forward: bool) {
        let Some(m) = &self.media else { return };
        let pos = self.clock.position();
        let ch = &m.info.chapters;
        let target = if forward {
            ch.iter()
                .map(|c| c.start)
                .find(|&s| s > pos + MediaTime::from_millis(500))
        } else {
            // Within 2 s of a chapter start, go to the previous one.
            ch.iter()
                .map(|c| c.start)
                .rfind(|&s| s < pos - MediaTime::from_secs_f64(2.0))
                .or(Some(MediaTime::ZERO))
        };
        if let Some(t) = target {
            self.seek(t, SeekMode::Precise);
        }
    }

    fn select_video(&mut self, id: u32) {
        let Some(track) = self.media.as_ref().and_then(|m| m.demux.track(id).cloned()) else {
            return;
        };
        if track.kind != TrackKind::Video {
            return;
        }
        match self.backend.open_video_decoder(&track) {
            Ok(sel) => {
                let m = self.media.as_mut().unwrap();
                if let Some(old) = &m.video {
                    m.demux.set_track_enabled(old.track, false);
                }
                m.demux.set_track_enabled(id, true);
                m.decoder_path = Some(sel.path.clone());
                m.video = Some(VideoState::new(id, sel.decoder));
                self.emit(PlayerEvent::DecoderSelected {
                    path: sel.path,
                    warnings: sel.warnings,
                });
                let pos = self.clock.position();
                self.seek(pos, SeekMode::Precise);
            }
            Err(e) => self.warn(format!("cannot switch to video track {id}: {e}")),
        }
    }

    fn select_audio(&mut self, id: Option<u32>) {
        let Some(m) = &mut self.media else { return };
        if let Some(old) = m.audio.take() {
            m.demux.set_track_enabled(old.track, false);
        }
        let Some(id) = id else {
            self.clock.set_audio_master(None);
            if let Some(o) = &mut self.output {
                o.flush();
            }
            return;
        };
        let Some(track) = m.demux.track(id).cloned() else {
            return;
        };
        match self.open_audio_track(&track) {
            Ok(a) => {
                let m = self.media.as_mut().unwrap();
                m.demux.set_track_enabled(id, true);
                m.audio = Some(a);
                self.clock.set_audio_master(
                    self.output
                        .as_ref()
                        .map(|o| o.clock() as Arc<dyn fp_audio::AudioClock>),
                );
                let pos = self.clock.position();
                self.seek(pos, SeekMode::Precise);
            }
            Err(e) => self.warn(format!("cannot switch to audio track {id}: {e}")),
        }
    }

    fn select_subtitle(&mut self, id: Option<u32>) {
        let Some(m) = &mut self.media else { return };
        if let Some((old, _)) = m.subtitle.take() {
            m.demux.set_track_enabled(old, false);
        }
        *self.subtitles.lock() = SubtitleTrack::default();
        let Some(id) = id else { return };
        let Some(track) = m.demux.track(id).cloned() else {
            return;
        };
        m.demux.set_track_enabled(id, true);
        m.subtitle = Some((id, EmbeddedSubtitleDecoder::new(&track)));
        // Re-read from the current position so the active cue appears now.
        let pos = self.clock.position();
        self.seek(pos, SeekMode::Precise);
    }

    // ---- the pump -------------------------------------------------------

    fn wants_packets(&self, m: &Media) -> bool {
        let pending = m.video.as_ref().map_or(0, |v| v.pending.len());
        if pending >= self.cfg.max_pending_packets {
            return false;
        }
        let video_wants = m.video.as_ref().is_some_and(|v| v.pending.len() < 8);
        let audio_wants = match (&m.audio, &self.output) {
            (Some(a), Some(o)) => {
                let rate = o.sample_rate() as usize;
                let queued = a.pipeline.as_ref().map_or(0, |p| p.pending_frames());
                queued < rate / 4 && o.free_frames() > rate / 50
            }
            _ => false,
        };
        let subtitle_only = m.video.is_none() && m.audio.is_none();
        video_wants || audio_wants || subtitle_only
    }

    fn route(&mut self, pkt: Packet) {
        let head = self.head;
        let speed = self.speed;
        let binaural = self.cfg.binaural;
        let out_rate = self.output.as_ref().map_or(48_000, |o| o.sample_rate());
        let out_ch = self.output.as_ref().map_or(2, |o| o.channels());
        let m = self.media.as_mut().unwrap();
        m.buffered = m.buffered.max(pkt.pts);
        if m.video.as_ref().is_some_and(|v| v.track == pkt.track) {
            m.video.as_mut().unwrap().pending.push_back(pkt);
            return;
        }
        if let Some(a) = m.audio.as_mut().filter(|a| a.track == pkt.track) {
            match a.dec.decode(&pkt) {
                Ok(Some(frame)) => {
                    if let Some(frame) = trim_audio(frame, &mut a.discard_before) {
                        let p = a.pipeline.get_or_insert_with(|| {
                            let layout = match a.ambisonic {
                                Some((order, fuma)) => ChannelLayout::Ambisonic {
                                    order,
                                    norm: if fuma {
                                        AmbisonicNorm::FuMa
                                    } else {
                                        AmbisonicNorm::AmbiX
                                    },
                                },
                                None => ChannelLayout::guess(frame.channels, None),
                            };
                            let layout = if layout.channels() > frame.channels as usize {
                                ChannelLayout::guess(frame.channels, None)
                            } else {
                                layout
                            };
                            let mut cfg =
                                PipelineConfig::new(frame.sample_rate, layout, out_rate, out_ch);
                            cfg.binaural = binaural;
                            let mut p = AudioPipeline::new(cfg);
                            p.set_speed(speed);
                            p.set_head_orientation(head);
                            p
                        });
                        p.push(&frame.samples, frame.pts);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("audio decode error: {e}"),
            }
            return;
        }
        if let Some((_, dec)) = m.subtitle.as_mut().filter(|(id, _)| *id == pkt.track) {
            let updates = dec.decode(&pkt);
            let mut subs = self.subtitles.lock();
            for u in updates {
                subs.apply(u);
            }
        }
    }

    /// One engine iteration. Returns whether anything moved.
    fn pump(&mut self) -> bool {
        if self.media.is_none() {
            return false;
        }
        let mut progressed = false;

        // 1. Demux.
        let mut reads = 0;
        while reads < 32 {
            let m = self.media.as_ref().unwrap();
            if m.demux_eof || !self.wants_packets(m) {
                break;
            }
            let r = self.media.as_mut().unwrap().demux.read_packet();
            match r {
                Ok(Some(p)) => {
                    reads += 1;
                    progressed = true;
                    self.route(p);
                }
                Ok(None) => self.media.as_mut().unwrap().demux_eof = true,
                Err(e) => {
                    self.media.as_mut().unwrap().demux_eof = true;
                    self.warn(format!("demux error (treating as end of stream): {e}"));
                }
            }
        }

        // 2. Video decode.
        if !self.clock.is_paused() {
            self.queue.drop_late(self.clock.position());
        }
        let mut decode_error = None;
        {
            let m = self.media.as_mut().unwrap();
            let threshold = m.seek.as_ref().map(|s| s.target);
            if let Some(v) = &mut m.video {
                loop {
                    if let Some(h) = v.held.take() {
                        match self.queue.try_push(h) {
                            Ok(()) => progressed = true,
                            Err(h) => {
                                v.held = Some(h);
                                break;
                            }
                        }
                    }
                    match v.dec.receive_frame() {
                        Ok(Some(f)) => {
                            progressed = true;
                            let pts = f.pts();
                            v.last_pts = Some(v.last_pts.map_or(pts, |l| l.max(pts)));
                            let vf = VideoFrame {
                                frame: f,
                                pts,
                                duration: m.frame_dur,
                                serial: self.queue.serial(),
                            };
                            // Precise seek: drop frames that end before the target.
                            if threshold.is_some_and(|t| pts + m.frame_dur <= t) {
                                m.last_discarded = Some(vf);
                            } else {
                                v.held = Some(vf);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            decode_error = Some(e);
                            break;
                        }
                    }
                }
                while let Some(p) = v.pending.front() {
                    match v.dec.send_packet(p) {
                        Ok(true) => {
                            v.pending.pop_front();
                            progressed = true;
                        }
                        Ok(false) => break,
                        Err(e) => {
                            tracing::warn!("dropping undecodable packet at {}: {e}", p.pts);
                            v.pending.pop_front();
                        }
                    }
                }
                if m.demux_eof && v.pending.is_empty() && !v.drain_sent {
                    v.drain_sent = true;
                    if let Err(e) = v.dec.drain() {
                        decode_error = Some(e);
                    }
                }
                if !progressed && !v.pending.is_empty() {
                    v.dec.wait(Duration::from_millis(2));
                }
            }
        }
        if let Some(e) = decode_error {
            self.warn(format!("video decode error: {e}"));
        }
        // Runtime hardware → software fallback inside the decoder.
        let switched = self
            .media
            .as_mut()
            .unwrap()
            .video
            .as_mut()
            .and_then(|v| v.dec.take_fallback_notice().map(|n| (n, v.dec.path())));
        if let Some((notice, path)) = switched {
            self.media.as_mut().unwrap().decoder_path = Some(path.clone());
            self.warn(notice.clone());
            self.emit(PlayerEvent::DecoderSelected {
                path,
                warnings: vec![notice],
            });
        }

        // 3. Audio out.
        if let (Some(a), Some(o)) = (
            self.media.as_mut().unwrap().audio.as_mut(),
            self.output.as_mut(),
        ) {
            if let Some(p) = &mut a.pipeline {
                if p.write_to(o.as_mut()) > 0 {
                    progressed = true;
                }
            }
        }

        // 4. Seek / open completion.
        self.finish_seek_if_ready();

        // 5. Frame stepping.
        {
            let m = self.media.as_mut().unwrap();
            if m.pending_step > 0 && m.seek.is_none() {
                if let Some(f) = self.queue.step() {
                    self.clock.set_position(f.pts);
                    m.pending_step -= 1;
                    progressed = true;
                }
            }
        }

        // 6. A-B loop and end of stream.
        let pos = self.clock.position();
        let m = self.media.as_ref().unwrap();
        if m.seek.is_none() {
            if let Some((a, b)) = self.loop_ab {
                if pos >= b {
                    self.seek(a, SeekMode::Precise);
                    return true;
                }
            }
            if !m.ended && self.at_end(pos) {
                if self.loop_file {
                    self.seek(MediaTime::ZERO, SeekMode::Precise);
                } else {
                    self.media.as_mut().unwrap().ended = true;
                    self.clock.set_paused(true);
                    self.emit(PlayerEvent::EndOfStream);
                }
                return true;
            }
        }
        progressed
    }

    fn at_end(&self, pos: MediaTime) -> bool {
        let m = self.media.as_ref().unwrap();
        if !m.demux_eof {
            return false;
        }
        let video_done = m.video.as_ref().is_none_or(|v| {
            v.dec.is_drained()
                && v.pending.is_empty()
                && v.held.is_none()
                // Queued frames count as shown once their time has passed
                // (the render thread may not have sampled them yet).
                && v.last_pts.is_none_or(|l| pos >= l + m.frame_dur)
        });
        let audio_done = match (&m.audio, &self.output) {
            (Some(a), Some(o)) => {
                a.pipeline.as_ref().is_none_or(|p| p.pending_frames() == 0)
                    && o.clock().queued_frames() == 0
            }
            _ => true,
        };
        video_done && audio_done
    }

    fn finish_seek_if_ready(&mut self) {
        let m = self.media.as_mut().unwrap();
        let Some(s) = &m.seek else { return };
        let target = s.target;
        let timed_out = s.started.elapsed() > Duration::from_secs(5);
        let first_pts = match &m.video {
            Some(v) => {
                let ready = self.queue.next_pts().or(v.held.as_ref().map(|h| h.pts));
                match ready {
                    Some(p) => Some(p),
                    None => {
                        // Target beyond the last frame: show the last one.
                        let exhausted = m.demux_eof
                            && v.pending.is_empty()
                            && (v.dec.is_drained() || timed_out);
                        if exhausted {
                            match m.last_discarded.take() {
                                Some(f) => {
                                    let p = f.pts;
                                    let _ = self.queue.try_push(VideoFrame {
                                        serial: self.queue.serial(),
                                        ..f
                                    });
                                    Some(p)
                                }
                                None => Some(target),
                            }
                        } else if timed_out {
                            Some(target)
                        } else {
                            return;
                        }
                    }
                }
            }
            None => Some(target),
        };
        // Give audio a moment to prime so playback starts in sync.
        let audio_ready = match (&m.audio, &self.output) {
            (Some(a), Some(o)) => {
                m.demux_eof
                    || timed_out
                    || o.clock().queued_frames() > (o.sample_rate() / 20) as u64
                    || a.pipeline
                        .as_ref()
                        .is_some_and(|p| p.pending_frames() > 0 && o.free_frames() == 0)
            }
            _ => true,
        };
        if !audio_ready {
            return;
        }
        // Precise seeks report the target itself (the frame covering it is
        // on screen); if the first frame comes later (gap), start there.
        let start = first_pts.map_or(target, |p| p.max(target));
        m.seek = None;
        m.last_discarded = None;
        self.clock.set_position(start);
        if !self.user_paused {
            self.clock.set_paused(false);
            if let Some(o) = &mut self.output {
                o.set_paused(false);
            }
        }
        self.emit(PlayerEvent::SeekCompleted(start));
    }

    // ---- status ---------------------------------------------------------

    fn set_state(&mut self, s: PlaybackState) {
        if s != self.last_state {
            self.last_state = s;
            self.emit(PlayerEvent::StateChanged(s));
        }
    }

    fn publish(&mut self) {
        let state = match &self.media {
            None if self.error.is_some() => PlaybackState::Error,
            None => PlaybackState::Idle,
            Some(m) if m.seek.is_some() => PlaybackState::Seeking,
            Some(m) if m.ended => PlaybackState::Ended,
            Some(_) if self.user_paused => PlaybackState::Paused,
            Some(_) => PlaybackState::Playing,
        };
        self.set_state(state);
        let pos = self.clock.position();
        let mut st = self.status.write();
        st.state = state;
        st.speed = self.speed;
        st.av_offset = self.clock.av_offset();
        st.loop_ab = self.loop_ab;
        st.loop_file = self.loop_file;
        st.warnings = self.warnings.clone();
        st.error = self.error.clone();
        st.late_frames = self.queue.stats().late_dropped;
        match &self.media {
            Some(m) => {
                let dur = m.info.duration;
                st.position = match dur {
                    Some(d) => pos.clamp_to(MediaTime::ZERO, d),
                    None => pos.max(MediaTime::ZERO),
                };
                st.duration = dur;
                st.buffered = m.buffered;
                if st.media.as_ref() != Some(&m.info) {
                    st.media = Some(m.info.clone());
                }
                st.name = Some(m.name.clone());
                st.video_track = m.video.as_ref().map(|v| v.track);
                st.audio_track = m.audio.as_ref().map(|a| a.track);
                st.subtitle_track = m.subtitle.as_ref().map(|s| s.0);
                st.decoder = m.decoder_path.clone();
                st.current_chapter = m.info.chapters.iter().rposition(|c| c.start <= st.position);
            }
            None => {
                let backend = st.audio_backend.take();
                *st = PlayerStatus {
                    state,
                    error: self.error.clone(),
                    audio_backend: backend,
                    ..Default::default()
                };
            }
        }
    }
}

impl VideoState {
    fn new(track: u32, dec: Box<dyn VideoDecoder>) -> Self {
        VideoState {
            track,
            dec,
            pending: VecDeque::new(),
            held: None,
            drain_sent: false,
            last_pts: None,
        }
    }
}

/// Drop audio before `discard_before` (precise seek), trimming the first
/// overlapping frame at sample accuracy.
fn trim_audio(mut f: AudioFrame, discard_before: &mut Option<MediaTime>) -> Option<AudioFrame> {
    let Some(t) = *discard_before else {
        return Some(f);
    };
    let end = f.pts + f.duration();
    if end <= t {
        return None;
    }
    if f.pts < t {
        let skip = ((t - f.pts).as_secs_f64() * f.sample_rate as f64).round() as usize;
        let ch = f.channels.max(1) as usize;
        let skip = skip.min(f.frames());
        f.samples.drain(..skip * ch);
        f.pts = t;
    }
    *discard_before = None;
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockBackend, MockMedia};
    use fp_core::media::Chapter;
    use std::io::Cursor;

    fn player(media: MockMedia) -> Player {
        Player::spawn(Box::new(MockBackend { media }), PlayerConfig::default())
    }

    fn open(p: &Player, paused: bool) {
        p.send(PlayerCommand::Open(OpenRequest {
            input: Box::new(Cursor::new(Vec::<u8>::new())),
            name: "mock".into(),
            start: None,
            start_paused: paused,
        }))
        .unwrap();
    }

    fn wait_for(p: &Player, what: &str, mut f: impl FnMut(&PlayerStatus) -> bool) -> PlayerStatus {
        let t0 = Instant::now();
        loop {
            let s = p.status();
            if f(&s) {
                return s;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}: {s:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shown(p: &Player) -> MediaTime {
        let out = p.video_output();
        // Wait until the queue has something presentable at the clock time.
        let t0 = Instant::now();
        loop {
            let t = out.clock().position();
            if let Some(f) = out.frame_for(t) {
                return f.pts;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "no frame shown");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Mock media whose "hardware" decoder fails on its first frame.
    struct FailingHwBackend(MockBackend);

    impl MediaBackend for FailingHwBackend {
        fn open_demuxer(&mut self, input: Box<dyn MediaInput>) -> Result<Box<dyn Demuxer>> {
            self.0.open_demuxer(input)
        }
        fn open_video_decoder(&mut self, track: &TrackDesc) -> Result<DecoderSelection> {
            use crate::decode::fallback::tests::{FailingHw, Failure};
            let mut hw = |_: &DecoderRequest, _: &DecoderOptions| {
                Ok(Box::new(FailingHw::new(Failure::ReceiveError)) as Box<dyn VideoDecoder>)
            };
            crate::decode::select_decoder_with(
                &DecoderRequest::from_track(track),
                &DecoderOptions::default(),
                &mut hw,
                &["mock".to_string()],
                Arc::new(|_: &str, _: &DecoderRequest, _| {
                    Ok(Box::new(crate::mock::MockVideoDecoder::default()) as Box<dyn VideoDecoder>)
                }),
                Default::default(),
            )
        }
        fn open_audio_decoder(&mut self, track: &TrackDesc) -> Result<Box<dyn AudioDecoder>> {
            self.0.open_audio_decoder(track)
        }
        fn open_audio_output(
            &mut self,
            sample_rate: u32,
            channels: u16,
        ) -> Result<Box<dyn AudioOutput>> {
            self.0.open_audio_output(sample_rate, channels)
        }
    }

    #[test]
    fn runtime_hardware_failure_falls_back_to_software() {
        let media = MockMedia {
            duration: MediaTime::from_secs_f64(1.0),
            ..Default::default()
        };
        let p = Player::spawn(
            Box::new(FailingHwBackend(MockBackend { media })),
            PlayerConfig::default(),
        );
        open(&p, false);
        let s = wait_for(&p, "end", |s| s.state == PlaybackState::Ended);
        assert_eq!(
            s.decoder,
            Some(DecoderPath::Software {
                library: "mock".into()
            })
        );
        assert!(
            s.warnings.iter().any(|w| w.contains("falling back")),
            "{:?}",
            s.warnings
        );
        let events: Vec<PlayerEvent> = p.events().try_iter().collect();
        let selected: Vec<&DecoderPath> = events
            .iter()
            .filter_map(|e| match e {
                PlayerEvent::DecoderSelected { path, .. } => Some(path),
                _ => None,
            })
            .collect();
        assert!(matches!(selected[0], DecoderPath::Hardware { .. }));
        assert!(matches!(selected[1], DecoderPath::Software { .. }));
    }

    #[test]
    fn opens_and_plays_in_real_time() {
        let p = player(MockMedia::default());
        open(&p, false);
        let s = wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        assert_eq!(s.duration, Some(MediaTime::from_secs_f64(10.0)));
        assert_eq!(
            s.decoder,
            Some(DecoderPath::Software {
                library: "mock".into()
            })
        );
        assert_eq!(s.audio_track, Some(2));
        let (t0, p0) = (Instant::now(), p.status().position);
        std::thread::sleep(Duration::from_millis(300));
        let p1 = p.status().position;
        let ratio = (p1 - p0).as_secs_f64() / t0.elapsed().as_secs_f64();
        assert!((0.7..1.3).contains(&ratio), "clock rate {ratio} at 1×");
        // Frames are flowing to the render side.
        let f = p
            .video_output()
            .frame_for_display(Instant::now())
            .expect("frame");
        assert!((f.pts - p1).as_secs_f64().abs() < 0.2);
    }

    #[test]
    fn pause_freezes_and_resumes() {
        let p = player(MockMedia::default());
        open(&p, false);
        wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        p.pause().unwrap();
        let s = wait_for(&p, "paused", |s| s.state == PlaybackState::Paused);
        std::thread::sleep(Duration::from_millis(150));
        let s2 = p.status();
        assert!((s2.position - s.position).as_secs_f64().abs() < 0.01);
        p.play().unwrap();
        wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        std::thread::sleep(Duration::from_millis(150));
        assert!(p.status().position > s2.position);
    }

    #[test]
    fn precise_and_keyframe_seek() {
        let p = player(MockMedia::default());
        open(&p, true);
        wait_for(&p, "paused", |s| s.state == PlaybackState::Paused);
        // 25 fps, keyframes every second. 3.5 s is frame 87.5 → frame 87 (3.48 s).
        p.seek(MediaTime::from_secs_f64(3.5), SeekMode::Precise)
            .unwrap();
        let s = wait_for(&p, "precise seek", |s| {
            s.state == PlaybackState::Paused && s.position == MediaTime::from_secs_f64(3.5)
        });
        assert_eq!(s.position, MediaTime::from_secs_f64(3.5));
        assert_eq!(shown(&p), MediaTime::from_secs_f64(87.0 / 25.0));
        p.seek(MediaTime::from_secs_f64(6.7), SeekMode::Keyframe)
            .unwrap();
        let s = wait_for(&p, "keyframe seek", |s| {
            s.state == PlaybackState::Paused && s.position == MediaTime::from_secs_f64(6.0)
        });
        assert_eq!(s.position, MediaTime::from_secs_f64(6.0));
        assert_eq!(shown(&p), MediaTime::from_secs_f64(6.0));
        // Burst of scrub seeks: only the last lands.
        for i in 0..20 {
            p.seek(MediaTime::from_secs_f64(i as f64 * 0.4), SeekMode::Keyframe)
                .unwrap();
        }
        wait_for(&p, "scrub", |s| {
            s.state == PlaybackState::Paused && s.position == MediaTime::from_secs_f64(7.0)
        });
    }

    #[test]
    fn frame_step_forward_and_back() {
        let p = player(MockMedia::default());
        open(&p, true);
        wait_for(&p, "paused", |s| s.state == PlaybackState::Paused);
        p.seek(MediaTime::from_secs_f64(2.0), SeekMode::Precise)
            .unwrap();
        wait_for(&p, "seek", |s| {
            s.state == PlaybackState::Paused && s.position == MediaTime::from_secs_f64(2.0)
        });
        assert_eq!(shown(&p), MediaTime::from_secs_f64(2.0));
        p.step(1).unwrap();
        wait_for(&p, "step +1", |s| {
            s.position == MediaTime::from_secs_f64(2.04)
        });
        assert_eq!(
            p.video_output().queue().current().unwrap().pts,
            MediaTime::from_secs_f64(2.04)
        );
        p.step(-2).unwrap();
        wait_for(&p, "step -2", |s| {
            s.state == PlaybackState::Paused && s.position == MediaTime::from_secs_f64(1.96)
        });
        assert_eq!(shown(&p), MediaTime::from_secs_f64(1.96));
    }

    #[test]
    fn speed_scales_clock() {
        let p = player(MockMedia::default());
        open(&p, false);
        wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        p.set_speed(2.0).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let (t0, p0) = (Instant::now(), p.status().position);
        std::thread::sleep(Duration::from_millis(400));
        let ratio = (p.status().position - p0).as_secs_f64() / t0.elapsed().as_secs_f64();
        assert!((1.5..2.5).contains(&ratio), "clock rate {ratio} at 2×");
        assert_eq!(p.status().speed, 2.0);
    }

    #[test]
    fn ab_loop_stays_inside() {
        let p = player(MockMedia::default());
        open(&p, false);
        wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        p.set_loop(Some((
            MediaTime::from_secs_f64(1.0),
            MediaTime::from_secs_f64(1.4),
        )))
        .unwrap();
        p.set_speed(4.0).unwrap();
        p.seek(MediaTime::from_secs_f64(1.0), SeekMode::Precise)
            .unwrap();
        let mut looped = false;
        let mut last = MediaTime::ZERO;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(1200) {
            let s = p.status();
            if s.state == PlaybackState::Playing {
                assert!(
                    s.position <= MediaTime::from_secs_f64(1.6),
                    "escaped loop: {}",
                    s.position
                );
                if s.position < last {
                    looped = true;
                }
                last = s.position;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(looped, "loop never wrapped");
    }

    #[test]
    fn chapters_and_end_of_stream() {
        let media = MockMedia {
            duration: MediaTime::from_secs_f64(3.0),
            chapters: vec![
                Chapter {
                    start: MediaTime::ZERO,
                    title: "A".into(),
                },
                Chapter {
                    start: MediaTime::from_secs_f64(2.5),
                    title: "B".into(),
                },
            ],
            ..Default::default()
        };
        let p = player(media);
        open(&p, true);
        wait_for(&p, "paused", |s| s.state == PlaybackState::Paused);
        p.send(PlayerCommand::NextChapter).unwrap();
        let s = wait_for(&p, "chapter", |s| {
            s.position == MediaTime::from_secs_f64(2.5) && s.state == PlaybackState::Paused
        });
        assert_eq!(s.current_chapter, Some(1));
        p.play().unwrap();
        wait_for(&p, "end", |s| s.state == PlaybackState::Ended);
        assert!(p.events().try_iter().any(|e| e == PlayerEvent::EndOfStream));
        p.send(PlayerCommand::PrevChapter).unwrap();
        wait_for(&p, "prev chapter", |s| {
            s.state != PlaybackState::Ended && s.position < MediaTime::from_secs_f64(2.6)
        });
    }

    #[test]
    fn video_only_and_av_offset() {
        let p = player(MockMedia {
            audio: false,
            ..Default::default()
        });
        open(&p, false);
        let s = wait_for(&p, "playing", |s| s.state == PlaybackState::Playing);
        assert_eq!(s.audio_track, None);
        p.set_av_offset(MediaTime::from_millis(100)).unwrap();
        wait_for(&p, "offset", |s| s.av_offset == MediaTime::from_millis(100));
        let out = p.video_output();
        let now = Instant::now();
        let diff = out.media_time_at(now) - out.clock().position();
        assert!((diff.as_secs_f64() - 0.1).abs() < 0.01);
    }

    #[test]
    fn trims_audio_on_seek() {
        let f = AudioFrame {
            pts: MediaTime::from_millis(1000),
            sample_rate: 1000,
            channels: 2,
            samples: vec![0.0; 200],
        };
        let mut d = Some(MediaTime::from_millis(1040));
        let t = trim_audio(f.clone(), &mut d).unwrap();
        assert_eq!(t.frames(), 60);
        assert_eq!(t.pts, MediaTime::from_millis(1040));
        assert!(d.is_none());
        let mut d = Some(MediaTime::from_millis(2000));
        assert!(trim_audio(f, &mut d).is_none());
        assert!(d.is_some());
    }
}
