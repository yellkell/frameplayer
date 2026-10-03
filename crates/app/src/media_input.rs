//! Turning a URI into the blocking byte stream the demuxers read
//! ([`fp_video::MediaInput`]).
//!
//! * local paths / `file://` → `std::fs::File` behind a read-ahead buffer;
//! * HLS / DASH manifests → the best variant's segment stream, made
//!   seekable within its first megabytes by [`SequentialInput`];
//! * everything else (SMB, WebDAV, HTTP, SFTP, DLNA, DeoVR) → the source's
//!   [`RandomAccess`](fp_sources::RandomAccess) through
//!   [`BlockingReader`] (read-ahead on the tokio runtime, blocking reads on
//!   the playback thread).
//!
//! Opening runs on the services runtime; only the returned reader is used
//! from the (non-tokio) playback thread.

use anyhow::{anyhow, Context, Result};
use fp_sources::{BlockingReader, ReadAheadConfig, Source, SourceConfig};
use fp_video::{BufferedInput, MediaInput};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;

/// A source file as a `MediaInput`.
pub struct SourceInput(pub BlockingReader);

impl Read for SourceInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for SourceInput {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl MediaInput for SourceInput {
    fn size(&self) -> Option<u64> {
        self.0.size()
    }
}

/// How much of the start of a sequential stream stays seekable (container
/// headers are re-read after probing).
pub const HEAD_KEEP: usize = 16 << 20;

/// Seekable view of a forward-only stream: the first [`HEAD_KEEP`] bytes
/// are retained, forward seeks skip data, other backward seeks fail.
pub struct SequentialInput<R: Read + Send> {
    inner: R,
    head: Vec<u8>,
    inner_pos: u64,
    pos: u64,
}

impl<R: Read + Send> SequentialInput<R> {
    pub fn new(inner: R) -> Self {
        SequentialInput {
            inner,
            head: Vec::new(),
            inner_pos: 0,
            pos: 0,
        }
    }

    fn fill_to(&mut self, target: u64) -> io::Result<()> {
        let mut scratch = [0u8; 64 * 1024];
        while self.inner_pos < target {
            let want = ((target - self.inner_pos) as usize).min(scratch.len());
            let n = self.inner.read(&mut scratch[..want])?;
            if n == 0 {
                break;
            }
            self.keep(&scratch[..n]);
            self.inner_pos += n as u64;
        }
        Ok(())
    }

    fn keep(&mut self, data: &[u8]) {
        if (self.inner_pos as usize) < HEAD_KEEP && self.inner_pos as usize == self.head.len() {
            let room = HEAD_KEEP - self.head.len();
            self.head.extend_from_slice(&data[..data.len().min(room)]);
        }
    }
}

impl<R: Read + Send> Read for SequentialInput<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if (self.pos as usize) < self.head.len() && self.pos < self.inner_pos {
            let start = self.pos as usize;
            let n = buf.len().min(self.head.len() - start);
            buf[..n].copy_from_slice(&self.head[start..start + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        if self.pos > self.inner_pos {
            self.fill_to(self.pos)?;
            if self.pos > self.inner_pos {
                return Ok(0);
            }
        }
        if self.pos < self.inner_pos {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stream position is no longer available",
            ));
        }
        let n = self.inner.read(buf)?;
        self.keep(&buf[..n]);
        self.inner_pos += n as u64;
        self.pos = self.inner_pos;
        Ok(n)
    }
}

impl<R: Read + Send> Seek for SequentialInput<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "stream has no known end",
                ))
            }
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        let t = target as u64;
        if t < self.inner_pos && t as usize >= self.head.len() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "backward seek past the retained stream head",
            ));
        }
        self.pos = t;
        Ok(t)
    }
}

impl<R: Read + Send> MediaInput for SequentialInput<R> {}

/// True for local paths and `file://` URIs.
pub fn is_local(uri: &str) -> bool {
    uri.starts_with("file://") || !uri.contains("://")
}

/// Filesystem path of a local URI.
pub fn local_path(uri: &str) -> Result<PathBuf> {
    if uri.starts_with("file://") {
        fp_sources::local::uri_to_path(uri).map_err(|e| anyhow!("{e}"))
    } else {
        Ok(PathBuf::from(uri))
    }
}

/// Canonical URI for a user-supplied path or URI (`--open`, remote).
pub fn normalize_uri(s: &str) -> String {
    if s.contains("://") {
        return s.to_string();
    }
    let p = std::path::Path::new(s);
    let abs = std::fs::canonicalize(p).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    });
    fp_sources::local::path_to_uri(&abs)
}

/// An ad-hoc source able to open `uri` when it belongs to no configured source.
pub fn adhoc_source_config(uri: &str) -> Result<SourceConfig> {
    let kind = fp_sources::kind_for_uri(uri).ok_or_else(|| anyhow!("unsupported URI {uri}"))?;
    let root = match uri.rfind('/') {
        Some(i) if i > uri.find("://").map_or(0, |s| s + 2) => &uri[..=i],
        _ => uri,
    };
    Ok(SourceConfig {
        id: format!("adhoc:{root}"),
        name: root.to_string(),
        kind,
        uri: root.to_string(),
        pinned_host_key: None,
    })
}

/// Open `uri` for the player. `source` is the configured source the URI
/// belongs to (credentials already applied), if any.
pub async fn open_input(
    uri: &str,
    source: Option<Arc<dyn Source>>,
    handle: Handle,
) -> Result<Box<dyn MediaInput>> {
    if is_local(uri) {
        let path = local_path(uri)?;
        let file = tokio::task::spawn_blocking(move || {
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))
        })
        .await??;
        return Ok(Box::new(BufferedInput::new(file)));
    }
    if let Some(_fmt) = fp_sources::stream::detect_format(uri) {
        let client = fp_sources::http::HttpClient::new(None).map_err(|e| anyhow!("{e}"))?;
        let manifest = fp_sources::stream::resolve(&client, uri)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        // [verify] decode limits on the Frame; prefer ≤ 4K for adaptive streams.
        let variant = manifest
            .best_variant(Some(2160))
            .ok_or_else(|| anyhow!("stream has no variants"))?
            .id
            .clone();
        let stream = fp_sources::stream::open_variant(&client, &manifest, &variant)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let reader = fp_sources::BlockingSegmentReader::new(stream, &handle, 4);
        return Ok(Box::new(SequentialInput::new(reader)));
    }
    let source = match source {
        Some(s) => s,
        None => fp_sources::connect(&adhoc_source_config(uri)?, None)
            .await
            .map_err(|e| anyhow!("{e}"))?,
    };
    let ra = source.open(uri).await.map_err(|e| anyhow!("{e}"))?;
    let reader = BlockingReader::from_box(ra, handle, ReadAheadConfig::default());
    Ok(Box::new(SourceInput(reader)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A reader that only goes forward (like a segment stream).
    struct Forward(Cursor<Vec<u8>>);
    impl Read for Forward {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(7);
            self.0.read(&mut buf[..n])
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn sequential_input_rereads_head_and_skips_forward() {
        let d = data(1000);
        let mut s = SequentialInput::new(Forward(Cursor::new(d.clone())));
        let mut buf = [0u8; 100];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &d[..100]);
        s.seek(SeekFrom::Start(10)).unwrap();
        s.read_exact(&mut buf[..20]).unwrap();
        assert_eq!(&buf[..20], &d[10..30]);
        s.seek(SeekFrom::Start(500)).unwrap();
        s.read_exact(&mut buf[..10]).unwrap();
        assert_eq!(&buf[..10], &d[500..510]);
        s.seek(SeekFrom::Current(-505)).unwrap();
        s.read_exact(&mut buf[..5]).unwrap();
        assert_eq!(&buf[..5], &d[5..10]);
        assert!(s.seek(SeekFrom::End(0)).is_err());
        s.seek(SeekFrom::Start(2000)).unwrap();
        assert_eq!(s.read(&mut buf).unwrap(), 0, "past end reads nothing");
    }

    #[test]
    fn uri_helpers() {
        assert!(is_local("/home/deck/a.mp4"));
        assert!(is_local("file:///home/deck/a.mp4"));
        assert!(!is_local("smb://nas/a.mp4"));
        assert_eq!(
            local_path("file:///tmp/a%20b.mp4").unwrap(),
            PathBuf::from("/tmp/a b.mp4")
        );
        let c = adhoc_source_config("https://host/videos/a.mp4").unwrap();
        assert_eq!(c.uri, "https://host/videos/");
        assert_eq!(c.kind, fp_sources::SourceKind::Http);
        assert!(adhoc_source_config("gopher://x/y").is_err());
        assert!(normalize_uri("relative.mp4").starts_with("file:///"));
        assert_eq!(normalize_uri("smb://nas/x.mkv"), "smb://nas/x.mkv");
    }

    #[test]
    fn local_open_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, b"hello media").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let uri = fp_sources::local::path_to_uri(&p);
        let mut input = rt
            .block_on(open_input(&uri, None, rt.handle().clone()))
            .unwrap();
        let mut s = String::new();
        input.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello media");
        assert_eq!(input.size(), Some(11));
    }

    #[test]
    fn source_input_over_memory_file() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ra: Arc<dyn fp_sources::RandomAccess> =
            Arc::new(fp_sources::MemoryFile::new(data(5000)));
        let mut input = SourceInput(BlockingReader::new(
            ra,
            rt.handle().clone(),
            ReadAheadConfig::local(),
        ));
        assert_eq!(input.size(), Some(5000));
        input.seek(SeekFrom::Start(4000)).unwrap();
        let mut buf = [0u8; 10];
        input.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data(5000)[4000..4010]);
    }
}
