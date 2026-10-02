//! `frameplayer-install` as a library (OUTLINE §4 Tier 2).
//!
//! The CLI in `main.rs` is a thin shell over [`installer::Installer`]; a GUI
//! wrapper can drive the same type and render [`installer::Event`]s itself.
//!
//! Flow, reusing Valve's devkit pairing (the same one the SteamOS Devkit
//! Client, Frame Control and FrameDrop use):
//! 1. [`discovery`]: find the headset via mDNS `_steamos-devkit._tcp`, or `--host`.
//! 2. [`devkit`]: talk to `steamos-devkit-service` (HTTP, port 32000):
//!    read `/properties.json`, `POST /register` our SSH public key; the user
//!    approves on the headset.
//! 3. [`sshkey`]: ed25519 key generated in pure Rust, stored in the platform
//!    config dir.
//! 4. [`ssh`]: system `ssh`/`scp` (present on macOS, Windows 10+, Linux) for
//!    upload and remote commands, plus local port forwards.
//! 5. [`remote`]: the shell scripts run on the headset (extract, status, logs,
//!    uninstall).
//! 6. [`steam`]: Steam client integration over the CEF DevTools endpoint
//!    (`127.0.0.1:8080` on the headset, tunnelled over SSH): create the
//!    shortcut, set artwork, pin, launch.
//!
//! [`site_manifest`] and [`release`] hold the release tooling (website
//! manifest for Frame Control / FrameDrop, signed update manifests).

pub mod config;
pub mod devkit;
pub mod discovery;
pub mod installer;
pub mod release;
pub mod remote;
pub mod site_manifest;
pub mod ssh;
pub mod sshkey;
pub mod steam;

/// Directory name under `~/devkit-game/` on the headset.
pub const GAME_DIR: &str = "frameplayer";
/// Display name in the Steam library.
pub const DISPLAY_NAME: &str = "FramePlayer";
