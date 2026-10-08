//! DeoVR remote API over real loopback sockets.

mod common;

use common::*;
use fp_core::{PlaybackStatus, PlayerCommand};
use fp_remote::{RemoteError, RemoteEvent};
use std::time::{Duration, Instant};

#[test]
fn sends_idle_status_then_changes_keepalives_and_periodic_status() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    assert_ne!(addr.port(), 0);
    assert_eq!(hub.deovr_addr(), Some(addr));

    let mut c = Deovr::connect(addr);
    // Initial status: nothing open.
    assert_eq!(c.status(), serde_json::json!({}));

    hub.publish(&playing("/media/a_180_LR.mp4", 42.0));
    let s = c.status_where(|s| s.get("path").is_some());
    assert_eq!(s["path"], "/media/a_180_LR.mp4");
    assert_eq!(s["duration"], 600.0);
    assert_eq!(s["playbackSpeed"], 1.0);
    assert_eq!(s["playerState"], 0);
    let t = s["currentTime"].as_f64().unwrap();
    assert!((42.0..43.0).contains(&t), "{t}");

    // Keep-alive within ~1 s, and status again within ~1 s while playing,
    // even though nothing was published since.
    let start = Instant::now();
    let (mut pings, mut statuses) = (0, 0);
    while start.elapsed() < Duration::from_millis(2500) && (pings == 0 || statuses == 0) {
        match c.frame().expect("open") {
            f if f.is_empty() => pings += 1,
            _ => statuses += 1,
        }
    }
    assert!(pings > 0, "no keep-alive");
    assert!(statuses > 0, "no periodic status while playing");

    // Pausing is pushed promptly.
    hub.publish(&PlaybackStatus {
        playing: false,
        ..playing("/media/a_180_LR.mp4", 50.0)
    });
    let s = c.status_where(|s| s["playerState"] == 1);
    assert_eq!(s["currentTime"], 50.0);

    // Back to idle: `{}`.
    hub.publish(&PlaybackStatus::default());
    c.status_where(|s| *s == serde_json::json!({}));
}

#[test]
fn client_messages_become_player_commands() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    let mut c = Deovr::connect(addr);
    c.status();

    c.send(b""); // keep-alive from the client: no event
    c.send(br#"{"path":"/media/b.mp4","currentTime":12.5,"playbackSpeed":1.5,"playerState":0}"#);
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::Open {
            location: "/media/b.mp4".into()
        })
    );
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::SetSpeed { speed: 1.5 })
    );
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::Seek { position: 12.5 })
    );
    assert_eq!(next_event(&hub), RemoteEvent::Command(PlayerCommand::Play));

    // Path of the open file plus a time: just a seek.
    hub.publish(&playing("/media/b.mp4", 12.5));
    c.send(br#"{"path":"/media/b.mp4","currentTime":30}"#);
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::Seek { position: 30.0 })
    );
    c.send(br#"{"playerState":1}"#);
    assert_eq!(next_event(&hub), RemoteEvent::Command(PlayerCommand::Pause));
    assert!(
        hub.events()
            .recv_timeout(Duration::from_millis(200))
            .is_err()
    );
}

#[test]
fn malformed_and_oversized_frames_drop_only_that_client() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    let mut good = Deovr::connect(addr);
    good.status();
    let mut bad = Deovr::connect(addr);
    bad.status();
    let mut huge = Deovr::connect(addr);
    huge.status();
    assert!(wait_for(Duration::from_secs(2), || hub
        .deovr_client_count()
        == 3));

    bad.send(b"this is not json");
    bad.expect_closed();
    // Header announcing 2 MiB: dropped without waiting for the body.
    use std::io::Write;
    huge.stream.write_all(&(2u32 << 20).to_le_bytes()).unwrap();
    huge.expect_closed();
    assert!(wait_for(Duration::from_secs(2), || hub
        .deovr_client_count()
        == 1));

    // The well-behaved client is unaffected.
    hub.publish(&playing("/media/c.mp4", 1.0));
    good.status_where(|s| s["path"] == "/media/c.mp4");
    good.send(br#"{"playerState":1}"#);
    assert_eq!(next_event(&hub), RemoteEvent::Command(PlayerCommand::Pause));
}

#[test]
fn multiple_clients_all_receive_status() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    let mut clients: Vec<Deovr> = (0..3).map(|_| Deovr::connect(addr)).collect();
    for c in &mut clients {
        assert_eq!(c.status(), serde_json::json!({}));
    }
    assert!(wait_for(Duration::from_secs(2), || hub
        .deovr_client_count()
        == 3));
    hub.publish(&playing("/media/multi.mp4", 5.0));
    for c in &mut clients {
        c.status_where(|s| s["path"] == "/media/multi.mp4");
    }
    // Commands from any client arrive.
    clients[2].send(br#"{"playbackSpeed":2}"#);
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::SetSpeed { speed: 2.0 })
    );
    drop(clients);
    assert!(wait_for(Duration::from_secs(3), || hub
        .deovr_client_count()
        == 0));
}

#[test]
fn stop_disconnects_and_double_start_is_refused() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    assert!(matches!(
        hub.start_deovr_server(),
        Err(RemoteError::AlreadyRunning(_))
    ));
    let mut c = Deovr::connect(addr);
    c.status();
    hub.stop();
    c.expect_closed();
    assert_eq!(hub.deovr_client_count(), 0);
    assert!(hub.deovr_addr().is_none());
    // Can be started again.
    hub.start_deovr_server().unwrap();
}

#[test]
fn stalled_partial_frame_does_not_block_others() {
    let hub = hub();
    let addr = hub.start_deovr_server().unwrap();
    let mut stuck = Deovr::connect(addr);
    stuck.status();
    // Half a header, then silence.
    use std::io::Write;
    stuck.stream.write_all(&[10, 0]).unwrap();
    let mut other = Deovr::connect(addr);
    other.status();
    other.send(br#"{"playerState":0}"#);
    assert_eq!(next_event(&hub), RemoteEvent::Command(PlayerCommand::Play));
    // The stalled client is dropped after the frame timeout (5 s).
    stuck
        .stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let start = Instant::now();
    while stuck.frame().is_some() {
        assert!(start.elapsed() < Duration::from_secs(8));
    }
}
