//! In-headset test (default mode only). Runs the real app stack — fp-xr
//! session, fp-gfx renderer, fp-ui panel — and:
//!
//! 1. measures frame pacing for ~10 s while rendering a 3840×1920
//!    equirect sphere (and ~5 s at the highest offered refresh rate);
//! 2. walks through every controller button with big on-screen prompts,
//!    recording which inputs fire and the active interaction profile;
//! 3. asks for a pinch with each hand (hand tracking) and a look at two
//!    targets (eye gaze).
//!
//! Each step times out after 10 s ("skipped"). Answers P1, P10, P11.

use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::Out;
use fp_gfx::{EyeRenderRequest, EyeView, Fov, RenderTarget, UiRenderRequest};
use fp_ui::{Align, Color, Rect, Ui};
use fp_xr::{InputState, Layer, Pose, XrEvent, XrSession, XrSwapchain};
use glam::{Mat4, Quat, Vec2, Vec3};
use serde::Serialize;
use serde_json::json;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

pub const STEP_TIMEOUT_S: f64 = 10.0;
const PANEL_PX: [f32; 2] = [1024.0, 640.0];
const PANEL_M: [f32; 2] = [1.0, 0.625];
const PANEL_DIST_M: f32 = 1.2;
/// Gaze targets: panel-local x offset (metres) of the left/right dots.
const GAZE_DOT_M: f32 = 0.35;
const GAZE_YAW_THRESHOLD: f32 = 0.14;

/// One prompt of the walk-through.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub id: &'static str,
    pub prompt: &'static str,
    /// Input name (see [`active_inputs`]) that completes the step.
    pub wants: &'static str,
}

const fn s(id: &'static str, prompt: &'static str, wants: &'static str) -> Step {
    Step { id, prompt, wants }
}

pub const STEPS: &[Step] = &[
    s("a", "Press  A", "a"),
    s("b", "Press  B", "b"),
    s("x", "Press  X", "x"),
    s("y", "Press  Y", "y"),
    s("menu", "Press the MENU button", "menu"),
    s("view", "Press the VIEW button", "view"),
    s("left_trigger", "Pull the LEFT trigger", "left_trigger"),
    s("right_trigger", "Pull the RIGHT trigger", "right_trigger"),
    s("left_grip", "Squeeze the LEFT grip", "left_grip"),
    s("right_grip", "Squeeze the RIGHT grip", "right_grip"),
    s("left_bumper", "Press the LEFT bumper", "left_bumper"),
    s("right_bumper", "Press the RIGHT bumper", "right_bumper"),
    s(
        "left_stick_click",
        "Click the LEFT stick down",
        "left_stick_click",
    ),
    s(
        "right_stick_click",
        "Click the RIGHT stick down",
        "right_stick_click",
    ),
    s(
        "left_stick_move",
        "Push the LEFT stick to the side",
        "left_stick_move",
    ),
    s(
        "right_stick_move",
        "Push the RIGHT stick to the side",
        "right_stick_move",
    ),
    s("dpad_up", "Press D-pad UP", "dpad_up"),
    s("dpad_down", "Press D-pad DOWN", "dpad_down"),
    s("dpad_left", "Press D-pad LEFT", "dpad_left"),
    s("dpad_right", "Press D-pad RIGHT", "dpad_right"),
    s(
        "left_pinch",
        "Put the controllers down. PINCH with your LEFT hand",
        "left_pinch",
    ),
    s("right_pinch", "PINCH with your RIGHT hand", "right_pinch"),
    s(
        "gaze_left",
        "Without turning your head, LOOK at the LEFT dot",
        "gaze_left",
    ),
    s("gaze_right", "Now LOOK at the RIGHT dot", "gaze_right"),
];

/// Names of all inputs currently held, from fp-xr's per-frame state.
pub fn active_inputs(st: &InputState, head: Option<Pose>) -> BTreeSet<&'static str> {
    let mut a = BTreeSet::new();
    let mut put = |on: bool, n: &'static str| {
        if on {
            a.insert(n);
        }
    };
    put(st.a.pressed, "a");
    put(st.b.pressed, "b");
    put(st.x.pressed, "x");
    put(st.y.pressed, "y");
    put(st.menu.pressed, "menu");
    put(st.view.pressed, "view");
    for (c, side) in [(&st.left, "left"), (&st.right, "right")] {
        let n = |what: &str| -> &'static str {
            match (side, what) {
                ("left", "trigger") => "left_trigger",
                ("left", "grip") => "left_grip",
                ("left", "bumper") => "left_bumper",
                ("left", "click") => "left_stick_click",
                ("left", "move") => "left_stick_move",
                ("right", "trigger") => "right_trigger",
                ("right", "grip") => "right_grip",
                ("right", "bumper") => "right_bumper",
                ("right", "click") => "right_stick_click",
                _ => "right_stick_move",
            }
        };
        put(c.select.pressed || c.trigger > 0.8, n("trigger"));
        put(c.grip_button.pressed || c.squeeze > 0.8, n("grip"));
        put(c.bumper.pressed, n("bumper"));
        put(c.thumbstick_button.pressed, n("click"));
        put(c.thumbstick.length() > 0.7, n("move"));
    }
    put(st.dpad.up.pressed, "dpad_up");
    put(st.dpad.down.pressed, "dpad_down");
    put(st.dpad.left.pressed, "dpad_left");
    put(st.dpad.right.pressed, "dpad_right");
    put(st.hands[0].pinch.pinching, "left_pinch");
    put(st.hands[1].pinch.pinching, "right_pinch");
    if let (Some(g), Some(h)) = (st.gaze, head) {
        let yaw = gaze_yaw(g.orientation, h.orientation);
        put(yaw < -GAZE_YAW_THRESHOLD, "gaze_left");
        put(yaw > GAZE_YAW_THRESHOLD, "gaze_right");
    }
    a
}

/// Horizontal angle of the gaze relative to the head (negative = left).
pub fn gaze_yaw(gaze: Quat, head: Quat) -> f32 {
    let d = head.inverse() * (gaze * Vec3::NEG_Z);
    d.x.atan2(-d.z)
}

/// Result of one step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepResult {
    pub id: &'static str,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// Other inputs that started during the step (mapping surprises).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_fired: Vec<&'static str>,
}

/// Pure step sequencer: feed it the held inputs each frame.
#[derive(Debug, Clone)]
pub struct Walkthrough {
    pub steps: Vec<Step>,
    idx: usize,
    step_started: Option<f64>,
    passed_at: Option<f64>,
    prev: BTreeSet<&'static str>,
    fired: Vec<&'static str>,
    pub results: Vec<StepResult>,
    pub timeout_s: f64,
    /// Pause after a success before the next prompt.
    pub gap_s: f64,
}

impl Walkthrough {
    pub fn new(steps: &[Step]) -> Walkthrough {
        Walkthrough {
            steps: steps.to_vec(),
            idx: 0,
            step_started: None,
            passed_at: None,
            prev: BTreeSet::new(),
            fired: Vec::new(),
            results: Vec::new(),
            timeout_s: STEP_TIMEOUT_S,
            gap_s: 0.6,
        }
    }

    pub fn done(&self) -> bool {
        self.idx >= self.steps.len()
    }

    pub fn current(&self) -> Option<&Step> {
        self.steps.get(self.idx)
    }

    pub fn index(&self) -> usize {
        self.idx
    }

    /// Seconds left in the current step.
    pub fn remaining(&self, now: f64) -> f64 {
        self.step_started
            .map_or(self.timeout_s, |t| (self.timeout_s - (now - t)).max(0.0))
    }

    /// Showing the "OK" confirmation of the previous success.
    pub fn confirming(&self) -> bool {
        self.passed_at.is_some()
    }

    pub fn update(&mut self, now: f64, active: &BTreeSet<&'static str>) {
        if self.done() {
            return;
        }
        if let Some(t) = self.passed_at {
            if now - t >= self.gap_s {
                self.passed_at = None;
                self.advance(now, active);
            }
            self.prev = active.clone();
            return;
        }
        let start = *self.step_started.get_or_insert(now);
        let step = self.steps[self.idx];
        let rising: Vec<&'static str> = active.difference(&self.prev).copied().collect();
        self.prev = active.clone();
        for r in &rising {
            if *r != step.wants && !self.fired.contains(r) {
                self.fired.push(r);
            }
        }
        if rising.contains(&step.wants) {
            self.results.push(StepResult {
                id: step.id,
                status: "pass",
                ms: Some(((now - start) * 1000.0) as u64),
                also_fired: std::mem::take(&mut self.fired),
            });
            self.passed_at = Some(now);
        } else if now - start >= self.timeout_s {
            self.results.push(StepResult {
                id: step.id,
                status: "skipped",
                ms: None,
                also_fired: std::mem::take(&mut self.fired),
            });
            self.advance(now, active);
        }
    }

    fn advance(&mut self, now: f64, active: &BTreeSet<&'static str>) {
        self.idx += 1;
        self.step_started = Some(now);
        self.prev = active.clone();
        self.fired.clear();
    }

    pub fn passed(&self, id: &str) -> Option<bool> {
        self.results
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.status == "pass")
    }
}

/// Frame pacing summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Timing {
    pub frames: usize,
    pub seconds: f64,
    pub fps: f64,
    pub period_ms: f64,
    pub missed: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub cpu_ms_avg: f64,
}

/// Stats from frame-to-frame intervals (ms), the runtime's predicted
/// display period and CPU time per frame. "Missed" = interval > 1.5 periods.
pub fn timing_stats(intervals: &[f64], period_ms: f64, cpu_ms: &[f64]) -> Option<Timing> {
    if intervals.is_empty() {
        return None;
    }
    let mut v = intervals.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    let total: f64 = intervals.iter().sum();
    let r2 = |x: f64| (x * 100.0).round() / 100.0;
    Some(Timing {
        frames: intervals.len(),
        seconds: r2(total / 1000.0),
        fps: r2(intervals.len() as f64 * 1000.0 / total.max(1e-9)),
        period_ms: r2(period_ms),
        missed: if period_ms > 0.0 {
            intervals.iter().filter(|&&i| i > period_ms * 1.5).count()
        } else {
            0
        },
        p50_ms: r2(pct(0.5)),
        p95_ms: r2(pct(0.95)),
        max_ms: r2(v[v.len() - 1]),
        cpu_ms_avg: r2(if cpu_ms.is_empty() {
            0.0
        } else {
            cpu_ms.iter().sum::<f64>() / cpu_ms.len() as f64
        }),
    })
}

/// 3840×1920 NV12 test pattern (grid, gradients, coloured quadrants).
pub fn test_pattern(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            let grid = row % 120 < 4 || col % 120 < 4;
            y[row * w + col] = if grid {
                235
            } else {
                (40 + (col * 120 / w) + (row * 60 / h)) as u8
            };
        }
    }
    let (cw, ch) = (w / 2, h / 2);
    let mut uv = vec![128u8; cw * ch * 2];
    for row in 0..ch {
        for col in 0..cw {
            let (u, v) = match (col * 4 / cw, row * 2 / ch) {
                (0, 0) => (90, 200),
                (1, 0) => (200, 110),
                (2, 0) => (60, 90),
                (3, 0) => (160, 160),
                (0, _) => (200, 60),
                (1, _) => (110, 90),
                (2, _) => (150, 220),
                _ => (128, 128),
            };
            uv[(row * cw + col) * 2] = u;
            uv[(row * cw + col) * 2 + 1] = v;
        }
    }
    (y, uv)
}

fn eye_view(v: &fp_xr::View) -> EyeView {
    EyeView {
        position: v.pose.position,
        orientation: v.pose.orientation,
        fov: Fov {
            angle_left: v.fov.angle_left,
            angle_right: v.fov.angle_right,
            angle_up: v.fov.angle_up,
            angle_down: v.fov.angle_down,
        },
    }
}

fn target(sc: &XrSwapchain, image: ash::vk::Image, layer: u32) -> RenderTarget {
    RenderTarget {
        image,
        format: sc.format,
        width: sc.width,
        height: sc.height,
        array_layer: layer,
    }
}

/// What the panel shows this frame.
struct Screen<'a> {
    title: &'a str,
    lines: Vec<String>,
    progress: Option<f32>,
    footer: String,
    gaze_dots: bool,
    ok: bool,
}

fn draw(ui: &mut Ui, s: &Screen<'_>) -> fp_ui::UiOutput {
    ui.begin_frame(fp_ui::FrameInput {
        dt: 1.0 / 72.0,
        pointers: Vec::new(),
        nav: Default::default(),
        text: Vec::new(),
        gaze_on_panel: None,
    });
    let [w, h] = PANEL_PX;
    ui.painter().rect_rounded(
        Rect::new(0.0, 0.0, w, h),
        28.0,
        Color::hex(0x101820).alpha(0.92),
    );
    ui.draw_text_in(
        Rect::new(40.0, 24.0, w - 80.0, 60.0),
        s.title,
        34.0,
        Color::hex(0x9fb3c8),
        Align::Center,
    );
    let n = s.lines.len().max(1) as f32;
    let line_h = 76.0;
    let top = (h - n * line_h) * 0.5;
    for (i, l) in s.lines.iter().enumerate() {
        let color = if s.ok {
            Color::hex(0x5ee08a)
        } else {
            Color::WHITE
        };
        let size = if i == 0 { 54.0 } else { 40.0 };
        ui.draw_text_in(
            Rect::new(40.0, top + i as f32 * line_h, w - 80.0, line_h),
            l,
            size,
            color,
            Align::Center,
        );
    }
    if let Some(p) = s.progress {
        ui.painter().rect_rounded(
            Rect::new(162.0, h - 120.0, 700.0, 14.0),
            7.0,
            Color::hex(0x2a3a4a),
        );
        ui.painter().rect_rounded(
            Rect::new(162.0, h - 120.0, 700.0 * p.clamp(0.0, 1.0), 14.0),
            7.0,
            Color::hex(0x4aa3ff),
        );
    }
    if s.gaze_dots {
        let ppm = w / PANEL_M[0];
        for dx in [-GAZE_DOT_M, GAZE_DOT_M] {
            ui.painter().circle(
                Vec2::new(w * 0.5 + dx * ppm, h * 0.5 + 60.0),
                22.0,
                Color::hex(0x5ee08a),
            );
        }
    }
    ui.draw_text_in(
        Rect::new(40.0, h - 80.0, w - 80.0, 50.0),
        &s.footer,
        26.0,
        Color::hex(0x9fb3c8),
        Align::Center,
    );
    ui.end_frame()
}

/// XR + GPU objects; field order is drop order.
struct Stack {
    renderer: fp_gfx::Renderer,
    stereo: XrSwapchain,
    panel: XrSwapchain,
    session: XrSession,
    _vk: fp_xr::VulkanContext,
    ctx: fp_xr::XrContext,
}

fn init() -> Result<Stack, String> {
    let ctx = fp_xr::XrContext::new(fp_xr::XrConfig {
        app_name: "frameplayer-probe".into(),
        ..Default::default()
    })
    .map_err(|e| format!("OpenXR: {e}"))?;
    let vk = fp_xr::VulkanContext::new(&ctx).map_err(|e| format!("Vulkan via OpenXR: {e}"))?;
    let session = XrSession::new(&ctx, &vk).map_err(|e| format!("XR session: {e}"))?;
    let ext = vk.device_extensions;
    let caps = fp_gfx::DeviceCaps {
        dmabuf_import: ext.external_memory_fd && ext.external_memory_dma_buf,
        drm_format_modifier: ext.image_drm_format_modifier,
        queue_family_foreign: ext.queue_family_foreign,
        sampler_ycbcr_conversion: vk.sampler_ycbcr_conversion,
    };
    // SAFETY: fp-xr created these handles with the 1.3 features fp-gfx
    // needs; the VulkanContext outlives the renderer (Stack field order).
    let gpu = unsafe {
        fp_gfx::GpuContext::new(
            vk.instance.clone(),
            vk.physical_device,
            vk.device.clone(),
            vk.queue_family_index,
            vk.queue_index,
            caps,
        )
    };
    let renderer = fp_gfx::Renderer::new(gpu, fp_gfx::RendererConfig::default())
        .map_err(|e| format!("renderer: {e}"))?;
    let stereo = session
        .create_stereo_swapchain()
        .map_err(|e| format!("stereo swapchain: {e}"))?;
    let panel = session
        .create_ui_swapchain(PANEL_PX[0] as u32, PANEL_PX[1] as u32)
        .map_err(|e| format!("UI swapchain: {e}"))?;
    Ok(Stack {
        renderer,
        stereo,
        panel,
        session,
        _vk: vk,
        ctx,
    })
}

fn profile_paths(st: &Stack) -> Vec<String> {
    let i = &st.ctx.instance;
    ["/user/hand/left", "/user/hand/right"]
        .iter()
        .map(|h| {
            let p = i
                .string_to_path(h)
                .ok()
                .and_then(|p| st.session.raw().current_interaction_profile(p).ok())
                .and_then(|p| {
                    if p == openxr::Path::NULL {
                        None
                    } else {
                        i.path_to_string(p).ok()
                    }
                });
            format!("{h}: {}", p.unwrap_or_else(|| "(none)".into()))
        })
        .collect()
}

pub fn run(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let mut st = match init() {
        Ok(s) => s,
        Err(e) => {
            o.finding(
                "xr_render",
                Status::Fail,
                &["P12"],
                format!("could not start XR rendering: {e}"),
            );
            return o.finish(Status::Fail, format!("XR rendering failed to start: {e}"));
        }
    };
    let (y, uv) = test_pattern(3840, 1920);
    let mut uploaded = false;
    let mut ui = Ui::new(
        fp_ui::Vec2::new(PANEL_PX[0], PANEL_PX[1]),
        PANEL_PX[0] / PANEL_M[0],
    );
    let settings = fp_core::ViewSettings {
        projection: fp_core::Projection::EQUIRECT_360,
        ..Default::default()
    };
    let corrections = fp_core::Corrections::default();
    let t0 = Instant::now();
    let now = || t0.elapsed().as_secs_f64();

    #[derive(PartialEq, Clone, Copy, Debug)]
    enum Phase {
        WaitFocus,
        Timing,
        TimingMax,
        Walk,
        Done,
    }
    let mut phase = Phase::WaitFocus;
    let mut phase_start = 0.0;
    let mut intervals: Vec<f64> = Vec::new();
    let mut cpu: Vec<f64> = Vec::new();
    let mut period_ms = 0.0;
    let mut last_wait: Option<Instant> = None;
    // Skip prompts the runtime cannot report anyway.
    let gaze_ok = st.ctx.enabled.eye_gaze_interaction;
    let hands_ok = st.ctx.hand_tracking_supported;
    let steps: Vec<Step> = STEPS
        .iter()
        .copied()
        .filter(|s| (gaze_ok || !s.id.starts_with("gaze")) && (hands_ok || !s.id.contains("pinch")))
        .collect();
    o.set(
        "steps_omitted",
        json!({ "eye_gaze": !gaze_ok, "hand_pinch": !hands_ok }),
    );
    let mut walk = Walkthrough::new(&steps);
    let mut profiles_seen: BTreeSet<String> = BTreeSet::new();
    let mut original_rate: Option<f32> = None;
    let mut events = Vec::new();
    let mut error: Option<String> = None;
    let mut ever_focused = false;
    let mut fp_rendered = 0u64;
    let rates = st.session.refresh_rates().to_vec();

    loop {
        events.clear();
        if let Err(e) = st.session.poll_events(&mut events) {
            error = Some(format!("poll_events: {e}"));
            break;
        }
        if events.iter().any(|e| matches!(e, XrEvent::Exit)) {
            if phase != Phase::Done {
                error = Some("the runtime ended the session early".into());
            }
            break;
        }
        if events
            .iter()
            .any(|e| matches!(e, XrEvent::InteractionProfileChanged))
        {
            profiles_seen.extend(profile_paths(&st));
        }
        let t = now();
        if phase == Phase::WaitFocus && t > 45.0 && !ever_focused {
            error = Some("the session never got input focus within 45 s (headset not worn, or another app in front)".into());
            break;
        }
        if phase == Phase::Done && t - phase_start > 6.0 {
            break;
        }
        if !st.session.lifecycle().should_run_frame_loop() {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let frame = (|| -> Result<(), String> {
            let timing = st
                .session
                .wait_frame()
                .map_err(|e| format!("wait_frame: {e}"))?;
            let woke = Instant::now();
            if let Some(prev) = last_wait.replace(woke) {
                if matches!(phase, Phase::Timing | Phase::TimingMax) {
                    intervals.push(woke.duration_since(prev).as_secs_f64() * 1000.0);
                }
            }
            period_ms = timing.predicted_display_period_ns as f64 / 1e6;
            st.session
                .begin_frame()
                .map_err(|e| format!("begin_frame: {e}"))?;
            let tt = timing.predicted_display_time;
            let input = *st
                .session
                .sync_input(tt)
                .map_err(|e| format!("sync_input: {e}"))?;
            let head = st.session.locate_head(tt);
            let views = st
                .session
                .locate_views(tt)
                .map_err(|e| format!("locate_views: {e}"))?;
            if st.session.lifecycle().is_focused() {
                ever_focused = true;
            }

            // Phase transitions + what to show.
            let t = now();
            let screen = match phase {
                Phase::WaitFocus => {
                    if ever_focused {
                        phase = Phase::Timing;
                        phase_start = t;
                        intervals.clear();
                        profiles_seen.extend(profile_paths(&st));
                    }
                    Screen {
                        title: "FramePlayer self-test",
                        lines: vec!["Starting…".into()],
                        progress: None,
                        footer: String::new(),
                        gaze_dots: false,
                        ok: false,
                    }
                }
                Phase::Timing | Phase::TimingMax => {
                    let len = if phase == Phase::Timing { 10.0 } else { 5.0 };
                    if t - phase_start >= len {
                        let stats = timing_stats(&intervals, period_ms, &cpu);
                        if phase == Phase::Timing {
                            o.set("timing_default", &stats);
                            original_rate = st.session.current_refresh_rate();
                            let max = rates.iter().copied().fold(0.0f32, f32::max);
                            if max > original_rate.unwrap_or(0.0) + 1.0
                                && st.session.request_refresh_rate(max).is_ok()
                            {
                                o.set("timing_max_rate_requested", max);
                                phase = Phase::TimingMax;
                            } else {
                                phase = Phase::Walk;
                            }
                        } else {
                            o.set("timing_max_rate", &stats);
                            o.set(
                                "refresh_rate_after_request",
                                st.session.current_refresh_rate(),
                            );
                            if let Some(r) = original_rate {
                                let _ = st.session.request_refresh_rate(r);
                            }
                            phase = Phase::Walk;
                        }
                        intervals.clear();
                        cpu.clear();
                        phase_start = t;
                        ctx.partial(&o.snapshot(Status::Unknown, "frame timing measured"));
                    }
                    Screen {
                        title: "FramePlayer self-test",
                        lines: vec![
                            "Measuring smoothness…".into(),
                            "Please keep still for a moment.".into(),
                        ],
                        progress: Some(((t - phase_start) / len) as f32),
                        footer: format!("{:.0} Hz", 1000.0 / period_ms.max(1.0)),
                        gaze_dots: false,
                        ok: false,
                    }
                }
                Phase::Walk => {
                    let active = active_inputs(&input, head);
                    walk.update(t, &active);
                    if walk.done() {
                        phase = Phase::Done;
                        phase_start = t;
                        ctx.partial(&o.snapshot(Status::Unknown, "walk-through finished"));
                    }
                    match walk.current() {
                        Some(step) if !walk.done() => Screen {
                            title: "FramePlayer self-test — do what the panel says",
                            lines: if walk.confirming() {
                                vec!["OK!".into()]
                            } else {
                                vec![step.prompt.to_string()]
                            },
                            progress: Some((walk.remaining(t) / STEP_TIMEOUT_S) as f32),
                            footer: format!(
                                "Step {} of {} · skips by itself after 10 s",
                                walk.index() + 1,
                                steps.len()
                            ),
                            gaze_dots: step.id.starts_with("gaze") && !walk.confirming(),
                            ok: walk.confirming(),
                        },
                        _ => Screen {
                            title: "",
                            lines: vec![],
                            progress: None,
                            footer: String::new(),
                            gaze_dots: false,
                            ok: true,
                        },
                    }
                }
                Phase::Done => Screen {
                    title: "FramePlayer self-test",
                    lines: vec![
                        "Done! The report is saved.".into(),
                        "You can take the headset off.".into(),
                    ],
                    progress: None,
                    footer: String::new(),
                    gaze_dots: false,
                    ok: true,
                },
            };
            let cpu_start = Instant::now();
            let Some(views) = views.filter(|_| timing.should_render) else {
                st.session
                    .end_frame(&timing, &[])
                    .map_err(|e| format!("end_frame: {e}"))?;
                return Ok(());
            };
            let ui_out = draw(&mut ui, &screen);
            let r = &mut st.renderer;
            r.begin_frame()
                .map_err(|e| format!("renderer begin: {e}"))?;
            if !uploaded {
                let f = fp_gfx::CpuFrame {
                    width: 3840,
                    height: 1920,
                    format: fp_gfx::PixelFormat::Nv12,
                    planes: [&y, &uv],
                    strides: [3840, 3840],
                    color: fp_gfx::ColorInfo::SDR_709,
                };
                r.upload_video_frame(&fp_gfx::VideoFrame::Cpu(f))
                    .map_err(|e| format!("4K texture upload: {e}"))?;
                uploaded = true;
            }
            let (_, eye_img) = st.stereo.acquire().map_err(|e| format!("acquire: {e}"))?;
            r.render_eyes(&EyeRenderRequest {
                views: [eye_view(&views[0]), eye_view(&views[1])],
                targets: [
                    target(&st.stereo, eye_img, 0),
                    target(&st.stereo, eye_img, 1),
                ],
                settings: &settings,
                corrections: &corrections,
                model: Mat4::IDENTITY,
                follow_head: true,
                clear_color: [0.0, 0.0, 0.0, 1.0],
                tint: [1.0; 4],
            })
            .map_err(|e| format!("render_eyes: {e}"))?;
            let (_, ui_img) = st.panel.acquire().map_err(|e| format!("acquire UI: {e}"))?;
            r.render_ui(&UiRenderRequest {
                draw_list: &ui_out.draw_list,
                atlas: Some(ui.atlas()),
                target: target(&st.panel, ui_img, 0),
                panel_size: PANEL_PX,
                clear_color: [0.0; 4],
            })
            .map_err(|e| format!("render_ui: {e}"))?;
            r.end_frame().map_err(|e| format!("renderer end: {e}"))?;
            st.stereo.release().map_err(|e| format!("release: {e}"))?;
            st.panel.release().map_err(|e| format!("release UI: {e}"))?;
            let layers = [
                Layer::Projection {
                    swapchain: &st.stereo,
                    views: &views,
                    alpha_blend: false,
                },
                Layer::Quad {
                    swapchain: &st.panel,
                    pose: Pose::new(Vec3::new(0.0, 0.0, -PANEL_DIST_M), Quat::IDENTITY),
                    size: Vec2::new(PANEL_M[0], PANEL_M[1]),
                    head_locked: true,
                },
            ];
            st.session
                .end_frame(&timing, &layers)
                .map_err(|e| format!("end_frame: {e}"))?;
            cpu.push(cpu_start.elapsed().as_secs_f64() * 1000.0);
            fp_rendered += 1;
            Ok(())
        })();
        if let Err(e) = frame {
            error = Some(e);
            break;
        }
    }
    let _ = st.session.request_exit();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        events.clear();
        if st.session.poll_events(&mut events).is_err()
            || events.iter().any(|e| matches!(e, XrEvent::Exit))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    o.set("frames_rendered", fp_rendered);
    o.set("interaction_profiles", &profiles_seen);
    o.set("steps", &walk.results);
    o.set("refresh_rates_offered", &rates);
    if let Some(e) = &error {
        o.set("error", e);
        o.finding(
            "xr_render",
            Status::Fail,
            &[],
            format!("XR rendering stopped: {e}"),
        );
    } else {
        o.finding(
            "xr_render",
            Status::Pass,
            &[],
            format!("rendered {fp_rendered} frames with fp-gfx (4K equirect sphere + fp-ui panel)"),
        );
    }
    if let Some(t) = o.data.get("timing_default").and_then(|v| v.as_object()) {
        let fps = t.get("fps").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let missed = t.get("missed").and_then(|v| v.as_u64()).unwrap_or(0);
        let frames = t.get("frames").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        let good = (missed as f64 / frames as f64) < 0.05;
        o.finding(
            "frame_timing",
            if good { Status::Pass } else { Status::Fail },
            &["P1"],
            format!(
                "4K sphere: {fps:.1} fps at the default rate, {missed} of {frames} frames late"
            ),
        );
    }
    if let Some(t) = o.data.get("timing_max_rate").cloned() {
        o.finding(
            "frame_timing_max",
            Status::Pass,
            &["P1"],
            format!(
                "after requesting {} Hz: {} fps (runtime now reports {})",
                o.data["timing_max_rate_requested"], t["fps"], o.data["refresh_rate_after_request"]
            ),
        );
    }
    let buttons: Vec<&StepResult> = walk
        .results
        .iter()
        .filter(|r| !r.id.contains("pinch") && !r.id.starts_with("gaze"))
        .collect();
    if !buttons.is_empty() {
        let ok: Vec<&str> = buttons
            .iter()
            .filter(|r| r.status == "pass")
            .map(|r| r.id)
            .collect();
        let miss: Vec<&str> = buttons
            .iter()
            .filter(|r| r.status != "pass")
            .map(|r| r.id)
            .collect();
        o.finding(
            "controller_inputs",
            if miss.is_empty() {
                Status::Pass
            } else if ok.is_empty() {
                Status::Fail
            } else {
                Status::Pass
            },
            &["P10"],
            format!(
                "{} of {} buttons registered ({}); no response: {}",
                ok.len(),
                buttons.len(),
                profiles_seen.iter().cloned().collect::<Vec<_>>().join("; "),
                if miss.is_empty() {
                    "none".to_string()
                } else {
                    miss.join(", ")
                }
            ),
        );
    }
    for (id, refs, what) in [
        ("left_pinch", &[][..], "left-hand pinch"),
        ("right_pinch", &[][..], "right-hand pinch"),
        ("gaze_left", &["P11"][..], "eye gaze (left target)"),
        ("gaze_right", &["P11"][..], "eye gaze (right target)"),
    ] {
        if let Some(p) = walk.passed(id) {
            o.finding(
                id,
                if p { Status::Pass } else { Status::Fail },
                refs,
                format!(
                    "{what}: {}",
                    if p {
                        "detected"
                    } else {
                        "not detected within 10 s"
                    }
                ),
            );
        }
    }
    let status = if error.is_some() && walk.results.is_empty() {
        Status::Fail
    } else {
        crate::checks::combine(&o)
    };
    let passed = walk.results.iter().filter(|r| r.status == "pass").count();
    let summary = match &error {
        Some(e) if walk.results.is_empty() => format!("stopped early: {e}"),
        _ => format!(
            "{passed}/{} steps completed; {}",
            walk.results.len(),
            o.data
                .get("timing_default")
                .map_or("no timing".into(), |t| format!("{} fps", t["fps"]))
        ),
    };
    drop(st);
    o.finish(status, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&'static str]) -> BTreeSet<&'static str> {
        items.iter().copied().collect()
    }

    #[test]
    fn walkthrough_passes_skips_and_records_extras() {
        let steps = [s("a", "A", "a"), s("b", "B", "b"), s("x", "X", "x")];
        let mut w = Walkthrough::new(&steps);
        w.update(0.0, &set(&[]));
        w.update(0.5, &set(&["b"])); // wrong button first
        w.update(1.0, &set(&["a"]));
        assert!(w.confirming());
        assert_eq!(w.results[0].status, "pass");
        assert_eq!(w.results[0].ms, Some(1000));
        assert_eq!(w.results[0].also_fired, ["b"]);
        // A still held through the gap must not count for B.
        w.update(1.7, &set(&["a"]));
        assert_eq!(w.current().unwrap().id, "b");
        w.update(2.0, &set(&["a"]));
        // Nothing pressed for 10 s: skipped.
        w.update(11.8, &set(&["x"]));
        assert_eq!(w.results[1].status, "skipped");
        assert_eq!(w.current().unwrap().id, "x");
        // Held before the step started: needs a fresh press.
        w.update(12.0, &set(&["x"]));
        assert!(
            !w.confirming(),
            "x was already held when the step began? {:?}",
            w.results
        );
        w.update(12.1, &set(&[]));
        w.update(12.2, &set(&["x"]));
        assert!(w.confirming());
        w.update(13.0, &set(&[]));
        assert!(w.done());
        assert_eq!(w.passed("x"), Some(true));
        assert_eq!(w.passed("b"), Some(false));
        assert_eq!(w.passed("zz"), None);
    }

    #[test]
    fn timing_statistics() {
        let mut iv = vec![13.9; 98];
        iv.push(27.8);
        iv.push(41.7);
        let t = timing_stats(&iv, 13.89, &[2.0, 4.0]).unwrap();
        assert_eq!(t.frames, 100);
        assert_eq!(t.missed, 2);
        assert_eq!(t.p50_ms, 13.9);
        assert_eq!(t.max_ms, 41.7);
        assert_eq!(t.cpu_ms_avg, 3.0);
        assert!((t.fps - 69.5).abs() < 0.5, "{}", t.fps);
        assert!(timing_stats(&[], 13.9, &[]).is_none());
    }

    #[test]
    fn gaze_yaw_sign() {
        let head = Quat::from_rotation_y(0.3);
        let look_left = head * Quat::from_rotation_y(0.25);
        assert!(gaze_yaw(look_left, head) < -0.2);
        let look_right = head * Quat::from_rotation_y(-0.25);
        assert!(gaze_yaw(look_right, head) > 0.2);
        assert!(gaze_yaw(head, head).abs() < 1e-5);
    }

    #[test]
    fn inputs_from_state() {
        let mut st = InputState::default();
        st.a.pressed = true;
        st.left.trigger = 0.9;
        st.right.thumbstick = glam::Vec2::new(0.9, 0.0);
        st.dpad.up.pressed = true;
        st.hands[1].pinch.pinching = true;
        let head = Pose::new(Vec3::ZERO, Quat::IDENTITY);
        st.gaze = Some(Pose::new(Vec3::ZERO, Quat::from_rotation_y(0.3)));
        let a = active_inputs(&st, Some(head));
        assert_eq!(
            a,
            set(&[
                "a",
                "left_trigger",
                "right_stick_move",
                "dpad_up",
                "right_pinch",
                "gaze_left"
            ])
        );
        assert!(STEPS.iter().all(|s| !s.wants.is_empty()));
    }

    #[test]
    fn pattern_sizes() {
        let (y, uv) = test_pattern(64, 32);
        assert_eq!(y.len(), 64 * 32);
        assert_eq!(uv.len(), 64 * 32 / 2);
    }
}
