//! Which shared libraries the system offers outside any runtime container.
//! Answers "what must FramePlayer bundle, and what can it take from SteamOS".

use serde::Serialize;
use std::ffi::CString;

#[derive(Serialize)]
pub struct LibReport {
    pub soname: &'static str,
    pub purpose: &'static str,
    pub found: bool,
    pub path: Option<String>,
    pub error: Option<String>,
}

const LIBS: &[(&str, &str)] = &[
    ("libvulkan.so.1", "Vulkan loader (rendering, Vulkan Video)"),
    (
        "libopenxr_loader.so.1",
        "Khronos OpenXR loader (we negotiate directly; informational)",
    ),
    ("libGLESv2.so.2", "OpenGL ES fallback"),
    ("libEGL.so.1", "EGL (DMA-BUF import fallback)"),
    ("libdrm.so.2", "DRM: format modifiers, DMA-BUF"),
    ("libgbm.so.1", "GBM buffer allocation"),
    ("libva.so.2", "VA-API (not expected on Qualcomm)"),
    ("libgstreamer-1.0.so.0", "GStreamer (V4L2 decoder elements)"),
    ("libavcodec.so.61", "FFmpeg 7 decoders"),
    ("libavcodec.so.62", "FFmpeg 8 decoders"),
    ("libavformat.so.61", "FFmpeg 7 demuxers"),
    ("libdav1d.so.7", "dav1d software AV1"),
    ("libpipewire-0.3.so.0", "PipeWire audio"),
    ("libpulse.so.0", "PulseAudio client"),
    ("libasound.so.2", "ALSA"),
    ("libsmbclient.so.0", "Samba client (SMB shares)"),
    ("libssl.so.3", "OpenSSL 3 (HTTPS, WebDAV)"),
    ("libsqlite3.so.0", "SQLite (library index)"),
    ("libwayland-client.so.0", "Wayland (desktop-mode window)"),
    ("libstdc++.so.6", "C++ runtime"),
];

pub fn probe() -> Vec<LibReport> {
    LIBS.iter()
        .map(|(soname, purpose)| {
            let c = CString::new(*soname).expect("static soname");
            let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if handle.is_null() {
                let err = unsafe { crate::util::cstr_ptr(libc::dlerror()) };
                return LibReport {
                    soname,
                    purpose,
                    found: false,
                    path: None,
                    error: err,
                };
            }
            let path = loaded_path(soname);
            // Leave the library mapped: unloading GPU drivers can crash on exit.
            LibReport {
                soname,
                purpose,
                found: true,
                path,
                error: None,
            }
        })
        .collect()
}

/// Finds where the dynamic linker mapped `soname` by scanning our own maps.
fn loaded_path(soname: &str) -> Option<String> {
    let stem = soname.split(".so").next()?;
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .filter_map(|l| l.split_whitespace().nth(5))
        .find(|p| {
            let file = p.rsplit('/').next().unwrap_or(p);
            file.starts_with(&format!("{stem}.so"))
        })
        .map(str::to_string)
}
