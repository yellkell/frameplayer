//! Runtime hardware → software decoder fallback.
//!
//! The V4L2 decoder can open fine and still fail later: CAPTURE setup on the
//! first `SOURCE_CHANGE`, `STREAMON`, a driver error on the first frames, or
//! a decoder that accepts bitstream but never produces a picture.
//! [`FallbackDecoder`] wraps the hardware decoder picked by
//! [`select_decoder`](super::select_decoder) and, until the hardware has
//! proven itself (returned [`FallbackPolicy::confirm_frames`] frames), keeps
//! a copy of every packet sent since open / the last flush (the engine always
//! restarts at a keyframe there). If the hardware errors, stalls, or drains
//! without output during that window, the software decoder is opened, the
//! buffered packets are replayed into it and decoding continues; frames the
//! hardware already returned are not emitted twice. Once confirmed, errors
//! pass through unchanged (the engine drops bad packets as before).
//!
//! The switch is reported once through
//! [`VideoDecoder::take_fallback_notice`] so the engine can update the
//! decoder path shown to the user.

use super::{DecodedFrame, DecoderPath, VideoDecoder};
use crate::error::{Result, VideoError};
use crate::packet::Packet;
use fp_core::MediaTime;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Opens the software decoder when the hardware fails.
pub type SoftwareFactory = Box<dyn FnMut() -> Result<Box<dyn VideoDecoder>> + Send>;

/// When the hardware decoder counts as failed.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackPolicy {
    /// Frames the hardware must return before fallback is disarmed.
    pub confirm_frames: u32,
    /// No frame at all this long after the first accepted packet = stalled.
    pub stall_timeout: Duration,
    /// Unconfirmed bitstream kept for replay; exceeding it without a single
    /// frame counts as a stall.
    pub max_buffered_bytes: usize,
    pub max_buffered_packets: usize,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        FallbackPolicy {
            confirm_frames: 8,
            // [verify] Generous for a 8K HEVC first frame on iris; tune on hardware.
            stall_timeout: Duration::from_secs(3),
            max_buffered_bytes: 64 << 20,
            max_buffered_packets: 300,
        }
    }
}

/// Software frames emitted after a switch before the retired hardware
/// decoder (whose DMA-BUFs may still be queued for display) is dropped.
const RETIRE_AFTER_FRAMES: u32 = 32;

pub struct FallbackDecoder {
    active: Box<dyn VideoDecoder>,
    /// Hardware decoder kept alive after a switch until its frames are gone.
    retired: Option<Box<dyn VideoDecoder>>,
    factory: Option<SoftwareFactory>,
    policy: FallbackPolicy,
    switched: bool,
    confirmed: bool,
    /// Packets accepted by the hardware since open/flush (unconfirmed only).
    history: Vec<Packet>,
    history_bytes: usize,
    /// Packets still to be fed to the software decoder after a switch.
    replay: VecDeque<Packet>,
    frames_out: u32,
    sw_frames_out: u32,
    last_out_pts: Option<MediaTime>,
    first_accept: Option<Instant>,
    drain_requested: bool,
    drain_forwarded: bool,
    notice: Option<String>,
}

impl FallbackDecoder {
    pub fn new(hardware: Box<dyn VideoDecoder>, software: SoftwareFactory) -> Self {
        Self::with_policy(hardware, software, FallbackPolicy::default())
    }

    pub fn with_policy(
        hardware: Box<dyn VideoDecoder>,
        software: SoftwareFactory,
        policy: FallbackPolicy,
    ) -> Self {
        FallbackDecoder {
            active: hardware,
            retired: None,
            factory: Some(software),
            policy,
            switched: false,
            confirmed: false,
            history: Vec::new(),
            history_bytes: 0,
            replay: VecDeque::new(),
            frames_out: 0,
            sw_frames_out: 0,
            last_out_pts: None,
            first_accept: None,
            drain_requested: false,
            drain_forwarded: false,
            notice: None,
        }
    }

    /// True once decoding moved to the software decoder.
    pub fn switched(&self) -> bool {
        self.switched
    }

    fn armed(&self) -> bool {
        !self.switched && !self.confirmed && self.factory.is_some()
    }

    /// Replace the hardware decoder with the software one and queue the
    /// buffered packets for replay. On failure the original error is
    /// returned (with the software error appended).
    fn switch(&mut self, reason: String) -> Result<()> {
        let Some(mut factory) = self.factory.take() else {
            return Err(VideoError::Device(reason));
        };
        let hw_path = self.active.path();
        match factory() {
            Ok(sw) => {
                let msg = format!("{hw_path} failed ({reason}); falling back to {}", sw.path());
                tracing::warn!("{msg}");
                self.notice = Some(msg);
                self.retired = Some(std::mem::replace(&mut self.active, sw));
                self.switched = true;
                self.replay = self.history.drain(..).collect();
                self.history_bytes = 0;
                self.drain_forwarded = false;
                Ok(())
            }
            Err(e) => {
                tracing::warn!("{hw_path} failed ({reason}) and no software decoder opened: {e}");
                Err(VideoError::Device(format!(
                    "{reason}; software fallback failed: {e}"
                )))
            }
        }
    }

    /// Feed queued replay packets to the software decoder. Returns whether
    /// the replay queue is empty.
    fn pump_replay(&mut self) -> Result<bool> {
        while let Some(p) = self.replay.front() {
            match self.active.send_packet(p) {
                Ok(true) => {
                    self.replay.pop_front();
                }
                Ok(false) => return Ok(false),
                Err(e) => {
                    tracing::warn!("software decoder dropped replayed packet at {}: {e}", p.pts);
                    self.replay.pop_front();
                }
            }
        }
        if self.drain_requested && !self.drain_forwarded {
            self.drain_forwarded = true;
            self.active.drain()?;
        }
        Ok(true)
    }

    fn stalled(&self) -> Option<String> {
        if !self.armed() || self.frames_out > 0 {
            return None;
        }
        if let Some(t) = self.first_accept {
            if t.elapsed() >= self.policy.stall_timeout {
                return Some(format!(
                    "no frame {:.1} s after the first packet",
                    t.elapsed().as_secs_f64()
                ));
            }
        }
        if self.history.len() > self.policy.max_buffered_packets
            || self.history_bytes > self.policy.max_buffered_bytes
        {
            return Some(format!(
                "no frame after {} packets ({} KiB)",
                self.history.len(),
                self.history_bytes / 1024
            ));
        }
        if self.drain_requested && self.active.is_drained() && !self.history.is_empty() {
            return Some("drained without producing a frame".into());
        }
        None
    }

    fn record(&mut self, pkt: &Packet) {
        if self.armed() {
            self.first_accept.get_or_insert_with(Instant::now);
            self.history_bytes += pkt.data.len();
            self.history.push(pkt.clone());
        }
    }

    fn receive_software(&mut self) -> Result<Option<DecodedFrame>> {
        self.pump_replay()?;
        loop {
            match self.active.receive_frame()? {
                Some(f) => {
                    // Already shown from the hardware decoder before the switch.
                    if self.last_out_pts.is_some_and(|l| f.pts() <= l) {
                        continue;
                    }
                    self.last_out_pts = Some(f.pts());
                    self.sw_frames_out += 1;
                    if self.sw_frames_out >= RETIRE_AFTER_FRAMES {
                        self.retired = None;
                    }
                    return Ok(Some(f));
                }
                None => {
                    // Room may have opened up for more replayed packets.
                    if !self.replay.is_empty() && self.pump_replay()? {
                        continue;
                    }
                    return Ok(None);
                }
            }
        }
    }
}

impl VideoDecoder for FallbackDecoder {
    fn path(&self) -> DecoderPath {
        self.active.path()
    }

    fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
        if self.switched {
            if !self.pump_replay()? {
                return Ok(false);
            }
            return self.active.send_packet(pkt);
        }
        if let Some(reason) = self.stalled() {
            self.switch(reason)?;
            return self.send_packet(pkt);
        }
        match self.active.send_packet(pkt) {
            Ok(true) => {
                self.record(pkt);
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e) if self.armed() => {
                // The packet goes to the software decoder with the rest.
                self.history.push(pkt.clone());
                self.switch(e.to_string())?;
                self.pump_replay()?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        if self.switched {
            return self.receive_software();
        }
        match self.active.receive_frame() {
            Ok(Some(f)) => {
                self.frames_out += 1;
                if self.frames_out >= self.policy.confirm_frames && !self.confirmed {
                    self.confirmed = true;
                    self.history = Vec::new();
                    self.history_bytes = 0;
                    self.factory = None;
                    tracing::debug!(
                        "hardware decoder confirmed after {} frames",
                        self.frames_out
                    );
                }
                self.last_out_pts = Some(self.last_out_pts.map_or(f.pts(), |l| l.max(f.pts())));
                Ok(Some(f))
            }
            Ok(None) => match self.stalled() {
                Some(reason) => {
                    self.switch(reason)?;
                    self.receive_software()
                }
                None => Ok(None),
            },
            Err(e) if self.armed() => {
                self.switch(e.to_string())?;
                self.receive_software()
            }
            Err(e) => Err(e),
        }
    }

    fn drain(&mut self) -> Result<()> {
        self.drain_requested = true;
        if self.switched {
            self.pump_replay()?;
            return Ok(());
        }
        match self.active.drain() {
            Ok(()) => Ok(()),
            Err(e) if self.armed() => {
                self.switch(e.to_string())?;
                self.pump_replay()?;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn is_drained(&self) -> bool {
        if self.switched {
            self.replay.is_empty() && self.drain_forwarded && self.active.is_drained()
        } else {
            // A hardware decoder that drained without output is about to be
            // replaced (see `stalled`), so it is not "done" yet.
            self.active.is_drained()
                && !(self.armed() && self.frames_out == 0 && !self.history.is_empty())
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.replay.clear();
        self.history.clear();
        self.history_bytes = 0;
        self.first_accept = None;
        self.last_out_pts = None;
        self.drain_requested = false;
        self.drain_forwarded = false;
        match self.active.flush() {
            Ok(()) => Ok(()),
            // Nothing to replay after a flush: just start the software decoder.
            Err(e) if self.armed() => self.switch(e.to_string()),
            Err(e) => Err(e),
        }
    }

    fn wait(&mut self, timeout: Duration) {
        if !(self.switched && !self.replay.is_empty()) {
            self.active.wait(timeout);
        }
    }

    fn take_fallback_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::decode::{CpuFrame, PixelFormat};
    use crate::mock::MockVideoDecoder;

    /// How the fake hardware decoder misbehaves.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub(crate) enum Failure {
        /// Works.
        None,
        /// `receive_frame` errors (e.g. CAPTURE setup / STREAMON failure).
        ReceiveError,
        /// `send_packet` errors after `n` packets.
        SendErrorAfter(usize),
        /// Accepts everything, never outputs.
        Silent,
        /// Outputs `n` frames, then errors.
        ErrorAfterFrames(u32),
    }

    pub(crate) struct FailingHw {
        pub failure: Failure,
        inner: MockVideoDecoder,
        sent: usize,
        out: u32,
    }

    impl FailingHw {
        pub(crate) fn new(failure: Failure) -> Self {
            FailingHw {
                failure,
                inner: MockVideoDecoder::default(),
                sent: 0,
                out: 0,
            }
        }
    }

    impl VideoDecoder for FailingHw {
        fn path(&self) -> DecoderPath {
            DecoderPath::Hardware {
                device: "/dev/video-mock".into(),
                driver: "failing".into(),
            }
        }
        fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
            if let Failure::SendErrorAfter(n) = self.failure {
                if self.sent >= n {
                    return Err(VideoError::Device("QBUF(OUTPUT): EIO".into()));
                }
            }
            if self.failure == Failure::Silent {
                self.sent += 1;
                return Ok(true);
            }
            let r = self.inner.send_packet(pkt)?;
            if r {
                self.sent += 1;
            }
            Ok(r)
        }
        fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
            match self.failure {
                Failure::ReceiveError => {
                    Err(VideoError::Device("STREAMON(CAPTURE): EINVAL".into()))
                }
                Failure::Silent => Ok(None),
                Failure::ErrorAfterFrames(n) if self.out >= n => {
                    Err(VideoError::Device("DQBUF(CAPTURE): EPIPE".into()))
                }
                _ => {
                    let f = self.inner.receive_frame()?;
                    if f.is_some() {
                        self.out += 1;
                    }
                    Ok(f)
                }
            }
        }
        fn drain(&mut self) -> Result<()> {
            self.inner.drain()
        }
        fn is_drained(&self) -> bool {
            match self.failure {
                Failure::Silent => true,
                _ => self.inner.is_drained(),
            }
        }
        fn flush(&mut self) -> Result<()> {
            self.inner.flush()
        }
    }

    fn packets(n: u64) -> Vec<Packet> {
        (0..n)
            .map(|i| Packet {
                track: 1,
                pts: MediaTime::from_millis(i as i64 * 40),
                dts: MediaTime::from_millis(i as i64 * 40),
                duration: MediaTime::from_millis(40),
                keyframe: i % 25 == 0,
                data: vec![i as u8; 16],
            })
            .collect()
    }

    fn sw_factory() -> SoftwareFactory {
        Box::new(|| Ok(Box::new(MockVideoDecoder::default()) as Box<dyn VideoDecoder>))
    }

    /// Run the engine's decode protocol to the end; returns frame pts (ms).
    fn run(dec: &mut dyn VideoDecoder, pkts: &[Packet]) -> Vec<i64> {
        let mut out = Vec::new();
        let mut i = 0;
        let mut guard = 0;
        let mut drain_sent = false;
        loop {
            guard += 1;
            assert!(guard < 10_000, "decoder never finished");
            while let Some(f) = dec.receive_frame().unwrap() {
                if let DecodedFrame::Cpu(CpuFrame { format, .. }) = &f {
                    assert_eq!(*format, PixelFormat::Nv12);
                }
                out.push(f.pts().0 / 1000);
            }
            while i < pkts.len() {
                match dec.send_packet(&pkts[i]) {
                    Ok(true) => i += 1,
                    Ok(false) => break,
                    Err(e) => panic!("send failed: {e}"),
                }
            }
            if i == pkts.len() && !drain_sent {
                drain_sent = true;
                dec.drain().unwrap();
            }
            if drain_sent && dec.is_drained() {
                // Final receive after drained flag.
                while let Some(f) = dec.receive_frame().unwrap() {
                    out.push(f.pts().0 / 1000);
                }
                return out;
            }
        }
    }

    fn expect_all(out: &[i64], n: i64) {
        let want: Vec<i64> = (0..n).map(|i| i * 40).collect();
        assert_eq!(out, want.as_slice());
    }

    #[test]
    fn working_hardware_is_kept() {
        let mut d = FallbackDecoder::new(Box::new(FailingHw::new(Failure::None)), sw_factory());
        let out = run(&mut d, &packets(50));
        expect_all(&out, 50);
        assert!(!d.switched());
        assert!(matches!(d.path(), DecoderPath::Hardware { .. }));
        assert!(d.take_fallback_notice().is_none());
    }

    #[test]
    fn receive_error_falls_back_and_replays() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::ReceiveError)),
            sw_factory(),
        );
        let out = run(&mut d, &packets(50));
        expect_all(&out, 50);
        assert!(d.switched());
        assert_eq!(
            d.path(),
            DecoderPath::Software {
                library: "mock".into()
            }
        );
        let n = d.take_fallback_notice().unwrap();
        assert!(
            n.contains("STREAMON") && n.contains("software (mock)"),
            "{n}"
        );
        assert!(
            d.take_fallback_notice().is_none(),
            "notice is reported once"
        );
    }

    #[test]
    fn send_error_falls_back() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::SendErrorAfter(2))),
            sw_factory(),
        );
        let out = run(&mut d, &packets(30));
        expect_all(&out, 30);
        assert!(d.switched());
    }

    #[test]
    fn silent_hardware_falls_back_on_stall() {
        let policy = FallbackPolicy {
            stall_timeout: Duration::from_millis(30),
            ..Default::default()
        };
        let mut d = FallbackDecoder::with_policy(
            Box::new(FailingHw::new(Failure::Silent)),
            sw_factory(),
            policy,
        );
        let pkts = packets(20);
        for p in &pkts[..5] {
            assert!(d.send_packet(p).unwrap());
        }
        assert!(d.receive_frame().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(40));
        let f = d
            .receive_frame()
            .unwrap()
            .expect("software output after stall");
        assert_eq!(f.pts(), MediaTime::ZERO);
        let mut rest = vec![0];
        rest.extend(run(&mut d, &pkts[5..]));
        expect_all(&rest, 20);
    }

    #[test]
    fn silent_hardware_falls_back_at_drain() {
        let mut d = FallbackDecoder::new(Box::new(FailingHw::new(Failure::Silent)), sw_factory());
        let out = run(&mut d, &packets(3));
        expect_all(&out, 3);
        assert!(d.switched());
    }

    #[test]
    fn early_mid_stream_error_skips_frames_already_shown() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::ErrorAfterFrames(3))),
            sw_factory(),
        );
        let out = run(&mut d, &packets(40));
        expect_all(&out, 40);
        assert!(d.switched());
    }

    #[test]
    fn confirmed_hardware_errors_pass_through() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::ErrorAfterFrames(10))),
            sw_factory(),
        );
        let pkts = packets(40);
        let mut frames = 0;
        let mut err = None;
        'outer: for p in &pkts {
            while !d.send_packet(p).unwrap() {
                match d.receive_frame() {
                    Ok(Some(_)) => frames += 1,
                    Ok(None) => {}
                    Err(e) => {
                        err = Some(e);
                        break 'outer;
                    }
                }
            }
        }
        assert_eq!(frames, 10);
        assert!(err.is_some_and(|e| e.to_string().contains("EPIPE")));
        assert!(!d.switched());
    }

    #[test]
    fn failing_software_factory_reports_both_errors() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::ReceiveError)),
            Box::new(|| Err(VideoError::NoSoftwareDecoder("none".into()))),
        );
        d.send_packet(&packets(1)[0]).unwrap();
        let e = d.receive_frame().unwrap_err().to_string();
        assert!(
            e.contains("STREAMON") && e.contains("software fallback failed"),
            "{e}"
        );
    }

    #[test]
    fn flush_restarts_collection_at_the_next_keyframe() {
        let mut d = FallbackDecoder::new(
            Box::new(FailingHw::new(Failure::SendErrorAfter(5))),
            sw_factory(),
        );
        let pkts = packets(75);
        // Decode a little, then "seek" to the keyframe at 25.
        for p in &pkts[..3] {
            d.send_packet(p).unwrap();
        }
        while d.receive_frame().unwrap().is_some() {}
        d.flush().unwrap();
        let out = run(&mut d, &pkts[25..]);
        let want: Vec<i64> = (25..75).map(|i| i * 40).collect();
        assert_eq!(out, want);
        assert!(d.switched());
    }
}
