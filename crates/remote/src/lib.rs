//! Remote control for FramePlayer.
//!
//! * [`deovr`]: DeoVR-compatible TCP remote (length-prefixed JSON on port 23554) so ohdoki,
//!   ScriptPlayer, MultiFunPlayer and friends work unchanged.
//! * [`api`]: our REST + WebSocket API (axum) and the embedded LAN web remote page.
//! * [`pairing`]: pairing URL + QR code (SVG / PNG).
//! * [`net`]: LAN-only policy and token helpers.
//!
//! Security (OUTLINE §3.6): both servers are off by default, refuse peers outside loopback /
//! RFC 1918 / link-local / ULA, and the HTTP API requires a bearer token.
//!
//! Integration: create the channels with [`link`], hand the [`RemoteLink`] plus a
//! [`LibraryProvider`] to [`start`], then consume [`RemoteCommand`]s from
//! [`AppLink::commands`] and publish [`PlayerStatus`] into [`AppLink::status`].

pub mod api;
pub mod deovr;
pub mod net;
pub mod pairing;
pub mod server;
pub mod types;

pub use net::{detect_lan_ip, generate_token, is_lan_ip};
pub use pairing::{pairing_url, qr_png, qr_svg};
pub use server::{start, DeoVrConfig, HttpConfig, RemoteConfig, RemoteHandle};
pub use types::{
    link, AppLink, LibraryItem, LibraryPage, LibraryProvider, LibraryQuery, PlayerStatus,
    ProviderError, RemoteCommand, RemoteLink, Thumbnail,
};

/// Errors from starting the servers or rendering pairing codes.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("QR code error: {0}")]
    Qr(String),
}
