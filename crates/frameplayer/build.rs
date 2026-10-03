//! Lets the installed binary find the bundled FFmpeg libraries in `lib/`
//! next to it, also when started without `frameplayer.sh`. All five FFmpeg
//! libraries are direct dependencies, so this also covers their references
//! to each other even where the linker emits RUNPATH instead of RPATH.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/lib");
        println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
    }
}
