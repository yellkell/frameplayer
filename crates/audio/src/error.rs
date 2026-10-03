//! Error type for the audio crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no audio output device available")]
    NoDevice,
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("unsupported audio format: {0}")]
    Unsupported(String),
    #[error("invalid parameter: {0}")]
    InvalidParam(String),
}

pub type Result<T> = std::result::Result<T, AudioError>;
