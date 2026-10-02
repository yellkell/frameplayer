//! The integration boundary between the remote servers and the app.
//!
//! The app owns playback and the library; the servers only translate. Commands flow to the app
//! over an `mpsc` channel ([`RemoteCommand`]), the app publishes [`PlayerStatus`] through a
//! `watch` channel, and library queries go through the [`LibraryProvider`] trait.

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

/// What the player is doing right now. Times are in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerStatus {
    /// URI / path of the loaded media; `None` when nothing is loaded.
    pub path: Option<String>,
    pub title: Option<String>,
    /// Library id of the loaded media, if it came from the library.
    pub item_id: Option<String>,
    pub duration: Option<f64>,
    pub position: f64,
    pub speed: f64,
    pub playing: bool,
}

impl Default for PlayerStatus {
    fn default() -> Self {
        PlayerStatus {
            path: None,
            title: None,
            item_id: None,
            duration: None,
            position: 0.0,
            speed: 1.0,
            playing: false,
        }
    }
}

/// Requests from remote clients for the app to carry out. Also accepted as JSON over the
/// event WebSocket, e.g. `{"cmd":"seek","seconds":12.5}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum RemoteCommand {
    /// Open a URI or path, optionally starting at `start` seconds.
    Open {
        uri: String,
        start: Option<f64>,
    },
    /// Open a library item by id.
    OpenItem {
        id: String,
    },
    Play,
    Pause,
    TogglePause,
    Stop,
    Seek {
        seconds: f64,
    },
    SeekRelative {
        seconds: f64,
    },
    SetSpeed {
        speed: f64,
    },
    /// Text typed on the phone/desktop keyboard, for whatever text field has focus in the
    /// headset (e.g. library search). `submit` = Enter was pressed.
    Text {
        text: String,
        submit: bool,
    },
}

/// One library entry as shown by the web remote.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryItem {
    pub id: String,
    pub title: String,
    pub uri: String,
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Free-form projection/stereo badge, e.g. "180° SBS".
    pub projection: Option<String>,
    pub tags: Vec<String>,
    pub favourite: bool,
    pub has_script: bool,
    /// Resume position in seconds.
    pub resume: Option<f64>,
    pub has_thumbnail: bool,
}

/// A browse/search request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LibraryQuery {
    /// Search text; empty/`None` lists everything.
    pub q: Option<String>,
    pub offset: usize,
    pub limit: usize,
}

/// One page of results.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LibraryPage {
    pub items: Vec<LibraryItem>,
    /// Total matches (for paging).
    pub total: usize,
}

/// Encoded thumbnail image.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub bytes: Bytes,
    /// e.g. `image/jpeg`.
    pub content_type: String,
}

/// Library access implemented by the app (backed by fp-library).
#[async_trait]
pub trait LibraryProvider: Send + Sync + 'static {
    async fn search(&self, query: &LibraryQuery) -> Result<LibraryPage, ProviderError>;
    async fn item(&self, id: &str) -> Result<Option<LibraryItem>, ProviderError>;
    async fn thumbnail(&self, id: &str) -> Result<Option<Thumbnail>, ProviderError>;
}

/// Error type for [`LibraryProvider`] results (any error; its message is logged, clients get 500).
pub type ProviderError = Box<dyn std::error::Error + Send + Sync>;

/// The app's ends of the channels.
pub struct AppLink {
    /// Commands from remote clients, in arrival order.
    pub commands: mpsc::Receiver<RemoteCommand>,
    /// Publish player state here (on every change and at least a few times per second while
    /// playing; consumers throttle).
    pub status: watch::Sender<PlayerStatus>,
}

/// The servers' ends of the channels.
#[derive(Clone)]
pub struct RemoteLink {
    pub commands: mpsc::Sender<RemoteCommand>,
    pub status: watch::Receiver<PlayerStatus>,
}

/// Create the channel pair connecting the app and the remote servers.
pub fn link(command_capacity: usize) -> (AppLink, RemoteLink) {
    let (cmd_tx, cmd_rx) = mpsc::channel(command_capacity.max(1));
    let (st_tx, st_rx) = watch::channel(PlayerStatus::default());
    (
        AppLink {
            commands: cmd_rx,
            status: st_tx,
        },
        RemoteLink {
            commands: cmd_tx,
            status: st_rx,
        },
    )
}
