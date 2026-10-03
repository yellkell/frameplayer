//! Session lifecycle, swapchains and the frame loop.

use crate::context::XrContext;
use crate::input::{Actions, InputState};
use crate::{Error, Result, XrContextExt};
use ash::vk::{self, Handle};
use fp_render::{EyeTarget, EyeView, Gpu};
use glam::{Mat4, Quat, Vec3};
use openxr as xr;
use std::sync::Arc;

/// What happened while polling events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// Nothing that needs the app's attention.
    None,
    /// The session is running and frames should be produced.
    Running,
    /// The runtime asked us to stop (headset removed, app closed from the
    /// system menu): leave the loop.
    Exit,
}

struct Eye {
    swapchain: xr::Swapchain<xr::Vulkan>,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
}

pub struct XrSession {
    ctx: Arc<XrContext>,
    gpu: Arc<Gpu>,
    pub session: xr::Session<xr::Vulkan>,
    waiter: xr::FrameWaiter,
    stream: xr::FrameStream<xr::Vulkan>,
    space: xr::Space,
    view_space: xr::Space,
    eyes: Vec<Eye>,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    actions: Actions,
    state: xr::SessionState,
    running: bool,
    pub blend_mode: xr::EnvironmentBlendMode,
    event_buf: xr::EventDataBuffer,
    /// Offset of our play space within the runtime's LOCAL space.
    space_origin: (Vec3, Quat),
}

/// One frame in progress.
pub struct FrameCtx {
    pub state: xr::FrameState,
    /// Eye cameras for rendering (empty when the runtime says not to render).
    pub eyes: Option<[EyeView; 2]>,
    /// Head pose in the play space.
    pub head: Option<(Vec3, Quat)>,
    pub targets: Option<[EyeTarget; 2]>,
    views: Vec<xr::View>,
}

/// Preferred eye formats, sRGB first.
const FORMATS: [vk::Format; 2] = [vk::Format::R8G8B8A8_SRGB, vk::Format::B8G8R8A8_SRGB];

impl XrSession {
    /// Creates the session on `gpu` (which must have been created with
    /// `ctx.creator()`).
    pub fn new(ctx: Arc<XrContext>, gpu: Arc<Gpu>) -> Result<XrSession> {
        let mut actions = Actions::new(&ctx.instance)?;
        let info = xr::vulkan::SessionCreateInfo {
            instance: gpu.instance.handle().as_raw() as _,
            physical_device: gpu.pdev.as_raw() as _,
            device: gpu.device.handle().as_raw() as _,
            queue_family_index: gpu.queue_family,
            queue_index: 0,
        };
        // SAFETY: the Vulkan objects were created through this runtime and
        // outlive the session (the app drops the session first).
        let (session, waiter, stream) =
            unsafe { ctx.instance.create_session::<xr::Vulkan>(ctx.system, &info) }
                .ctx("xrCreateSession")?;
        actions.attach(&session)?;
        let space = session
            .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            .ctx("local space")?;
        let view_space = session
            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
            .ctx("view space")?;
        let formats = session
            .enumerate_swapchain_formats()
            .ctx("xrEnumerateSwapchainFormats")?;
        let format = FORMATS
            .iter()
            .copied()
            .find(|f| formats.contains(&(f.as_raw() as u32)))
            .ok_or_else(|| Error::Runtime(format!("no sRGB swapchain format among {formats:?}")))?;
        let (w, h) = ctx.eye_size()?;
        let mut eyes = Vec::new();
        for _ in 0..2 {
            let swapchain = session
                .create_swapchain(&xr::SwapchainCreateInfo {
                    create_flags: xr::SwapchainCreateFlags::EMPTY,
                    usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | xr::SwapchainUsageFlags::SAMPLED,
                    format: format.as_raw() as u32,
                    sample_count: 1,
                    width: w,
                    height: h,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .ctx("xrCreateSwapchain")?;
            let images: Vec<vk::Image> = swapchain
                .enumerate_images()
                .ctx("xrEnumerateSwapchainImages")?
                .into_iter()
                .map(vk::Image::from_raw)
                .collect();
            let mut views = Vec::new();
            for &img in &images {
                views.push(fp_render::mem::view(&gpu, img, format, 0, 1)?);
            }
            eyes.push(Eye {
                swapchain,
                images,
                views,
            });
        }
        let blend_mode = ctx
            .blend_modes
            .first()
            .copied()
            .unwrap_or(xr::EnvironmentBlendMode::OPAQUE);
        log::info!("XR session: {w}x{h} per eye, {format:?}, blend {blend_mode:?}");
        Ok(XrSession {
            ctx,
            gpu,
            session,
            waiter,
            stream,
            space,
            view_space,
            eyes,
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
            format,
            actions,
            state: xr::SessionState::UNKNOWN,
            running: false,
            blend_mode,
            event_buf: xr::EventDataBuffer::new(),
            space_origin: (Vec3::ZERO, Quat::IDENTITY),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn state(&self) -> xr::SessionState {
        self.state
    }

    /// Whether the runtime supports a blend mode showing the real world.
    pub fn supports_passthrough_blend(&self) -> bool {
        self.ctx
            .blend_modes
            .contains(&xr::EnvironmentBlendMode::ALPHA_BLEND)
    }

    pub fn set_passthrough(&mut self, on: bool) {
        self.blend_mode = if on && self.supports_passthrough_blend() {
            xr::EnvironmentBlendMode::ALPHA_BLEND
        } else {
            xr::EnvironmentBlendMode::OPAQUE
        };
    }

    /// Processes runtime events. Call once per loop iteration.
    pub fn poll(&mut self) -> Result<SessionEvent> {
        let mut result = SessionEvent::None;
        while let Some(event) = self
            .ctx
            .instance
            .poll_event(&mut self.event_buf)
            .ctx("xrPollEvent")?
        {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    self.state = e.state();
                    log::info!("XR session state: {:?}", self.state);
                    match self.state {
                        xr::SessionState::READY => {
                            self.session
                                .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                .ctx("xrBeginSession")?;
                            self.running = true;
                        }
                        xr::SessionState::STOPPING => {
                            self.session.end().ctx("xrEndSession")?;
                            self.running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            return Ok(SessionEvent::Exit);
                        }
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => return Ok(SessionEvent::Exit),
                _ => {}
            }
        }
        if self.running {
            result = SessionEvent::Running;
        }
        Ok(result)
    }

    /// Waits for the next frame, locates the views and acquires swapchain
    /// images. Call [`XrSession::end_frame`] after submitting GPU work.
    pub fn begin_frame(&mut self) -> Result<FrameCtx> {
        let state = self.waiter.wait().ctx("xrWaitFrame")?;
        self.stream.begin().ctx("xrBeginFrame")?;
        let mut ctx = FrameCtx {
            state,
            eyes: None,
            head: None,
            targets: None,
            views: Vec::new(),
        };
        if !state.should_render {
            return Ok(ctx);
        }
        let (flags, views) = self
            .session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                state.predicted_display_time,
                &self.space,
            )
            .ctx("xrLocateViews")?;
        if !flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) || views.len() < 2 {
            return Ok(ctx);
        }
        if let Ok(loc) = self
            .view_space
            .locate(&self.space, state.predicted_display_time)
        {
            ctx.head = Some(crate::pose(loc.pose));
        }
        let eye = |v: &xr::View| {
            let (p, q) = crate::pose(v.pose);
            EyeView {
                view: fp_render::math::view(p, q),
                proj: fp_render::math::projection(
                    fp_render::math::Fov {
                        left: v.fov.angle_left,
                        right: v.fov.angle_right,
                        up: v.fov.angle_up,
                        down: v.fov.angle_down,
                    },
                    0.05,
                    200.0,
                ),
            }
        };
        ctx.eyes = Some([eye(&views[0]), eye(&views[1])]);
        let mut targets = Vec::new();
        for e in &mut self.eyes {
            let i = e.swapchain.acquire_image().ctx("xrAcquireSwapchainImage")? as usize;
            e.swapchain
                .wait_image(xr::Duration::INFINITE)
                .ctx("xrWaitSwapchainImage")?;
            targets.push(EyeTarget {
                image: e.images[i],
                view: e.views[i],
                extent: self.extent,
                final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            });
        }
        ctx.targets = Some([targets[0], targets[1]]);
        ctx.views = views;
        Ok(ctx)
    }

    /// Releases the images and submits the projection layer.
    pub fn end_frame(&mut self, ctx: FrameCtx) -> Result<()> {
        let time = ctx.state.predicted_display_time;
        if ctx.targets.is_none() {
            return self
                .stream
                .end(time, self.blend_mode, &[])
                .ctx("xrEndFrame");
        }
        for e in &mut self.eyes {
            e.swapchain.release_image().ctx("xrReleaseSwapchainImage")?;
        }
        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.extent.width as i32,
                height: self.extent.height as i32,
            },
        };
        let views: Vec<xr::CompositionLayerProjectionView<xr::Vulkan>> = ctx
            .views
            .iter()
            .zip(&self.eyes)
            .map(|(v, e)| {
                xr::CompositionLayerProjectionView::new()
                    .pose(v.pose)
                    .fov(v.fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&e.swapchain)
                            .image_rect(rect)
                            .image_array_index(0),
                    )
            })
            .collect();
        let mut flags = xr::CompositionLayerFlags::EMPTY;
        if self.blend_mode == xr::EnvironmentBlendMode::ALPHA_BLEND {
            flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;
        }
        let layer = xr::CompositionLayerProjection::new()
            .layer_flags(flags)
            .space(&self.space)
            .views(&views);
        self.stream
            .end(time, self.blend_mode, &[&layer])
            .ctx("xrEndFrame")
    }

    /// Controller state at `time`.
    pub fn input(&self, time: xr::Time) -> InputState {
        self.actions.read(&self.session, &self.space, time)
    }

    pub fn buzz(&self, hand: usize, amplitude: f32, millis: i64) {
        self.actions.buzz(&self.session, hand, amplitude, millis);
    }

    /// Interaction profiles that accepted bindings.
    pub fn bindings(&self) -> &[(String, usize)] {
        &self.actions.bound
    }

    /// Makes the current head direction (yaw only) and position the origin.
    pub fn recenter(&mut self, time: xr::Time) -> Result<()> {
        let loc = self
            .view_space
            .locate(&self.space, time)
            .ctx("locate head")?;
        let (p, q) = crate::pose(loc.pose);
        let (yaw, _, _) = q.to_euler(glam::EulerRot::YXZ);
        // New origin in the old space: head position at floor-relative height
        // kept, facing the head's yaw.
        let origin = crate::to_pose(Vec3::new(p.x, 0.0, p.z), Quat::from_rotation_y(yaw));
        // Compose with the existing reference offset by re-creating the space
        // relative to the runtime's LOCAL origin.
        let current = self.space_origin;
        let combined = Mat4::from_rotation_translation(current.1, current.0)
            * Mat4::from_rotation_translation(crate::pose(origin).1, crate::pose(origin).0);
        let (_, rot, pos) = combined.to_scale_rotation_translation();
        self.space = self
            .session
            .create_reference_space(xr::ReferenceSpaceType::LOCAL, crate::to_pose(pos, rot))
            .ctx("recenter")?;
        self.space_origin = (pos, rot);
        Ok(())
    }
}

impl Drop for XrSession {
    fn drop(&mut self) {
        self.gpu.wait_idle();
        for e in &self.eyes {
            for &v in &e.views {
                // SAFETY: GPU idle; views were created by us.
                unsafe { self.gpu.device.destroy_image_view(v, None) };
            }
        }
    }
}
