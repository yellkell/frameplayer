//! Media sources for FramePlayer: browsing and random-access reading of
//! local and network media.
//!
//! Every source implements [`Source`]: it lists folders as
//! [`fp_core::Entry`] values and opens files as [`fp_core::ByteSource`]s.
//! The media pipeline wraps a `ByteSource` in an FFmpeg I/O context, so
//! FFmpeg never does network I/O itself; network readers here are tuned for
//! demuxing (block cache, read-ahead, connection reuse, retries).
//!
//! | Source | Module | Locations |
//! |---|---|---|
//! | Local folder | [`local`] | absolute paths |
//! | HTTP(S) autoindex | [`httpdir`] | `http(s)://` URLs |
//! | WebDAV | [`webdav`] | `http(s)://` URLs |
//! | DLNA/UPnP | [`dlna`] | `dlna:<object id>` folders, `http://` media |
//! | DeoVR / HereSphere feeds | [`feed`] | feed and scene URLs |
//! | SMB2/3 | [`smb`] | `smb://host/share/path` |
//!
//! Sources are built from a serializable [`SourceConfig`] with [`build`];
//! [`save_configs`] / [`load_configs`] persist them (credentials included)
//! in a 0600 JSON file. Blocking I/O and plain threads only.

#![warn(missing_docs)]

pub mod cache;
pub mod config;
pub mod dlna;
pub mod error;
pub mod feed;
pub mod http;
pub mod httpdir;
pub mod local;
pub mod sidecar;
pub mod smb;
pub mod timeutil;
pub mod urlutil;
pub mod webdav;
mod xml;

pub use cache::{CacheOptions, CacheStats};
pub use config::{
    Credentials, DlnaConfig, FeedConfig, HttpConfig, LocalConfig, SmbConfig, SourceConfig,
    SourceKind, default_config_path, load_configs, save_configs,
};
pub use dlna::{DlnaDevice, DlnaSource, SsdpResponse, discover};
pub use error::{Error, Result};
pub use feed::{DeoVrSource, HereSphereSource, MediaSource, SceneInfo};
pub use http::{HttpClient, HttpFile, HttpOptions};
pub use httpdir::HttpDirSource;
pub use local::LocalSource;
pub use sidecar::{ScriptFile, Sidecars, SubtitleFile, match_sidecars};
pub use smb::SmbSource;
pub use webdav::WebDavSource;

use fp_core::source::{ByteSource, Entry, EntryKind, is_video_name};
use std::sync::Arc;

/// A place media can be browsed and read from.
///
/// Locations are opaque strings produced by the source itself (in
/// [`Entry::location`]); pass them back unchanged. Implementations block and
/// may be called from any thread.
pub trait Source: Send + Sync {
    /// Stable identifier (from the configuration).
    fn id(&self) -> &str;

    /// Display name.
    fn name(&self) -> &str;

    /// What kind of source this is.
    fn kind(&self) -> SourceKind;

    /// One-line description for logs and the UI, without secrets.
    fn describe(&self) -> String;

    /// Lists a folder. `None` lists the root of the source.
    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>>;

    /// Opens a file for random access.
    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>>;

    /// Folder containing `location`, when the source can list it.
    fn parent(&self, _location: &str) -> Option<String> {
        None
    }

    /// Completes an entry with details that are too expensive to fetch for a
    /// whole listing (feeds: format, scripts, subtitles, markers). Other
    /// sources return the entry unchanged.
    fn details(&self, entry: &Entry) -> Result<Entry> {
        Ok(entry.clone())
    }

    /// Finds the haptic scripts (`stem.funscript`, `stem.<axis>.funscript`)
    /// and subtitles (`stem.srt`, `stem.<lang>.srt`, ...) belonging to a
    /// video: those the entry already carries, plus matching files in the
    /// same folder when the source can list it.
    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        sidecars_from_folder(self, video)
    }
}

/// Default [`Source::sidecars`]: the entry's own scripts and subtitles plus
/// matching files listed in [`Source::parent`] of the video.
pub fn sidecars_from_folder<S: Source + ?Sized>(source: &S, video: &Entry) -> Result<Sidecars> {
    let mut out = Sidecars::from_entry(video);
    if let Some(parent) = source.parent(&video.location) {
        let siblings = source.list(Some(&parent))?;
        let mut file_name = urlutil::last_segment(&video.location);
        if file_name.is_empty() {
            file_name = video.name.clone();
        }
        out.merge(match_sidecars(&file_name, &siblings));
    }
    Ok(out)
}

/// Builds the source described by a configuration. No network I/O happens
/// until the source is used.
pub fn build(config: &SourceConfig) -> Result<Box<dyn Source>> {
    Ok(match config {
        SourceConfig::Local(c) => Box::new(LocalSource::new(c)),
        SourceConfig::Http(c) => Box::new(HttpDirSource::new(c)?),
        SourceConfig::WebDav(c) => Box::new(WebDavSource::new(c)?),
        SourceConfig::Dlna(c) => Box::new(DlnaSource::new(c)?),
        SourceConfig::DeoVr(c) => Box::new(DeoVrSource::new(c)?),
        SourceConfig::HereSphere(c) => Box::new(HereSphereSource::new(c)?),
        SourceConfig::Smb(c) => Box::new(SmbSource::new(c)?),
    })
}

/// Builds the entry for a file: `Video` (with the format detected from its
/// name) when the extension is a video one, `Other` otherwise.
pub fn file_entry(
    name: impl Into<String>,
    location: impl Into<String>,
    size: Option<u64>,
    modified: Option<i64>,
) -> Entry {
    let name = name.into();
    let kind = if is_video_name(&name) {
        EntryKind::Video
    } else {
        EntryKind::Other
    };
    let mut e = Entry::new(name, location, kind);
    e.size = size;
    e.modified = modified;
    if kind == EntryKind::Video {
        e.format = fp_core::format::detect_from_name(&e.name);
    }
    e
}

/// Builds the entry for a folder.
pub fn dir_entry(
    name: impl Into<String>,
    location: impl Into<String>,
    modified: Option<i64>,
) -> Entry {
    let mut e = Entry::new(name, location, EntryKind::Directory);
    e.modified = modified;
    e
}

/// Sorts folders first, then names in natural order (`ep2` before `ep10`).
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        let da = a.kind != EntryKind::Directory;
        let db = b.kind != EntryKind::Directory;
        da.cmp(&db)
            .then_with(|| urlutil::natural_cmp(&a.name, &b.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::{Projection, StereoLayout};

    #[test]
    fn file_entries_classify_and_detect() {
        let e = file_entry("Scene_180_LR.mp4", "/v/Scene_180_LR.mp4", Some(5), Some(7));
        assert_eq!(e.kind, EntryKind::Video);
        let f = e.format.unwrap();
        assert_eq!(f.projection, Projection::EQUIRECT_180);
        assert_eq!(f.stereo, StereoLayout::SideBySide);
        assert_eq!((e.size, e.modified), (Some(5), Some(7)));
        let e = file_entry("Movie.mkv", "/v/Movie.mkv", None, None);
        assert_eq!(e.kind, EntryKind::Video);
        assert!(e.format.is_none());
        let e = file_entry("Scene_180_LR.funscript", "/v/x", None, None);
        assert_eq!(e.kind, EntryKind::Other);
        assert!(e.format.is_none());
    }

    #[test]
    fn sorting() {
        let mut v = vec![
            file_entry("b10.mp4", "x", None, None),
            dir_entry("zeta", "z", None),
            file_entry("B2.mp4", "x", None, None),
            dir_entry("Alpha", "a", None),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zeta", "B2.mp4", "b10.mp4"]);
    }
}
