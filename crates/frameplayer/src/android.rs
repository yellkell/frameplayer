//! Start-up on Android (Meta Quest): app folders, then the same OpenXR loop
//! as on the Frame, with the activity's events handled between frames.

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use std::sync::OnceLock;
use std::time::Duration;

static APP: OnceLock<AndroidApp> = OnceLock::new();

/// Shared storage, where videos copied to the headset land (Movies,
/// Download, ...): the home folder for the default library folders.
const SHARED_STORAGE: &str = "/storage/emulated/0";

/// Runs FramePlayer in the activity `app`; called once from
/// `android_main`, ends the process when done.
pub fn run(app: AndroidApp) {
    // Settings, library and logs live in the app's private storage; the
    // XDG variables point fp_core::dirs there.
    if let Some(private) = app.internal_data_path() {
        // SAFETY: no other threads exist yet.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", private.join("config"));
            std::env::set_var("XDG_DATA_HOME", private.join("data"));
            std::env::set_var("XDG_CACHE_HOME", private.join("cache"));
            std::env::set_var("HOME", SHARED_STORAGE);
        }
    }
    let _ = APP.set(app);
    crate::logger::init(false);
    fp_media::init_logging();
    log::info!(
        "FramePlayer {} starting (Android)",
        env!("CARGO_PKG_VERSION")
    );
    if let Err(e) = crate::run_xr(&crate::Args::default()) {
        log::error!("{e}");
    }
    // A finished activity's process may be kept and android_main called
    // again, with every static still set: start clean instead.
    std::process::exit(0);
}

/// Handles the activity's pending events without waiting. Returns true
/// once the activity is being destroyed.
pub(crate) fn pump() -> bool {
    let Some(app) = APP.get() else {
        return false;
    };
    let mut destroyed = false;
    app.poll_events(Some(Duration::ZERO), |event| match event {
        PollEvent::Main(MainEvent::Destroy) => destroyed = true,
        // Controllers come through OpenXR; key and touch events are only
        // drained so the system never waits on them.
        PollEvent::Main(MainEvent::InputAvailable) => {
            if let Ok(mut events) = app.input_events_iter() {
                while events.next(|_| InputStatus::Unhandled) {}
            }
        }
        _ => {}
    });
    destroyed
}
