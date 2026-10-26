//! Links the FFmpeg shared libraries built by tools/build-ffmpeg.sh.
//!
//! Library directory: `$FP_FFMPEG_LIB_DIR`, else
//! `<workspace>/third_party/ffmpeg/<target arch>/lib` (`android-<arch>` for
//! Android, built by tools/build-ffmpeg-android.sh).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FP_FFMPEG_LIB_DIR");
    let mut arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        arch = format!("android-{arch}");
    }
    let dir = match std::env::var_os("FP_FFMPEG_LIB_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
            manifest
                .join("../../third_party/ffmpeg")
                .join(&arch)
                .join("lib")
        }
    };
    if !dir.join("libavcodec.so").exists() {
        println!(
            "cargo:warning=FFmpeg libraries not found in {}; run tools/build-ffmpeg{}.sh",
            dir.display(),
            if arch.starts_with("android") {
                "-android"
            } else {
                ""
            }
        );
    }
    println!("cargo:rustc-link-search=native={}", dir.display());
    for lib in ["avformat", "avcodec", "swresample", "swscale", "avutil"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    // Let dependent build scripts and tests find the libraries.
    println!("cargo:lib_dir={}", dir.display());
}
