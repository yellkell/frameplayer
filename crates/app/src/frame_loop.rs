//! The render thread: OpenXR session + Vulkan renderer, one iteration per
//! `xrWaitFrame` (§3.3 step 7, §3.4).
//!
//! Per frame:
//! 1. poll XR events (lifecycle), wait / begin the frame;
//! 2. sample input, locate the eyes and the head at the predicted display time;
//! 3. tick the app (services, player status, controller), map controller
//!    input (`input_map`) and laser / hand / gaze pointers onto the UI panel;
//! 4. pick the decoded frame for the predicted display instant
//!    (`VideoOutput::frame_for_display`), convert and upload it;
//! 5. render both eyes with the session's view settings and the
//!    keyframe-interpolated corrections, the UI into a quad or cylinder layer
//!    (premultiplied), subtitles into a quad at the subtitle depth;
//! 6. submit the layers (projection alpha-blended over system passthrough
//!    when that is enabled and offered), record perf, and mark the build
//!    healthy after the first presented frame.
//!
//! The thread never blocks on I/O: everything slow happens on the services
//! runtime and arrives through `App::tick`.

use crate::app::App;
use crate::config::{Comfort, Paths};
use crate::controller::THUMB_KEY_BASE;
use crate::frame_convert::{color_for_track, FrameConverter};
use crate::input_map::{InputContext, InputMapper};
use crate::perf::PerfLog;
use crate::state::Screen;
use crate::{subtitles, view_models};
use anyhow::{Context, Result};
use fp_core::{MediaTime, ViewSettings};
use fp_gfx::{
    DeviceCaps, EyeRenderRequest, EyeView, Fov, GpuContext, RenderTarget, Renderer, RendererConfig,
    UiRenderRequest,
};
use fp_ui::screens::{LibraryScreen, PictureAdjustScreen, PlayerControls, SettingsScreen};
use fp_ui::{
    CylinderPanel, Feedback, FrameInput, Hand, PointerInput, PointerSource, QuadPanel, Ray, Ui,
    UiAction, UiOutput,
};
use fp_xr::{InputState, Layer, Pose, View, XrConfig, XrContext, XrEvent, XrSession, XrSwapchain};
use glam::{Mat4, Quat, Vec2, Vec3};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// UI panel resolution and physical width.
pub const UI_PX: Vec2 = Vec2::new(1600.0, 1000.0);
pub const UI_WIDTH_M: f32 = 1.4;
/// UI panel centre below eye height.
pub const UI_DROP_M: f32 = 0.12;

/// Where the UI panel sits this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelPlacement {
    /// Pose relative to the layer's space (app space, or the head when head-locked).
    pub local: Pose,
    /// Pose in the app space (for hit testing).
    pub world: Pose,
    pub size_m: Vec2,
    pub curved: bool,
    pub radius: f32,
    pub head_locked: bool,
}

impl PanelPlacement {
    pub fn new(comfort: &Comfort, base: Quat, head: Option<Pose>) -> PanelPlacement {
        let dist = comfort.ui_distance_m.clamp(0.5, 5.0);
        let size_m = Vec2::new(
            UI_WIDTH_M * dist / 1.3,
            UI_WIDTH_M * dist / 1.3 * UI_PX.y / UI_PX.x,
        );
        let head_locked = comfort.head_locked_screen;
        let curved = comfort.curved_ui;
        // A cylinder layer's pose is its centre (the viewer); a quad's is the panel.
        let local = if curved {
            Pose::new(base * Vec3::new(0.0, -UI_DROP_M, 0.0), base)
        } else {
            Pose::new(base * Vec3::new(0.0, -UI_DROP_M, -dist), base)
        };
        let world = match (head_locked, head) {
            (true, Some(h)) => h.mul(&local),
            _ => local,
        };
        PanelPlacement {
            local,
            world,
            size_m,
            curved,
            radius: dist,
            head_locked,
        }
    }

    /// Hit a ray against the panel; returns panel pixels when inside.
    pub fn hit(&self, ray: &Ray) -> Option<(Vec2, f32)> {
        let hit = if self.curved {
            fp_ui::ray_cylinder(
                ray,
                &CylinderPanel {
                    position: self.world.position,
                    orientation: self.world.orientation,
                    radius: self.radius,
                    central_angle: self.size_m.x / self.radius,
                    aspect_ratio: self.size_m.x / self.size_m.y,
                    size_px: UI_PX,
                },
            )
        } else {
            fp_ui::ray_quad(
                ray,
                &QuadPanel {
                    position: self.world.position,
                    orientation: self.world.orientation,
                    size_m: self.size_m,
                    size_px: UI_PX,
                },
            )
        }?;
        hit.inside().then_some((hit.px, hit.distance))
    }
}

/// OpenXR view → renderer eye view.
pub fn eye_view(v: &View) -> EyeView {
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

/// Mesh placement: head-locked flat screens follow the head; everything else
/// is world-locked (rotated by the lying-down base orientation).
pub fn model_matrix(vs: &ViewSettings, head_locked: bool, head: Option<Pose>, base: Quat) -> Mat4 {
    match (head_locked && !vs.projection.is_immersive(), head) {
        (true, Some(h)) => h.to_mat4(),
        _ => Mat4::from_quat(base),
    }
}

/// Environment colour behind the video / UI (premultiplied; transparent
/// when system passthrough is blended in).
pub fn clear_color(comfort: &Comfort, passthrough: bool) -> [f32; 4] {
    if passthrough {
        return [0.0; 4];
    }
    let v = 0.004 + 0.06 * comfort.environment_dim.clamp(0.0, 1.0);
    [v, v, v * 1.15, 1.0]
}

/// UI-local screen state and per-frame UI work.
struct UiLayer {
    ui: Ui,
    library: LibraryScreen,
    controls: PlayerControls,
    picture: PictureAdjustScreen,
    settings: SettingsScreen,
    mapper: InputMapper,
    subs: Ui,
    output: Option<UiOutput>,
    sub_output: Option<UiOutput>,
}

impl UiLayer {
    fn new() -> UiLayer {
        UiLayer {
            ui: Ui::new(UI_PX, UI_PX.x / UI_WIDTH_M),
            library: LibraryScreen::new(),
            controls: PlayerControls::new(),
            picture: PictureAdjustScreen::new(),
            settings: SettingsScreen::new(),
            mapper: InputMapper::new(),
            subs: Ui::new(subtitles::PANEL_PX, 1000.0),
            output: None,
            sub_output: None,
        }
    }

    /// Pointers from controllers, hands and gaze against the panel.
    fn pointers(
        input: &InputState,
        panel: &PanelPlacement,
    ) -> (Vec<PointerInput>, Option<bool>, bool) {
        let mut out = Vec::new();
        let mut pointing = false;
        for (i, hand) in [Hand::Left, Hand::Right].into_iter().enumerate() {
            let c = input.controller(i);
            if c.active {
                if let Some(aim) = c.aim {
                    let hit = panel.hit(&Ray::from_pose(aim.position, aim.orientation));
                    pointing |= hit.is_some();
                    out.push(PointerInput {
                        source: PointerSource::Laser(hand),
                        pos: hit.map(|h| h.0),
                        pressed: c.select.pressed,
                        touch_hint: c.select.touched,
                        scroll: if hit.is_some() {
                            c.thumbstick
                        } else {
                            Vec2::ZERO
                        },
                    });
                    continue;
                }
            }
            let h = &input.hands[i];
            if h.tracked {
                if let Some(aim) = h.aim {
                    let hit = panel.hit(&Ray::new(aim.origin, aim.dir));
                    pointing |= hit.is_some();
                    out.push(PointerInput::new(
                        PointerSource::HandPinch(hand),
                        hit.map(|h| h.0),
                        h.pinch.pinching,
                    ));
                }
            }
        }
        let mut gaze_on = None;
        if let Some(g) = input.gaze {
            let hit = panel.hit(&Ray::from_pose(g.position, g.orientation));
            gaze_on = Some(hit.is_some());
            let pinch = input.hands.iter().any(|h| h.tracked && h.pinch.pinching);
            out.push(PointerInput::new(
                PointerSource::Gaze,
                hit.map(|h| h.0),
                pinch,
            ));
        }
        (out, gaze_on, pointing)
    }

    /// Input mapping + UI for this frame; feeds actions into the app.
    fn frame(&mut self, app: &mut App, input: &InputState, panel: &PanelPlacement, dt: f32) {
        let screen = app.ctl.screen();
        let visible = !matches!(
            screen,
            Screen::Player {
                controls_visible: false
            }
        );
        let in_player = matches!(screen, Screen::Player { .. });
        let (pointers, gaze_on, pointing) = if visible {
            Self::pointers(input, panel)
        } else {
            (Vec::new(), None, false)
        };
        let mapped = self.mapper.update(
            input,
            &InputContext {
                pointing_at_ui: pointing,
                ui_visible: visible,
                in_player,
                paused: app.ctl.player.state == fp_video::PlaybackState::Paused,
            },
            dt,
        );
        app.handle_input(&mapped.actions);
        if let Some(t) = app.search_text.take() {
            self.library.search = t;
        }
        for (t, kind) in app.toasts.drain(..) {
            self.ui.toast(t, kind, 4.0);
        }
        self.output = None;
        // The screen may have changed through input actions.
        let screen = app.ctl.screen();
        if matches!(
            screen,
            Screen::Player {
                controls_visible: false
            }
        ) {
            return;
        }
        self.ui.dimmer.enabled =
            app.ctl.config.comfort.gaze_dimming && matches!(screen, Screen::Player { .. });
        self.ui.begin_frame(FrameInput {
            dt,
            pointers,
            nav: mapped.nav,
            text: Vec::new(),
            gaze_on_panel: gaze_on,
        });
        let actions: Vec<UiAction> = match screen {
            Screen::Library | Screen::Keyboard => {
                let v = view_models::library_view(&app.ctl);
                self.library.show(&mut self.ui, &v)
            }
            Screen::Player { .. } => {
                let v = view_models::player_view(&app.ctl);
                self.controls.show(&mut self.ui, &v)
            }
            Screen::PictureAdjust => {
                let v = view_models::picture_view(&app.ctl);
                self.picture.show(&mut self.ui, &v)
            }
            Screen::Settings => {
                let m = view_models::settings_model(&app.ctl);
                self.settings.show(&mut self.ui, &m)
            }
        };
        let out = self.ui.end_frame();
        if out.wants_pointer || !actions.is_empty() {
            app.ctl.activity();
        }
        app.handle_ui(actions);
        self.output = Some(out);
    }
}

/// XR + GPU objects. Field order is drop order: renderer, swapchains,
/// session, Vulkan, instance.
struct Xr {
    renderer: Renderer,
    stereo: XrSwapchain,
    ui_chain: XrSwapchain,
    sub_chain: XrSwapchain,
    session: XrSession,
    _vk: fp_xr::VulkanContext,
    _ctx: XrContext,
}

fn target(sc: &XrSwapchain, image: fp_gfx::ash::vk::Image, layer: u32) -> RenderTarget {
    RenderTarget {
        image,
        format: sc.format,
        width: sc.width,
        height: sc.height,
        array_layer: layer,
    }
}

fn init(app: &App, paths: &Paths) -> Result<Xr> {
    let comfort = &app.ctl.config.comfort;
    let ctx = XrContext::new(XrConfig {
        want_passthrough: comfort.passthrough_background,
        ..Default::default()
    })
    .context("OpenXR initialisation failed (is SteamVR running?)")?;
    tracing::info!(
        "OpenXR runtime {} on {}; enabled {:?}",
        ctx.runtime_name,
        ctx.system_name,
        ctx.enabled
    );
    let vk = fp_xr::VulkanContext::new(&ctx).context("Vulkan device via OpenXR")?;
    tracing::info!(
        "Vulkan device {} ({:?})",
        vk.device_name,
        vk.device_extensions
    );
    let session = XrSession::new(&ctx, &vk).context("creating the XR session")?;
    let ext = vk.device_extensions;
    let caps = DeviceCaps {
        dmabuf_import: ext.external_memory_fd && ext.external_memory_dma_buf,
        drm_format_modifier: ext.image_drm_format_modifier,
        queue_family_foreign: ext.queue_family_foreign,
        sampler_ycbcr_conversion: vk.sampler_ycbcr_conversion,
    };
    // SAFETY: fp-xr created these handles with Vulkan 1.3, dynamicRendering
    // and synchronization2 plus the extensions reported in `caps`; the
    // VulkanContext outlives the renderer (field order in `Xr`).
    let gpu = unsafe {
        GpuContext::new(
            vk.instance.clone(),
            vk.physical_device,
            vk.device.clone(),
            vk.queue_family_index,
            vk.queue_index,
            caps,
        )
    };
    let renderer = Renderer::new(
        gpu,
        RendererConfig {
            mesh_dir: paths.data_dir.join("meshes"),
            ..Default::default()
        },
    )
    .context("creating the renderer")?;
    let stereo = session.create_stereo_swapchain()?;
    let ui_chain = session.create_ui_swapchain(UI_PX.x as u32, UI_PX.y as u32)?;
    let sub_chain =
        session.create_ui_swapchain(subtitles::PANEL_PX.x as u32, subtitles::PANEL_PX.y as u32)?;
    tracing::info!(
        "swapchains: eyes {}x{} {:?}, UI {}x{}; refresh rates {:?}; blend {:?}",
        stereo.width,
        stereo.height,
        stereo.format,
        ui_chain.width,
        ui_chain.height,
        session.refresh_rates(),
        session.blend_mode()
    );
    Ok(Xr {
        renderer,
        stereo,
        ui_chain,
        sub_chain,
        session,
        _vk: vk,
        _ctx: ctx,
    })
}

/// Run until the runtime or the user ends the session.
pub fn run(
    app: &mut App,
    paths: &Paths,
    mut guard: Option<&mut fp_updater::LaunchGuard>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut xr = init(app, paths)?;
    app.ctl.runtime.refresh_rates = xr.session.refresh_rates().to_vec();
    if app.ctl.config.general.refresh_rate_hz > 0.0 {
        app.refresh_rate = Some(app.ctl.config.general.refresh_rate_hz);
    }
    let passthrough_mode =
        xr.session.blend_mode() == fp_xr::openxr::EnvironmentBlendMode::ALPHA_BLEND;
    let mut ui = UiLayer::new();
    let mut conv = FrameConverter::new();
    let mut perf = if crate::perf::enabled(&paths.perf_dir()) {
        PerfLog::start(&paths.perf_dir()).ok()
    } else {
        None
    };
    let mut events = Vec::new();
    let mut base_rot = Quat::IDENTITY;
    let mut last_frame: Option<(u64, MediaTime)> = None;
    let mut had_session = false;
    let mut convert_warned = false;
    let mut presented = 0u64;
    let mut exit_requested_at: Option<Instant> = None;

    loop {
        if stop.load(Ordering::Relaxed) && exit_requested_at.is_none() {
            tracing::info!("stop requested; ending the XR session");
            let _ = xr.session.request_exit();
            exit_requested_at = Some(Instant::now());
        }
        if exit_requested_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            break;
        }
        events.clear();
        xr.session.poll_events(&mut events)?;
        let mut exit = false;
        for ev in &events {
            match ev {
                XrEvent::Exit => exit = true,
                XrEvent::RefreshRateChanged { from, to } => {
                    tracing::info!("display refresh {from} → {to} Hz")
                }
                other => tracing::debug!("XR event {other:?}"),
            }
        }
        if exit || xr.session.lifecycle().exit_requested() {
            break;
        }
        if !xr.session.lifecycle().should_run_frame_loop() {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        let timing = xr.session.wait_frame()?;
        let frame_start = Instant::now();
        xr.session.begin_frame()?;
        let t = timing.predicted_display_time;
        let input = *xr.session.sync_input(t)?;
        let head = xr.session.locate_head(t);
        let views = xr.session.locate_views(t)?;
        let delay_ns = timing
            .display_delay_ns
            .unwrap_or(timing.predicted_display_period_ns * 2)
            .max(0);
        let display_at = Instant::now() + Duration::from_nanos(delay_ns as u64);

        let dt = app.tick();
        if let Some(hz) = app.refresh_rate.take() {
            if let Err(e) = xr.session.request_refresh_rate(hz) {
                tracing::debug!("refresh rate {hz} Hz not applied: {e}");
            }
        }
        if std::mem::take(&mut app.recenter) {
            match xr.session.recenter(t) {
                Ok(()) => {
                    base_rot = match (app.ctl.config.comfort.lying_down, xr.session.locate_head(t))
                    {
                        (true, Some(h)) => h.orientation,
                        _ => Quat::IDENTITY,
                    };
                }
                Err(e) => tracing::warn!("recenter failed: {e}"),
            }
        }
        if !app.ctl.config.comfort.lying_down {
            base_rot = Quat::IDENTITY;
        }
        let comfort = app.ctl.config.comfort.clone();
        let panel = PanelPlacement::new(&comfort, base_rot, head);
        ui.frame(app, &input, &panel, dt);
        if let Some(h) = head {
            // Relative to the (world-locked) video sphere.
            app.set_head_orientation(base_rot.inverse() * h.orientation);
        }
        if let Some(out) = &ui.output {
            for f in &out.feedback {
                let (src, amp, dur) = match f {
                    Feedback::HoverTick(s) => (s, 0.15, 0.008),
                    Feedback::ClickTick(s) => (s, 0.45, 0.02),
                };
                if let PointerSource::Laser(h) = src {
                    let idx = if *h == Hand::Left { 0 } else { 1 };
                    let _ = xr.session.vibrate(idx, amp, dur, 0.0);
                }
            }
        }

        // Video frame and subtitles for the predicted display instant.
        let session = app.ctl.session.as_ref().filter(|s| !s.resolving);
        let view_settings = session.map(|s| s.view.clone()).unwrap_or_default();
        let has_session = session.is_some();
        // Media time on screen at the predicted display instant (keyframed
        // corrections and subtitles follow it, not the decode position).
        let position = if has_session {
            app.video.media_time_at(display_at)
        } else {
            MediaTime::ZERO
        };
        let frame = has_session
            .then(|| app.video.frame_for_display(display_at))
            .flatten();
        let cues = if has_session {
            app.video.subtitles_at(position)
        } else {
            Vec::new()
        };
        ui.sub_output = subtitles::draw(&mut ui.subs, &cues, dt);

        let Some(views) = views.filter(|_| timing.should_render) else {
            xr.session.end_frame(&timing, &[])?;
            continue;
        };
        let r = &mut xr.renderer;
        r.begin_frame()?;
        for (key, img) in app.images.drain(..) {
            if let Err(e) = r.upload_image(key, img.width, img.height, &img.data) {
                tracing::warn!("image upload: {e}");
            }
        }
        // Thumbnails evicted from the GPU cache get requested again.
        let evicted: Vec<i64> = app
            .ctl
            .library
            .thumbs
            .iter()
            .filter_map(|(id, s)| match s {
                crate::controller::ThumbState::Ready(k)
                    if *k == (THUMB_KEY_BASE | *id as u64) && !r.has_image(*k) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for id in evicted {
            app.ctl.forget_thumbnail(id);
        }
        if has_session {
            had_session = true;
            if let Some(f) = &frame {
                let key = (f.serial, f.pts);
                if last_frame != Some(key) {
                    last_frame = Some(key);
                    let track = app
                        .ctl
                        .player
                        .media
                        .as_ref()
                        .and_then(|m| m.primary_video());
                    let (w, h) = f.frame.size();
                    match conv.convert(&f.frame, color_for_track(track, w, h)) {
                        Ok(vf) => {
                            if let Err(e) = r.upload_video_frame(&vf) {
                                tracing::warn!("video upload: {e}");
                            }
                        }
                        Err(e) if !convert_warned => {
                            convert_warned = true;
                            tracing::warn!("frame not presentable: {e}");
                        }
                        Err(_) => {}
                    }
                }
            }
        } else if had_session {
            had_session = false;
            last_frame = None;
            r.clear_video();
        }
        for c in &cues {
            if let fp_video::subtitle::CueContent::Bitmap(b) = &c.content {
                let key = subtitles::bitmap_key(b);
                if !r.has_image(key) {
                    let _ = r.upload_image(key, b.width, b.height, &b.rgba);
                }
            }
        }

        let (_, eye_img) = xr.stereo.acquire()?;
        let corrections = view_settings.corrections_at(position);
        r.render_eyes(&EyeRenderRequest {
            views: [eye_view(&views[0]), eye_view(&views[1])],
            targets: [
                target(&xr.stereo, eye_img, 0),
                target(&xr.stereo, eye_img, 1),
            ],
            settings: &view_settings,
            corrections: &corrections,
            model: model_matrix(&view_settings, comfort.head_locked_screen, head, base_rot),
            follow_head: view_settings.projection.is_immersive(),
            clear_color: clear_color(&comfort, passthrough_mode && comfort.passthrough_background),
            tint: [1.0; 4],
        })?;
        let ui_drawn = if let Some(out) = &ui.output {
            let (_, img) = xr.ui_chain.acquire()?;
            r.render_ui(&UiRenderRequest {
                draw_list: &out.draw_list,
                atlas: Some(ui.ui.atlas()),
                target: target(&xr.ui_chain, img, 0),
                panel_size: [UI_PX.x, UI_PX.y],
                clear_color: [0.0; 4],
            })?;
            true
        } else {
            false
        };
        let subs_drawn = if let Some(out) = &ui.sub_output {
            let (_, img) = xr.sub_chain.acquire()?;
            r.render_ui(&UiRenderRequest {
                draw_list: &out.draw_list,
                atlas: Some(ui.subs.atlas()),
                target: target(&xr.sub_chain, img, 0),
                panel_size: [subtitles::PANEL_PX.x, subtitles::PANEL_PX.y],
                clear_color: [0.0; 4],
            })?;
            true
        } else {
            false
        };
        r.end_frame()?;
        xr.stereo.release()?;
        if ui_drawn {
            xr.ui_chain.release()?;
        }
        if subs_drawn {
            xr.sub_chain.release()?;
        }

        let mut layers = vec![Layer::Projection {
            swapchain: &xr.stereo,
            views: &views,
            alpha_blend: passthrough_mode && comfort.passthrough_background,
        }];
        if subs_drawn {
            let depth = app.ctl.config.playback.subtitle_depth_m;
            let (pos, rot) = subtitles::panel_pose(depth, base_rot);
            layers.push(Layer::Quad {
                swapchain: &xr.sub_chain,
                pose: Pose::new(pos, rot),
                size: subtitles::panel_size_m(depth),
                head_locked: false,
            });
        }
        if ui_drawn {
            layers.push(if panel.curved {
                Layer::Cylinder {
                    swapchain: &xr.ui_chain,
                    pose: panel.local,
                    radius: panel.radius,
                    central_angle: panel.size_m.x / panel.radius,
                    aspect_ratio: panel.size_m.x / panel.size_m.y,
                    head_locked: panel.head_locked,
                }
            } else {
                Layer::Quad {
                    swapchain: &xr.ui_chain,
                    pose: panel.local,
                    size: panel.size_m,
                    head_locked: panel.head_locked,
                }
            });
        }
        xr.session.end_frame(&timing, &layers)?;
        presented += 1;
        if presented == 1 {
            tracing::info!("first frame presented");
            if let Some(g) = guard.as_deref_mut() {
                g.mark_healthy_now();
            }
        }
        if let Some(p) = perf.as_mut() {
            // gpu_ms: the renderer has no timestamp queries yet; 0 = not measured.
            p.record(
                timing.predicted_display_time_ns(),
                timing.predicted_display_period_ns,
                frame_start.elapsed().as_secs_f32() * 1000.0,
                0.0,
                app.video.queue().len(),
            );
        }
    }
    xr.renderer.wait_idle().ok();
    tracing::info!("XR session ended after {presented} frames");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::Projection;

    #[test]
    fn panel_placement_and_hits() {
        let mut comfort = Comfort {
            curved_ui: false,
            ..Default::default()
        };
        let p = PanelPlacement::new(&comfort, Quat::IDENTITY, None);
        assert!((p.local.position.z + 1.3).abs() < 1e-5);
        // Ray straight at the panel centre hits the middle pixel.
        let ray = Ray::new(Vec3::new(0.0, -UI_DROP_M, 0.0), Vec3::NEG_Z);
        let (px, d) = p.hit(&ray).unwrap();
        assert!((px - UI_PX / 2.0).length() < 1.0, "{px:?}");
        assert!((d - 1.3).abs() < 1e-3);
        // Pointing away misses.
        assert!(p.hit(&Ray::new(Vec3::ZERO, Vec3::Z)).is_none());
        // Curved panel: same centre hit.
        comfort.curved_ui = true;
        let c = PanelPlacement::new(&comfort, Quat::IDENTITY, None);
        let (px, _) = c.hit(&ray).unwrap();
        assert!((px - UI_PX / 2.0).length() < 2.0, "{px:?}");
        // Head-locked panels follow the head for hit testing.
        comfort.head_locked_screen = true;
        comfort.curved_ui = false;
        let head = Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY);
        let h = PanelPlacement::new(&comfort, Quat::IDENTITY, Some(head));
        assert!((h.world.position.x - 1.0).abs() < 1e-5);
        assert!((h.local.position.x).abs() < 1e-5);
    }

    #[test]
    fn model_and_clear() {
        let flat = ViewSettings::default();
        let head = Pose::new(Vec3::new(0.0, 1.6, 0.0), Quat::from_rotation_y(0.5));
        assert_eq!(
            model_matrix(&flat, true, Some(head), Quat::IDENTITY),
            head.to_mat4()
        );
        assert_eq!(
            model_matrix(&flat, false, Some(head), Quat::IDENTITY),
            Mat4::IDENTITY
        );
        let sphere = ViewSettings {
            projection: Projection::EQUIRECT_180,
            ..Default::default()
        };
        assert_eq!(
            model_matrix(&sphere, true, Some(head), Quat::IDENTITY),
            Mat4::IDENTITY
        );
        let c = Comfort::default();
        assert_eq!(clear_color(&c, true), [0.0; 4]);
        assert_eq!(clear_color(&c, false)[3], 1.0);
    }

    #[test]
    fn eye_view_copies_pose_and_fov() {
        let v = View {
            pose: Pose::new(Vec3::new(0.03, 0.0, 0.0), Quat::IDENTITY),
            fov: fp_xr::openxr::Fovf {
                angle_left: -0.9,
                angle_right: 0.8,
                angle_up: 0.7,
                angle_down: -0.75,
            },
        };
        let e = eye_view(&v);
        assert_eq!(e.position, Vec3::new(0.03, 0.0, 0.0));
        assert_eq!(e.fov.angle_left, -0.9);
        assert_eq!(e.fov.angle_down, -0.75);
    }

    #[test]
    fn ui_layer_frame_without_xr() {
        // The UI half of the frame loop runs without a session.
        let mut layer = UiLayer::new();
        let input = InputState::default();
        let panel = PanelPlacement::new(&Comfort::default(), Quat::IDENTITY, None);
        let mut app = crate::tests::test_app();
        layer.frame(&mut app, &input, &panel, 1.0 / 72.0);
        let out = layer.output.as_ref().expect("library is visible");
        assert!(!out.draw_list.cmds.is_empty());
        drop(layer);
        app.shutdown();
    }
}
