//! The crate's error type.

/// Everything that can go wrong in fp-haptics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading a file or talking to a socket or serial port failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A funscript could not be understood at all.
    #[error("invalid funscript: {0}")]
    Script(String),
    /// JSON (de)serialisation failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The device is not (or no longer) connected.
    #[error("device not connected: {0}")]
    NotConnected(String),
    /// The device or backend cannot do what was asked.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// An HTTP request failed or the server answered with an error.
    #[error("HTTP error: {0}")]
    Http(String),
    /// WebSocket transport error.
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    /// The peer answered something we did not expect.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Waited too long for an answer.
    #[error("timed out: {0}")]
    Timeout(String),
    /// A configuration value (URL, address, path) is malformed.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Io(io) => Error::Io(io),
            other => Error::Http(other.to_string()),
        }
    }
}

impl From<tungstenite::Error> for Error {
    fn from(e: tungstenite::Error) -> Self {
        match e {
            tungstenite::Error::Io(io) => Error::Io(io),
            other => Error::WebSocket(other.to_string()),
        }
    }
}

/// `Result` with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;
