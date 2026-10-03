//! Error type for the remote-control crate.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Everything that can go wrong while starting or running the remote servers.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The listening socket could not be created (port in use, no permission).
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        /// Address we tried to bind.
        addr: SocketAddr,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The pairing token file could not be read or written.
    #[error("pairing token file {path}: {source}")]
    TokenFile {
        /// Path the app configured for the token.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The operating system's random number generator failed.
    #[error("random number generator failed: {0}")]
    Random(String),
    /// The embedded HTTP server failed to start.
    #[error("web server: {0}")]
    Http(String),
    /// The pairing URL does not fit in a QR code.
    #[error("QR code: {0}")]
    Qr(String),
    /// `start_*` was called while that server was already running.
    #[error("{0} server is already running")]
    AlreadyRunning(&'static str),
    /// Any other I/O error (thread spawn, socket options).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
