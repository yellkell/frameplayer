//! Byte-source abstraction feeding the demuxers.
//!
//! [`MediaInput`] is a blocking `Read + Seek + Send` with an optional total
//! size. `fp-sources` adapts local files, SMB, WebDAV and HTTP range
//! requests to it; the pure-Rust demuxers read through it directly and the
//! ffmpeg demuxer wraps it in a custom AVIO context, so every source looks
//! like a file to every demuxer.

use std::io::{self, Read, Seek, SeekFrom};

/// A seekable, blocking media byte stream.
pub trait MediaInput: Read + Seek + Send {
    /// Total size in bytes, when known (needed for some seeks and for
    /// `size == 0` "extends to end of file" boxes).
    fn size(&self) -> Option<u64> {
        None
    }
}

impl MediaInput for std::fs::File {
    fn size(&self) -> Option<u64> {
        self.metadata().ok().map(|m| m.len())
    }
}

impl<T: AsRef<[u8]> + Send> MediaInput for io::Cursor<T> {
    fn size(&self) -> Option<u64> {
        Some(self.get_ref().as_ref().len() as u64)
    }
}

impl<T: MediaInput + ?Sized> MediaInput for Box<T> {
    fn size(&self) -> Option<u64> {
        (**self).size()
    }
}

/// Read-ahead buffer over a [`MediaInput`] that turns small sequential reads
/// and short forward seeks into large reads (important for network sources
/// where each read is a round trip).
pub struct BufferedInput<R: MediaInput> {
    inner: R,
    buf: Vec<u8>,
    /// Absolute stream position of `buf[0]`.
    buf_pos: u64,
    /// Read cursor within `buf`.
    cursor: usize,
    /// Absolute position of the inner reader.
    inner_pos: u64,
    capacity: usize,
}

impl<R: MediaInput> BufferedInput<R> {
    pub fn new(inner: R) -> Self {
        Self::with_capacity(inner, 256 * 1024)
    }

    pub fn with_capacity(mut inner: R, capacity: usize) -> Self {
        let inner_pos = inner.stream_position().unwrap_or(0);
        BufferedInput {
            inner,
            buf: Vec::new(),
            buf_pos: inner_pos,
            cursor: 0,
            inner_pos,
            capacity: capacity.max(4096),
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    fn position(&self) -> u64 {
        self.buf_pos + self.cursor as u64
    }
}

impl<R: MediaInput> Read for BufferedInput<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cursor >= self.buf.len() {
            let pos = self.position();
            // Large reads bypass the buffer.
            if out.len() >= self.capacity {
                if self.inner_pos != pos {
                    self.inner.seek(SeekFrom::Start(pos))?;
                }
                let n = self.inner.read(out)?;
                self.inner_pos = pos + n as u64;
                self.buf.clear();
                self.cursor = 0;
                self.buf_pos = self.inner_pos;
                return Ok(n);
            }
            if self.inner_pos != pos {
                self.inner.seek(SeekFrom::Start(pos))?;
                self.inner_pos = pos;
            }
            self.buf.resize(self.capacity, 0);
            let mut filled = 0;
            // Fill as much as one read gives us (don't block for more).
            while filled == 0 {
                let n = self.inner.read(&mut self.buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            self.buf.truncate(filled);
            self.buf_pos = pos;
            self.cursor = 0;
            self.inner_pos = pos + filled as u64;
            if filled == 0 {
                return Ok(0);
            }
        }
        let n = out.len().min(self.buf.len() - self.cursor);
        out[..n].copy_from_slice(&self.buf[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

impl<R: MediaInput> Seek for BufferedInput<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self
                .position()
                .checked_add_signed(d)
                .ok_or(io::ErrorKind::InvalidInput)?,
            SeekFrom::End(d) => {
                let size = match self.inner.size() {
                    Some(s) => s,
                    None => {
                        let s = self.inner.seek(SeekFrom::End(0))?;
                        self.inner_pos = s;
                        s
                    }
                };
                size.checked_add_signed(d)
                    .ok_or(io::ErrorKind::InvalidInput)?
            }
        };
        if target >= self.buf_pos && target <= self.buf_pos + self.buf.len() as u64 {
            self.cursor = (target - self.buf_pos) as usize;
        } else {
            self.buf.clear();
            self.cursor = 0;
            self.buf_pos = target;
        }
        Ok(target)
    }
}

impl<R: MediaInput> MediaInput for BufferedInput<R> {
    fn size(&self) -> Option<u64> {
        self.inner.size()
    }
}

/// Read exactly `len` bytes at `offset`.
pub fn read_at(input: &mut dyn MediaInput, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    input.seek(SeekFrom::Start(offset))?;
    let mut v = vec![0u8; len];
    input.read_exact(&mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Counts inner reads to prove buffering works.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        reads: usize,
    }
    impl Read for Counting {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.inner.read(b)
        }
    }
    impl Seek for Counting {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.inner.seek(p)
        }
    }
    impl MediaInput for Counting {
        fn size(&self) -> Option<u64> {
            Some(self.inner.get_ref().len() as u64)
        }
    }

    #[test]
    fn buffered_reads_and_seeks() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut b = BufferedInput::with_capacity(
            Counting {
                inner: Cursor::new(data.clone()),
                reads: 0,
            },
            8192,
        );
        let mut out = [0u8; 10];
        for i in 0..100 {
            b.read_exact(&mut out).unwrap();
            assert_eq!(out[0], data[i * 10]);
        }
        assert_eq!(b.inner.reads, 1);
        b.seek(SeekFrom::Start(50_000)).unwrap();
        b.read_exact(&mut out).unwrap();
        assert_eq!(&out[..], &data[50_000..50_010]);
        b.seek(SeekFrom::Current(-5)).unwrap();
        b.read_exact(&mut out).unwrap();
        assert_eq!(&out[..], &data[50_005..50_015]);
        assert_eq!(b.seek(SeekFrom::End(-10)).unwrap(), 99_990);
        let mut rest = Vec::new();
        b.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[99_990..]);
        assert_eq!(b.size(), Some(100_000));
    }
}
