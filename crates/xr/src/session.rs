//! Instance/system setup ([`XrContext`]) and the running session
//! ([`XrSession`]): lifecycle-driven event polling, frame loop, views,
//! reference spaces with recentering, swapchains and composition layers.

use crate::extensions::{ExtensionReport, ExtensionWishes};
use crate::input::{InputState, InputSystem};
use crate::lifecycle::{Lifecycle, LifecycleAction, SessionState};
use crate::math::{recenter_pose, Pose};
use crate::select::{
    choose_blend_mode, choose_color_format, choose_ui_format, ColorFormatPreference,
};
use crate::vulkan::VulkanContext;
use crate::XrError;
use ash::vk::{self, Handle};
use glam::Vec2;
use openxr as xr;

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;
/// Maximum composition layers per frame.
pub const MAX_LAYERS: usize = 8;

/// Startup options.
#[derive(Debug, Clone)]
pub struct XrConfig {
    pub app_name: String,
    pub app_version: u32,
    pub wishes: ExtensionWishes,
    /// Ask for `ALPHA_BLEND` so system passthrough can show through.
    pub want_passthrough: bool,
    pub color_format: ColorFormatPreference,
    /// Base reference space: STAGE (floor-level, room-scale) when available,
    /// else LOCAL.
    pub prefer_stage: bool,
}

impl Default for XrConfig {
    fn default() -> Self {
        XrConfig {
            app_name: "FramePlayer".into(),
            app_version: 1,
            wishes: ExtensionWishes::default(),
            want_passthrough: false,
            color_format: ColorFormatPreference::default(),
            prefer_stage: false,
        }
    }
}

/// OpenXR instance + HMD system.
pub struct XrContext {
    pub instance: xr::Instance,
    pub system: xr::SystemId,
    /// What the runtime exposes.
    pub available: ExtensionReport,
    /// What we enabled.
    pub enabled: ExtensionReport,
    pub runtime_name: String,
    pub system_name: String,
    pub view_configs: [xr::ViewConfigurationView; 2],
    pub blend_modes: Vec<xr::EnvironmentBlendMode>,
    pub hand_tracking_supported: bool,
    pub config: XrConfig,
    // Keeps the loader alive for the instance's lifetime.
    _entry: xr::Entry,
}

/// Candidate OpenXR loader paths, most specific first: a copy bundled next to
/// the binary (`<exe dir>/../lib`), then the system's versioned soname (the
/// only name runtime packages ship), then the unversioned dev symlink.
// [verify] Whether SteamOS on the Frame ships libopenxr_loader.so.1 at all, or
// whether native apps must bundle it (frameplayer-probe reports this).
pub fn loader_candidates(exe: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(lib) = exe
        .and_then(|e| e.parent())
        .and_then(|bin| bin.parent())
        .map(|root| root.join("lib"))
    {
        out.push(lib.join("libopenxr_loader.so.1"));
        out.push(lib.join("libopenxr_loader.so"));
    }
    out.push("libopenxr_loader.so.1".into());
    out.push("libopenxr_loader.so".into());
    out
}

fn load_loader() -> Result<xr::Entry, XrError> {
    let exe = std::env::current_exe().ok();
    let mut errors = Vec::new();
    for path in loader_candidates(exe.as_deref()) {
        if path.is_absolute() && !path.exists() {
            continue;
        }
        match unsafe { xr::Entry::load_from(&path) } {
            Ok(entry) => {
                tracing::info!("OpenXR loader: {}", path.display());
                return Ok(entry);
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    Err(XrError::Load(errors.join("; ")))
}

impl XrContext {
    pub fn new(config: XrConfig) -> Result<XrContext, XrError> {
        let entry = load_loader()?;
        let available_set = entry.enumerate_extensions()?;
        let available = ExtensionReport::from_available(&available_set);
        tracing::info!("OpenXR runtime extensions: {}", available.summary());
        // [verify] Run on the Frame and record this line in docs/platform-notes.md.
        if !available.vulkan_enable2 {
            return Err(XrError::Unsupported(
                "XR_KHR_vulkan_enable2 not offered by the runtime".into(),
            ));
        }
        let enable = available.to_enable(&config.wishes);
        let enabled = available.enabled(&config.wishes);
        let app = xr::ApplicationInfo {
            application_name: &config.app_name,
            application_version: config.app_version,
            engine_name: "fp-xr",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        };
        let instance = entry.create_instance(&app, &enable, &[])?;
        let props = instance.properties()?;
        let runtime_name = format!(
            "{} {}.{}.{}",
            props.runtime_name,
            props.runtime_version.major(),
            props.runtime_version.minor(),
            props.runtime_version.patch()
        );
        let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)?;
        let sys_props = instance.system_properties(system)?;
        let hand_tracking_supported =
            enabled.hand_tracking && instance.supports_hand_tracking(system).unwrap_or(false);
        let blend_modes = instance.enumerate_environment_blend_modes(system, VIEW_TYPE)?;
        let views = instance.enumerate_view_configuration_views(system, VIEW_TYPE)?;
        if views.len() < 2 {
            return Err(XrError::Unsupported(
                "primary stereo view configuration missing".into(),
            ));
        }
        tracing::info!(
            "OpenXR: {runtime_name}, system '{}', views {}x{} (max {}x{}), blend modes {:?}, hand tracking {}",
            sys_props.system_name,
            views[0].recommended_image_rect_width,
            views[0].recommended_image_rect_height,
            views[0].max_image_rect_width,
            views[0].max_image_rect_height,
            blend_modes,
            hand_tracking_supported
        );
        Ok(XrContext {
            instance,
            system,
            available,
            enabled,
            runtime_name,
            system_name: sys_props.system_name,
            view_configs: [views[0], views[1]],
            blend_modes,
            hand_tracking_supported,
            config,
            _entry: entry,
        })
    }

    /// Recommended per-eye render size.
    pub fn recommended_eye_size(&self) -> (u32, u32) {
        (
            self.view_configs[0].recommended_image_rect_width,
            self.view_configs[0].recommended_image_rect_height,
        )
    }
}

/// Result of `wait_frame`.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    pub predicted_display_time: xr::Time,
    pub predicted_display_period_ns: i64,
    pub should_render: bool,
    /// Predicted display time minus "now" (needs XR_KHR_convert_timespec_time).
    /// The video engine adds this to `CLOCK_MONOTONIC` to pick the frame that
    /// will actually be on screen.
    pub display_delay_ns: Option<i64>,
}

impl FrameTiming {
    pub fn predicted_display_time_ns(&self) -> i64 {
        self.predicted_display_time.as_nanos()
    }
}

/// One located eye view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub pose: Pose,
    pub fov: xr::Fovf,
}

impl View {
    /// FOV angles as `[left, right, up, down]` radians.
    pub fn fov_array(&self) -> [f32; 4] {
        crate::math::fov_to_array(&self.fov)
    }
}

/// Events surfaced to the app after lifecycle handling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XrEvent {
    StateChanged(SessionState),
    InteractionProfileChanged,
    ReferenceSpaceChangePending,
    RefreshRateChanged {
        from: f32,
        to: f32,
    },
    EventsLost(u32),
    /// Tear down: session exiting, loss pending or instance lost.
    Exit,
}

/// An OpenXR swapchain with its Vulkan images.
pub struct XrSwapchain {
    handle: xr::Swapchain<xr::Vulkan>,
    pub images: Vec<vk::Image>,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub array_size: u32,
    acquired: Option<u32>,
}

impl XrSwapchain {
    /// Acquire and wait for the next image. Returns `(index, image)`.
    pub fn acquire(&mut self) -> Result<(u32, vk::Image), XrError> {
        let i = self.handle.acquire_image()?;
        self.handle.wait_image(xr::Duration::INFINITE)?;
        self.acquired = Some(i);
        Ok((i, self.images[i as usize]))
    }

    /// Release the acquired image (after GPU work was submitted).
    pub fn release(&mut self) -> Result<(), XrError> {
        if self.acquired.take().is_some() {
            self.handle.release_image()?;
        }
        Ok(())
    }

    pub fn acquired_image(&self) -> Option<vk::Image> {
        self.acquired.map(|i| self.images[i as usize])
    }

    fn rect(&self) -> xr::Rect2Di {
        xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.width as i32,
                height: self.height as i32,
            },
        }
    }
}

/// A composition layer to submit, back to front.
pub enum Layer<'a> {
    /// Stereo projection from an array swapchain (layer 0 = left, 1 = right).
    Projection {
        swapchain: &'a XrSwapchain,
        views: &'a [View; 2],
        alpha_blend: bool,
    },
    /// Flat UI panel facing +Z of `pose`, `size` in metres.
    Quad {
        swapchain: &'a XrSwapchain,
        pose: Pose,
        size: Vec2,
        head_locked: bool,
    },
    /// Curved UI panel; falls back to a quad without the cylinder extension.
    Cylinder {
        swapchain: &'a XrSwapchain,
        pose: Pose,
        radius: f32,
        central_angle: f32,
        aspect_ratio: f32,
        head_locked: bool,
    },
}

struct Spaces {
    base_type: xr::ReferenceSpaceType,
    base: xr::Space,
    view: xr::Space,
    app: xr::Space,
    recenter: Pose,
}

/// The running OpenXR session.
pub struct XrSession {
    // Field order is drop order: spaces/actions before the session.
    input: InputSystem,
    input_state: InputState,
    spaces: Spaces,
    stream: xr::FrameStream<xr::Vulkan>,
    waiter: xr::FrameWaiter,
    session: xr::Session<xr::Vulkan>,
    instance: xr::Instance,
    lifecycle: Lifecycle,
    blend_mode: xr::EnvironmentBlendMode,
    enabled: ExtensionReport,
    view_configs: [xr::ViewConfigurationView; 2],
    color_format: ColorFormatPreference,
    refresh_rates: Vec<f32>,
    event_buf: Box<xr::EventDataBuffer>,
}

impl XrSession {
    /// Create the session on the Vulkan device from [`VulkanContext`].
    pub fn new(ctx: &XrContext, vk_ctx: &VulkanContext) -> Result<XrSession, XrError> {
        let instance = ctx.instance.clone();
        let mut input = InputSystem::new(&instance, ctx.enabled.eye_gaze_interaction)?;
        let (session, waiter, stream) = unsafe {
            instance.create_session::<xr::Vulkan>(ctx.system, &vk_ctx.session_create_info())?
        };
        input.attach(&session, ctx.hand_tracking_supported)?;

        let available = session.enumerate_reference_spaces()?;
        let base_type =
            if ctx.config.prefer_stage && available.contains(&xr::ReferenceSpaceType::STAGE) {
                xr::ReferenceSpaceType::STAGE
            } else {
                xr::ReferenceSpaceType::LOCAL
            };
        let base = session.create_reference_space(base_type, xr::Posef::IDENTITY)?;
        let view =
            session.create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)?;
        let app = session.create_reference_space(base_type, xr::Posef::IDENTITY)?;
        let blend_mode = choose_blend_mode(&ctx.blend_modes, ctx.config.want_passthrough);
        let refresh_rates = if ctx.enabled.display_refresh_rate {
            session
                .enumerate_display_refresh_rates()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        tracing::info!("XR session: base space {base_type:?}, blend {blend_mode:?}, refresh rates {refresh_rates:?}");
        let mut s = XrSession {
            input,
            input_state: InputState::default(),
            spaces: Spaces {
                base_type,
                base,
                view,
                app,
                recenter: Pose::IDENTITY,
            },
            stream,
            waiter,
            session,
            instance,
            lifecycle: Lifecycle::default(),
            blend_mode,
            enabled: ctx.enabled,
            view_configs: ctx.view_configs,
            color_format: ctx.config.color_format,
            refresh_rates,
            event_buf: Box::default(),
        };
        s.input.refresh_profiles(&s.session, &mut s.input_state);
        Ok(s)
    }

    pub fn raw(&self) -> &xr::Session<xr::Vulkan> {
        &self.session
    }
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }
    pub fn blend_mode(&self) -> xr::EnvironmentBlendMode {
        self.blend_mode
    }
    pub fn input(&self) -> &InputState {
        &self.input_state
    }
    pub fn input_system(&mut self) -> &mut InputSystem {
        &mut self.input
    }
    /// The recentered app space all poses are reported in.
    pub fn app_space(&self) -> &xr::Space {
        &self.spaces.app
    }
    pub fn view_space(&self) -> &xr::Space {
        &self.spaces.view
    }
    pub fn recenter_pose(&self) -> Pose {
        self.spaces.recenter
    }

    /// Drain runtime events, driving begin/end session automatically.
    pub fn poll_events(&mut self, out: &mut Vec<XrEvent>) -> Result<(), XrError> {
        loop {
            // Copy what we need out of the borrowed event before acting on it.
            let ev = match self.instance.poll_event(&mut self.event_buf)? {
                None => break,
                Some(xr::Event::SessionStateChanged(e)) => XrEvent::StateChanged(e.state().into()),
                Some(xr::Event::InstanceLossPending(_)) => {
                    self.lifecycle.on_instance_loss();
                    XrEvent::Exit
                }
                Some(xr::Event::InteractionProfileChanged(_)) => XrEvent::InteractionProfileChanged,
                Some(xr::Event::ReferenceSpaceChangePending(_)) => {
                    XrEvent::ReferenceSpaceChangePending
                }
                Some(xr::Event::DisplayRefreshRateChangedFB(e)) => XrEvent::RefreshRateChanged {
                    from: e.from_display_refresh_rate(),
                    to: e.to_display_refresh_rate(),
                },
                Some(xr::Event::EventsLost(e)) => XrEvent::EventsLost(e.lost_event_count()),
                Some(_) => continue,
            };
            out.push(ev);
            match ev {
                XrEvent::StateChanged(s) => match self.lifecycle.on_state_changed(s) {
                    LifecycleAction::BeginSession => {
                        self.session.begin(VIEW_TYPE)?;
                        tracing::info!("XR session begun");
                    }
                    LifecycleAction::EndSession => {
                        self.session.end()?;
                        tracing::info!("XR session ended");
                    }
                    LifecycleAction::Exit => out.push(XrEvent::Exit),
                    LifecycleAction::None => {}
                },
                XrEvent::InteractionProfileChanged => self
                    .input
                    .refresh_profiles(&self.session, &mut self.input_state),
                _ => {}
            }
        }
        Ok(())
    }

    /// Ask the runtime to end the session (it will go STOPPING → EXITING).
    pub fn request_exit(&self) -> Result<(), XrError> {
        Ok(self.session.request_exit()?)
    }

    /// Block until the runtime wants the next frame.
    pub fn wait_frame(&mut self) -> Result<FrameTiming, XrError> {
        let s = self.waiter.wait()?;
        let display_delay_ns = if self.enabled.convert_timespec_time {
            self.instance
                .now()
                .ok()
                .map(|now| s.predicted_display_time.as_nanos() - now.as_nanos())
        } else {
            None
        };
        Ok(FrameTiming {
            predicted_display_time: s.predicted_display_time,
            predicted_display_period_ns: s.predicted_display_period.as_nanos(),
            should_render: s.should_render,
            display_delay_ns,
        })
    }

    pub fn begin_frame(&mut self) -> Result<(), XrError> {
        Ok(self.stream.begin()?)
    }

    /// Locate both eyes in the app space (no allocation). `None` while
    /// orientation tracking is lost.
    pub fn locate_views(&self, time: xr::Time) -> Result<Option<[View; 2]>, XrError> {
        let info = xr::sys::ViewLocateInfo {
            ty: xr::sys::ViewLocateInfo::TYPE,
            next: std::ptr::null(),
            view_configuration_type: VIEW_TYPE,
            display_time: time,
            space: self.spaces.app.as_raw(),
        };
        let mut state = xr::sys::ViewState {
            ty: xr::sys::ViewState::TYPE,
            next: std::ptr::null_mut(),
            view_state_flags: xr::ViewStateFlags::EMPTY,
        };
        let blank = xr::sys::View {
            ty: xr::sys::View::TYPE,
            next: std::ptr::null_mut(),
            pose: xr::Posef::IDENTITY,
            fov: xr::Fovf::default(),
        };
        let mut views = [blank; 2];
        let mut count = 0u32;
        let r = unsafe {
            (self.instance.fp().locate_views)(
                self.session.as_raw(),
                &info,
                &mut state,
                2,
                &mut count,
                views.as_mut_ptr(),
            )
        };
        if r.into_raw() < 0 {
            return Err(XrError::Xr(r));
        }
        if count < 2
            || !state
                .view_state_flags
                .contains(xr::ViewStateFlags::ORIENTATION_VALID)
        {
            return Ok(None);
        }
        let pos_valid = state
            .view_state_flags
            .contains(xr::ViewStateFlags::POSITION_VALID);
        Ok(Some(views.map(|v| {
            let mut pose = Pose::from_xr(&v.pose);
            if !pos_valid {
                pose.position = glam::Vec3::ZERO;
            }
            View { pose, fov: v.fov }
        })))
    }

    /// Head pose in the app space.
    pub fn locate_head(&self, time: xr::Time) -> Option<Pose> {
        let loc = self.spaces.view.locate(&self.spaces.app, time).ok()?;
        loc.location_flags
            .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
            .then(|| Pose::from_xr(&loc.pose))
    }

    /// Sample controllers, gaze and hands for `time` (normally the
    /// predicted display time).
    pub fn sync_input(&mut self, time: xr::Time) -> Result<&InputState, XrError> {
        let focused = self.lifecycle.is_focused();
        self.input.update(
            &self.session,
            &self.spaces.app,
            time,
            focused,
            &mut self.input_state,
        )?;
        Ok(&self.input_state)
    }

    pub fn vibrate(
        &self,
        hand: usize,
        amplitude: f32,
        duration_s: f32,
        frequency_hz: f32,
    ) -> Result<(), XrError> {
        self.input
            .vibrate(&self.session, hand, amplitude, duration_s, frequency_hz)
    }

    /// Make the current head heading "forward" and the head position the
    /// origin of the app space.
    pub fn recenter(&mut self, time: xr::Time) -> Result<(), XrError> {
        let loc = self.spaces.view.locate(&self.spaces.base, time)?;
        if !loc
            .location_flags
            .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
        {
            return Err(XrError::Unsupported("head pose not tracked".into()));
        }
        let head = Pose::from_xr(&loc.pose);
        let pose = recenter_pose(
            &head,
            self.spaces.base_type == xr::ReferenceSpaceType::STAGE,
        );
        self.spaces.app = self
            .session
            .create_reference_space(self.spaces.base_type, pose.to_xr())?;
        self.spaces.recenter = pose;
        Ok(())
    }

    /// Stereo array swapchain at the recommended size.
    pub fn create_stereo_swapchain(&self) -> Result<XrSwapchain, XrError> {
        let formats = self.session.enumerate_swapchain_formats()?;
        let offered: Vec<i64> = formats.iter().map(|&f| f as i64).collect();
        tracing::info!(
            "XR swapchain formats offered: {:?}",
            formats
                .iter()
                .map(|&f| vk::Format::from_raw(f as i32))
                .collect::<Vec<_>>()
        );
        let format = choose_color_format(&offered, self.color_format)
            .ok_or_else(|| XrError::Unsupported("no swapchain formats".into()))?;
        let v = &self.view_configs[0];
        self.create_swapchain(
            format,
            v.recommended_image_rect_width,
            v.recommended_image_rect_height,
            2,
        )
    }

    /// Single-layer swapchain for a UI quad/cylinder layer.
    pub fn create_ui_swapchain(&self, width: u32, height: u32) -> Result<XrSwapchain, XrError> {
        let formats = self.session.enumerate_swapchain_formats()?;
        let offered: Vec<i64> = formats.iter().map(|&f| f as i64).collect();
        let format = choose_ui_format(&offered)
            .ok_or_else(|| XrError::Unsupported("no swapchain formats".into()))?;
        self.create_swapchain(format, width, height, 1)
    }

    fn create_swapchain(
        &self,
        format: vk::Format,
        width: u32,
        height: u32,
        array_size: u32,
    ) -> Result<XrSwapchain, XrError> {
        let info = xr::SwapchainCreateInfo::<xr::Vulkan> {
            create_flags: xr::SwapchainCreateFlags::EMPTY,
            usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                | xr::SwapchainUsageFlags::SAMPLED,
            format: format.as_raw() as _,
            sample_count: 1,
            width,
            height,
            face_count: 1,
            array_size,
            mip_count: 1,
        };
        let handle = self.session.create_swapchain(&info)?;
        let images = handle
            .enumerate_images()?
            .into_iter()
            .map(vk::Image::from_raw)
            .collect();
        Ok(XrSwapchain {
            handle,
            images,
            format,
            width,
            height,
            array_size,
            acquired: None,
        })
    }

    /// Offered refresh rates (empty without XR_FB_display_refresh_rate).
    pub fn refresh_rates(&self) -> &[f32] {
        &self.refresh_rates
    }

    pub fn current_refresh_rate(&self) -> Option<f32> {
        self.enabled
            .display_refresh_rate
            .then(|| self.session.get_display_refresh_rate().ok())
            .flatten()
    }

    pub fn request_refresh_rate(&self, hz: f32) -> Result<(), XrError> {
        if !self.enabled.display_refresh_rate {
            return Err(XrError::Unsupported("XR_FB_display_refresh_rate".into()));
        }
        Ok(self.session.request_display_refresh_rate(hz)?)
    }

    /// Submit the frame. With `timing.should_render == false` the layers are
    /// ignored and an empty frame is submitted, as the spec requires.
    pub fn end_frame(&mut self, timing: &FrameTiming, layers: &[Layer<'_>]) -> Result<(), XrError> {
        if !timing.should_render || layers.is_empty() {
            self.stream
                .end(timing.predicted_display_time, self.blend_mode, &[])?;
            return Ok(());
        }
        let layers = &layers[..layers.len().min(MAX_LAYERS)];
        let cylinder_ok = self.enabled.composition_layer_cylinder;
        let alpha = xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;
        let space_for = |head_locked: bool| {
            if head_locked {
                &self.spaces.view
            } else {
                &self.spaces.app
            }
        };

        // Pass 1: projection views (borrowed by the projection layers below).
        let mut proj_views: [Option<[xr::CompositionLayerProjectionView<'_, xr::Vulkan>; 2]>;
            MAX_LAYERS] = Default::default();
        for (i, l) in layers.iter().enumerate() {
            if let Layer::Projection {
                swapchain, views, ..
            } = l
            {
                proj_views[i] = Some(std::array::from_fn(|eye| {
                    xr::CompositionLayerProjectionView::new().pose(views[eye].pose.to_xr()).fov(views[eye].fov).sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&swapchain.handle)
                            .image_array_index(eye.min(swapchain.array_size as usize - 1) as u32)
                            .image_rect(swapchain.rect()),
                    )
                }));
            }
        }
        // Pass 2: typed layers.
        let mut projs: [Option<xr::CompositionLayerProjection<'_, xr::Vulkan>>; MAX_LAYERS] =
            Default::default();
        let mut quads: [Option<xr::CompositionLayerQuad<'_, xr::Vulkan>>; MAX_LAYERS] =
            Default::default();
        let mut cyls: [Option<xr::CompositionLayerCylinderKHR<'_, xr::Vulkan>>; MAX_LAYERS] =
            Default::default();
        for (i, l) in layers.iter().enumerate() {
            match *l {
                Layer::Projection { alpha_blend, .. } => {
                    let flags = if alpha_blend {
                        alpha
                    } else {
                        xr::CompositionLayerFlags::EMPTY
                    };
                    if let Some(v) = &proj_views[i] {
                        projs[i] = Some(
                            xr::CompositionLayerProjection::new()
                                .layer_flags(flags)
                                .space(&self.spaces.app)
                                .views(v),
                        );
                    }
                }
                Layer::Quad {
                    swapchain,
                    pose,
                    size,
                    head_locked,
                } => {
                    quads[i] = Some(quad(swapchain, pose, size, space_for(head_locked), alpha));
                }
                Layer::Cylinder {
                    swapchain,
                    pose,
                    radius,
                    central_angle,
                    aspect_ratio,
                    head_locked,
                } => {
                    if cylinder_ok {
                        cyls[i] = Some(
                            xr::CompositionLayerCylinderKHR::new()
                                .layer_flags(alpha)
                                .space(space_for(head_locked))
                                .eye_visibility(xr::EyeVisibility::BOTH)
                                .sub_image(
                                    xr::SwapchainSubImage::new()
                                        .swapchain(&swapchain.handle)
                                        .image_array_index(0)
                                        .image_rect(swapchain.rect()),
                                )
                                .pose(pose.to_xr())
                                .radius(radius)
                                .central_angle(central_angle)
                                .aspect_ratio(aspect_ratio),
                        );
                    } else {
                        // Flat stand-in of the same width, pushed out to the radius.
                        let w = radius * central_angle;
                        let size = Vec2::new(w, w / aspect_ratio.max(0.01));
                        let centre = Pose::new(
                            pose.transform_point(glam::Vec3::new(0.0, 0.0, -radius)),
                            pose.orientation,
                        );
                        quads[i] =
                            Some(quad(swapchain, centre, size, space_for(head_locked), alpha));
                    }
                }
            }
        }
        // Pass 3: base references in submission order.
        let mut refs: [Option<&xr::CompositionLayerBase<'_, xr::Vulkan>>; MAX_LAYERS] =
            [None; MAX_LAYERS];
        let mut n = 0;
        for i in 0..layers.len() {
            let base: Option<&xr::CompositionLayerBase<'_, xr::Vulkan>> = projs[i]
                .as_deref()
                .or_else(|| quads[i].as_deref())
                .or_else(|| cyls[i].as_deref());
            if let Some(b) = base {
                refs[n] = Some(b);
                n += 1;
            }
        }
        let Some(first) = refs[0] else {
            self.stream
                .end(timing.predicted_display_time, self.blend_mode, &[])?;
            return Ok(());
        };
        let mut flat: [&xr::CompositionLayerBase<'_, xr::Vulkan>; MAX_LAYERS] = [first; MAX_LAYERS];
        for (dst, src) in flat.iter_mut().zip(refs.iter()).take(n) {
            *dst = src.expect("filled");
        }
        self.stream
            .end(timing.predicted_display_time, self.blend_mode, &flat[..n])?;
        Ok(())
    }

    /// Session state as last reported.
    pub fn state(&self) -> SessionState {
        self.lifecycle.state()
    }
}

fn quad<'a>(
    sc: &'a XrSwapchain,
    pose: Pose,
    size: Vec2,
    space: &'a xr::Space,
    flags: xr::CompositionLayerFlags,
) -> xr::CompositionLayerQuad<'a, xr::Vulkan> {
    xr::CompositionLayerQuad::new()
        .layer_flags(flags)
        .space(space)
        .eye_visibility(xr::EyeVisibility::BOTH)
        .sub_image(
            xr::SwapchainSubImage::new()
                .swapchain(&sc.handle)
                .image_array_index(0)
                .image_rect(sc.rect()),
        )
        .pose(pose.to_xr())
        .size(xr::Extent2Df {
            width: size.x,
            height: size.y,
        })
}

#[cfg(test)]
mod loader_tests {
    use super::loader_candidates;
    use std::path::{Path, PathBuf};

    #[test]
    fn bundled_copy_first_then_versioned_soname() {
        let c = loader_candidates(Some(Path::new("/opt/fp/versions/1/bin/frameplayer")));
        assert_eq!(
            c[0],
            PathBuf::from("/opt/fp/versions/1/lib/libopenxr_loader.so.1")
        );
        assert_eq!(c[2], PathBuf::from("libopenxr_loader.so.1"));
        assert_eq!(c.last().unwrap(), &PathBuf::from("libopenxr_loader.so"));
        assert_eq!(loader_candidates(None).len(), 2);
    }
}
