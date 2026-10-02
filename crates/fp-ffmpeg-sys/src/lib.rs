//! Raw bindings to the FFmpeg 7.1 libraries FramePlayer bundles.
//!
//! Generated per architecture by `tools/gen-ffmpeg-bindings.sh` from the
//! headers of our own build, so struct layouts match the shipped libraries
//! exactly. Enums are plain integer constants: values coming back from C can
//! never be invalid Rust enums.

#[cfg(target_arch = "aarch64")]
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]
mod bindings {
    include!("bindings_aarch64.rs");
}
#[cfg(target_arch = "x86_64")]
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all
)]
mod bindings {
    include!("bindings_x86_64.rs");
}
pub use bindings::*;

/// `AV_NOPTS_VALUE`: no timestamp.
pub const AV_NOPTS_VALUE: i64 = i64::MIN;
/// `AV_TIME_BASE` as a rational.
pub const AV_TIME_BASE_Q: AVRational = AVRational {
    num: 1,
    den: AV_TIME_BASE as i32,
};

/// `AVERROR(e)` for a positive POSIX errno.
pub const fn averror(errno: i32) -> i32 {
    -errno
}
pub const AVERROR_EAGAIN: i32 = averror(11);

const fn fferrtag(a: u8, b: u8, c: u8, d: u8) -> i32 {
    -((a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24))
}
/// `AVERROR_EOF`.
pub const AVERROR_EOF_: i32 = fferrtag(b'E', b'O', b'F', b' ');
/// `AVERROR_EXIT`: an interrupt callback asked FFmpeg to stop.
pub const AVERROR_EXIT_: i32 = fferrtag(b'E', b'X', b'I', b'T');
/// `AVERROR_INVALIDDATA`.
pub const AVERROR_INVALIDDATA_: i32 = fferrtag(b'I', b'N', b'D', b'A');

/// Human-readable text for an FFmpeg error code.
pub fn err_to_string(code: i32) -> String {
    let mut buf = [0 as core::ffi::c_char; 128];
    // SAFETY: buf is writable for its full length and NUL-terminated by av_strerror.
    unsafe {
        av_strerror(code, buf.as_mut_ptr(), buf.len());
        core::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_reports_version() {
        let v = unsafe { avcodec_version() };
        assert_eq!(v >> 16, 61, "libavcodec major");
        assert!(
            err_to_string(AVERROR_EOF_)
                .to_lowercase()
                .contains("end of file")
        );
    }

    #[test]
    fn decoders_present() {
        for name in [
            c"hevc",
            c"h264",
            c"libdav1d",
            c"vp9",
            c"aac",
            c"opus",
            c"hevc_v4l2m2m",
        ] {
            let d = unsafe { avcodec_find_decoder_by_name(name.as_ptr()) };
            assert!(!d.is_null(), "decoder {name:?} missing");
        }
    }
}
