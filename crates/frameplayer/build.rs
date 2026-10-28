//! Lets the installed binary find the bundled FFmpeg libraries in `lib/`
//! next to it, also when started without `frameplayer.sh`. All five FFmpeg
//! libraries are direct dependencies, so this also covers their references
//! to each other even where the linker emits RUNPATH instead of RPATH.
//!
//! For Android (Meta Quest), and the quest-look preview, it writes
//! `$OUT_DIR/rust-crates.txt` for the About page: the list
//! tools/package-quest.sh made (`$FP_RUST_CRATES`), or a placeholder in
//! development builds.

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "linux" {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/lib");
        println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
    }
    if os == "android" || std::env::var_os("CARGO_FEATURE_QUEST_LOOK").is_some() {
        println!("cargo:rerun-if-env-changed=FP_RUST_CRATES");
        let list = std::env::var("FP_RUST_CRATES")
            .ok()
            .and_then(|p| {
                println!("cargo:rerun-if-changed={p}");
                std::fs::read_to_string(p).ok()
            })
            .unwrap_or_else(|| "(listed in builds made with tools/package-quest.sh)\n".into());
        let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
        let _ = std::fs::write(out.join("rust-crates.txt"), list);
    }
}
