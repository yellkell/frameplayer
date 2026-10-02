//! FramePlayer's media pipeline on top of the bundled FFmpeg.
//!
//! - [`io`]: FFmpeg reads through [`fp_core::ByteSource`], so all network
//!   access stays in Rust.
//! - [`info`]: stream inspection, including spherical/stereo metadata and HDR.
//! - [`decode`]: video (hardware V4L2 first, software fallback), audio and
//!   subtitle decoders.
//! - [`frame`]: decoded video frames in GPU-uploadable layouts.
//! - [`player`]: the threaded player with a video-master clock.
//! - [`audio`]: output, time-stretch and ambisonic rendering.
//! - [`thumb`]: probing and frame grabs for the library.

pub mod audio;
pub mod clock;
pub(crate) mod decode;
pub mod frame;
pub mod info;
pub(crate) mod input;
pub(crate) mod io;
pub mod player;
pub mod subtitle;
pub mod thumb;

pub use decode::HwDecode;
pub use frame::{ColorInfo, PixelLayout, VideoFrame};
pub use info::{MediaInfo, StreamInfo, StreamKind};
pub use player::{Player, PlayerConfig, PlayerState, Stats};
pub use subtitle::Cue;

use fp_ffmpeg_sys as ff;

/// Errors from the media pipeline.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {message}")]
    Ffmpeg {
        context: &'static str,
        code: i32,
        message: String,
    },
    #[error("no {0} stream")]
    NoStream(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("audio output: {0}")]
    Audio(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("interrupted")]
    Interrupted,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Turns a negative FFmpeg return code into an [`Error`].
pub(crate) fn check(code: i32, context: &'static str) -> Result<i32> {
    if code < 0 {
        Err(Error::Ffmpeg {
            context,
            code,
            message: ff::err_to_string(code),
        })
    } else {
        Ok(code)
    }
}

/// Route FFmpeg's own log output through the `log` crate at warning level
/// and above, so it does not spam stderr.
pub fn init_logging() {
    // SAFETY: plain setters on global FFmpeg state.
    unsafe { ff::av_log_set_level(ff::AV_LOG_ERROR as i32) };
}

/// FFmpeg rational to f64 seconds multiplier.
pub(crate) fn q2d(q: ff::AVRational) -> f64 {
    if q.den == 0 {
        0.0
    } else {
        q.num as f64 / q.den as f64
    }
}
