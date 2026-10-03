//! Error type for the video crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid or corrupt data: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no decoder available: {0}")]
    NoDecoder(String),
    #[error("no software decoder compiled in for {0} (enable the `dav1d` or `ffmpeg` feature)")]
    NoSoftwareDecoder(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("end of stream")]
    EndOfStream,
    #[error("player is shut down")]
    Closed,
}

impl VideoError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        VideoError::Invalid(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, VideoError>;
