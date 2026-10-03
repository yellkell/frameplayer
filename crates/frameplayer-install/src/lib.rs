//! `frameplayer-install`: puts FramePlayer on a paired Steam Frame from a
//! Windows, macOS or Linux PC.
//!
//! It drives the system `ssh`/`scp` (so it reuses the pairing done by
//! Frame Control, FrameDrop or Valve's SteamOS Devkit Client), checks the
//! device, uploads a verified release zip, unpacks it into
//! `~/frameplayer` with the same `.old`/`.new` layout the in-app updater
//! uses, and adds a Steam library shortcut with artwork.
//!
//! Everything that touches the headset goes through the [`remote::Remote`]
//! trait, so the whole flow is tested against a fake device.

pub mod device;
pub mod error;
pub mod ops;
pub mod remote;
pub mod shortcut;
pub mod vdf;

pub use error::{InstallError, Result};
