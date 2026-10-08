//! The device abstraction the engine drives.

use crate::axis::Axis;
use crate::error::{Error, Result};
use crate::script::Script;
use serde::{Deserialize, Serialize};

/// One timed move: reach `pos` on `axis` in `duration_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AxisMove {
    /// Target axis.
    pub axis: Axis,
    /// Target position, `0.0..=1.0`.
    pub pos: f32,
    /// Time to reach the target, milliseconds of wall-clock time.
    pub duration_ms: u32,
}

/// How a device follows the script.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// The engine streams timed moves ([`Device::move_axes`]) as playback
    /// progresses (TCode, Buttplug).
    Stream,
    /// The device holds the whole script and plays it itself; the engine
    /// only uploads it ([`Device::load_script`]) and starts or stops it at
    /// a script time ([`Device::play_script`], [`Device::stop`]). The
    /// Handy's HSSP mode works like this.
    Script,
}

/// An interactive device.
///
/// The engine calls these methods from its worker thread about 100 times a
/// second, so they must return quickly: backends with slow transports (HTTP,
/// WebSocket) queue the work on their own thread. Positions are normalised
/// to `0.0..=1.0` after the user's range and invert settings.
pub trait Device: Send {
    /// Stable, human-readable name. Also the key for per-device settings.
    fn name(&self) -> String;

    /// Axes the device can move. May change while connected (Buttplug
    /// devices come and go); must be cheap.
    fn axes(&self) -> Vec<Axis>;

    /// Moves one axis to `pos` over `duration_ms`.
    fn move_to(&mut self, axis: Axis, pos: f32, duration_ms: u32) -> Result<()>;

    /// Moves several axes at once. The default calls [`Device::move_to`]
    /// for each; backends that can combine them (TCode on one line)
    /// override it.
    fn move_axes(&mut self, moves: &[AxisMove]) -> Result<()> {
        for m in moves {
            self.move_to(m.axis, m.pos, m.duration_ms)?;
        }
        Ok(())
    }

    /// Stops all motion now.
    fn stop(&mut self) -> Result<()>;

    /// Whether the device is reachable. Must be cheap (a cached flag).
    fn is_connected(&self) -> bool;

    /// The shortest time between two commands the device accepts; the
    /// engine never sends faster.
    fn min_interval_ms(&self) -> u32 {
        20
    }

    /// Whether the engine streams moves or the device plays the script
    /// itself.
    fn sync_mode(&self) -> SyncMode {
        SyncMode::Stream
    }

    /// [`SyncMode::Script`] devices: replaces the device's script. The
    /// engine passes the scripts for the axes this device should play, with
    /// the user's range and invert settings already applied. An empty list
    /// means no script.
    fn load_script(&mut self, scripts: &[(Axis, Script)]) -> Result<()> {
        let _ = scripts;
        Ok(())
    }

    /// [`SyncMode::Script`] devices: plays the loaded script from
    /// `script_time_ms` (offset already applied) at `speed`. Called on
    /// start, seek and speed changes.
    fn play_script(&mut self, script_time_ms: i64, speed: f64) -> Result<()> {
        let _ = (script_time_ms, speed);
        Err(Error::Unsupported(format!(
            "{} does not play scripts itself",
            self.name()
        )))
    }

    /// A message for the user about the device's state (for example an
    /// unsupported playback speed), shown next to it in the device list.
    fn notice(&self) -> Option<String> {
        None
    }
}

/// Identifies a device added to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub u64);

/// A device as the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    /// Engine-assigned identifier (use with
    /// [`crate::HapticsEngine::remove_device`]).
    pub id: DeviceId,
    /// [`Device::name`].
    pub name: String,
    /// [`Device::axes`].
    pub axes: Vec<Axis>,
    /// [`Device::is_connected`].
    pub connected: bool,
    /// Whether the user has the device switched on.
    pub enabled: bool,
    /// [`Device::sync_mode`].
    pub sync_mode: SyncMode,
    /// [`Device::notice`].
    pub notice: Option<String>,
    /// The last error a command returned, cleared by the next success.
    pub last_error: Option<String>,
    /// Commands sent so far (moves, stops, script plays).
    pub commands_sent: u64,
}
