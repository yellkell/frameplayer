//! [`RemoteHub`]: the app-facing entry point that owns both servers.

use crate::deovr::DeovrServer;
use crate::status::{StatusCell, lock};
use crate::web::WebServer;
use crate::{RemoteConfig, RemoteError, token};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use fp_core::{PlaybackStatus, PlayerCommand};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

/// Something a remote client asked the player to do.
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteEvent {
    /// A playback command (from either server).
    Command(PlayerCommand),
    /// Text typed on the web remote, for whatever text field is focused in
    /// the headset (library search, URL entry).
    Text(String),
}

/// One library entry as the web remote shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteItem {
    /// Library id; used for `GET /api/thumb/<id>`.
    pub id: i64,
    /// Display title.
    pub title: String,
    /// Location passed back in `PlayerCommand::Open` when tapped.
    pub location: String,
    /// Seconds, when known.
    pub duration: Option<f64>,
    /// Short format description, e.g. "180° Side by side".
    pub format_label: String,
    /// Whether [`RemoteLibrary::thumbnail_path`] has an image for `id`.
    pub has_thumbnail: bool,
}

/// Library access the app provides to the web remote. Called from server
/// worker threads, so implementations must be thread-safe and should not
/// block for long.
pub trait RemoteLibrary: Send + Sync {
    /// Items matching `query` (empty = everything, app-defined order).
    fn search(&self, query: &str, limit: usize, offset: usize) -> Vec<RemoteItem>;
    /// Path of a JPEG thumbnail for item `id`, if there is one.
    fn thumbnail_path(&self, id: i64) -> Option<PathBuf>;
}

/// Capacity of the event queue; when the app stops draining it, further
/// events are dropped (and logged) instead of growing without bound.
const EVENT_QUEUE: usize = 1024;

/// State shared by the hub and every server thread.
pub(crate) struct Shared {
    pub(crate) status: StatusCell,
    pub(crate) token: RwLock<String>,
    events: Sender<RemoteEvent>,
}

impl Shared {
    pub(crate) fn emit(&self, ev: RemoteEvent) {
        match self.events.try_send(ev) {
            Ok(()) => {}
            Err(TrySendError::Full(ev)) => log::warn!("remote event queue full, dropping {ev:?}"),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub(crate) fn token(&self) -> String {
        self.token
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Owns the DeoVR API server and the web remote, the shared playback
/// status they report, and the queue of commands they receive.
///
/// Nothing listens until [`start_deovr_server`](Self::start_deovr_server)
/// or [`start_web`](Self::start_web) is called. Dropping the hub stops both.
pub struct RemoteHub {
    config: RemoteConfig,
    shared: Arc<Shared>,
    events: Receiver<RemoteEvent>,
    deovr: Mutex<Option<DeovrServer>>,
    web: Mutex<Option<WebServer>>,
}

impl RemoteHub {
    /// Creates the hub and loads (or creates and persists) the pairing token
    /// at `config.token_path`. Does not open any socket.
    pub fn new(config: RemoteConfig) -> Result<RemoteHub, RemoteError> {
        let token = match &config.token_path {
            Some(path) => token::load_or_create(path)?,
            None => token::generate_token()?,
        };
        let (tx, rx) = crossbeam_channel::bounded(EVENT_QUEUE);
        Ok(RemoteHub {
            config,
            shared: Arc::new(Shared {
                status: StatusCell::default(),
                token: RwLock::new(token),
                events: tx,
            }),
            events: rx,
            deovr: Mutex::new(None),
            web: Mutex::new(None),
        })
    }

    /// The configuration the hub was created with.
    pub fn config(&self) -> &RemoteConfig {
        &self.config
    }

    /// Starts the DeoVR-compatible TCP API on `bind_address:deovr_port` and
    /// returns the bound address (useful with port 0).
    pub fn start_deovr_server(&self) -> Result<SocketAddr, RemoteError> {
        let mut slot = lock(&self.deovr);
        if slot.is_some() {
            return Err(RemoteError::AlreadyRunning("DeoVR"));
        }
        let bind = SocketAddr::new(self.config.bind_address, self.config.deovr_port);
        let server = DeovrServer::start(
            bind,
            self.shared.clone(),
            self.config.max_deovr_clients.max(1),
        )?;
        let addr = server.addr();
        *slot = Some(server);
        Ok(addr)
    }

    /// Starts the web remote on `bind_address:web_port`, serving `lib`, and
    /// returns the bound address.
    pub fn start_web(&self, lib: Arc<dyn RemoteLibrary>) -> Result<SocketAddr, RemoteError> {
        let mut slot = lock(&self.web);
        if slot.is_some() {
            return Err(RemoteError::AlreadyRunning("web remote"));
        }
        let bind = SocketAddr::new(self.config.bind_address, self.config.web_port);
        let server = WebServer::start(
            bind,
            self.shared.clone(),
            lib,
            self.config.max_event_streams.max(1),
        )?;
        let addr = server.addr();
        *slot = Some(server);
        Ok(addr)
    }

    /// Stops the DeoVR API server and disconnects its clients.
    pub fn stop_deovr_server(&self) {
        if let Some(mut s) = lock(&self.deovr).take() {
            s.stop();
        }
    }

    /// Stops the web remote. Open live-status streams end within ~100 ms.
    pub fn stop_web(&self) {
        if let Some(mut s) = lock(&self.web).take() {
            s.stop();
        }
    }

    /// Stops both servers.
    pub fn stop(&self) {
        self.stop_deovr_server();
        self.stop_web();
    }

    /// Records the current playback status. Cheap enough to call every
    /// frame: it compares, copies into existing buffers and never blocks on
    /// the network. The servers pick changes up on their own threads.
    pub fn publish(&self, status: &PlaybackStatus) {
        self.shared.status.publish(status);
    }

    /// The command/text queue. Each receiver clone competes for events, so
    /// drain it from one place (the main loop) with `try_recv`.
    pub fn events(&self) -> Receiver<RemoteEvent> {
        self.events.clone()
    }

    /// Address the DeoVR API is bound to, while running.
    pub fn deovr_addr(&self) -> Option<SocketAddr> {
        lock(&self.deovr).as_ref().map(DeovrServer::addr)
    }

    /// Address the web remote is bound to, while running.
    pub fn web_addr(&self) -> Option<SocketAddr> {
        lock(&self.web).as_ref().map(WebServer::addr)
    }

    /// Connected DeoVR API clients (script players and similar tools).
    pub fn deovr_client_count(&self) -> usize {
        lock(&self.deovr)
            .as_ref()
            .map_or(0, DeovrServer::client_count)
    }

    /// Web remote pages currently open (live-status streams).
    pub fn web_client_count(&self) -> usize {
        lock(&self.web).as_ref().map_or(0, WebServer::client_count)
    }

    /// The current pairing token. Treat as a secret.
    pub fn token(&self) -> String {
        self.shared.token()
    }

    /// Replaces the pairing token (persisting it when a path is configured),
    /// which unpairs every phone. Returns the new token.
    pub fn regenerate_token(&self) -> Result<String, RemoteError> {
        let new = token::generate_token()?;
        if let Some(path) = &self.config.token_path {
            token::store(path, &new)?;
        }
        *self
            .shared
            .token
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new.clone();
        Ok(new)
    }

    /// The URL to show as a QR code: `http://<lan_ip>:<port>/?token=...`,
    /// using the bound port while the web remote runs, else the configured one.
    pub fn pairing_url(&self, lan_ip: impl Into<IpAddr>) -> String {
        let port = self.web_addr().map_or(self.config.web_port, |a| a.port());
        let host = match lan_ip.into() {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        format!("http://{host}:{port}/?token={}", self.token())
    }
}

impl Drop for RemoteHub {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn pairing_url_and_token_rotation() {
        let hub = RemoteHub::new(RemoteConfig::default()).unwrap();
        let t = hub.token();
        assert_eq!(
            hub.pairing_url(Ipv4Addr::new(192, 168, 1, 9)),
            format!("http://192.168.1.9:8790/?token={t}")
        );
        let v6: IpAddr = "fe80::1".parse().unwrap();
        assert!(hub.pairing_url(v6).starts_with("http://[fe80::1]:8790/"));
        let t2 = hub.regenerate_token().unwrap();
        assert_ne!(t, t2);
        assert_eq!(hub.token(), t2);
        assert_eq!(hub.deovr_client_count(), 0);
        assert_eq!(hub.web_client_count(), 0);
        assert!(hub.deovr_addr().is_none());
    }
}
