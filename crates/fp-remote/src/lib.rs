//! Remote control of FramePlayer from other devices on the LAN.
//!
//! Two servers, both off until the app starts them through [`RemoteHub`]:
//!
//! - a DeoVR-compatible TCP API ([`deovr`], default port 23554) so script
//!   players and haptics tools that already speak DeoVR's protocol (ohdoki,
//!   MultiFunPlayer, ScriptPlayer) work unchanged;
//! - a web remote ([`web`], default port 8790): an embedded single-page app
//!   for phones with now-playing controls, library search and text entry,
//!   paired by scanning a QR code ([`pairing_qr`]) that carries a
//!   per-install token.
//!
//! The render thread calls [`RemoteHub::publish`] every frame and drains
//! [`RemoteHub::events`] for [`RemoteEvent`]s. Only loopback and
//! private/link-local peers are served ([`is_allowed_peer`]).
//!
//! All servers use plain threads and blocking sockets.

pub mod config;
pub mod deovr;
mod error;
mod hub;
pub mod net;
pub mod qr;
mod status;
pub mod token;
pub mod web;

pub use config::{DEFAULT_DEOVR_PORT, DEFAULT_WEB_PORT, RemoteConfig};
pub use error::RemoteError;
pub use hub::{RemoteEvent, RemoteHub, RemoteItem, RemoteLibrary};
pub use net::{is_allowed_peer, lan_addresses};
pub use qr::{QrMatrix, pairing_qr};
pub use status::significant_change;
