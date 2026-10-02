//! FramePlayer's own REST + WebSocket API and the embedded LAN web remote.
//!
//! | Method | Path | Body / query | |
//! |---|---|---|---|
//! | GET | `/` | | web remote page (no token needed; it reads `?token=`) |
//! | GET | `/api/status` | | [`PlayerStatus`] |
//! | POST | `/api/play` | `{uri?, id?, start?}` | open a URI / library item, or resume when empty |
//! | POST | `/api/pause`, `/api/toggle`, `/api/stop` | | |
//! | POST | `/api/seek` | `{t, relative?}` | seconds |
//! | POST | `/api/speed` | `{speed}` | 0.25–4.0 |
//! | POST | `/api/text` | `{text, submit?}` | keyboard input for the headset |
//! | GET | `/api/library` | `?q=&offset=&limit=` | [`LibraryPage`] |
//! | GET | `/api/item/:id` | | [`LibraryItem`] |
//! | GET | `/api/thumb/:id` | | image bytes |
//! | GET | `/api/events` | WebSocket | pushes `{"type":"status","status":{..}}`; accepts [`RemoteCommand`] JSON |
//! | GET | `/api/pairing.svg`, `/api/pairing.png` | | pairing QR code |
//!
//! Every route rejects peers outside the LAN with 403; `/api/*` additionally needs the token
//! as `Authorization: Bearer <token>` or `?token=<token>` (browsers can't set headers on
//! WebSocket or `<img>` requests), else 401.

use crate::net::{is_lan_ip, tokens_match};
use crate::pairing::{qr_png, qr_svg};
use crate::types::{
    LibraryItem, LibraryPage, LibraryProvider, LibraryQuery, PlayerStatus, RemoteCommand,
    RemoteLink,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The web remote page.
pub const INDEX_HTML: &str = include_str!("web/index.html");

/// Minimum spacing between status pushes on one WebSocket.
const WS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const WS_PING_EVERY: Duration = Duration::from_secs(15);
const MAX_TEXT: usize = 2000;

/// Shared state of the HTTP API.
#[derive(Clone)]
pub struct ApiState(Arc<ApiInner>);

struct ApiInner {
    token: String,
    link: RemoteLink,
    library: Arc<dyn LibraryProvider>,
    pairing_url: Option<String>,
}

impl ApiState {
    pub fn new(
        token: String,
        link: RemoteLink,
        library: Arc<dyn LibraryProvider>,
        pairing_url: Option<String>,
    ) -> Self {
        ApiState(Arc::new(ApiInner {
            token,
            link,
            library,
            pairing_url,
        }))
    }
}

/// Build the complete router. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()` so the LAN check can see the peer.
pub fn router(state: ApiState) -> Router {
    let api = Router::new()
        .route("/api/status", get(status))
        .route("/api/play", post(play))
        .route(
            "/api/pause",
            post(|st: State<ApiState>| dispatch(st, RemoteCommand::Pause)),
        )
        .route(
            "/api/toggle",
            post(|st: State<ApiState>| dispatch(st, RemoteCommand::TogglePause)),
        )
        .route(
            "/api/stop",
            post(|st: State<ApiState>| dispatch(st, RemoteCommand::Stop)),
        )
        .route("/api/seek", post(seek))
        .route("/api/speed", post(speed))
        .route("/api/text", post(text))
        .route("/api/library", get(library))
        .route("/api/item/:id", get(item))
        .route("/api/thumb/:id", get(thumbnail))
        .route("/api/events", get(events))
        .route("/api/pairing.svg", get(pairing_svg))
        .route("/api/pairing.png", get(pairing_png))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/", get(index))
        .merge(api)
        .layer(middleware::from_fn(lan_only))
        .with_state(state)
}

async fn lan_only(req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    match peer {
        Some(p) if is_lan_ip(p.ip()) => next.run(req).await,
        _ => {
            tracing::warn!(?peer, "rejected non-LAN remote API request");
            (StatusCode::FORBIDDEN, "LAN clients only").into_response()
        }
    }
}

/// Extract `token` from a raw query string (percent-decoding it).
fn token_from_query(q: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "token").then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len())
            .then(|| {
                std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .flatten();
        match (hex, b[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, c) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn require_token(State(st): State<ApiState>, req: Request, next: Next) -> Response {
    let header_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| tokens_match(&st.0.token, t.trim()));
    // Evaluated eagerly: a borrow of `req` must not live across the await below (Body is !Sync).
    let ok = header_ok
        || req
            .uri()
            .query()
            .and_then(token_from_query)
            .is_some_and(|t| tokens_match(&st.0.token, &t));
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid token",
        )
            .into_response()
    }
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            // The token travels in the URL; never leak it to other origins.
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                 script-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:",
            ),
        ],
        Html(INDEX_HTML),
    )
        .into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::warn!("library provider error: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "library error" })),
    )
        .into_response()
}

async fn dispatch(State(st): State<ApiState>, cmd: RemoteCommand) -> Response {
    match st.0.link.commands.send(cmd).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "player unavailable" })),
        )
            .into_response(),
    }
}

async fn status(State(st): State<ApiState>) -> Json<PlayerStatus> {
    Json(st.0.link.status.borrow().clone())
}

#[derive(Debug, Default, Deserialize)]
struct PlayBody {
    uri: Option<String>,
    id: Option<String>,
    start: Option<f64>,
}

async fn play(st: State<ApiState>, body: Option<Json<PlayBody>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let start = b.start.filter(|s| s.is_finite() && *s >= 0.0);
    let cmd = match (
        b.uri.filter(|u| !u.is_empty()),
        b.id.filter(|i| !i.is_empty()),
    ) {
        (Some(uri), _) => RemoteCommand::Open { uri, start },
        (None, Some(id)) => RemoteCommand::OpenItem { id },
        (None, None) => RemoteCommand::Play,
    };
    dispatch(st, cmd).await
}

#[derive(Debug, Deserialize)]
struct SeekBody {
    t: f64,
    #[serde(default)]
    relative: bool,
}

async fn seek(st: State<ApiState>, Json(b): Json<SeekBody>) -> Response {
    if !b.t.is_finite() || (!b.relative && b.t < 0.0) {
        return bad_request("invalid time");
    }
    let cmd = if b.relative {
        RemoteCommand::SeekRelative { seconds: b.t }
    } else {
        RemoteCommand::Seek { seconds: b.t }
    };
    dispatch(st, cmd).await
}

#[derive(Debug, Deserialize)]
struct SpeedBody {
    speed: f64,
}

async fn speed(st: State<ApiState>, Json(b): Json<SpeedBody>) -> Response {
    if !(0.25..=4.0).contains(&b.speed) {
        return bad_request("speed must be between 0.25 and 4.0");
    }
    dispatch(st, RemoteCommand::SetSpeed { speed: b.speed }).await
}

#[derive(Debug, Deserialize)]
struct TextBody {
    text: String,
    #[serde(default)]
    submit: bool,
}

async fn text(st: State<ApiState>, Json(b): Json<TextBody>) -> Response {
    if b.text.chars().count() > MAX_TEXT {
        return bad_request("text too long");
    }
    dispatch(
        st,
        RemoteCommand::Text {
            text: b.text,
            submit: b.submit,
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
struct LibraryParams {
    q: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn library(State(st): State<ApiState>, Query(p): Query<LibraryParams>) -> Response {
    let query = LibraryQuery {
        q: p.q.map(|q| q.trim().to_string()).filter(|q| !q.is_empty()),
        offset: p.offset.unwrap_or(0),
        limit: p.limit.unwrap_or(50).clamp(1, 200),
    };
    match st.0.library.search(&query).await {
        Ok(page) => Json::<LibraryPage>(page).into_response(),
        Err(e) => internal(e),
    }
}

async fn item(State(st): State<ApiState>, Path(id): Path<String>) -> Response {
    match st.0.library.item(&id).await {
        Ok(Some(it)) => Json::<LibraryItem>(it).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response(),
        Err(e) => internal(e),
    }
}

async fn thumbnail(State(st): State<ApiState>, Path(id): Path<String>) -> Response {
    match st.0.library.thumbnail(&id).await {
        Ok(Some(t)) => {
            let ct = HeaderValue::from_str(&t.content_type)
                .unwrap_or(HeaderValue::from_static("application/octet-stream"));
            (
                [
                    (header::CONTENT_TYPE, ct),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, max-age=3600"),
                    ),
                ],
                t.bytes,
            )
                .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

async fn pairing_svg(State(st): State<ApiState>) -> Response {
    let Some(url) = &st.0.pairing_url else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match qr_svg(url) {
        Ok(svg) => (
            [
                (header::CONTENT_TYPE, "image/svg+xml"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            svg,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn pairing_png(State(st): State<ApiState>) -> Response {
    let Some(url) = &st.0.pairing_url else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match qr_png(url, 8) {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            png,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn events(State(st): State<ApiState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, st))
}

fn status_message(s: &PlayerStatus) -> Message {
    Message::Text(json!({ "type": "status", "status": s }).to_string())
}

async fn ws_session(mut socket: WebSocket, st: ApiState) {
    let mut rx = st.0.link.status.clone();
    let first = status_message(&rx.borrow_and_update());
    if socket.send(first).await.is_err() {
        return;
    }
    let mut last_sent = tokio::time::Instant::now();
    let mut ping = tokio::time::interval(WS_PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                // Coalesce bursts of updates into at most one push per WS_MIN_INTERVAL.
                tokio::time::sleep_until(last_sent + WS_MIN_INTERVAL).await;
                let msg = status_message(&rx.borrow_and_update());
                if socket.send(msg).await.is_err() {
                    break;
                }
                last_sent = tokio::time::Instant::now();
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<RemoteCommand>(&t) {
                    Ok(cmd) => {
                        if st.0.link.commands.send(cmd).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let err = json!({ "type": "error", "error": e.to_string() }).to_string();
                        if socket.send(Message::Text(err)).await.is_err() {
                            break;
                        }
                    }
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{link, AppLink, ProviderError, Thumbnail};
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TOKEN: &str = "s3cretTOKEN";

    pub(crate) struct MockLibrary;

    pub(crate) fn mock_items() -> Vec<LibraryItem> {
        (0..5)
            .map(|i| LibraryItem {
                id: format!("id{i}"),
                title: if i % 2 == 0 {
                    format!("Beach {i}")
                } else {
                    format!("Forest {i}")
                },
                uri: format!("file:///videos/{i}.mp4"),
                duration: Some(60.0 * i as f64),
                has_thumbnail: true,
                ..Default::default()
            })
            .collect()
    }

    #[async_trait]
    impl LibraryProvider for MockLibrary {
        async fn search(&self, q: &LibraryQuery) -> Result<LibraryPage, ProviderError> {
            if q.q.as_deref() == Some("explode") {
                return Err("db is on fire".into());
            }
            let all: Vec<LibraryItem> = mock_items()
                .into_iter()
                .filter(|i| {
                    q.q.as_ref()
                        .is_none_or(|s| i.title.to_lowercase().contains(&s.to_lowercase()))
                })
                .collect();
            let total = all.len();
            Ok(LibraryPage {
                items: all.into_iter().skip(q.offset).take(q.limit).collect(),
                total,
            })
        }
        async fn item(&self, id: &str) -> Result<Option<LibraryItem>, ProviderError> {
            Ok(mock_items().into_iter().find(|i| i.id == id))
        }
        async fn thumbnail(&self, id: &str) -> Result<Option<Thumbnail>, ProviderError> {
            Ok((id == "id1").then(|| Thumbnail {
                bytes: bytes::Bytes::from_static(b"\xff\xd8jpeg"),
                content_type: "image/jpeg".into(),
            }))
        }
    }

    fn app() -> (Router, AppLink) {
        let (app, remote) = link(16);
        let st = ApiState::new(
            TOKEN.into(),
            remote,
            Arc::new(MockLibrary),
            Some("http://192.168.1.2:23560/?token=x".into()),
        );
        (router(st), app)
    }

    fn req(
        method: &str,
        uri: &str,
        peer: Option<&str>,
        bearer: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> HttpRequest<Body> {
        let mut b = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = bearer {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let mut r = b.body(body).unwrap();
        if let Some(p) = peer {
            r.extensions_mut()
                .insert(ConnectInfo::<SocketAddr>(p.parse().unwrap()));
        }
        r
    }

    const LAN: Option<&str> = Some("192.168.1.50:5000");

    async fn call(
        r: &Router,
        rq: HttpRequest<Body>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let resp = r.clone().oneshot(rq).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body, headers)
    }

    fn json_of(b: &[u8]) -> serde_json::Value {
        serde_json::from_slice(b).unwrap()
    }

    #[tokio::test]
    async fn auth_and_lan_policy() {
        let (r, _app) = app();
        assert_eq!(
            call(&r, req("GET", "/api/status", LAN, None, None)).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&r, req("GET", "/api/status", LAN, Some("wrong"), None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&r, req("GET", "/api/status?token=wrong", LAN, None, None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&r, req("GET", "/api/status", LAN, Some(TOKEN), None))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &r,
                req(
                    "GET",
                    &format!("/api/status?x=1&token={TOKEN}"),
                    LAN,
                    None,
                    None
                )
            )
            .await
            .0,
            StatusCode::OK
        );
        // Non-LAN peers are refused even with the right token, and so is an unknown peer.
        assert_eq!(
            call(
                &r,
                req("GET", "/api/status", Some("8.8.8.8:1"), Some(TOKEN), None)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&r, req("GET", "/api/status", None, Some(TOKEN), None))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&r, req("GET", "/", Some("[2001:db8::1]:1"), None, None))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        // IPv6 ULA and loopback are fine.
        assert_eq!(
            call(
                &r,
                req("GET", "/api/status", Some("[fd00::5]:1"), Some(TOKEN), None)
            )
            .await
            .0,
            StatusCode::OK
        );
        // The page itself needs no token.
        let (s, body, h) = call(&r, req("GET", "/", Some("127.0.0.1:9"), None, None)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(String::from_utf8(body).unwrap().contains("<html"));
        assert_eq!(h.get("referrer-policy").unwrap(), "no-referrer");
    }

    #[test]
    fn query_token_parsing() {
        assert_eq!(
            token_from_query("a=1&token=abc%2Bd&b"),
            Some("abc+d".into())
        );
        assert_eq!(token_from_query("tokenx=1"), None);
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%41%zz+"), "A%zz ");
    }

    #[tokio::test]
    async fn status_reflects_app() {
        let (r, app) = app();
        app.status.send_replace(PlayerStatus {
            path: Some("x.mp4".into()),
            position: 3.5,
            playing: true,
            ..Default::default()
        });
        let (s, b, _) = call(&r, req("GET", "/api/status", LAN, Some(TOKEN), None)).await;
        assert_eq!(s, StatusCode::OK);
        let v = json_of(&b);
        assert_eq!(v["path"], "x.mp4");
        assert_eq!(v["position"], 3.5);
        assert_eq!(v["playing"], true);
    }

    #[tokio::test]
    async fn playback_commands() {
        let (r, mut app) = app();
        let cases: Vec<(&str, Option<serde_json::Value>, RemoteCommand)> = vec![
            (
                "/api/play",
                Some(json!({"uri": "smb://nas/a.mp4", "start": 5})),
                RemoteCommand::Open {
                    uri: "smb://nas/a.mp4".into(),
                    start: Some(5.0),
                },
            ),
            (
                "/api/play",
                Some(json!({"id": "id3"})),
                RemoteCommand::OpenItem { id: "id3".into() },
            ),
            ("/api/play", None, RemoteCommand::Play),
            ("/api/pause", None, RemoteCommand::Pause),
            ("/api/toggle", None, RemoteCommand::TogglePause),
            ("/api/stop", None, RemoteCommand::Stop),
            (
                "/api/seek",
                Some(json!({"t": 42.5})),
                RemoteCommand::Seek { seconds: 42.5 },
            ),
            (
                "/api/seek",
                Some(json!({"t": -10, "relative": true})),
                RemoteCommand::SeekRelative { seconds: -10.0 },
            ),
            (
                "/api/speed",
                Some(json!({"speed": 1.5})),
                RemoteCommand::SetSpeed { speed: 1.5 },
            ),
            (
                "/api/text",
                Some(json!({"text": "beach", "submit": true})),
                RemoteCommand::Text {
                    text: "beach".into(),
                    submit: true,
                },
            ),
        ];
        for (path, body, expect) in cases {
            let (s, _, _) = call(&r, req("POST", path, LAN, Some(TOKEN), body)).await;
            assert_eq!(s, StatusCode::ACCEPTED, "{path}");
            assert_eq!(app.commands.recv().await.unwrap(), expect);
        }
        assert_eq!(
            call(
                &r,
                req(
                    "POST",
                    "/api/speed",
                    LAN,
                    Some(TOKEN),
                    Some(json!({"speed": 9}))
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &r,
                req(
                    "POST",
                    "/api/seek",
                    LAN,
                    Some(TOKEN),
                    Some(json!({"t": -1}))
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert!(call(
            &r,
            req("POST", "/api/seek", LAN, Some(TOKEN), Some(json!({"x": 1})))
        )
        .await
        .0
        .is_client_error());
        assert_eq!(
            call(&r, req("GET", "/api/pause", LAN, Some(TOKEN), None))
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        drop(app);
        assert_eq!(
            call(&r, req("POST", "/api/pause", LAN, Some(TOKEN), None))
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn library_endpoints() {
        let (r, _app) = app();
        let (s, b, _) = call(
            &r,
            req(
                "GET",
                "/api/library?q=beach&limit=2",
                LAN,
                Some(TOKEN),
                None,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let v = json_of(&b);
        assert_eq!(v["total"], 3);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert_eq!(v["items"][0]["id"], "id0");
        let v = json_of(
            &call(
                &r,
                req("GET", "/api/library?offset=4", LAN, Some(TOKEN), None),
            )
            .await
            .1,
        );
        assert_eq!(
            (v["total"].as_u64(), v["items"].as_array().unwrap().len()),
            (Some(5), 1)
        );
        assert_eq!(
            call(
                &r,
                req("GET", "/api/library?q=explode", LAN, Some(TOKEN), None)
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            call(
                &r,
                req("GET", "/api/library?limit=abc", LAN, Some(TOKEN), None)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );

        let (s, b, _) = call(&r, req("GET", "/api/item/id2", LAN, Some(TOKEN), None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json_of(&b)["uri"], "file:///videos/2.mp4");
        assert_eq!(
            call(&r, req("GET", "/api/item/nope", LAN, Some(TOKEN), None))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        let (s, b, h) = call(
            &r,
            req(
                "GET",
                &format!("/api/thumb/id1?token={TOKEN}"),
                LAN,
                None,
                None,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h.get("content-type").unwrap(), "image/jpeg");
        assert_eq!(b, b"\xff\xd8jpeg");
        assert_eq!(
            call(&r, req("GET", "/api/thumb/id2", LAN, Some(TOKEN), None))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn pairing_endpoints() {
        let (r, _app) = app();
        let (s, b, h) = call(&r, req("GET", "/api/pairing.svg", LAN, Some(TOKEN), None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h.get("content-type").unwrap(), "image/svg+xml");
        assert!(String::from_utf8(b).unwrap().contains("<svg"));
        let (s, b, _) = call(&r, req("GET", "/api/pairing.png", LAN, Some(TOKEN), None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[1..4], b"PNG");
        assert_eq!(
            call(&r, req("GET", "/api/pairing.svg", LAN, None, None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}
