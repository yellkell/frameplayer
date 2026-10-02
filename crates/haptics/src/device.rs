//! The device abstraction every backend implements.
//!
//! Devices come in two flavours, and some (The Handy) are both:
//! * **Script-synced**: the device stores the whole script and plays it against a shared clock
//!   (Handy HSSP). The engine uploads once via [`HapticDevice::prepare_script`] and then only
//!   sends [`HapticDevice::sync_play`] / [`HapticDevice::sync_stop`] on resync events.
//! * **Streaming**: the engine sends position targets as playback progresses
//!   ([`HapticDevice::send`]), either "next action" targets (move to X over D ms) or a steady
//!   stream of interpolated samples, as declared per axis in [`DeviceInfo::axes`].

use crate::funscript::{Axis, ScriptSet};
use crate::{HapticsError, Result};
use async_trait::async_trait;

/// How the engine should stream an axis to a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStyle {
    /// Send the next script action as soon as the previous one starts: "go to `pos` over
    /// `duration`". Best for devices with their own motion planner (Handy HDSP, buttplug
    /// LinearCmd).
    NextAction,
    /// Send the interpolated position every engine tick with `duration = tick`. Best for
    /// TCode devices and vibration intensity.
    Interpolated,
}

/// One movement command for one axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisTarget {
    pub axis: Axis,
    /// Target position / intensity in 0..=1 (after user range and inversion).
    pub position: f64,
    /// Time to reach the target, in milliseconds of wall time.
    pub duration_ms: u32,
}

/// Static description of what a connected device can do.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    /// Axes the device can be streamed, with the preferred style for each.
    pub axes: Vec<(Axis, StreamStyle)>,
    /// Supports [`HapticDevice::prepare_script`] + [`HapticDevice::sync_play`].
    pub script_sync: bool,
    /// Whether script sync can follow a playback speed other than 1.0.
    pub script_sync_any_speed: bool,
    /// Typical command-to-motion latency; the engine looks this far ahead.
    pub latency_ms: u32,
    /// Engine tick for [`StreamStyle::Interpolated`] axes.
    pub update_interval_ms: u32,
}

impl DeviceInfo {
    pub fn supports(&self, axis: Axis) -> bool {
        self.axes.iter().any(|(a, _)| *a == axis)
    }
}

/// A haptic output device. All methods are async and must never block.
#[async_trait]
pub trait HapticDevice: Send {
    fn info(&self) -> DeviceInfo;

    /// Open the connection / verify the device is reachable.
    async fn connect(&mut self) -> Result<()>;

    /// Release the device (stop motion first where possible).
    async fn disconnect(&mut self) -> Result<()> {
        self.stop().await
    }

    /// Hand the (already processed) scripts to a script-synced device. Returns `Ok(true)` if
    /// the device will play them itself, `Ok(false)` if the engine should stream instead.
    async fn prepare_script(&mut self, _scripts: &ScriptSet) -> Result<bool> {
        Ok(false)
    }

    /// Start script playback so that script time `script_time_ms` is "now".
    async fn sync_play(&mut self, _script_time_ms: i64, _speed: f64) -> Result<()> {
        Err(HapticsError::Unsupported("script sync".into()))
    }

    /// Stop script playback.
    async fn sync_stop(&mut self) -> Result<()> {
        Err(HapticsError::Unsupported("script sync".into()))
    }

    /// Stream position targets (one per axis at most).
    async fn send(&mut self, targets: &[AxisTarget]) -> Result<()>;

    /// Halt all motion immediately.
    async fn stop(&mut self) -> Result<()>;
}

#[async_trait]
impl<T: HapticDevice + ?Sized> HapticDevice for Box<T> {
    fn info(&self) -> DeviceInfo {
        (**self).info()
    }
    async fn connect(&mut self) -> Result<()> {
        (**self).connect().await
    }
    async fn disconnect(&mut self) -> Result<()> {
        (**self).disconnect().await
    }
    async fn prepare_script(&mut self, scripts: &ScriptSet) -> Result<bool> {
        (**self).prepare_script(scripts).await
    }
    async fn sync_play(&mut self, t: i64, speed: f64) -> Result<()> {
        (**self).sync_play(t, speed).await
    }
    async fn sync_stop(&mut self) -> Result<()> {
        (**self).sync_stop().await
    }
    async fn send(&mut self, targets: &[AxisTarget]) -> Result<()> {
        (**self).send(targets).await
    }
    async fn stop(&mut self) -> Result<()> {
        (**self).stop().await
    }
}
