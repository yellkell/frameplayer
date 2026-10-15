//! `frameplayer --seek-test FILE|URL`: plays a video the way a viewer
//! skipping through it does, without a headset, GPU or sound, and checks
//! what testers ran into: skips that buffer for long or forever (an SMB
//! share stalled after a few), pausing, and hardware decoder sessions not
//! given back when a video closes (every later video "Hardware decoder
//! busy" until a restart).
//!
//! Each round opens the video, plays a little, skips `--step` seconds
//! `--seeks` times (back to the start near the end), pauses for a few
//! seconds, then closes it and checks that FramePlayer holds no hardware
//! decoder session any more. Network locations go through the sources set
//! up in FramePlayer (SMB credentials included).

use crate::services::Opener;
use crate::settings::Settings;
use fp_media::{Player, PlayerConfig, PlayerState, audio::output::Backend};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct SeekTestOptions {
    pub seeks: usize,
    pub step: f64,
    pub rounds: usize,
    pub hw: fp_media::HwDecode,
}

impl Default for SeekTestOptions {
    fn default() -> Self {
        SeekTestOptions {
            seeks: 30,
            step: 60.0,
            rounds: 3,
            hw: fp_media::HwDecode::Auto,
        }
    }
}

/// A skip taking longer than this to show a picture is a failure.
const STUCK: Duration = Duration::from_secs(20);
/// Slower than this is reported as buffering.
const SLOW: Duration = Duration::from_secs(2);

/// Runs the display loop at ~90 Hz for `d`, or until `until` holds.
/// Returns whether `until` held.
fn run_for(p: &Player, d: Duration, mut until: impl FnMut(&Player) -> bool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        let _ = p.current_frame();
        if until(p) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(11));
    }
    false
}

/// Shows the first picture at or after `target` (playing, not buffering).
fn landed(p: &Player, target: f64) -> bool {
    p.state() != PlayerState::Buffering && p.current_frame().is_some_and(|f| f.pts >= target - 1.0)
}

/// Plays `d` and returns (stalls, frames dropped, frames shown) meanwhile.
fn play(p: &Player, d: Duration) -> (u64, u64, u64) {
    let a = p.stats();
    run_for(p, d, |_| false);
    let b = p.stats();
    (
        b.stalls - a.stalls,
        b.frames_dropped - a.frames_dropped,
        b.frames_shown - a.frames_shown,
    )
}

/// Closes on a thread of its own, like the app, and says how long it took.
fn close(p: Player) -> Result<Duration, String> {
    let t = Instant::now();
    let h = std::thread::spawn(move || drop(p));
    while !h.is_finished() {
        if t.elapsed() > STUCK {
            return Err(format!(
                "closing the video did not finish in {} s",
                STUCK.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = h.join();
    Ok(t.elapsed())
}

pub fn run(location: &str, opts: &SeekTestOptions) -> Result<(), String> {
    let opener = Opener::new();
    let configs = fp_sources::load_configs(&Settings::sources_path()).unwrap_or_default();
    for e in opener.set_sources(&configs) {
        log::warn!("source unavailable: {e}");
    }
    let name = location.rsplit('/').next().unwrap_or(location).to_string();
    let mut failures = Vec::new();
    println!(
        "seek test: {name}, {} rounds of {} skips of {} s",
        opts.rounds, opts.seeks, opts.step
    );
    for round in 1..=opts.rounds {
        let held = fp_media::hardware_sessions();
        if held != 0 {
            failures.push(format!(
                "round {round}: {held} hardware session(s) held before opening"
            ));
        }
        let t = Instant::now();
        let src = opener.open_in(None, location)?;
        let cfg = PlayerConfig {
            hw_decode: opts.hw,
            audio: Backend::Null,
            ..Default::default()
        };
        let p = Player::open(src, &name, cfg).map_err(|e| format!("{name}: {e}"))?;
        if !run_for(&p, STUCK, |p| landed(p, 0.0)) {
            failures.push(format!("round {round}: no picture after opening"));
            close(p)?;
            break;
        }
        let st = p.stats();
        println!(
            "round {round}: opened in {} ms, decoder {}{}, {} hardware session(s)",
            t.elapsed().as_millis(),
            st.video_decoder,
            if st.hardware { " (hardware)" } else { "" },
            fp_media::hardware_sessions()
        );
        if !st.hardware && opts.hw == fp_media::HwDecode::Auto {
            failures.push(format!("round {round}: decoding in software"));
        }
        let (stalls, dropped, shown) = play(&p, Duration::from_secs(2));
        println!("  play 2 s: {shown} frames, {dropped} dropped, {stalls} stalls");

        let duration = p.duration();
        let mut slowest = Duration::ZERO;
        for seek in 1..=opts.seeks {
            let mut target = p.position() + opts.step;
            if duration > 0.0 && target > duration - 5.0 {
                target = 5.0_f64.min(duration / 2.0);
            }
            let t = Instant::now();
            p.seek(target, true);
            if !run_for(&p, STUCK, |p| landed(p, target)) {
                let msg = format!(
                    "round {round} skip {seek} to {target:.0} s: no picture after {} s (state {:?}{})",
                    STUCK.as_secs(),
                    p.state(),
                    p.error().map(|e| format!(", {e}")).unwrap_or_default()
                );
                println!("  {msg}");
                failures.push(msg);
                break;
            }
            let took = t.elapsed();
            slowest = slowest.max(took);
            let (stalls, dropped, shown) = play(&p, Duration::from_millis(1500));
            let st = p.stats();
            let flag = if took > SLOW { "  <- buffered" } else { "" };
            println!(
                "  skip {seek:>3} to {:>6.0} s: picture in {:>5} ms, then {shown} frames, {dropped} dropped, {stalls} stalls, {}{flag}",
                target,
                took.as_millis(),
                st.video_decoder
            );
            if took > SLOW {
                failures.push(format!(
                    "round {round} skip {seek}: buffered {} ms",
                    took.as_millis()
                ));
            }
        }
        println!("  slowest skip: {} ms", slowest.as_millis());

        // Paused, the picture stands still: a decoder short of buffers
        // used to wait forever here and never close.
        p.pause();
        run_for(&p, Duration::from_secs(3), |_| false);
        p.play();
        let (_, _, shown) = play(&p, Duration::from_secs(2));
        println!("  after a 3 s pause: {shown} frames in 2 s");
        if shown == 0 {
            failures.push(format!("round {round}: no frames after pausing"));
        }

        let took = close(p)?;
        // The native decoder's session goes with the decode thread; give
        // the driver a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        while fp_media::hardware_sessions() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let held = fp_media::hardware_sessions();
        println!(
            "  closed in {} ms, {held} hardware session(s) still held",
            took.as_millis()
        );
        if held != 0 {
            failures.push(format!(
                "round {round}: {held} hardware session(s) not given back"
            ));
        }
    }
    if failures.is_empty() {
        println!("PASS");
        Ok(())
    } else {
        println!("FAIL");
        for f in &failures {
            println!("  {f}");
        }
        Err(format!("{} problem(s)", failures.len()))
    }
}
