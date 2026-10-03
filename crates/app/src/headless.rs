//! Headless mode (`--headless` / `--no-xr`): the full player pipeline
//! (services, library, sources, demux, decode, A/V clock, remote APIs,
//! haptics) without OpenXR or Vulkan. Frames are pulled at "display time"
//! exactly like the XR loop and converted to renderer descriptors, but not
//! drawn. Useful for CI, debugging on a desktop, and proving the wiring.

use crate::app::App;
use crate::frame_convert::{color_for_track, FrameConverter};
use fp_core::MediaTime;
use fp_video::PlaybackState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Loop period (50 Hz is plenty without a display).
const TICK: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Default)]
pub struct HeadlessOptions {
    /// Stop after this long.
    pub max_seconds: Option<f64>,
    /// Exit once the opened media has finished (or failed).
    pub exit_when_done: bool,
    /// Set from a signal handler to stop.
    pub stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadlessReport {
    /// Frames presented (distinct frames converted successfully).
    pub frames: u64,
    pub convert_errors: u64,
    pub max_position: MediaTime,
    /// States the player went through, deduplicated.
    pub states: Vec<PlaybackState>,
    pub decoder: Option<String>,
    pub error: Option<String>,
}

impl HeadlessReport {
    pub fn reached(&self, s: PlaybackState) -> bool {
        self.states.contains(&s)
    }
}

pub fn run(app: &mut App, opts: &HeadlessOptions) -> HeadlessReport {
    let mut report = HeadlessReport::default();
    let mut conv = FrameConverter::new();
    let started = Instant::now();
    let mut last_log = Instant::now();
    let mut last_frame: Option<(u64, MediaTime)> = None;
    loop {
        app.tick();
        for (t, kind) in app.toasts.drain(..) {
            tracing::info!("[{kind:?}] {t}");
        }
        app.images.clear();
        app.recenter = false;
        if let Some(hz) = app.refresh_rate.take() {
            tracing::info!("would request {hz} Hz display refresh");
        }
        let st = &app.ctl.player;
        if report.states.last() != Some(&st.state) {
            report.states.push(st.state);
            tracing::info!("player state: {:?}", st.state);
        }
        if let Some(d) = &st.decoder {
            report.decoder = Some(d.to_string());
        }
        if app.ctl.session.as_ref().is_some_and(|s| !s.resolving) {
            report.max_position = report.max_position.max(st.position);
            if let Some(f) = app.video.frame_for_display(Instant::now()) {
                let key = (f.serial, f.pts);
                if last_frame != Some(key) {
                    last_frame = Some(key);
                    let track = st.media.as_ref().and_then(|m| m.primary_video());
                    let (w, h) = f.frame.size();
                    match conv.convert(&f.frame, color_for_track(track, w, h)) {
                        Ok(_) => report.frames += 1,
                        Err(e) => {
                            if report.convert_errors == 0 {
                                tracing::warn!("frame not presentable: {e}");
                            }
                            report.convert_errors += 1;
                        }
                    }
                }
            }
        }
        if last_log.elapsed() >= Duration::from_secs(1) {
            last_log = Instant::now();
            if let Some(s) = &app.ctl.session {
                tracing::info!(
                    "{}: {:?} {} / {} · speed {}× · {} frames · decoder {}",
                    s.title,
                    st.state,
                    st.position,
                    st.duration.unwrap_or_default(),
                    st.speed,
                    report.frames,
                    report.decoder.as_deref().unwrap_or("none")
                );
            }
        }
        if app.last_error.is_some() {
            report.error = app.last_error.clone();
        }
        let done = app.opened_once && app.ctl.session.is_none();
        let failed = app.last_error.is_some() && app.ctl.session.is_none();
        if opts.stop.load(Ordering::Relaxed)
            || opts
                .max_seconds
                .is_some_and(|m| started.elapsed().as_secs_f64() >= m)
            || (opts.exit_when_done && (done || failed))
        {
            break;
        }
        std::thread::sleep(TICK);
    }
    report
}
