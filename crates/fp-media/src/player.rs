//! The threaded player.
//!
//! Threads: a demuxer reading packets into per-stream queues, a video decoder
//! filling a small frame queue, and an audio thread that decodes, stretches,
//! syncs and writes to the device. The clock is video-master: it runs on wall
//! time at the playback speed, and audio drops or pads samples to follow it.
//!
//! Every seek bumps a generation counter ("serial"); packets and frames of an
//! older generation are discarded wherever they are found.
//!
//! The renderer drives presentation by calling [`Player::current_frame`] once
//! per display frame; that call also resolves buffering and end of stream.

use crate::audio::{self, ambisonic, output, resample::Resampler, stretch::Stretch};
use crate::clock::Clock;
use crate::decode::{Decoder, Frame, HwDecode, Packet};
use crate::frame::{FrameConverter, VideoFrame};
use crate::info::{MediaInfo, read_info};
use crate::input::Input;
use crate::subtitle::{self, Cue};
use crate::{Error, Result};
use fp_core::ByteSource;
use fp_ffmpeg_sys as ff;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Player settings.
#[derive(Clone, Debug)]
pub struct PlayerConfig {
    pub hw_decode: HwDecode,
    pub audio: output::Backend,
    /// Decoded frames kept ahead of the display.
    pub video_queue: usize,
    /// Demuxer read-ahead limit in seconds of video.
    pub read_ahead_secs: f64,
    /// Demuxer read-ahead limit in bytes (network sources).
    pub read_ahead_bytes: usize,
    /// Start paused at this position instead of 0.
    pub start_at: f64,
    pub start_paused: bool,
    pub volume: f32,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        PlayerConfig {
            hw_decode: HwDecode::Auto,
            audio: output::Backend::default(),
            video_queue: 8,
            read_ahead_secs: 4.0,
            read_ahead_bytes: 96 << 20,
            start_at: 0.0,
            start_paused: false,
            volume: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Buffering,
    Playing,
    Paused,
    Ended,
    Failed,
}

/// Counters for the debug overlay.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub video_decoder: String,
    pub hardware: bool,
    pub audio_decoder: String,
    pub audio_sink: String,
    pub frames_shown: u64,
    pub frames_dropped: u64,
    /// Times the clock was held because decoding fell behind.
    pub stalls: u64,
    pub video_queue: usize,
    pub packets_queued: usize,
    pub bytes_queued: usize,
}

struct PacketQueue {
    q: Mutex<VecDeque<(u64, Option<Packet>)>>,
    cv: Condvar,
    bytes: AtomicUsize,
}

impl PacketQueue {
    fn new() -> PacketQueue {
        PacketQueue {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            bytes: AtomicUsize::new(0),
        }
    }
    fn lock(&self) -> MutexGuard<'_, VecDeque<(u64, Option<Packet>)>> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn push(&self, serial: u64, p: Option<Packet>) {
        if let Some(p) = &p {
            self.bytes.fetch_add(p.size(), Ordering::Relaxed);
        }
        self.lock().push_back((serial, p));
        self.cv.notify_all();
    }
    fn pop(&self, timeout: Duration) -> Option<(u64, Option<Packet>)> {
        let mut q = self.lock();
        if q.is_empty() {
            q = self
                .cv
                .wait_timeout(q, timeout)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let item = q.pop_front();
        if let Some((_, Some(p))) = &item {
            self.bytes.fetch_sub(p.size(), Ordering::Relaxed);
        }
        item
    }
    fn clear(&self) {
        self.lock().clear();
        self.bytes.store(0, Ordering::Relaxed);
        self.cv.notify_all();
    }
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[derive(Clone, Copy)]
struct SeekRequest {
    target: f64,
    precise: bool,
}

struct Control {
    paused: bool,
    seek: Option<SeekRequest>,
    audio_stream: Option<usize>,
    audio_changed: bool,
    subtitle_stream: Option<usize>,
}

struct Shared {
    quit: Arc<AtomicBool>,
    clock: Clock,
    ctl: Mutex<Control>,
    ctl_cv: Condvar,
    vpkts: PacketQueue,
    apkts: PacketQueue,
    frames: Mutex<VecDeque<Arc<VideoFrame>>>,
    frames_cv: Condvar,
    current: Mutex<Option<Arc<VideoFrame>>>,
    serial: AtomicU64,
    /// Precise seek: frames before this time (in serial) are dropped.
    seek_floor: Mutex<Option<(u64, f64)>>,
    /// Waiting for the first frame after open/seek before the clock runs.
    buffering: AtomicBool,
    demux_eof: AtomicBool,
    video_done: AtomicBool,
    audio_done: AtomicBool,
    ended: AtomicBool,
    error: Mutex<Option<String>>,
    cues: Mutex<Vec<Cue>>,
    external_cues: Mutex<Vec<Cue>>,
    head: Mutex<ambisonic::Quat>,
    volume: Mutex<f32>,
    speed: Mutex<f64>,
    stats: Mutex<Stats>,
    has_video: bool,
    has_audio: AtomicBool,
    duration: f64,
    video_fps: f64,
}

impl Shared {
    fn ctl(&self) -> MutexGuard<'_, Control> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn frames(&self) -> MutexGuard<'_, VecDeque<Arc<VideoFrame>>> {
        self.frames.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn fail(&self, msg: String) {
        log::error!("player: {msg}");
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
    }
    fn stats(&self) -> MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn serial(&self) -> u64 {
        self.serial.load(Ordering::Acquire)
    }
}

pub struct Player {
    shared: Arc<Shared>,
    info: MediaInfo,
    threads: Vec<JoinHandle<()>>,
}

impl Player {
    /// Opens `src` and starts playback (or holds paused, per config).
    pub fn open(src: Arc<dyn ByteSource>, name: &str, config: PlayerConfig) -> Result<Player> {
        let quit = Arc::new(AtomicBool::new(false));
        let input = Input::open(src, name, quit.clone())?;
        let info = read_info(&input);
        if info.video.is_none() && info.audio.is_none() {
            return Err(Error::NoStream("video or audio"));
        }
        let shared = Arc::new(Shared {
            quit,
            clock: Clock::default(),
            ctl: Mutex::new(Control {
                paused: config.start_paused,
                seek: None,
                audio_stream: info.audio,
                audio_changed: false,
                subtitle_stream: info.subtitle_streams().next().map(|s| s.index),
            }),
            ctl_cv: Condvar::new(),
            vpkts: PacketQueue::new(),
            apkts: PacketQueue::new(),
            frames: Mutex::new(VecDeque::new()),
            frames_cv: Condvar::new(),
            current: Mutex::new(None),
            serial: AtomicU64::new(1),
            seek_floor: Mutex::new(None),
            buffering: AtomicBool::new(true),
            demux_eof: AtomicBool::new(false),
            video_done: AtomicBool::new(info.video.is_none()),
            audio_done: AtomicBool::new(info.audio.is_none()),
            ended: AtomicBool::new(false),
            error: Mutex::new(None),
            cues: Mutex::new(Vec::new()),
            external_cues: Mutex::new(Vec::new()),
            head: Mutex::new(ambisonic::IDENTITY),
            volume: Mutex::new(config.volume),
            speed: Mutex::new(1.0),
            stats: Mutex::new(Stats::default()),
            has_video: info.video.is_some(),
            has_audio: AtomicBool::new(info.audio.is_some()),
            duration: info.duration,
            video_fps: info
                .video_stream()
                .map(|v| v.fps)
                .filter(|f| *f > 0.0)
                .unwrap_or(30.0),
        });

        // Decoders are opened here so errors surface from open().
        let video_dec = match info.video {
            Some(i) => Some(Decoder::open(input.streams()[i], config.hw_decode)?),
            None => None,
        };
        if let Some(d) = &video_dec {
            let mut st = shared.stats();
            st.video_decoder = d.name.clone();
            st.hardware = d.hardware;
        }
        let audio_streams: Vec<(usize, *mut ff::AVStream)> = info
            .audio_streams()
            .map(|s| (s.index, input.streams()[s.index]))
            .collect();
        let mut audio_decoders = Vec::new();
        for (i, st) in audio_streams {
            match Decoder::open(st, HwDecode::Off) {
                Ok(d) => audio_decoders.push((i, d)),
                Err(e) => log::warn!("audio stream {i}: {e}"),
            }
        }
        let subtitle_decoders: Vec<(usize, Decoder)> = info
            .subtitle_streams()
            .filter_map(|s| {
                Decoder::open(input.streams()[s.index], HwDecode::Off)
                    .ok()
                    .map(|d| (s.index, d))
            })
            .collect();

        let mut threads = Vec::new();
        if config.start_at > 0.0 {
            shared.ctl().seek = Some(SeekRequest {
                target: config.start_at,
                precise: true,
            });
        }
        {
            let sh = shared.clone();
            let video_index = info.video;
            let cfg = config.clone();
            threads.push(spawn("fp-demux", move || {
                demux_thread(sh, input, video_index, subtitle_decoders, cfg)
            }));
        }
        if let Some(dec) = video_dec {
            let sh = shared.clone();
            let cap = config.video_queue.max(2);
            threads.push(spawn("fp-video", move || video_thread(sh, dec, cap)));
        }
        if !audio_decoders.is_empty() {
            let sh = shared.clone();
            let ambi: Vec<usize> = info
                .audio_streams()
                .filter(|s| s.ambisonic && s.channels == 4)
                .map(|s| s.index)
                .collect();
            let backend = config.audio.clone();
            threads.push(spawn("fp-audio", move || {
                audio_thread(sh, audio_decoders, ambi, backend)
            }));
        } else {
            shared.has_audio.store(false, Ordering::Release);
            shared.audio_done.store(true, Ordering::Release);
        }
        Ok(Player {
            shared,
            info,
            threads,
        })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// Current media time in seconds.
    pub fn position(&self) -> f64 {
        let t = self.shared.clock.now();
        if self.shared.duration > 0.0 {
            t.clamp(0.0, self.shared.duration)
        } else {
            t.max(0.0)
        }
    }

    pub fn duration(&self) -> f64 {
        self.shared.duration
    }

    pub fn state(&self) -> PlayerState {
        let s = &self.shared;
        if s.error.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            PlayerState::Failed
        } else if s.ended.load(Ordering::Acquire) {
            PlayerState::Ended
        } else if s.ctl().paused {
            PlayerState::Paused
        } else if s.buffering.load(Ordering::Acquire) {
            PlayerState::Buffering
        } else {
            PlayerState::Playing
        }
    }

    pub fn error(&self) -> Option<String> {
        self.shared
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_paused(&self) -> bool {
        self.shared.ctl().paused
    }

    pub fn play(&self) {
        let s = &self.shared;
        if s.ended.load(Ordering::Acquire) {
            self.seek(0.0, true);
        }
        s.ctl().paused = false;
        if !s.buffering.load(Ordering::Acquire) {
            s.clock.resume();
        }
        s.ctl_cv.notify_all();
    }

    pub fn pause(&self) {
        let s = &self.shared;
        s.ctl().paused = true;
        s.clock.pause();
        s.ctl_cv.notify_all();
    }

    pub fn toggle_pause(&self) {
        if self.is_paused() || self.state() == PlayerState::Ended {
            self.play()
        } else {
            self.pause()
        }
    }

    /// Seeks to `target` seconds. `precise` decodes up to the exact time;
    /// otherwise playback resumes from the nearest earlier keyframe (fast
    /// scrubbing).
    pub fn seek(&self, target: f64, precise: bool) {
        let s = &self.shared;
        let target = if s.duration > 0.0 {
            target.clamp(0.0, (s.duration - 0.05).max(0.0))
        } else {
            target.max(0.0)
        };
        s.ctl().seek = Some(SeekRequest { target, precise });
        // Reflect the new position immediately in the UI.
        s.clock.set(target);
        s.ended.store(false, Ordering::Release);
        s.ctl_cv.notify_all();
        s.frames_cv.notify_all();
    }

    pub fn seek_relative(&self, delta: f64) {
        self.seek(self.position() + delta, true);
    }

    pub fn speed(&self) -> f64 {
        *self.shared.speed.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_speed(&self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        *self.shared.speed.lock().unwrap_or_else(|e| e.into_inner()) = speed;
        self.shared.clock.set_speed(speed);
    }

    pub fn volume(&self) -> f32 {
        *self.shared.volume.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_volume(&self, v: f32) {
        *self.shared.volume.lock().unwrap_or_else(|e| e.into_inner()) = v.clamp(0.0, 2.0);
    }

    /// Selects an audio stream by stream index.
    pub fn select_audio(&self, index: usize) {
        let mut c = self.shared.ctl();
        if c.audio_stream != Some(index) {
            c.audio_stream = Some(index);
            c.audio_changed = true;
        }
        drop(c);
        self.seek(self.position(), true);
    }

    pub fn audio_stream(&self) -> Option<usize> {
        self.shared.ctl().audio_stream
    }

    /// Selects an embedded subtitle stream (`None` hides embedded subtitles).
    pub fn select_subtitle(&self, index: Option<usize>) {
        self.shared.ctl().subtitle_stream = index;
        self.shared
            .cues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.seek(self.position(), true);
    }

    pub fn subtitle_stream(&self) -> Option<usize> {
        self.shared.ctl().subtitle_stream
    }

    /// Loads an external subtitle file; replaces any previous one.
    pub fn load_subtitles(&self, src: Arc<dyn ByteSource>, name: &str) -> Result<usize> {
        let cues = subtitle::load_file(src, name)?;
        let n = cues.len();
        *self
            .shared
            .external_cues
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = cues;
        Ok(n)
    }

    pub fn clear_external_subtitles(&self) {
        self.shared
            .external_cues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Subtitle cues visible now (external file first, then embedded).
    pub fn subtitles(&self) -> Vec<Cue> {
        let t = self.position();
        let ext = self
            .shared
            .external_cues
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !ext.is_empty() {
            return subtitle::active(&ext, t).into_iter().cloned().collect();
        }
        let cues = self.shared.cues.lock().unwrap_or_else(|e| e.into_inner());
        subtitle::active(&cues, t).into_iter().cloned().collect()
    }

    /// Head orientation for ambisonic audio (OpenXR quaternion x, y, z, w).
    pub fn set_head_orientation(&self, q: [f32; 4]) {
        *self.shared.head.lock().unwrap_or_else(|e| e.into_inner()) = q;
    }

    pub fn stats(&self) -> Stats {
        let s = &self.shared;
        let mut st = s.stats().clone();
        st.video_queue = s.frames().len();
        st.packets_queued = s.vpkts.len() + s.apkts.len();
        st.bytes_queued =
            s.vpkts.bytes.load(Ordering::Relaxed) + s.apkts.bytes.load(Ordering::Relaxed);
        st
    }

    /// The frame to display now. Call once per display frame: it advances the
    /// frame queue, starts the clock once the first frame after a seek is
    /// ready, pauses it while starved, and detects the end of the media.
    pub fn current_frame(&self) -> Option<Arc<VideoFrame>> {
        let s = &self.shared;
        let serial = s.serial();
        let paused = s.ctl().paused;
        let mut frames = s.frames();
        frames.retain(|f| f.serial == serial);

        if s.buffering.load(Ordering::Acquire) {
            let ready = if s.has_video {
                !frames.is_empty() || s.video_done.load(Ordering::Acquire)
            } else {
                true
            };
            if ready && s.ctl().seek.is_none() {
                if let Some(f) = frames.pop_front() {
                    // Start the clock exactly at the first frame shown.
                    s.clock.set(f.pts);
                    *s.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
                    s.stats().frames_shown += 1;
                    s.frames_cv.notify_all();
                }
                s.buffering.store(false, Ordering::Release);
                if !paused {
                    s.clock.resume();
                }
            }
        } else {
            let t = s.clock.now();
            let late = (3.0 / s.video_fps.max(1.0)).clamp(0.03, 0.1);
            let mut shown: Option<Arc<VideoFrame>> = None;
            let mut dropped = 0;
            if let Some(i) = pick_due(frames.iter().map(|f| f.pts), t, late) {
                frames.drain(..i);
                dropped = i as u64;
                shown = frames.pop_front();
            }
            if let Some(f) = shown {
                *s.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
                let mut st = s.stats();
                st.frames_shown += 1;
                st.frames_dropped += dropped;
                s.frames_cv.notify_all();
            }
            let video_drained =
                !s.has_video || (s.video_done.load(Ordering::Acquire) && frames.is_empty());
            let audio_drained =
                !s.has_audio.load(Ordering::Acquire) || s.audio_done.load(Ordering::Acquire);
            if video_drained && audio_drained && s.demux_eof.load(Ordering::Acquire) {
                if !s.ended.swap(true, Ordering::AcqRel) {
                    s.clock.pause();
                    if s.duration > 0.0 {
                        s.clock.set(s.duration);
                    }
                }
            } else if s.has_video
                && frames.is_empty()
                && !s.video_done.load(Ordering::Acquire)
                && !paused
            {
                // Starved: hold the clock until decoding catches up.
                let behind = s
                    .current
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|c| t - c.pts)
                    .unwrap_or(1.0);
                if behind > 2.0 / s.video_fps.max(1.0) + 0.25 {
                    s.buffering.store(true, Ordering::Release);
                    s.clock.pause();
                    s.stats().stalls += 1;
                }
            }
        }
        s.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stops all threads. Also done on drop.
    pub fn close(&mut self) {
        let s = &self.shared;
        s.quit.store(true, Ordering::Release);
        s.ctl_cv.notify_all();
        s.frames_cv.notify_all();
        s.vpkts.cv.notify_all();
        s.apkts.cv.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.close();
    }
}

/// Which queued frame (times oldest first) to show at clock `t`; the ones
/// before it are dropped. The oldest due frame at most `late` behind the
/// clock, so frames a software decoder delivers in bursts while behind are
/// shown in turn (the newest alone would waste the rest of the burst);
/// frames further behind are dropped to catch up. With every due frame
/// that late, the newest. `None` when nothing is due.
fn pick_due(pts: impl Iterator<Item = f64>, t: f64, late: f64) -> Option<usize> {
    let mut newest_due = None;
    for (i, p) in pts.enumerate() {
        if p > t + 0.002 {
            break;
        }
        if p >= t - late {
            return Some(i);
        }
        newest_due = Some(i);
    }
    newest_due
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(h) => h,
        Err(e) => panic!("cannot start thread {name}: {e}"),
    }
}

struct DemuxPtrs(Vec<(usize, Decoder)>);
// SAFETY: subtitle decoders move into the demux thread and stay there.
unsafe impl Send for DemuxPtrs {}

fn demux_thread(
    sh: Arc<Shared>,
    mut input: Input,
    video_index: Option<usize>,
    subtitle_decoders: Vec<(usize, Decoder)>,
    cfg: PlayerConfig,
) {
    let mut subs = DemuxPtrs(subtitle_decoders);
    let mut pkt = Packet::new();
    let streams: Vec<(f64, f64)> = input
        .streams()
        .iter()
        // SAFETY: stream pointers are valid while input lives.
        .map(|&st| unsafe { (crate::q2d((*st).time_base), (*st).start_time as f64) })
        .collect();
    let mut last_video_ts = 0.0f64;
    let mut first_video_ts: Option<f64> = None;
    let mut read_errors = 0;
    while !sh.quit.load(Ordering::Acquire) {
        let (seek, audio_stream, sub_stream) = {
            let mut c = sh.ctl();
            (c.seek.take(), c.audio_stream, c.subtitle_stream)
        };
        if let Some(req) = seek {
            let serial = sh.serial.fetch_add(1, Ordering::AcqRel) + 1;
            sh.vpkts.clear();
            sh.apkts.clear();
            sh.frames().clear();
            *sh.seek_floor.lock().unwrap_or_else(|e| e.into_inner()) =
                req.precise.then_some((serial, req.target));
            if let Err(e) = input.seek(req.target) {
                log::warn!("seek to {:.2}: {e}", req.target);
            }
            for (_, d) in subs.0.iter_mut() {
                d.flush();
            }
            sh.demux_eof.store(false, Ordering::Release);
            sh.video_done.store(!sh.has_video, Ordering::Release);
            sh.audio_done
                .store(!sh.has_audio.load(Ordering::Acquire), Ordering::Release);
            sh.ended.store(false, Ordering::Release);
            sh.buffering.store(true, Ordering::Release);
            sh.clock.pause();
            sh.clock.set(req.target);
            first_video_ts = None;
            sh.frames_cv.notify_all();
        }
        if sh.demux_eof.load(Ordering::Acquire) {
            let c = sh.ctl();
            let _ = sh.ctl_cv.wait_timeout(c, Duration::from_millis(50));
            continue;
        }
        // Back-pressure: enough read ahead?
        let ahead = first_video_ts
            .map(|f| last_video_ts - f.max(sh.clock.now()))
            .unwrap_or(0.0);
        let bytes = sh.vpkts.bytes.load(Ordering::Relaxed) + sh.apkts.bytes.load(Ordering::Relaxed);
        let audio_starved = sh.has_audio.load(Ordering::Acquire) && sh.apkts.len() < 4;
        if (ahead > cfg.read_ahead_secs || bytes > cfg.read_ahead_bytes || sh.vpkts.len() > 600)
            && !audio_starved
        {
            let c = sh.ctl();
            let _ = sh.ctl_cv.wait_timeout(c, Duration::from_millis(10));
            continue;
        }
        match input.read(pkt.as_ptr()) {
            Ok(true) => {
                read_errors = 0;
                let serial = sh.serial();
                let idx = pkt.stream_index();
                if Some(idx) == video_index {
                    // SAFETY: valid packet.
                    let p = unsafe { &*pkt.as_ptr() };
                    let ts = if p.pts != ff::AV_NOPTS_VALUE {
                        p.pts
                    } else {
                        p.dts
                    };
                    if ts != ff::AV_NOPTS_VALUE {
                        let t = ts as f64 * streams[idx].0;
                        last_video_ts = t;
                        first_video_ts.get_or_insert(t);
                    }
                    sh.vpkts.push(serial, Some(pkt.take()));
                } else if Some(idx) == audio_stream {
                    sh.apkts.push(serial, Some(pkt.take()));
                } else if Some(idx) == sub_stream {
                    if let Some((_, dec)) = subs.0.iter_mut().find(|(i, _)| *i == idx)
                        && let Some(cue) = subtitle::decode_packet(dec, &pkt)
                    {
                        let mut cues = sh.cues.lock().unwrap_or_else(|e| e.into_inner());
                        if !cues
                            .iter()
                            .any(|c| (c.start - cue.start).abs() < 1e-3 && c.text == cue.text)
                        {
                            let pos = cues.partition_point(|c| c.start <= cue.start);
                            cues.insert(pos, cue);
                        }
                    }
                    // SAFETY: valid packet.
                    unsafe { ff::av_packet_unref(pkt.as_ptr()) };
                } else {
                    // SAFETY: valid packet.
                    unsafe { ff::av_packet_unref(pkt.as_ptr()) };
                }
            }
            Ok(false) => {
                let serial = sh.serial();
                sh.vpkts.push(serial, None);
                sh.apkts.push(serial, None);
                sh.demux_eof.store(true, Ordering::Release);
            }
            Err(e) => {
                if sh.quit.load(Ordering::Acquire) {
                    break;
                }
                read_errors += 1;
                if read_errors > 20 {
                    sh.fail(format!("reading media: {e}"));
                    let serial = sh.serial();
                    sh.vpkts.push(serial, None);
                    sh.apkts.push(serial, None);
                    sh.demux_eof.store(true, Ordering::Release);
                } else {
                    std::thread::sleep(Duration::from_millis(50 * read_errors));
                }
            }
        }
    }
}

fn video_thread(sh: Arc<Shared>, mut dec: Decoder, cap: usize) {
    let mut serial = 0u64;
    let mut frame = Frame::new();
    let mut conv = FrameConverter::default();
    let default_duration = 1.0 / sh.video_fps.max(1.0);
    while !sh.quit.load(Ordering::Acquire) {
        let Some((pkt_serial, pkt)) = sh.vpkts.pop(Duration::from_millis(20)) else {
            continue;
        };
        if pkt_serial != sh.serial() {
            continue; // stale (seek happened after it was queued)
        }
        if pkt_serial != serial {
            serial = pkt_serial;
            dec.flush();
        }
        let pkt_ptr = pkt
            .as_ref()
            .map(|p| p.as_ptr() as *const ff::AVPacket)
            .unwrap_or(std::ptr::null());
        loop {
            let accepted = match dec.send(pkt_ptr) {
                Ok(a) => a,
                Err(e) => {
                    log::warn!("video decode: {e}");
                    true
                }
            };
            // Drain everything ready.
            loop {
                match dec.receive(&mut frame) {
                    Ok(Some(true)) => {
                        let pts = dec.frame_time(&frame).unwrap_or(0.0);
                        let dur = Some(dec.frame_duration(&frame))
                            .filter(|d| *d > 0.0)
                            .unwrap_or(default_duration);
                        let floor = *sh.seek_floor.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some((fs, target)) = floor
                            && fs == serial
                            && pts + dur * 0.5 < target
                        {
                            // SAFETY: valid frame; drop its reference.
                            unsafe { ff::av_frame_unref(frame.as_ptr()) };
                            continue;
                        }
                        let vf = match conv.convert(frame.take_raw(), pts, dur, serial) {
                            Ok(v) => Arc::new(v),
                            Err(e) => {
                                log::warn!("frame conversion: {e}");
                                continue;
                            }
                        };
                        // Wait for room in the frame queue.
                        let mut q = sh.frames();
                        while q.len() >= cap
                            && !sh.quit.load(Ordering::Acquire)
                            && sh.serial() == serial
                        {
                            q = sh
                                .frames_cv
                                .wait_timeout(q, Duration::from_millis(20))
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                        if sh.serial() == serial {
                            q.push_back(vf);
                        }
                    }
                    Ok(Some(false)) => {
                        if sh.serial() == serial {
                            sh.video_done.store(true, Ordering::Release);
                        }
                        break;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::warn!("video decode: {e}");
                        break;
                    }
                }
                if sh.quit.load(Ordering::Acquire) || sh.serial() != serial {
                    break;
                }
            }
            if accepted || sh.quit.load(Ordering::Acquire) || sh.serial() != serial {
                break;
            }
        }
    }
}

fn audio_thread(
    sh: Arc<Shared>,
    mut decoders: Vec<(usize, Decoder)>,
    ambisonic_streams: Vec<usize>,
    backend: output::Backend,
) {
    let mut sink = output::open(&backend, 2);
    sh.stats().audio_sink = sink.name();
    let mut serial = 0u64;
    let mut current = sh.ctl().audio_stream;
    let mut resampler =
        Resampler::new(current.filter(|i| ambisonic_streams.contains(i)).map(|_| 4));
    let mut stretch = Stretch::new(2, audio::RATE);
    let mut frame = Frame::new();
    let mut buf: Vec<f32> = Vec::new();
    let mut was_paused = false;
    if let Some((_, d)) = decoders.iter().find(|(i, _)| Some(*i) == current) {
        sh.stats().audio_decoder = d.name.clone();
    }
    while !sh.quit.load(Ordering::Acquire) {
        // Track switch.
        {
            let mut c = sh.ctl();
            if c.audio_changed {
                c.audio_changed = false;
                current = c.audio_stream;
                resampler =
                    Resampler::new(current.filter(|i| ambisonic_streams.contains(i)).map(|_| 4));
                if let Some((_, d)) = decoders.iter().find(|(i, _)| Some(*i) == current) {
                    sh.stats().audio_decoder = d.name.clone();
                }
            }
        }
        let paused = sh.ctl().paused;
        if paused || sh.buffering.load(Ordering::Acquire) {
            if paused && !was_paused {
                sink.flush();
            }
            was_paused = paused;
            if sh.apkts.len() > 0 && sh.serial() != serial {
                // Drain stale packets while waiting.
            }
            let c = sh.ctl();
            let _ = sh.ctl_cv.wait_timeout(c, Duration::from_millis(10));
            if !paused {
                // Buffering after a seek: keep decoding so audio is ready,
                // but do not output until the clock runs.
                if sh.buffering.load(Ordering::Acquire) {
                    continue;
                }
            } else {
                continue;
            }
        }
        was_paused = false;
        let Some((pkt_serial, pkt)) = sh.apkts.pop(Duration::from_millis(20)) else {
            continue;
        };
        if pkt_serial != sh.serial() {
            continue;
        }
        let Some(dec) = decoders
            .iter_mut()
            .find(|(i, _)| Some(*i) == current)
            .map(|(_, d)| d)
        else {
            sh.audio_done.store(true, Ordering::Release);
            continue;
        };
        if pkt_serial != serial {
            serial = pkt_serial;
            dec.flush();
            resampler.reset();
            stretch.reset();
            sink.flush();
        }
        let Some(pkt) = pkt else {
            sh.audio_done.store(true, Ordering::Release);
            continue;
        };
        if let Err(e) = dec.send(pkt.as_ptr()) {
            log::debug!("audio decode: {e}");
            continue;
        }
        while let Ok(Some(true)) = dec.receive(&mut frame) {
            let Some(pts) = dec.frame_time(&frame) else {
                continue;
            };
            buf.clear();
            if let Err(e) = resampler.convert(&frame, &mut buf) {
                log::warn!("{e}");
                continue;
            }
            // SAFETY: valid frame.
            unsafe { ff::av_frame_unref(frame.as_ptr()) };
            let mut out = if resampler.out_channels == 4 {
                let head = *sh.head.lock().unwrap_or_else(|e| e.into_inner());
                ambisonic::render_stereo(&buf, head)
            } else {
                std::mem::take(&mut buf)
            };
            let volume = *sh.volume.lock().unwrap_or_else(|e| e.into_inner());
            if (volume - 1.0).abs() > 1e-3 {
                out.iter_mut().for_each(|s| *s *= volume);
            }
            let speed = *sh.speed.lock().unwrap_or_else(|e| e.into_inner());
            if (stretch.speed() - speed).abs() > 1e-6 {
                stretch.set_speed(speed);
            }
            let out = stretch.process(&out);
            if out.is_empty() {
                continue;
            }
            // Sync against the video clock: when will this chunk be heard,
            // and what media time will the clock show then?
            let heard_at = sh.clock.now() + sink.delay() * speed;
            let diff = pts - heard_at;
            let samples: &[f32] = if diff < -0.06 {
                let skip = ((-diff / speed) * audio::RATE as f64) as usize * 2;
                if skip >= out.len() {
                    continue;
                } else {
                    &out[skip..]
                }
            } else {
                if diff > 0.06 {
                    let pad = ((diff / speed).min(0.5) * audio::RATE as f64) as usize * 2;
                    let _ = sink.write(&vec![0.0; pad]);
                }
                &out
            };
            if let Err(e) = sink.write(samples) {
                log::warn!("{e}");
                sink = output::open(&output::Backend::Null, 2);
            }
            if sh.quit.load(Ordering::Acquire) || sh.serial() != serial || sh.ctl().paused {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;
    use std::time::Instant;

    fn open(name: &str, cfg: PlayerConfig) -> Player {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        Player::open(Arc::new(FileSource::open(&path).unwrap()), name, cfg).unwrap()
    }

    fn cfg() -> PlayerConfig {
        PlayerConfig {
            audio: output::Backend::Null,
            ..Default::default()
        }
    }

    /// Runs the display loop at ~90 Hz until `until` returns true or timeout.
    fn run(
        p: &Player,
        secs: f64,
        mut until: impl FnMut(&Player, Option<&Arc<VideoFrame>>) -> bool,
    ) -> Vec<f64> {
        let start = Instant::now();
        let mut shown = Vec::new();
        while start.elapsed().as_secs_f64() < secs {
            let f = p.current_frame();
            if let Some(f) = &f
                && shown.last() != Some(&f.pts)
            {
                shown.push(f.pts);
            }
            if until(p, f.as_ref()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(11));
        }
        shown
    }

    #[test]
    fn due_frames_are_shown_in_turn_unless_too_late() {
        let late = 0.05;
        let at = |pts: &[f64], t| pick_due(pts.iter().copied(), t, late);
        assert_eq!(at(&[], 1.0), None);
        assert_eq!(at(&[1.1, 1.2], 1.0), None, "nothing due yet");
        assert_eq!(at(&[1.0, 1.1], 1.0), Some(0));
        // A burst of due frames within the tolerance: oldest first.
        assert_eq!(at(&[0.97, 0.985, 1.0], 1.0), Some(0));
        // Too late ones are dropped up to the first within tolerance.
        assert_eq!(at(&[0.90, 0.93, 0.96, 0.98, 1.2], 1.0), Some(2));
        // Everything due is too late: the newest.
        assert_eq!(at(&[0.80, 0.85, 0.90, 1.2], 1.0), Some(2));
    }

    #[test]
    fn plays_to_the_end_in_real_time() {
        let p = open("h264_aac_180_LR.mp4", cfg());
        let t0 = Instant::now();
        let shown = run(&p, 6.0, |p, _| p.state() == PlayerState::Ended);
        let el = t0.elapsed().as_secs_f64();
        assert_eq!(p.state(), PlayerState::Ended);
        assert!((1.8..3.5).contains(&el), "2 s clip took {el:.2} s");
        assert!(
            shown.len() > 50,
            "only {} distinct frames shown",
            shown.len()
        );
        assert!(shown.windows(2).all(|w| w[1] > w[0]), "frames out of order");
        assert!((p.position() - 2.0).abs() < 0.1);
        let st = p.stats();
        assert_eq!(st.video_decoder, "h264");
        assert_eq!(st.audio_sink, "null");
    }

    #[test]
    fn seek_precise_lands_on_target() {
        let p = open(
            "h264_aac_180_LR.mp4",
            PlayerConfig {
                start_paused: true,
                ..cfg()
            },
        );
        run(&p, 2.0, |_, f| f.is_some());
        p.seek(1.25, true);
        let _ = run(&p, 2.0, |p, f| {
            f.is_some_and(|f| f.pts >= 1.0) && p.state() != PlayerState::Buffering
        });
        let f = p.current_frame().unwrap();
        assert!((f.pts - 1.2333).abs() < 0.05, "landed at {}", f.pts);
        assert_eq!(p.state(), PlayerState::Paused);
        assert!((p.position() - f.pts).abs() < 1e-6);
    }

    #[test]
    fn pause_holds_position_and_speed_scales() {
        let p = open("av1_opus.webm", cfg());
        run(&p, 2.0, |p, _| p.position() > 0.2);
        p.pause();
        let a = p.position();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(p.position(), a);
        p.set_speed(2.0);
        p.play();
        let t0 = Instant::now();
        run(&p, 3.0, |p, _| p.state() == PlayerState::Ended);
        let el = t0.elapsed().as_secs_f64();
        // ~0.8 s of media left at 2x is ~0.4 s.
        assert!(el < 0.9, "took {el:.2}");
    }

    #[test]
    fn embedded_subtitles_follow_position() {
        let p = open(
            "hevc10_tb.mkv",
            PlayerConfig {
                start_paused: true,
                ..cfg()
            },
        );
        run(&p, 2.0, |_, f| f.is_some());
        p.seek(0.5, true);
        run(&p, 2.0, |p, f| {
            f.is_some_and(|f| f.pts >= 0.4) && p.state() != PlayerState::Buffering
        });
        // The demuxer reads ahead, so the cue for 0.2–1.0 s is known by now.
        let deadline = Instant::now() + Duration::from_secs(2);
        while p.subtitles().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            p.subtitles().first().map(|c| c.text.clone()).as_deref(),
            Some("Hello world")
        );
    }

    #[test]
    fn external_subtitles_override() {
        let p = open(
            "h264_aac_180_LR.mp4",
            PlayerConfig {
                start_paused: true,
                start_at: 1.5,
                ..cfg()
            },
        );
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/subs.srt");
        assert_eq!(
            p.load_subtitles(Arc::new(FileSource::open(&path).unwrap()), "subs.srt")
                .unwrap(),
            2
        );
        run(&p, 2.0, |p, f| {
            f.is_some() && p.state() == PlayerState::Paused
        });
        assert!((p.position() - 1.5).abs() < 0.05, "{}", p.position());
        assert_eq!(p.subtitles()[0].text, "Second line\nwith break");
    }

    #[test]
    fn video_only_file_ends() {
        let p = open("vp9.webm", cfg());
        run(&p, 4.0, |p, _| p.state() == PlayerState::Ended);
        assert_eq!(p.state(), PlayerState::Ended);
        p.play(); // restarts from the beginning
        run(&p, 2.0, |p, f| {
            f.is_some() && p.state() == PlayerState::Playing
        });
        assert!(p.position() < 0.5);
    }
}
