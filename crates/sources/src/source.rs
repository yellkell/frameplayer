//! The unified [`Source`] / [`RandomAccess`] traits and directory entries.

use crate::config::SourceKind;
use crate::error::Result;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// File extensions treated as video, lowercase without the dot.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts", "mts", "avi", "wmv", "flv", "mpg", "mpeg",
    "3gp", "ogv",
];

/// True if `name` (a file name, path or URI) has a video extension.
pub fn is_video_name(name: &str) -> bool {
    extension(name).is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
}

/// True if `name` is a funscript.
pub fn is_script_name(name: &str) -> bool {
    extension(name).is_some_and(|e| e == "funscript")
}

fn extension(name: &str) -> Option<String> {
    let name = name.split(['?', '#']).next().unwrap_or(name);
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let dot = file.rfind('.')?;
    (dot > 0).then(|| file[dot + 1..].to_ascii_lowercase())
}

/// One row of a directory listing.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Entry {
    /// Display name (file name, or title for feed items).
    pub name: String,
    /// URI that [`Source::open`] (files) or [`Source::list`] (dirs) accepts.
    pub uri: String,
    pub is_dir: bool,
    /// Size in bytes when the source reports it.
    pub size: Option<u64>,
    /// Modification time, Unix seconds.
    pub mtime: Option<i64>,
    /// Thumbnail URL for feed/DLNA items.
    pub thumbnail: Option<String>,
    /// Duration in seconds when the source knows it (DLNA, DeoVR).
    pub duration_secs: Option<f64>,
    /// Set by sources that already know the entry is a video (DLNA items,
    /// feed scenes) even when its name has no video extension.
    pub is_media: bool,
}

impl Entry {
    pub fn is_video(&self) -> bool {
        !self.is_dir && (self.is_media || is_video_name(&self.name) || is_video_name(&self.uri))
    }

    /// File stem (name without the final extension).
    pub fn stem(&self) -> &str {
        match self.name.rfind('.') {
            Some(i) if i > 0 => &self.name[..i],
            _ => &self.name,
        }
    }
}

/// An opened file supporting positioned reads.
#[async_trait]
pub trait RandomAccess: Send + Sync {
    /// Read up to `len` bytes at `offset`. Returns fewer bytes only at end of
    /// file; an empty buffer means `offset` is at or past the end.
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes>;

    /// Total size in bytes, if known (live streams and some HTTP servers
    /// don't say).
    fn size(&self) -> Option<u64>;
}

/// A browsable place videos live.
#[async_trait]
pub trait Source: Send + Sync {
    fn kind(&self) -> SourceKind;

    /// URI of the top-level directory; `list(&root_uri())` lists it.
    fn root_uri(&self) -> String;

    /// List the immediate children of `dir` (a URI from [`root_uri`] or a
    /// previous [`Entry::uri`]). An empty string means the root.
    ///
    /// [`root_uri`]: Source::root_uri
    async fn list(&self, dir: &str) -> Result<Vec<Entry>>;

    /// Open a file entry for random access.
    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>>;
}

/// An in-memory file; handy for tests and for already-downloaded data.
#[derive(Debug, Clone)]
pub struct MemoryFile(Bytes);

impl MemoryFile {
    pub fn new(data: impl Into<Bytes>) -> Self {
        MemoryFile(data.into())
    }
}

#[async_trait]
impl RandomAccess for MemoryFile {
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        let n = self.0.len() as u64;
        if offset >= n {
            return Ok(Bytes::new());
        }
        let end = (offset + len as u64).min(n);
        Ok(self.0.slice(offset as usize..end as usize))
    }

    fn size(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
}

/// Read the whole of a (small) file, e.g. a funscript or playlist.
pub async fn read_all(ra: &dyn RandomAccess, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let chunk = ra.read_at(out.len() as u64, 256 * 1024).await?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
        if out.len() > limit {
            return Err(crate::SourceError::Protocol(format!(
                "file larger than {limit} bytes"
            )));
        }
    }
    Ok(out)
}

/// Recursively list every non-directory entry below `dir`, depth-first,
/// up to `max_depth` directory levels. Directory listing errors below the
/// top level are logged and skipped so one unreadable folder doesn't abort
/// a library scan.
pub async fn walk(source: &dyn Source, dir: &str, max_depth: usize) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_string(), 0usize)];
    let mut first = true;
    while let Some((d, depth)) = stack.pop() {
        let entries = match source.list(&d).await {
            Ok(e) => e,
            Err(e) if !first => {
                tracing::warn!("skipping unreadable directory {d}: {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        first = false;
        for e in entries {
            if e.is_dir {
                if depth < max_depth {
                    stack.push((e.uri, depth + 1));
                }
            } else {
                out.push(e);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_extensions() {
        assert!(is_video_name("a/b/Scene_180_LR.MP4"));
        assert!(is_video_name("https://x/y.mkv?token=1"));
        assert!(!is_video_name("notes.txt"));
        assert!(!is_video_name(".mp4"));
        assert!(!is_video_name("folder.mp4/"));
        assert!(is_script_name("x.surge.funscript"));
    }

    #[test]
    fn entry_stem() {
        let e = Entry {
            name: "clip.180.LR.mp4".into(),
            ..Default::default()
        };
        assert_eq!(e.stem(), "clip.180.LR");
    }
}
