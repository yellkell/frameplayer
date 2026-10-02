//! Interactive haptics for FramePlayer.
//!
//! * [`funscript`]: lenient funscript parser, multi-axis [`ScriptSet`], interpolation,
//!   speed limiting, timeline heat map and lookahead queries.
//! * [`engine`]: [`HapticsEngine`], a tokio task that follows the player clock and drives one
//!   [`HapticDevice`], resynchronising on seek / pause / speed change.
//! * Backends: [`handy`] (The Handy cloud API v2: HSSP script sync + HDSP direct positions, and
//!   optional Bluetooth LE), [`buttplug`] (Intiface / buttplug.io protocol v3 over WebSocket) and
//!   [`tcode`] (OSR2 / SR6 over TCP, UDP or serial).
//!
//! All device I/O is async and runs on the tokio runtime; the render thread only ever calls
//! the non-blocking [`HapticsEngine::update`].

pub mod buttplug;
pub mod device;
pub mod engine;
pub mod funscript;
pub mod handy;
pub mod tcode;

pub use device::{AxisTarget, DeviceInfo, HapticDevice, StreamStyle};
pub use engine::{AxisSettings, EngineConfig, EngineState, HapticsEngine, PlayerUpdate};
pub use funscript::{Action, Axis, HeatmapScale, Script, ScriptSet};

/// Errors from parsing scripts and talking to devices.
#[derive(Debug, thiserror::Error)]
pub enum HapticsError {
    #[error("script parse error: {0}")]
    Parse(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] Box<tokio_tungstenite::tungstenite::Error>),
    #[error("device error: {0}")]
    Device(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("not connected")]
    NotConnected,
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for HapticsError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        HapticsError::WebSocket(Box::new(e))
    }
}

pub type Result<T, E = HapticsError> = std::result::Result<T, E>;
