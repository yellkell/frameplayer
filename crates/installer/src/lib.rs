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
//! 4. [`transport`]: SSH to the headset for uploads, remote commands and
//!    port forwards, over either the system `ssh`/`scp` ([`ssh`], default on
//!    Linux/macOS) or built-in pure-Rust SSH (russh, default on Windows so
//!    nothing needs installing).
//! 5. [`remote`]: the shell scripts run on the headset (extract, status, logs,
//!    uninstall).
//! 6. [`steam`]: Steam client integration over the CEF DevTools endpoint
//!    (`127.0.0.1:8080` on the headset, tunnelled over SSH): create the
//!    shortcut, set artwork, pin, launch.
//!
//! [`site_manifest`] and [`release`] hold the release tooling (website
//! manifest for Frame Control / FrameDrop, signed update manifests).
//!
//! [`wizard`] is the guided mode a double-click starts (Windows): find,
//! pair, download from GitHub ([`github`]), install, run the self-test and
//! save a paste-ready, redacted ([`redact`]) report on the Desktop
//! ([`report`]), copied to the clipboard ([`share`]).

pub mod config;
pub mod console;
pub mod devkit;
pub mod discovery;
pub mod github;
pub mod installer;
pub mod redact;
pub mod release;
pub mod remote;
pub mod report;
pub mod share;
pub mod site_manifest;
pub mod ssh;
pub mod sshkey;
pub mod steam;
pub mod transport;
pub mod wizard;

/// Directory name under `~/devkit-game/` on the headset.
pub const GAME_DIR: &str = "frameplayer";
/// Display name in the Steam library.
pub const DISPLAY_NAME: &str = "FramePlayer";
/// Library entry that runs the self-test (`frameplayer-probe.sh`).
pub const PROBE_DISPLAY_NAME: &str = "FramePlayer Self-Test";
