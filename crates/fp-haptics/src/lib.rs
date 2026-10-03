//! Interactive-device support for FramePlayer: plays funscripts on haptic
//! devices in sync with the video.
//!
//! - [`funscript`]: parsing single- and multi-axis funscripts, mapping file
//!   names to axes, finding scripts next to a video.
//! - [`script`]: the per-axis timeline ([`Script`]): interpolation, next
//!   action, statistics.
//! - [`heatmap`]: speed-coloured timeline strips for the player UI.
//! - [`engine`]: [`HapticsEngine`], the 100 Hz worker that follows
//!   [`fp_core::PlaybackStatus`] and drives the devices.
//! - [`device`]: the [`Device`] trait, implemented by
//!   - [`tcode`]: TCode v0.3 over serial, TCP or UDP (OSR2, SR6, ...);
//!   - [`buttplug`]: Buttplug / Intiface Central over WebSocket;
//!   - [`handy`]: The Handy through its cloud API (HSSP).
//! - [`settings`]: offset, per-axis range and invert, speed limit,
//!   per-device switches; serialisable.
//!
//! Everything is plain threads and blocking I/O.
//!
//! ```no_run
//! use fp_haptics::{HapticsEngine, funscript, tcode};
//! # fn main() -> fp_haptics::Result<()> {
//! let engine = HapticsEngine::new();
//! engine.load(funscript::load_script_file("video.funscript".as_ref())?);
//! let osr = tcode::TcodeDevice::connect(tcode::TcodeConfig {
//!     endpoint: tcode::TcodeEndpoint::parse("/dev/ttyACM0")?,
//!     ..Default::default()
//! })?;
//! engine.add_device(Box::new(osr));
//! // Every frame:
//! # let status = fp_core::PlaybackStatus::default();
//! engine.set_status(status);
//! # Ok(())
//! # }
//! ```

pub mod axis;
pub mod buttplug;
pub mod device;
pub mod engine;
pub mod error;
pub mod funscript;
pub mod handy;
pub mod heatmap;
pub mod script;
pub mod settings;
pub mod tcode;

pub use axis::{Axis, axis_for_script_name, split_script_name};
pub use device::{AxisMove, Device, DeviceId, DeviceStatus, SyncMode};
pub use engine::{Clock, HapticsEngine, SystemClock};
pub use error::{Error, Result};
pub use funscript::Funscript;
pub use heatmap::{Heatmap, heatmap, heatmap_intensity, heatmap_range};
pub use script::{Action, Script, ScriptStats};
pub use settings::{AxisSettings, DeviceSettings, HapticsSettings};
