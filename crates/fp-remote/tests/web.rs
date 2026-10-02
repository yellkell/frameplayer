//! Web remote over real loopback HTTP.

mod common;

use common::*;
use fp_core::PlayerCommand;
use fp_remote::{RemoteEvent, RemoteHub};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn started() -> (RemoteHub, SocketAddr, String) {
    let hub = hub();
    let addr = hub.start_web(Arc::new(FakeLibrary::new())).unwrap();
    let token = hub.token();
    (hub, addr, token)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn everything_but_assets_requires_the_token() {
    let (_hub, addr, token) = started();
    for path in [
        "/api/status",
        "/api/library?q=",
        "/api/thumb/1",
        "/api/events",
    ] {
        let r = http(addr, "GET", path, &[], b"");
        assert_eq!(r.status, 401, "{path}");
    }
    let r = http(addr, "POST", "/api/command", &[], br#"{"cmd":"play"}"#);
    assert_eq!(r.status, 401);
    let wrong = "fp_remote_token=0000";
    let r = http(addr, "GET", "/api/status", &[("Cookie", wrong)], b"");
    assert_eq!(r.status, 401);
    let r = http(addr, "GET", "/api/status?token=nope", &[], b"");
    assert_eq!(r.status, 401);

    // Unpaired page explains what to do; it is not the app.
    let r = http(addr, "GET", "/", &[], b"");
    assert_eq!(r.status, 401);
    assert!(r.text().contains("Not paired"));
    assert!(!r.text().contains("app.js"));

    // Static code needs no token (it holds no data).
    let r = http(addr, "GET", "/app.js", &[], b"");
    assert_eq!(r.status, 200);
    assert!(
        r.header("Content-Type")
            .unwrap()
            .starts_with("text/javascript")
    );
    let r = http(addr, "GET", "/app.css", &[], b"");
    assert_eq!(r.status, 200);

    // Bearer works too.
    let r = http(
        addr,
        "GET",
        "/api/status",
        &[("Authorization", &bearer(&token))],
        b"",
    );
    assert_eq!(r.status, 200);
}

#[test]
fn pairing_url_sets_cookie_and_redirects() {
    let (hub, addr, token) = started();
    let url = hub.pairing_url(Ipv4Addr::LOCALHOST);
    assert_eq!(
        url,
        format!("http://127.0.0.1:{}/?token={token}", addr.port())
    );
    let path = &url[url.find("/?").unwrap()..];

    let r = http(addr, "GET", path, &[], b"");
    assert_eq!(r.status, 303);
    assert_eq!(r.header("Location"), Some("/"));
    let cookie = r.header("Set-Cookie").unwrap();
    assert!(cookie.starts_with(&format!("fp_remote_token={token};")));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert_eq!(r.header("Referrer-Policy"), Some("no-referrer"));

    // The browser now sends the cookie.
    let pair = cookie.split(';').next().unwrap();
    let r = http(addr, "GET", "/", &[("Cookie", pair)], b"");
    assert_eq!(r.status, 200);
    assert!(r.text().contains("/app.js"));
    let csp = r.header("Content-Security-Policy").unwrap();
    assert!(csp.contains("script-src 'self'"));
    assert_eq!(r.header("X-Frame-Options"), Some("DENY"));

    hub.publish(&playing("/media/x.mp4", 7.0));
    let r = http(addr, "GET", "/api/status", &[("Cookie", pair)], b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    let s = r.json();
    assert_eq!(s["location"], "/media/x.mp4");
    assert_eq!(s["title"], "Test video");
    assert_eq!(s["playing"], true);
    assert!(s["now_ms"].as_u64().unwrap() > 0);
    // Matches PlaybackStatus apart from the extra clock field.
    let back: fp_core::PlaybackStatus = serde_json::from_value(s).unwrap();
    assert_eq!(back.position, 7.0);

    // Rotating the token unpairs.
    hub.regenerate_token().unwrap();
    let r = http(addr, "GET", "/api/status", &[("Cookie", pair)], b"");
    assert_eq!(r.status, 401);
}

#[test]
fn commands_and_text_reach_the_event_queue() {
    let (hub, addr, token) = started();
    let auth = bearer(&token);
    let h = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];

    let r = http(
        addr,
        "POST",
        "/api/command",
        &h,
        br#"{"cmd":"seek","position":5}"#,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::Seek { position: 5.0 })
    );
    let r = http(
        addr,
        "POST",
        "/api/command",
        &h,
        br#"{"cmd":"open","location":"/media/1.mp4"}"#,
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::Open {
            location: "/media/1.mp4".into()
        })
    );
    let r = http(
        addr,
        "POST",
        "/api/command",
        &h,
        br#"{"cmd":"seek_relative","delta":-10}"#,
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        next_event(&hub),
        RemoteEvent::Command(PlayerCommand::SeekRelative { delta: -10.0 })
    );

    // Rejected: bad JSON, unknown command, bad values, wrong method.
    for body in [
        &b"nope"[..],
        br#"{"cmd":"explode"}"#,
        br#"{"cmd":"seek","position":-1}"#,
        br#"{"cmd":"set_speed","speed":0}"#,
        br#"{"cmd":"open","location":""}"#,
    ] {
        let r = http(addr, "POST", "/api/command", &h, body);
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(body));
    }
    let r = http(addr, "GET", "/api/command", &h, b"");
    assert_eq!(r.status, 405);
    let big = vec![b' '; 70 * 1024];
    let r = http(addr, "POST", "/api/command", &h, &big);
    assert_eq!(r.status, 413);

    let r = http(
        addr,
        "POST",
        "/api/text",
        &h,
        r#"{"text":"beach 180\u0007"}"#.as_bytes(),
    );
    assert_eq!(r.status, 200);
    assert_eq!(next_event(&hub), RemoteEvent::Text("beach 180".into()));
    let r = http(addr, "POST", "/api/text", &h, br#"{"words":"x"}"#);
    assert_eq!(r.status, 400);

    assert!(
        hub.events()
            .recv_timeout(Duration::from_millis(200))
            .is_err()
    );
}

#[test]
fn library_search_and_thumbnails() {
    let (_hub, addr, token) = started();
    let auth = bearer(&token);
    let h = [("Authorization", auth.as_str())];

    let r = http(addr, "GET", "/api/library", &h, b"");
    assert_eq!(r.status, 200);
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["id"], 1);
    assert_eq!(items[0]["title"], "Beach sunset");
    assert_eq!(items[0]["location"], "/media/1.mp4");
    assert_eq!(items[0]["duration"], 60.0);
    assert_eq!(items[0]["format_label"], "180° Side by side");
    assert_eq!(items[0]["has_thumbnail"], true);

    let r = http(
        addr,
        "GET",
        "/api/library?q=BEACH&limit=1&offset=1",
        &h,
        b"",
    );
    let j = r.json();
    let items = j["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    // Library text comes back verbatim as JSON data; the page renders it
    // with textContent.
    assert_eq!(
        items[0]["title"],
        "Beach volleyball <script>alert(1)</script>"
    );
    assert_eq!(j["limit"], 1);
    assert_eq!(j["offset"], 1);

    let r = http(addr, "GET", "/api/library?q=mountain%20hike", &h, b"");
    assert_eq!(r.json()["items"][0]["id"], 2);
    let r = http(addr, "GET", "/api/library?limit=99999&offset=x", &h, b"");
    assert_eq!(r.json()["limit"], 200);
    assert_eq!(r.json()["offset"], 0);

    let r = http(addr, "GET", "/api/thumb/1", &h, b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Type"), Some("image/jpeg"));
    assert_eq!(r.body, JPEG);
    assert_eq!(http(addr, "GET", "/api/thumb/2", &h, b"").status, 404);
    assert_eq!(http(addr, "GET", "/api/thumb/abc", &h, b"").status, 404);
    assert_eq!(http(addr, "GET", "/api/nothing", &h, b"").status, 404);
}

/// Reads server-sent events until one `data:` line satisfies `f`.
fn next_data(s: &mut TcpStream, buf: &mut Vec<u8>, f: impl Fn(&serde_json::Value) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    let mut chunk = [0u8; 4096];
    while Instant::now() < end {
        while let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
            let event = String::from_utf8_lossy(&buf[..i]).into_owned();
            buf.drain(..i + 2);
            for line in event.lines() {
                if let Some(json) = line.strip_prefix("data: ") {
                    if f(&serde_json::from_str(json).unwrap()) {
                        return;
                    }
                }
            }
        }
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "stream closed");
        buf.extend_from_slice(&chunk[..n]);
    }
    panic!("no matching event");
}

#[test]
fn live_status_stream() {
    let (hub, addr, token) = started();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET /api/events?token={token} HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
    }
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = parse_response(&buf[..split + 4]);
    assert_eq!(head.status, 200);
    assert!(
        head.header("Content-Type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert!(head.header("Set-Cookie").is_some());
    buf.drain(..split + 4);

    next_data(&mut s, &mut buf, |v| v["location"] == "");
    assert!(wait_for(Duration::from_secs(2), || hub.web_client_count() == 1));
    hub.publish(&playing("/media/live.mp4", 3.0));
    next_data(&mut s, &mut buf, |v| {
        v["location"] == "/media/live.mp4" && v["now_ms"].as_u64().is_some()
    });

    drop(s);
    // The stream notices the closed socket on its next write (the periodic
    // refresh while playing).
    assert!(wait_for(Duration::from_secs(8), || hub.web_client_count() == 0));
}

#[test]
fn event_streams_are_capped() {
    let hub = RemoteHub::new(fp_remote::RemoteConfig {
        bind_address: Ipv4Addr::LOCALHOST.into(),
        web_port: 0,
        max_event_streams: 1,
        ..Default::default()
    })
    .unwrap();
    let addr = hub.start_web(Arc::new(FakeLibrary::new())).unwrap();
    let token = hub.token();
    let mut first = TcpStream::connect(addr).unwrap();
    write!(
        first,
        "GET /api/events?token={token} HTTP/1.1\r\nHost: x\r\n\r\n"
    )
    .unwrap();
    assert!(wait_for(Duration::from_secs(2), || hub.web_client_count() == 1));
    let r = http(addr, "GET", &format!("/api/events?token={token}"), &[], b"");
    assert_eq!(r.status, 503);
    hub.stop();
    assert!(hub.web_addr().is_none());
}
