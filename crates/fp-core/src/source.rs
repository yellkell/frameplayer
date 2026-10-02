//! The contract between media sources (local, SMB, WebDAV, HTTP, DLNA,
//! DeoVR feeds) and the rest of FramePlayer.

use serde::{Deserialize, Serialize};
use std::io;

/// Random-access read of a remote or local file. The media pipeline wraps
/// this in an FFmpeg I/O context, so FFmpeg never touches the network.
///
/// Implementations must be safe to call from the demux thread while another
/// thread holds the same `Arc`.
pub trait ByteSource: Send + Sync {
    /// Total size in bytes, when known. Seeking needs it for most containers.
    fn size(&self) -> Option<u64>;
    /// Reads up to `buf.len()` bytes at `offset`. Returns 0 at end of file.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Human-readable description for logs and errors.
    fn describe(&self) -> String;
}

/// A `ByteSource` over a local file.
pub struct FileSource {
    file: std::fs::File,
    size: u64,
    path: String,
}

impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<FileSource> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        Ok(FileSource {
            file,
            size,
            path: path.display().to_string(),
        })
    }
}

impl ByteSource for FileSource {
    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buf, offset)
    }
    fn describe(&self) -> String {
        self.path.clone()
    }
}

/// An in-memory `ByteSource`, for tests.
pub struct MemorySource(pub Vec<u8>);

impl ByteSource for MemorySource {
    fn size(&self) -> Option<u64> {
        Some(self.0.len() as u64)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = (offset as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        Ok(n)
    }
    fn describe(&self) -> String {
        format!("memory ({} bytes)", self.0.len())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    Video,
    /// Haptic script (`.funscript`), subtitle or other side file.
    Other,
}

/// One item when browsing a source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Location understood by the source that produced it: a path for local
    /// files, a URL otherwise (`smb://`, `http(s)://`, `webdav(s)://`, ...).
    pub location: String,
    pub kind: EntryKind,
    #[serde(default)]
    pub size: Option<u64>,
    /// Unix seconds.
    #[serde(default)]
    pub modified: Option<i64>,
    #[serde(default)]
    pub duration: Option<f64>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Format the source declares (DeoVR feeds, DLNA metadata).
    #[serde(default)]
    pub format: Option<crate::format::VideoFormat>,
    /// Haptic script locations that belong to this video.
    #[serde(default)]
    pub scripts: Vec<String>,
    /// Subtitle file locations that belong to this video.
    #[serde(default)]
    pub subtitles: Vec<String>,
    /// Named timestamps (chapters) the source provides.
    #[serde(default)]
    pub markers: Vec<(f64, String)>,
}

impl Entry {
    pub fn new(name: impl Into<String>, location: impl Into<String>, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            location: location.into(),
            kind,
            size: None,
            modified: None,
            duration: None,
            thumbnail_url: None,
            format: None,
            scripts: Vec::new(),
            subtitles: Vec::new(),
            markers: Vec::new(),
        }
    }
}

/// File extensions treated as video.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "ts", "m2ts", "mts", "flv", "wmv", "mpg", "mpeg",
    "ogv",
];

/// File extensions treated as subtitles.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "ass", "ssa", "vtt"];

pub fn is_video_name(name: &str) -> bool {
    ext_in(name, VIDEO_EXTENSIONS)
}

pub fn is_subtitle_name(name: &str) -> bool {
    ext_in(name, SUBTITLE_EXTENSIONS)
}

pub fn is_script_name(name: &str) -> bool {
    ext_in(name, &["funscript"])
}

fn ext_in(name: &str, list: &[&str]) -> bool {
    name.rsplit_once('.')
        .map(|(_, e)| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_source_reads() {
        let s = MemorySource(b"hello world".to_vec());
        let mut b = [0u8; 5];
        assert_eq!(s.read_at(6, &mut b).unwrap(), 5);
        assert_eq!(&b, b"world");
        assert_eq!(s.read_at(11, &mut b).unwrap(), 0);
        assert_eq!(s.read_at(99, &mut b).unwrap(), 0);
    }

    #[test]
    fn classifies_names() {
        assert!(is_video_name("a.MP4"));
        assert!(is_video_name("x.y.mkv"));
        assert!(!is_video_name("a.funscript"));
        assert!(is_script_name("a.funscript"));
        assert!(is_subtitle_name("a.en.srt"));
        assert!(!is_video_name("mp4"));
    }
}
