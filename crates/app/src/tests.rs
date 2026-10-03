//! End-to-end tests of the wiring (services + player + controller) with the
//! deterministic mock media backend, no XR.

use crate::app::App;
use crate::config::{Config, Paths};
use crate::controller::OpenTarget;
use crate::headless::{self, HeadlessOptions};
use crate::runtime::services::{ServiceOptions, Services};
use fp_core::MediaTime;
use fp_library::{Library, NewItem};
use fp_video::mock::{MockBackend, MockMedia};
use fp_video::{PlaybackState, Player, PlayerConfig};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| crate::runtime::build_runtime().expect("runtime"))
}

pub struct Harness {
    pub app: App,
    pub lib: Arc<Library>,
    pub dir: tempfile::TempDir,
}

pub fn harness(duration_s: f64, config: Config) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    paths.create_all().unwrap();
    let lib = Arc::new(Library::open_in_memory().unwrap());
    let services = Services::start(
        runtime().handle(),
        config.clone(),
        paths.clone(),
        lib.clone(),
        ServiceOptions {
            config_path: paths.config_file(),
            scan_on_start: false,
            watch_mounts: false,
            default_sources: false,
            thumbnails: false,
        },
    );
    let player = Player::spawn(
        Box::new(MockBackend {
            media: MockMedia {
                duration: MediaTime::from_secs_f64(duration_s),
                ..Default::default()
            },
        }),
        PlayerConfig::default(),
    );
    Harness {
        app: App::new(config, services, player),
        lib,
        dir,
    }
}

/// A ready app for UI tests.
pub fn test_app() -> App {
    let h = harness(1.0, Config::default());
    // The temp dir may go; nothing in these tests writes there.
    std::mem::forget(h.dir);
    h.app
}

fn tick_until(app: &mut App, timeout: Duration, mut f: impl FnMut(&App) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        app.tick();
        if f(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn headless_end_to_end_with_mock_backend() {
    let mut h = harness(1.2, Config::default());
    let video = h.dir.path().join("clip_180_LR.mp4");
    std::fs::write(&video, b"mock media").unwrap();
    let uri = fp_sources::local::path_to_uri(&video);
    let id = h
        .lib
        .upsert_item(&NewItem {
            uri: uri.clone(),
            path: "clip_180_LR.mp4".into(),
            title: "clip".into(),
            duration: Some(MediaTime::from_secs_f64(1.2)),
            ..Default::default()
        })
        .unwrap();
    h.lib.set_resume(id, MediaTime::from_secs_f64(0.5)).unwrap();
    let mut status_rx = h.app.services.remote_status.subscribe();

    // Opening by URI finds the library item.
    h.app.open_uri(&uri, None);
    let report = headless::run(
        &mut h.app,
        &HeadlessOptions {
            max_seconds: Some(20.0),
            exit_when_done: true,
            ..Default::default()
        },
    );
    assert_eq!(report.error, None);
    assert!(
        report.reached(PlaybackState::Playing),
        "{:?}",
        report.states
    );
    assert!(report.reached(PlaybackState::Ended), "{:?}", report.states);
    assert!(report.frames > 5, "frames presented: {}", report.frames);
    assert_eq!(report.convert_errors, 0);
    assert!(report.max_position >= MediaTime::from_secs_f64(1.0));
    assert_eq!(report.decoder.as_deref(), Some("software (mock)"));

    // Watched to the end: resume cleared, history recorded (async writes).
    let lib = h.lib.clone();
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |_| {
        lib.history(10)
            .map(|v| v.iter().any(|e| e.item_id == id && e.completed))
            .unwrap_or(false)
    }));
    assert_eq!(h.lib.resume_point(id).unwrap(), None);
    // The filename detection reached the session (180° SBS).
    assert!(status_rx.has_changed().is_ok());
    let st = status_rx.borrow_and_update().clone();
    assert!(
        st.path.is_none(),
        "remote status cleared after the session: {st:?}"
    );
    h.app.shutdown();
}

#[test]
fn open_from_library_resumes_and_closes_with_resume_point() {
    let mut h = harness(120.0, Config::default());
    let video = h.dir.path().join("long.mp4");
    std::fs::write(&video, b"x").unwrap();
    let uri = fp_sources::local::path_to_uri(&video);
    let id = h
        .lib
        .upsert_item(&NewItem {
            uri,
            path: "long.mp4".into(),
            title: "long".into(),
            duration: Some(MediaTime::from_secs_f64(120.0)),
            ..Default::default()
        })
        .unwrap();
    h.lib
        .set_resume(id, MediaTime::from_secs_f64(40.0))
        .unwrap();
    assert_eq!(
        h.lib.resume_point(id).unwrap(),
        Some(MediaTime::from_secs_f64(40.0))
    );
    // Library query results arrive through the services.
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |a| !a
        .ctl
        .library
        .items
        .is_empty()));
    let fx = h.app.ctl.open(OpenTarget::Item(id), None);
    h.app.apply(fx);
    assert!(
        tick_until(&mut h.app, Duration::from_secs(10), |a| {
            a.ctl.player.state == PlaybackState::Playing
                && a.ctl.player.position >= MediaTime::from_secs_f64(40.0)
        }),
        "resumed at the saved point: {:?}",
        h.app.ctl.player.position
    );
    let mut remote = h.app.services.remote_status.subscribe();
    assert!(tick_until(&mut h.app, Duration::from_secs(2), |_| {
        remote.borrow_and_update().playing
    }));
    assert_eq!(
        remote.borrow().item_id.as_deref(),
        Some(id.to_string().as_str())
    );

    // Remote pause / seek are applied by the engine.
    let fx = h
        .app
        .ctl
        .handle_remote(fp_remote::RemoteCommand::Seek { seconds: 60.0 });
    h.app.apply(fx);
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |a| {
        a.ctl.player.position >= MediaTime::from_secs_f64(60.0)
    }));
    let fx = h.app.ctl.handle_remote(fp_remote::RemoteCommand::Pause);
    h.app.apply(fx);
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |a| a
        .ctl
        .player
        .state
        == PlaybackState::Paused));

    // Leaving the player saves the position.
    let fx = h.app.ctl.handle_ui(fp_ui::UiAction::OpenLibrary);
    h.app.apply(fx);
    let lib = h.lib.clone();
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |_| {
        lib.resume_point(id)
            .ok()
            .flatten()
            .is_some_and(|p| p >= MediaTime::from_secs_f64(60.0))
    }));
    h.app.shutdown();
}

#[test]
fn view_override_persists_per_file() {
    let mut h = harness(30.0, Config::default());
    let uri = "https://example.invalid/x.mp4";
    // Not resolvable: the open fails cleanly and returns to the library.
    h.app.open_uri(uri, None);
    assert!(tick_until(&mut h.app, Duration::from_secs(15), |a| a
        .ctl
        .session
        .is_none()));
    assert!(h.app.last_error.is_some());

    // A local file not in the library: overrides are keyed by URI.
    let video = h.dir.path().join("plain.mp4");
    std::fs::write(&video, b"x").unwrap();
    let file_uri = fp_sources::local::path_to_uri(&video);
    h.app.last_error = None;
    h.app.open_uri(&file_uri, None);
    assert!(tick_until(&mut h.app, Duration::from_secs(10), |a| {
        a.ctl.session.as_ref().is_some_and(|s| !s.resolving)
    }));
    let fx = h
        .app
        .ctl
        .handle_ui(fp_ui::UiAction::SetStereo(fp_core::StereoMode::Ou));
    h.app.apply(fx);
    let fx = h.app.ctl.handle_ui(fp_ui::UiAction::OpenLibrary);
    h.app.apply(fx);
    let lib = h.lib.clone();
    let u2 = file_uri.clone();
    assert!(tick_until(&mut h.app, Duration::from_secs(5), |_| {
        lib.get_override(None, Some(&u2))
            .ok()
            .flatten()
            .is_some_and(|v| v.stereo == fp_core::StereoMode::Ou)
    }));
    // Reopening applies the override.
    h.app.open_uri(&file_uri, None);
    assert!(tick_until(&mut h.app, Duration::from_secs(10), |a| {
        a.ctl
            .session
            .as_ref()
            .is_some_and(|s| !s.resolving && s.view.stereo == fp_core::StereoMode::Ou)
    }));
    h.app.shutdown();
}
