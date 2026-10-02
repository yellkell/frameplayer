//! Configuration and lifecycle of the remote-control servers.

use crate::api::{router, ApiState};
use crate::deovr::{self, DeoVrServerOptions};
use crate::net::{detect_lan_ip, generate_token};
use crate::pairing::{pairing_url, qr_svg};
use crate::types::{LibraryProvider, RemoteLink};
use crate::RemoteError;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Default port of FramePlayer's own HTTP API / web remote (arbitrary, next to DeoVR's).
pub const DEFAULT_HTTP_PORT: u16 = 23560;

/// REST/WebSocket API + web remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub enabled: bool,
    /// Listen address. `0.0.0.0` listens everywhere; non-LAN peers are still refused.
    pub bind: IpAddr,
    pub port: u16,
    /// Access token; generated on start when empty (persist [`RemoteHandle::token`]).
    pub token: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            enabled: false,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_HTTP_PORT,
            token: String::new(),
        }
    }
}

/// DeoVR-compatible TCP remote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeoVrConfig {
    pub enabled: bool,
    pub bind: IpAddr,
    pub port: u16,
    pub status_interval_ms: u64,
    /// Drop clients silent for this long (`None` = never).
    pub idle_timeout_secs: Option<u64>,
}

impl Default for DeoVrConfig {
    fn default() -> Self {
        DeoVrConfig {
            enabled: false,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: deovr::DEFAULT_PORT,
            status_interval_ms: 1000,
            idle_timeout_secs: None,
        }
    }
}

/// Both servers are off by default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub http: HttpConfig,
    pub deovr: DeoVrConfig,
}

/// Running servers. Call [`RemoteHandle::shutdown`] to stop them.
pub struct RemoteHandle {
    http_addr: Option<SocketAddr>,
    deovr_addr: Option<SocketAddr>,
    token: String,
    pairing_url: Option<String>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl RemoteHandle {
    pub fn http_addr(&self) -> Option<SocketAddr> {
        self.http_addr
    }
    pub fn deovr_addr(&self) -> Option<SocketAddr> {
        self.deovr_addr
    }
    /// The token in use (generated if the config had none).
    pub fn token(&self) -> &str {
        &self.token
    }
    /// URL for the phone to open, when the HTTP API runs and a LAN address is known.
    pub fn pairing_url(&self) -> Option<&str> {
        self.pairing_url.as_deref()
    }
    /// Pairing QR code as SVG, for display in the headset UI.
    pub fn pairing_qr_svg(&self) -> Option<String> {
        self.pairing_url.as_deref().and_then(|u| qr_svg(u).ok())
    }

    /// Stop accepting, close connections and wait (bounded) for the tasks to end.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        for t in self.tasks.drain(..) {
            let abort = t.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), t)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Start whichever servers are enabled.
pub async fn start(
    config: &RemoteConfig,
    link: RemoteLink,
    library: Arc<dyn LibraryProvider>,
) -> Result<RemoteHandle, RemoteError> {
    let (sd_tx, sd_rx) = watch::channel(false);
    let mut handle = RemoteHandle {
        http_addr: None,
        deovr_addr: None,
        token: if config.http.token.is_empty() {
            generate_token()
        } else {
            config.http.token.clone()
        },
        pairing_url: None,
        shutdown: sd_tx,
        tasks: Vec::new(),
    };

    if config.http.enabled {
        let listener = TcpListener::bind((config.http.bind, config.http.port)).await?;
        let addr = listener.local_addr()?;
        let lan_ip = if config.http.bind.is_unspecified() {
            detect_lan_ip()
        } else {
            Some(config.http.bind)
        };
        handle.pairing_url = lan_ip.map(|ip| pairing_url(ip, addr.port(), &handle.token));
        let state = ApiState::new(
            handle.token.clone(),
            link.clone(),
            library,
            handle.pairing_url.clone(),
        );
        let svc = router(state).into_make_service_with_connect_info::<SocketAddr>();
        let mut sd = sd_rx.clone();
        handle.tasks.push(tokio::spawn(async move {
            let graceful = async move {
                let _ = sd.wait_for(|v| *v).await;
            };
            if let Err(e) = axum::serve(listener, svc)
                .with_graceful_shutdown(graceful)
                .await
            {
                tracing::warn!("remote HTTP server stopped: {e}");
            }
        }));
        handle.http_addr = Some(addr);
        tracing::info!(%addr, "remote HTTP API listening");
    }

    if config.deovr.enabled {
        let listener = TcpListener::bind((config.deovr.bind, config.deovr.port)).await?;
        let addr = listener.local_addr()?;
        let opts = DeoVrServerOptions {
            status_interval: Duration::from_millis(config.deovr.status_interval_ms.max(50)),
            idle_timeout: config.deovr.idle_timeout_secs.map(Duration::from_secs),
        };
        handle
            .tasks
            .push(tokio::spawn(deovr::serve(listener, link, opts, sd_rx)));
        handle.deovr_addr = Some(addr);
        tracing::info!(%addr, "DeoVR remote listening");
    }

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        link, LibraryItem, LibraryPage, LibraryQuery, PlayerStatus, ProviderError, RemoteCommand,
        Thumbnail,
    };
    use async_trait::async_trait;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    struct EmptyLibrary;

    #[async_trait]
    impl LibraryProvider for EmptyLibrary {
        async fn search(&self, _: &LibraryQuery) -> Result<LibraryPage, ProviderError> {
            Ok(LibraryPage::default())
        }
        async fn item(&self, _: &str) -> Result<Option<LibraryItem>, ProviderError> {
            Ok(None)
        }
        async fn thumbnail(&self, _: &str) -> Result<Option<Thumbnail>, ProviderError> {
            Ok(None)
        }
    }

    fn local_config() -> RemoteConfig {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        RemoteConfig {
            http: HttpConfig {
                enabled: true,
                bind: lo,
                port: 0,
                token: "tok123".into(),
            },
            deovr: DeoVrConfig {
                enabled: true,
                bind: lo,
                port: 0,
                ..Default::default()
            },
        }
    }

    #[test]
    fn defaults_are_off() {
        let c = RemoteConfig::default();
        assert!(!c.http.enabled && !c.deovr.enabled);
        assert_eq!(c.deovr.port, 23554);
        let parsed: RemoteConfig = serde_json::from_str(r#"{"http":{"enabled":true}}"#).unwrap();
        assert!(
            parsed.http.enabled && parsed.http.port == DEFAULT_HTTP_PORT && !parsed.deovr.enabled
        );
    }

    #[tokio::test]
    async fn disabled_starts_nothing_and_generates_token() {
        let (_app, remote) = link(4);
        let h = start(&RemoteConfig::default(), remote, Arc::new(EmptyLibrary))
            .await
            .unwrap();
        assert!(h.http_addr().is_none() && h.deovr_addr().is_none());
        assert_eq!(h.token().len(), 32);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn websocket_pushes_status_and_accepts_commands() {
        let (mut app, remote) = link(16);
        let h = start(&local_config(), remote, Arc::new(EmptyLibrary))
            .await
            .unwrap();
        let addr = h.http_addr().unwrap();
        assert_eq!(
            h.pairing_url(),
            Some(format!("http://127.0.0.1:{}/?token=tok123", addr.port()).as_str())
        );
        assert!(h.pairing_qr_svg().unwrap().contains("<svg"));
        assert!(h.deovr_addr().is_some());

        // Wrong token: the upgrade is refused.
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/api/events?token=bad"))
                .await
                .is_err()
        );

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/api/events?token=tok123"))
                .await
                .unwrap();
        let next_status = |m: Message| -> serde_json::Value {
            let v: serde_json::Value = serde_json::from_str(m.to_text().unwrap()).unwrap();
            assert_eq!(v["type"], "status");
            v["status"].clone()
        };
        let first = next_status(ws.next().await.unwrap().unwrap());
        assert_eq!(first["playing"], false);

        app.status.send_replace(PlayerStatus {
            path: Some("v.mp4".into()),
            playing: true,
            position: 7.0,
            ..Default::default()
        });
        let pushed = loop {
            let s = next_status(ws.next().await.unwrap().unwrap());
            if s["playing"] == true {
                break s;
            }
        };
        assert_eq!(pushed["path"], "v.mp4");

        ws.send(Message::Text(r#"{"cmd":"seek","seconds":30}"#.into()))
            .await
            .unwrap();
        assert_eq!(
            app.commands.recv().await.unwrap(),
            RemoteCommand::Seek { seconds: 30.0 }
        );
        ws.send(Message::Text("not json".into())).await.unwrap();
        let err: serde_json::Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(err["type"], "error");

        drop(ws);
        h.shutdown().await;
    }
}
