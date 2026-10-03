//! fp-remote integration: the [`LibraryProvider`] the web remote and REST
//! API browse (backed by fp-library), item conversion, and the server
//! lifecycle (start / restart on config change with a persistent channel
//! pair, so remote commands keep flowing across restarts).
//!
//! Commands from remotes are forwarded to the app as
//! [`ServiceEvent::Remote`]; the controller maps them
//! ([`crate::controller::Controller::handle_remote`]). Player status goes
//! the other way through the `watch` sender the app owns.

use crate::runtime::services::ServiceEvent;
use async_trait::async_trait;
use fp_library::{Item, ItemQuery, Library};
use fp_remote::{
    LibraryItem, LibraryPage, LibraryProvider, LibraryQuery, ProviderError, RemoteConfig,
    RemoteHandle, RemoteLink, Thumbnail,
};
use std::sync::Arc;

/// Default page size when a client does not ask for one.
pub const DEFAULT_PAGE: usize = 60;

/// Library access for the remote servers.
pub struct LibraryBridge {
    lib: Arc<Library>,
}

impl LibraryBridge {
    pub fn new(lib: Arc<Library>) -> Self {
        LibraryBridge { lib }
    }
}

/// One library row as the web remote shows it.
pub fn to_remote_item(it: &Item) -> LibraryItem {
    LibraryItem {
        id: it.id.to_string(),
        title: it.title.clone(),
        uri: it.uri.clone(),
        duration: it.duration.map(|d| d.as_secs_f64()),
        width: it.width,
        height: it.height,
        projection: it
            .projection
            .as_ref()
            .map(|p| fp_ui::screens::projection_label(p, it.stereo.unwrap_or_default())),
        tags: it.tags.clone(),
        favourite: it.favourite,
        has_script: it.has_script,
        resume: it.resume.map(|r| r.as_secs_f64()),
        has_thumbnail: it.thumbnail_path.is_some(),
    }
}

/// The library query for a remote search request.
pub fn to_item_query(q: &LibraryQuery) -> ItemQuery {
    let text = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty());
    ItemQuery {
        text: text.map(str::to_string),
        ..Default::default()
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ProviderError> + Send + 'static,
) -> Result<T, ProviderError> {
    tokio::task::spawn_blocking(f).await?
}

#[async_trait]
impl LibraryProvider for LibraryBridge {
    async fn search(&self, query: &LibraryQuery) -> Result<LibraryPage, ProviderError> {
        let lib = self.lib.clone();
        let q = query.clone();
        blocking(move || {
            let all = lib.query(&to_item_query(&q))?;
            let limit = if q.limit == 0 { DEFAULT_PAGE } else { q.limit };
            Ok(LibraryPage {
                total: all.len(),
                items: all
                    .iter()
                    .skip(q.offset)
                    .take(limit)
                    .map(|s| to_remote_item(&s.item))
                    .collect(),
            })
        })
        .await
    }

    async fn item(&self, id: &str) -> Result<Option<LibraryItem>, ProviderError> {
        let Ok(id) = id.parse::<i64>() else {
            return Ok(None);
        };
        let lib = self.lib.clone();
        blocking(move || Ok(lib.item(id)?.map(|i| to_remote_item(&i)))).await
    }

    async fn thumbnail(&self, id: &str) -> Result<Option<Thumbnail>, ProviderError> {
        let Ok(id) = id.parse::<i64>() else {
            return Ok(None);
        };
        let lib = self.lib.clone();
        let Some(path) = blocking(move || Ok(lib.item(id)?.and_then(|i| i.thumbnail_path))).await?
        else {
            return Ok(None);
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(Thumbnail {
                content_type: if bytes.starts_with(b"\x89PNG") {
                    "image/png".into()
                } else {
                    "image/jpeg".into()
                },
                bytes: bytes.into(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Running remote servers plus the long-lived channel ends.
pub struct RemoteService {
    link: RemoteLink,
    provider: Arc<dyn LibraryProvider>,
    handle: Option<RemoteHandle>,
    events: crossbeam_channel::Sender<ServiceEvent>,
}

impl RemoteService {
    pub fn new(
        link: RemoteLink,
        provider: Arc<dyn LibraryProvider>,
        events: crossbeam_channel::Sender<ServiceEvent>,
    ) -> Self {
        RemoteService {
            link,
            provider,
            handle: None,
            events,
        }
    }

    /// (Re)start the servers for `cfg`; reports the pairing URL and the
    /// token actually in use (so a generated one can be persisted).
    pub async fn apply(&mut self, cfg: &RemoteConfig) {
        if let Some(h) = self.handle.take() {
            h.shutdown().await;
        }
        if !cfg.http.enabled && !cfg.deovr.enabled {
            let _ = self.events.send(ServiceEvent::RemoteStarted {
                pairing_url: None,
                token: None,
            });
            return;
        }
        match fp_remote::start(cfg, self.link.clone(), self.provider.clone()).await {
            Ok(h) => {
                let _ = self.events.send(ServiceEvent::RemoteStarted {
                    pairing_url: h.pairing_url().map(str::to_string),
                    token: cfg.http.enabled.then(|| h.token().to_string()),
                });
                self.handle = Some(h);
            }
            Err(e) => {
                tracing::warn!("remote servers failed to start: {e}");
                let _ = self.events.send(ServiceEvent::Error(format!(
                    "Remote control unavailable: {e}"
                )));
            }
        }
    }

    #[cfg(test)]
    pub fn http_addr(&self) -> Option<std::net::SocketAddr> {
        self.handle.as_ref().and_then(|h| h.http_addr())
    }

    pub async fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::{MediaTime, Projection, StereoMode};
    use fp_library::NewItem;

    fn lib_with_items() -> Arc<Library> {
        let lib = Library::open_in_memory().unwrap();
        for (i, name) in ["Beach_180_LR.mp4", "Forest.mkv", "Beach walk.mp4"]
            .iter()
            .enumerate()
        {
            lib.upsert_item(&NewItem {
                source_id: None,
                uri: format!("file:///v/{name}"),
                path: name.to_string(),
                title: name
                    .trim_end_matches(".mp4")
                    .trim_end_matches(".mkv")
                    .to_string(),
                size: Some(100 + i as u64),
                duration: Some(MediaTime::from_secs_f64(60.0)),
                ..Default::default()
            })
            .unwrap();
        }
        Arc::new(lib)
    }

    #[test]
    fn provider_search_item_thumbnail() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = LibraryBridge::new(lib_with_items());
        let page = rt
            .block_on(bridge.search(&LibraryQuery {
                q: Some("beach".into()),
                offset: 0,
                limit: 0,
            }))
            .unwrap();
        assert_eq!(page.total, 2);
        assert!(page
            .items
            .iter()
            .all(|i| i.title.to_lowercase().contains("beach")));
        let all = rt
            .block_on(bridge.search(&LibraryQuery {
                q: None,
                offset: 1,
                limit: 1,
            }))
            .unwrap();
        assert_eq!((all.total, all.items.len()), (3, 1));
        let id = &page.items[0].id;
        let it = rt.block_on(bridge.item(id)).unwrap().unwrap();
        assert_eq!(&it.id, id);
        assert!(rt.block_on(bridge.item("nope")).unwrap().is_none());
        assert!(rt.block_on(bridge.thumbnail(id)).unwrap().is_none());
    }

    #[test]
    fn item_conversion() {
        let mut it = crate::controller::tests::item(5, "file:///v/a.mp4");
        it.projection = Some(Projection::EQUIRECT_180);
        it.stereo = Some(StereoMode::Sbs);
        it.resume = Some(MediaTime::from_secs_f64(12.5));
        let r = to_remote_item(&it);
        assert_eq!(r.id, "5");
        assert_eq!(r.projection.as_deref(), Some("180° SBS"));
        assert_eq!(r.resume, Some(12.5));
        assert_eq!(r.duration, Some(600.0));
        assert_eq!(
            to_item_query(&LibraryQuery {
                q: Some("  ".into()),
                ..Default::default()
            })
            .text,
            None
        );
    }

    #[test]
    fn remote_service_starts_and_forwards() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_app, link) = fp_remote::link(8);
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut svc = RemoteService::new(link, Arc::new(LibraryBridge::new(lib_with_items())), tx);
        let mut cfg = RemoteConfig::default();
        cfg.http.enabled = true;
        cfg.http.port = 0;
        cfg.http.bind = "127.0.0.1".parse().unwrap();
        rt.block_on(svc.apply(&cfg));
        match rx.try_recv().unwrap() {
            ServiceEvent::RemoteStarted { token, .. } => {
                assert!(token.is_some_and(|t| !t.is_empty()))
            }
            other => panic!("{other:?}"),
        }
        assert!(svc.http_addr().is_some());
        rt.block_on(svc.apply(&RemoteConfig::default()));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServiceEvent::RemoteStarted { token: None, .. }
        ));
        assert!(svc.http_addr().is_none());
    }
}
