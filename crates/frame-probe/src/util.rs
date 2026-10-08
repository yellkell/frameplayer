//! Small helpers shared by the probes.

use std::ffi::CStr;
use std::os::raw::c_char;

/// Converts a fixed-size, NUL-terminated C char array to a `String`.
pub fn fixed_cstr(bytes: &[c_char]) -> String {
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let raw: Vec<u8> = bytes[..len].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&raw).into_owned()
}

/// Same as [`fixed_cstr`] for `u8` arrays (kernel structs).
pub fn fixed_bytes(bytes: &[u8]) -> String {
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Renders a V4L2/DRM style little-endian fourcc as text, e.g. `HEVC`.
pub fn fourcc(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Reads a small text file, trimmed, or `None` when unreadable.
pub fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Returns `std::io::Error::last_os_error()` as text.
pub fn last_os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Safe wrapper for a C string pointer returned by a library.
///
/// # Safety
/// `ptr` must be null or point to a NUL-terminated string valid for the call.
pub unsafe fn cstr_ptr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Linux `_IOC` request encoding (asm-generic, used by both x86_64 and arm64).
pub const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u64 {
    ((dir as u64) << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | (nr as u64)
}
pub const IOC_READ: u32 = 2;
pub const IOC_READWRITE: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_renders_ascii() {
        assert_eq!(fourcc(u32::from_le_bytes(*b"HEVC")), "HEVC");
        assert_eq!(fourcc(u32::from_le_bytes(*b"NV12")), "NV12");
        assert_eq!(fourcc(u32::from_le_bytes([b'P', b'0', b'1', 0])), "P01?");
    }

    #[test]
    fn fixed_cstr_stops_at_nul() {
        let raw: Vec<c_char> = b"msm\0junk".iter().map(|&b| b as c_char).collect();
        assert_eq!(fixed_cstr(&raw), "msm");
        assert_eq!(fixed_bytes(b"venus\0\0\0"), "venus");
    }
}
