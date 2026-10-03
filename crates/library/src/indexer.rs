//! Library indexer: scans a [`Source`] recursively, upserts items, detects
//! moved files by content hash, drops missing ones, attaches funscripts,
//! and (via an injected [`Thumbnailer`]) generates thumbnails, preview
//! sprite sheets and probe data in the background.
//!
//! The thumbnailer is a trait so this crate doesn't depend on the video
//! stack; the app implements it on top of fp-video's batch decoder.

use crate::db::Library;
use crate::error::Result;
use crate::funscript;
use crate::hash;
use crate::model::NewItem;
use async_trait::async_trait;
use fp_core::{MediaInfo, MediaTime, Projection, StereoMode};
use fp_sources::{Entry, Source, SourceKind};
use futures::StreamExt;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// Preview sprite sheet layout requested from the thumbnailer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpriteSpec {
    pub columns: u32,
    pub rows: u32,
    /// Tile width in pixels; height follows the (per-eye) aspect ratio.
    pub tile_width: u32,
}

impl Default for SpriteSpec {
    fn default() -> Self {
        SpriteSpec {
            columns: 10,
            rows: 10,
            tile_width: 192,
        }
    }
}

/// What a generated sprite sheet contains (stored per item for scrubbing).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpriteInfo {
    pub columns: u32,
    pub rows: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    /// Seconds of video between consecutive tiles.
    pub interval_secs: f64,
}

impl SpriteInfo {
    /// Tile index and pixel rect `(x, y, w, h)` for a timeline position.
    pub fn tile_for(&self, t: MediaTime) -> (u32, [u32; 4]) {
        let n = self.columns * self.rows;
        let idx = if self.interval_secs > 0.0 {
            ((t.as_secs_f64() / self.interval_secs).floor().max(0.0) as u32)
                .min(n.saturating_sub(1))
        } else {
            0
        };
        let (c, r) = (idx % self.columns.max(1), idx / self.columns.max(1));
        (
            idx,
            [
                c * self.tile_width,
                r * self.tile_height,
                self.tile_width,
                self.tile_height,
            ],
        )
    }
}

/// One thumbnail/probe request.
#[derive(Clone)]
pub struct ThumbnailJob {
    pub item_id: i64,
    pub uri: String,
    /// The source to open `uri` through.
    pub source: Arc<dyn Source>,
    /// Where to write the poster JPEG.
    pub thumbnail_path: PathBuf,
    /// Where to write the sprite sheet JPEG.
    pub sprite_path: PathBuf,
    pub sprite: SpriteSpec,
    pub duration_hint: Option<MediaTime>,
    /// Detected layout, so the thumbnailer can crop one eye / unwarp.
    pub projection: Projection,
    pub stereo: StereoMode,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThumbnailOutput {
    /// Probe results (duration, codec, size, signalled projection).
    pub media_info: Option<MediaInfo>,
    pub thumbnail_written: bool,
    pub sprite: Option<SpriteInfo>,
}

pub type ThumbnailError = Box<dyn std::error::Error + Send + Sync>;

/// Implemented by the app on top of the video decoder.
#[async_trait]
pub trait Thumbnailer: Send + Sync {
    async fn generate(
        &self,
        job: ThumbnailJob,
    ) -> std::result::Result<ThumbnailOutput, ThumbnailError>;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexReport {
    pub added: usize,
    pub updated: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// `(uri, message)` for files that couldn't be read/hashed.
    pub errors: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexProgress {
    Listing {
        source_id: String,
    },
    Listed {
        source_id: String,
        videos: usize,
    },
    Indexed {
        done: usize,
        total: usize,
    },
    Finished {
        source_id: String,
        report: IndexReport,
    },
    Thumbnail {
        done: usize,
        total: usize,
        item_id: i64,
        ok: bool,
    },
}

/// Probe states stored in `items.probed`.
const PROBE_OK: i64 = 1;
const PROBE_FAILED: i64 = 2;

pub struct Indexer {
    lib: Arc<Library>,
    cache_dir: PathBuf,
    thumbnailer: Option<Arc<dyn Thumbnailer>>,
    concurrency: usize,
    hash_concurrency: usize,
    max_depth: usize,
    sprite: SpriteSpec,
    deovr: Option<fp_sources::deovr::DeoVrClient>,
    progress: Option<UnboundedSender<IndexProgress>>,
}

impl Indexer {
    /// `cache_dir` receives thumbnails and sprites (e.g.
    /// `$XDG_CACHE_HOME/frameplayer/thumbs`).
    pub fn new(lib: Arc<Library>, cache_dir: impl Into<PathBuf>) -> Self {
        Indexer {
            lib,
            cache_dir: cache_dir.into(),
            thumbnailer: None,
            concurrency: 2,
            hash_concurrency: 8,
            max_depth: 16,
            sprite: SpriteSpec::default(),
            deovr: None,
            progress: None,
        }
    }

    pub fn with_thumbnailer(mut self, t: Arc<dyn Thumbnailer>) -> Self {
        self.thumbnailer = Some(t);
        self
    }

    /// Max simultaneous thumbnail jobs (decoding is heavy; default 2).
    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }

    /// Max simultaneous hash reads during scans (default 8).
    pub fn with_hash_concurrency(mut self, n: usize) -> Self {
        self.hash_concurrency = n.max(1);
        self
    }

    pub fn with_max_depth(mut self, d: usize) -> Self {
        self.max_depth = d;
        self
    }

    pub fn with_sprite_spec(mut self, s: SpriteSpec) -> Self {
        self.sprite = s;
        self
    }

    /// Fetch per-video DeoVR documents for `deovr+` items to fill in
    /// projection, tags and scripts.
    pub fn with_deovr_client(mut self, c: fp_sources::deovr::DeoVrClient) -> Self {
        self.deovr = Some(c);
        self
    }

    pub fn with_progress(mut self, tx: UnboundedSender<IndexProgress>) -> Self {
        self.progress = Some(tx);
        self
    }

    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }

    fn emit(&self, p: IndexProgress) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(p);
        }
    }

    /// Scan `source` and reconcile the items belonging to `source_id`.
    pub async fn index_source(
        &self,
        source_id: &str,
        source: Arc<dyn Source>,
    ) -> Result<IndexReport> {
        self.emit(IndexProgress::Listing {
            source_id: source_id.into(),
        });
        let root = source.root_uri();
        let listing = fp_sources::walk(source.as_ref(), "", self.max_depth).await?;
        let by_dir = funscript::group_by_dir(&listing);
        let videos: Vec<Entry> = {
            let mut seen = HashSet::new();
            listing
                .iter()
                .filter(|e| e.is_video() && seen.insert(e.uri.clone()))
                .cloned()
                .collect()
        };
        self.emit(IndexProgress::Listed {
            source_id: source_id.into(),
            videos: videos.len(),
        });

        let existing: HashMap<String, crate::items::ItemKey> = self
            .lib
            .item_keys(source_id)?
            .into_iter()
            .map(|k| (k.uri.clone(), k))
            .collect();
        let listed: HashSet<&str> = videos.iter().map(|e| e.uri.as_str()).collect();
        let is_feed = source.kind() == SourceKind::DeoVr;

        // Phase 1: decide which entries need hashing, hash them concurrently.
        let needs_hash = |e: &Entry| -> bool {
            if is_feed || e.uri.starts_with("deovr+") {
                return false;
            }
            match existing.get(&e.uri) {
                Some(k) => {
                    !(k.size.is_some()
                        && k.size == e.size
                        && k.mtime == e.mtime
                        && k.content_hash.is_some())
                }
                None => true,
            }
        };
        // Collected up front: a filter-closure iterator held across the
        // `.await` below makes this future impossible to `tokio::spawn`.
        let to_hash: Vec<Entry> = videos.iter().filter(|e| needs_hash(e)).cloned().collect();
        let hashes: HashMap<String, std::result::Result<String, String>> =
            futures::stream::iter(to_hash)
                .map(|e| {
                    let source = source.clone();
                    async move {
                        let r = async {
                            let ra = source.open(&e.uri).await?;
                            hash::content_hash(ra.as_ref()).await
                        }
                        .await
                        .map_err(|err| err.to_string());
                        (e.uri, r)
                    }
                })
                .buffer_unordered(self.hash_concurrency)
                .collect()
                .await;

        // Phase 2: sequential DB reconciliation.
        let mut report = IndexReport::default();
        let mut consumed: HashSet<String> = HashSet::new();
        let mut feed_items: Vec<(i64, String)> = Vec::new();
        let total = videos.len();
        for (done, e) in videos.iter().enumerate() {
            let hash = match hashes.get(&e.uri) {
                Some(Ok(h)) => Some(h.clone()),
                Some(Err(msg)) => {
                    report.errors.push((e.uri.clone(), msg.clone()));
                    None
                }
                None => existing.get(&e.uri).and_then(|k| k.content_hash.clone()),
            };
            let ni = NewItem {
                source_id: Some(source_id.to_string()),
                uri: e.uri.clone(),
                path: if is_feed {
                    e.name.clone()
                } else {
                    display_path(&root, &e.uri)
                },
                title: if is_feed {
                    e.name.clone()
                } else {
                    e.stem().to_string()
                },
                size: e.size,
                mtime: e.mtime,
                content_hash: hash.clone(),
                duration: e.duration_secs.map(MediaTime::from_secs_f64),
                remote_thumbnail: e.thumbnail.clone(),
            };
            let id = if let Some(k) = existing.get(&e.uri) {
                if hashes.contains_key(&e.uri) {
                    report.updated += 1;
                } else {
                    report.unchanged += 1;
                }
                if hashes.contains_key(&e.uri) || is_feed {
                    self.lib.upsert_item(&ni)?
                } else {
                    k.id
                }
            } else if let Some(old) =
                self.find_moved(hash.as_deref(), &listed, &consumed, source_id)?
            {
                consumed.insert(old.1.clone());
                self.lib.move_item(old.0, &ni)?;
                report.moved += 1;
                old.0
            } else {
                report.added += 1;
                self.lib.upsert_item(&ni)?
            };

            if e.uri.starts_with("deovr+") {
                feed_items.push((id, e.uri.clone()));
            } else {
                let scripts = funscript::discover_in_listing(e, &by_dir);
                let had: bool = self.lib.conn().query_row(
                    "SELECT EXISTS(SELECT 1 FROM scripts WHERE item_id = ?1)",
                    [id],
                    |r| r.get(0),
                )?;
                if !scripts.is_empty() || had {
                    self.lib.set_scripts(id, &scripts)?;
                }
            }
            self.emit(IndexProgress::Indexed {
                done: done + 1,
                total,
            });
        }

        // Phase 3: remove items that vanished (and weren't moved).
        for (uri, k) in &existing {
            if !listed.contains(uri.as_str()) && !consumed.contains(uri) {
                self.lib.delete_item(k.id)?;
                report.removed += 1;
            }
        }

        // Phase 4: DeoVR per-video metadata for new or unprobed feed items.
        if let Some(client) = &self.deovr {
            let todo: Vec<(i64, String)> = feed_items
                .into_iter()
                .filter(|(id, _)| {
                    self.lib
                        .item(*id)
                        .ok()
                        .flatten()
                        .is_some_and(|it| !it.probed)
                })
                .collect();
            let results: Vec<_> = futures::stream::iter(todo)
                .map(|(id, uri)| {
                    let c = client.clone();
                    async move { (id, uri.clone(), c.video(&uri).await) }
                })
                .buffer_unordered(self.hash_concurrency)
                .collect()
                .await;
            for (id, uri, r) in results {
                match r {
                    Ok(v) => {
                        self.lib.apply_deovr_video(id, &v)?;
                        self.lib.conn().execute(
                            "UPDATE items SET probed = ?2 WHERE id = ?1",
                            params![id, PROBE_OK],
                        )?;
                    }
                    Err(e) => report.errors.push((uri, e.to_string())),
                }
            }
        }

        self.lib.set_source_scanned(source_id)?;
        self.emit(IndexProgress::Finished {
            source_id: source_id.into(),
            report: report.clone(),
        });
        Ok(report)
    }

    /// An item of this source with the same hash whose URI disappeared.
    fn find_moved(
        &self,
        hash: Option<&str>,
        listed: &HashSet<&str>,
        consumed: &HashSet<String>,
        source_id: &str,
    ) -> Result<Option<(i64, String)>> {
        let Some(h) = hash else { return Ok(None) };
        Ok(self
            .lib
            .items_by_hash(h)?
            .into_iter()
            .find(|it| {
                it.source_id.as_deref() == Some(source_id)
                    && !listed.contains(it.uri.as_str())
                    && !consumed.contains(&it.uri)
            })
            .map(|it| (it.id, it.uri)))
    }

    /// Generate thumbnails/sprites and probe data for items of `source_id`
    /// that haven't been processed (all items if `force`). Returns how many
    /// succeeded. Requires a thumbnailer.
    pub async fn generate_thumbnails(
        &self,
        source_id: &str,
        source: Arc<dyn Source>,
        force: bool,
    ) -> Result<usize> {
        let Some(thumbnailer) = self.thumbnailer.clone() else {
            return Ok(0);
        };
        let items: Vec<crate::Item> = if force {
            self.lib.select_items(
                "WHERE i.source_id = ?1 AND i.uri NOT LIKE 'deovr+%'",
                [source_id],
            )?
        } else {
            self.lib.select_items(
                "WHERE i.source_id = ?1 AND i.probed = 0 AND i.uri NOT LIKE 'deovr+%'",
                [source_id],
            )?
        };
        std::fs::create_dir_all(&self.cache_dir)?;
        let total = items.len();
        let jobs = items.into_iter().map(|it| {
            let key = it
                .content_hash
                .clone()
                .unwrap_or_else(|| format!("item-{}", it.id));
            ThumbnailJob {
                item_id: it.id,
                uri: it.uri.clone(),
                source: source.clone(),
                thumbnail_path: self.cache_dir.join(format!("{key}.jpg")),
                sprite_path: self.cache_dir.join(format!("{key}.sprite.jpg")),
                sprite: self.sprite,
                duration_hint: it.duration,
                projection: it.projection.clone().unwrap_or_default(),
                stereo: it.stereo.unwrap_or_default(),
            }
        });
        let mut results = futures::stream::iter(jobs)
            .map(|job| {
                let t = thumbnailer.clone();
                async move {
                    let j = job.clone();
                    (j, t.generate(job).await)
                }
            })
            .buffer_unordered(self.concurrency);
        let mut ok = 0;
        let mut done = 0;
        while let Some((job, r)) = results.next().await {
            done += 1;
            let success = match r {
                Ok(out) => {
                    if let Some(info) = &out.media_info {
                        self.lib.apply_media_info(job.item_id, info)?;
                    }
                    let thumb = out
                        .thumbnail_written
                        .then(|| job.thumbnail_path.to_string_lossy().into_owned());
                    let sprite = out
                        .sprite
                        .map(|_| job.sprite_path.to_string_lossy().into_owned());
                    self.lib.set_thumbnails(
                        job.item_id,
                        thumb.as_deref(),
                        sprite.as_deref(),
                        out.sprite.as_ref(),
                    )?;
                    self.lib.conn().execute(
                        "UPDATE items SET probed = ?2 WHERE id = ?1",
                        params![job.item_id, PROBE_OK],
                    )?;
                    ok += 1;
                    true
                }
                Err(e) => {
                    tracing::warn!("thumbnail failed for {}: {e}", job.uri);
                    self.lib.conn().execute(
                        "UPDATE items SET probed = ?2 WHERE id = ?1",
                        params![job.item_id, PROBE_FAILED],
                    )?;
                    false
                }
            };
            self.emit(IndexProgress::Thumbnail {
                done,
                total,
                item_id: job.item_id,
                ok: success,
            });
        }
        Ok(ok)
    }

    /// Run [`generate_thumbnails`](Self::generate_thumbnails) as a background task.
    pub fn spawn_thumbnails(
        self: &Arc<Self>,
        source_id: String,
        source: Arc<dyn Source>,
    ) -> tokio::task::JoinHandle<Result<usize>> {
        let me = self.clone();
        tokio::spawn(async move { me.generate_thumbnails(&source_id, source, false).await })
    }
}

/// Human-readable path of `uri` relative to the source root.
pub fn display_path(root: &str, uri: &str) -> String {
    let rel = uri
        .strip_prefix(root.trim_end_matches('/'))
        .map(|r| r.trim_start_matches('/'))
        .filter(|r| !r.is_empty())
        .unwrap_or(uri);
    percent_encoding::percent_decode_str(rel)
        .decode_utf8_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::{Codec, VideoTrackInfo};
    use fp_sources::local::LocalSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeThumbs {
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    #[async_trait]
    impl Thumbnailer for FakeThumbs {
        async fn generate(
            &self,
            job: ThumbnailJob,
        ) -> std::result::Result<ThumbnailOutput, ThumbnailError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if job.uri.contains("broken") {
                return Err("cannot decode".into());
            }
            // Read through the source like a real thumbnailer would.
            let ra = job.source.open(&job.uri).await?;
            assert!(ra.size().is_some());
            std::fs::write(&job.thumbnail_path, b"jpg")?;
            std::fs::write(&job.sprite_path, b"sprite")?;
            Ok(ThumbnailOutput {
                media_info: Some(MediaInfo {
                    duration: Some(MediaTime::from_millis(60_000)),
                    video: vec![VideoTrackInfo {
                        index: 0,
                        codec: Codec::Av1,
                        width: 3840,
                        height: 1920,
                        fps: 30.0,
                        bit_depth: 8,
                        transfer: Default::default(),
                        signalled_projection: None,
                        signalled_stereo: None,
                    }],
                    ..Default::default()
                }),
                thumbnail_written: true,
                sprite: Some(SpriteInfo {
                    columns: 10,
                    rows: 10,
                    tile_width: 192,
                    tile_height: 96,
                    interval_secs: 0.6,
                }),
            })
        }
    }

    fn write(p: &std::path::Path, data: &[u8]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    fn content(seed: u8, n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed))
            .collect()
    }

    #[tokio::test]
    async fn scan_move_remove_and_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("videos");
        write(&root.join("a_180_LR.mp4"), &content(1, 200_000));
        write(&root.join("sub/b.mkv"), &content(2, 1000));
        write(&root.join("sub/b.funscript"), b"{}");
        write(&root.join("sub/b.twist.funscript"), b"{}");
        write(&root.join("c.mp4"), &content(3, 5000));
        write(&root.join("Interactive/c.funscript"), b"{}");
        write(&root.join("notes.txt"), b"x");

        let lib = Arc::new(Library::open_in_memory().unwrap());
        let cfg = fp_sources::SourceConfig {
            id: "local".into(),
            name: "Local".into(),
            kind: SourceKind::Local,
            uri: fp_sources::local::path_to_uri(&root),
            pinned_host_key: None,
        };
        lib.upsert_source(&cfg).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let idx = Indexer::new(lib.clone(), dir.path().join("cache")).with_progress(tx);
        let src: Arc<dyn Source> = Arc::new(LocalSource::new(&root));

        let r = idx.index_source("local", src.clone()).await.unwrap();
        assert_eq!(
            (r.added, r.updated, r.moved, r.removed, r.unchanged),
            (3, 0, 0, 0, 0),
            "{r:?}"
        );
        assert!(r.errors.is_empty());
        let items = lib.items(Some("local")).unwrap();
        assert_eq!(items.len(), 3);
        let a = lib
            .item_by_uri(&fp_sources::local::path_to_uri(&root.join("a_180_LR.mp4")))
            .unwrap()
            .unwrap();
        assert_eq!(a.title, "a_180_LR");
        assert_eq!(a.path, "a_180_LR.mp4");
        assert_eq!(a.projection, Some(Projection::EQUIRECT_180));
        assert_eq!(
            a.content_hash.as_deref(),
            Some(
                hash::hash_file(&root.join("a_180_LR.mp4"))
                    .unwrap()
                    .as_str()
            )
        );
        let b = lib
            .item_by_uri(&fp_sources::local::path_to_uri(&root.join("sub/b.mkv")))
            .unwrap()
            .unwrap();
        assert_eq!(b.path, "sub/b.mkv");
        assert_eq!(
            lib.scripts(b.id)
                .unwrap()
                .iter()
                .map(|s| s.axis.as_str())
                .collect::<Vec<_>>(),
            ["main", "twist"]
        );
        let c = lib
            .item_by_uri(&fp_sources::local::path_to_uri(&root.join("c.mp4")))
            .unwrap()
            .unwrap();
        assert!(lib.scripts(c.id).unwrap()[0].uri.contains("/Interactive/"));
        let mut saw_finished = false;
        while let Ok(p) = rx.try_recv() {
            saw_finished |= matches!(p, IndexProgress::Finished { .. });
        }
        assert!(saw_finished);

        // User data on `a`, then move it; rescan keeps the same item.
        lib.set_favourite(a.id, true).unwrap();
        lib.add_bookmark(a.id, MediaTime::from_millis(5000), "spot")
            .unwrap();
        std::fs::rename(
            root.join("a_180_LR.mp4"),
            root.join("sub/a_moved_180_LR.mp4"),
        )
        .unwrap();
        std::fs::remove_file(root.join("c.mp4")).unwrap();
        let r = idx.index_source("local", src.clone()).await.unwrap();
        assert_eq!(
            (r.added, r.moved, r.removed, r.unchanged),
            (0, 1, 1, 1),
            "{r:?}"
        );
        let moved = lib.item(a.id).unwrap().unwrap();
        assert_eq!(moved.path, "sub/a_moved_180_LR.mp4");
        assert!(moved.favourite);
        assert_eq!(lib.bookmarks(a.id).unwrap().len(), 1);
        assert!(lib.item(c.id).unwrap().is_none());

        // Modified file → updated; scripts removed → cleared.
        write(&root.join("sub/b.mkv"), &content(9, 3000));
        std::fs::remove_file(root.join("sub/b.funscript")).unwrap();
        std::fs::remove_file(root.join("sub/b.twist.funscript")).unwrap();
        let r = idx.index_source("local", src.clone()).await.unwrap();
        assert_eq!((r.updated, r.unchanged), (1, 1), "{r:?}");
        assert!(lib.scripts(b.id).unwrap().is_empty());
        assert_eq!(lib.item(b.id).unwrap().unwrap().size, Some(3000));
        assert!(lib.source_last_scan("local").unwrap().is_some());
    }

    #[tokio::test]
    async fn thumbnails_with_concurrency_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("v");
        for i in 0..6 {
            write(
                &root.join(format!("clip{i}.mp4")),
                &content(i as u8 + 1, 100),
            );
        }
        write(&root.join("broken.mp4"), b"zz");
        let lib = Arc::new(Library::open_in_memory().unwrap());
        lib.upsert_source(&fp_sources::SourceConfig {
            id: "s".into(),
            name: "s".into(),
            kind: SourceKind::Local,
            uri: fp_sources::local::path_to_uri(&root),
            pinned_host_key: None,
        })
        .unwrap();
        let fake = Arc::new(FakeThumbs {
            calls: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let idx = Arc::new(
            Indexer::new(lib.clone(), dir.path().join("cache"))
                .with_thumbnailer(fake.clone())
                .with_concurrency(2),
        );
        let src: Arc<dyn Source> = Arc::new(LocalSource::new(&root));
        idx.index_source("s", src.clone()).await.unwrap();
        let ok = idx
            .spawn_thumbnails("s".into(), src.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ok, 6);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 7);
        assert!(fake.max_in_flight.load(Ordering::SeqCst) <= 2);
        let it = lib
            .item_by_uri(&fp_sources::local::path_to_uri(&root.join("clip0.mp4")))
            .unwrap()
            .unwrap();
        assert!(it.probed);
        assert_eq!(it.codec, Some(Codec::Av1));
        assert_eq!(it.duration, Some(MediaTime::from_millis(60_000)));
        assert!(std::path::Path::new(it.thumbnail_path.as_ref().unwrap()).exists());
        let sprite = lib.sprite_info(it.id).unwrap().unwrap();
        assert_eq!(
            sprite.tile_for(MediaTime::from_millis(1300)),
            (2, [384, 0, 192, 96])
        );
        assert_eq!(sprite.tile_for(MediaTime::from_millis(999_999)).0, 99);
        // Second run: nothing left to do (failures aren't retried).
        assert_eq!(
            idx.generate_thumbnails("s", src.clone(), false)
                .await
                .unwrap(),
            0
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 7);
        // Forced run redoes everything.
        assert_eq!(idx.generate_thumbnails("s", src, true).await.unwrap(), 6);
    }

    #[test]
    fn display_paths() {
        assert_eq!(
            display_path("file:///v", "file:///v/a%20b/c.mp4"),
            "a b/c.mp4"
        );
        assert_eq!(display_path("smb://nas/s/", "smb://nas/s/x.mp4"), "x.mp4");
        assert_eq!(
            display_path("file:///other", "http://h/x.mp4"),
            "http://h/x.mp4"
        );
    }
}
