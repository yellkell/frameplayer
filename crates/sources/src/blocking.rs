//! Blocking adapters so a demuxer on a plain OS thread can consume async
//! sources.
//!
//! [`BlockingReader`] turns any [`RandomAccess`] into `std::io::Read + Seek`.
//! Reads are served from fixed-size chunks; after each read the next
//! `prefetch_chunks` chunks are requested concurrently on the tokio runtime,
//! so sequential playback over high-latency links (SMB, HTTP) streams at
//! link speed instead of one round trip per demuxer read. A seek outside
//! the window aborts the in-flight prefetches.
//!
//! Must not be used from a tokio worker thread: reads block the calling
//! thread until data arrives.

use crate::segments::SegmentStream;
use crate::source::RandomAccess;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// Read-ahead tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadAheadConfig {
    /// Bytes per request.
    pub chunk_size: usize,
    /// Chunks requested ahead of the read position.
    pub prefetch_chunks: usize,
    /// Chunks kept behind the read position (demuxers often seek back a
    /// little to re-read headers).
    pub keep_behind: usize,
}

impl Default for ReadAheadConfig {
    fn default() -> Self {
        // 8 x 1 MiB covers ~1 s of 8K60 HEVC at ~60 Mbit/s.
        ReadAheadConfig {
            chunk_size: 1 << 20,
            prefetch_chunks: 8,
            keep_behind: 2,
        }
    }
}

impl ReadAheadConfig {
    /// Settings for local disks where latency is negligible.
    pub fn local() -> Self {
        ReadAheadConfig {
            chunk_size: 512 << 10,
            prefetch_chunks: 2,
            keep_behind: 1,
        }
    }
}

enum Slot {
    Pending(JoinHandle<crate::Result<Bytes>>),
    Ready(Bytes),
}

/// `Read + Seek` over a [`RandomAccess`] with read-ahead.
pub struct BlockingReader {
    ra: Arc<dyn RandomAccess>,
    handle: Handle,
    cfg: ReadAheadConfig,
    pos: u64,
    size: Option<u64>,
    slots: BTreeMap<u64, Slot>,
    /// Chunk index known to be past EOF (short read seen), if any.
    eof_chunk: Option<u64>,
}

impl BlockingReader {
    pub fn new(ra: Arc<dyn RandomAccess>, handle: Handle, cfg: ReadAheadConfig) -> Self {
        let size = ra.size();
        let cfg = ReadAheadConfig {
            chunk_size: cfg.chunk_size.max(4096),
            ..cfg
        };
        BlockingReader {
            ra,
            handle,
            cfg,
            pos: 0,
            size,
            slots: BTreeMap::new(),
            eof_chunk: None,
        }
    }

    /// Convenience for the `Box` that [`crate::Source::open`] returns.
    pub fn from_box(ra: Box<dyn RandomAccess>, handle: Handle, cfg: ReadAheadConfig) -> Self {
        Self::new(Arc::from(ra), handle, cfg)
    }

    pub fn size(&self) -> Option<u64> {
        self.size
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    fn chunk_exists(&self, idx: u64) -> bool {
        let start = idx * self.cfg.chunk_size as u64;
        if self.eof_chunk.is_some_and(|e| idx > e) {
            return false;
        }
        self.size.is_none_or(|s| start < s)
    }

    fn spawn(&mut self, idx: u64) {
        if self.slots.contains_key(&idx) || !self.chunk_exists(idx) {
            return;
        }
        let ra = self.ra.clone();
        let off = idx * self.cfg.chunk_size as u64;
        let len = self.cfg.chunk_size;
        let jh = self.handle.spawn(async move { ra.read_at(off, len).await });
        self.slots.insert(idx, Slot::Pending(jh));
    }

    /// Drop chunks outside the window around `idx`, aborting pending ones.
    fn evict(&mut self, idx: u64) {
        let lo = idx.saturating_sub(self.cfg.keep_behind as u64);
        let hi = idx + self.cfg.prefetch_chunks as u64;
        self.slots.retain(|k, slot| {
            let keep = *k >= lo && *k <= hi;
            if !keep {
                if let Slot::Pending(jh) = slot {
                    jh.abort();
                }
            }
            keep
        });
    }

    fn chunk(&mut self, idx: u64) -> io::Result<Bytes> {
        self.spawn(idx);
        let slot = match self.slots.remove(&idx) {
            Some(s) => s,
            None => return Ok(Bytes::new()),
        };
        let data = match slot {
            Slot::Ready(b) => b,
            Slot::Pending(jh) => match futures::executor::block_on(jh) {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => return Err(e.into()),
                Err(e) => return Err(io::Error::other(e)),
            },
        };
        if data.len() < self.cfg.chunk_size {
            self.eof_chunk = Some(self.eof_chunk.map_or(idx, |e| e.min(idx)));
            if self.size.is_none() {
                self.size = Some(idx * self.cfg.chunk_size as u64 + data.len() as u64);
            }
        }
        self.slots.insert(idx, Slot::Ready(data.clone()));
        Ok(data)
    }
}

impl Read for BlockingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.size.is_some_and(|s| self.pos >= s) {
            return Ok(0);
        }
        let cs = self.cfg.chunk_size as u64;
        let idx = self.pos / cs;
        self.evict(idx);
        for i in 1..=self.cfg.prefetch_chunks as u64 {
            self.spawn(idx + i);
        }
        let data = self.chunk(idx)?;
        let off = (self.pos - idx * cs) as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for BlockingReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => match self.size {
                Some(s) => s as i128 + d as i128,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "stream size unknown",
                    ))
                }
            },
        };
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

impl Drop for BlockingReader {
    fn drop(&mut self) {
        for slot in self.slots.values() {
            if let Slot::Pending(jh) = slot {
                jh.abort();
            }
        }
    }
}

/// Sequential `Read` over an HLS/DASH [`SegmentStream`] (init segment
/// followed by media segments), fetching up to `prefetch` segments ahead
/// on the runtime.
pub struct BlockingSegmentReader {
    rx: std::sync::mpsc::Receiver<crate::Result<Bytes>>,
    current: Bytes,
    task: JoinHandle<()>,
    done: bool,
}

impl BlockingSegmentReader {
    pub fn new(mut stream: SegmentStream, handle: &Handle, prefetch: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(prefetch.max(1));
        let task = handle.spawn(async move {
            while let Some(item) = stream.next_segment().await {
                let failed = item.is_err();
                let tx = tx.clone();
                // sync_channel::send blocks when full; keep it off the worker.
                let ok = tokio::task::spawn_blocking(move || tx.send(item).is_ok())
                    .await
                    .unwrap_or(false);
                if !ok || failed {
                    break;
                }
            }
        });
        BlockingSegmentReader {
            rx,
            current: Bytes::new(),
            task,
            done: false,
        }
    }
}

impl Read for BlockingSegmentReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.current.is_empty() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Ok(b)) => self.current = b,
                Ok(Err(e)) => {
                    self.done = true;
                    return Err(io::Error::from(e));
                }
                Err(_) => {
                    self.done = true;
                    return Ok(0);
                }
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current.split_to(n));
        Ok(n)
    }
}

impl Drop for BlockingSegmentReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemoryFile;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        inner: MemoryFile,
        reads: AtomicUsize,
        report_size: bool,
    }

    #[async_trait]
    impl RandomAccess for Counting {
        async fn read_at(&self, offset: u64, len: usize) -> crate::Result<Bytes> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            self.inner.read_at(offset, len).await
        }
        fn size(&self) -> Option<u64> {
            self.report_size.then(|| self.inner.size()).flatten()
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn reader(
        n: usize,
        report_size: bool,
        cfg: ReadAheadConfig,
    ) -> (tokio::runtime::Runtime, BlockingReader, Arc<Counting>) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ra = Arc::new(Counting {
            inner: MemoryFile::new(data(n)),
            reads: AtomicUsize::new(0),
            report_size,
        });
        let r = BlockingReader::new(ra.clone(), rt.handle().clone(), cfg);
        (rt, r, ra)
    }

    #[test]
    fn sequential_read_matches() {
        let cfg = ReadAheadConfig {
            chunk_size: 4096,
            prefetch_chunks: 3,
            keep_behind: 1,
        };
        for report_size in [true, false] {
            let (_rt, mut r, ra) = reader(100_000, report_size, cfg);
            let mut out = Vec::new();
            r.read_to_end(&mut out).unwrap();
            assert_eq!(out, data(100_000));
            // 25 chunks, each fetched once (plus at most a few EOF probes).
            assert!(
                ra.reads.load(Ordering::SeqCst) <= 25 + cfg.prefetch_chunks,
                "{}",
                ra.reads.load(Ordering::SeqCst)
            );
        }
    }

    #[test]
    fn seek_and_read() {
        let cfg = ReadAheadConfig {
            chunk_size: 4096,
            prefetch_chunks: 2,
            keep_behind: 1,
        };
        let (_rt, mut r, _) = reader(50_000, true, cfg);
        let d = data(50_000);
        r.seek(SeekFrom::Start(30_000)).unwrap();
        let mut buf = vec![0u8; 10_000];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, d[30_000..40_000]);
        r.seek(SeekFrom::End(-10)).unwrap();
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, d[49_990..]);
        r.seek(SeekFrom::Current(-20)).unwrap();
        assert_eq!(r.position(), 49_980);
        r.seek(SeekFrom::Start(5)).unwrap();
        r.read_exact(&mut buf[..3]).unwrap();
        assert_eq!(&buf[..3], &d[5..8]);
        assert!(r.seek(SeekFrom::Current(-100)).is_err());
        r.seek(SeekFrom::Start(60_000)).unwrap();
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn unknown_size_end_seek_errors() {
        let (_rt, mut r, _) = reader(10, false, ReadAheadConfig::default());
        assert!(r.seek(SeekFrom::End(0)).is_err());
    }
}
