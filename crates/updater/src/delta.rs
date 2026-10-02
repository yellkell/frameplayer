//! Block delta between two uncompressed release tarballs ("FPDELTA1").
//!
//! The encoder is the rsync algorithm run offline: index every
//! non-overlapping `block_size` block of the old file by a rolling weak hash,
//! slide a window over the new file, confirm weak-hash hits by comparing
//! bytes, greedily extend matches, and emit `COPY(offset,len)` / `DATA(bytes)`
//! ops. Because the encoder holds both files, a byte compare replaces rsync's
//! strong per-block hash; integrity of the result is guaranteed instead by
//! whole-file SHA-256 of both source and target in the header.
//!
//! ```text
//! header (uncompressed, 92 bytes):
//!   "FPDELTA1" | block_size u32 | source_len u64 | source_sha256 [32]
//!              | target_len u64 | target_sha256 [32]          (little endian)
//! body (gzip):
//!   0x01 offset u64 len u64   copy from source
//!   0x02 len u32 bytes…        literal data
//!   0x00                       end
//! ```

use crate::{Result, UpdateError};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"FPDELTA1";
pub const DEFAULT_BLOCK_SIZE: usize = 4096;
const OP_END: u8 = 0;
const OP_COPY: u8 = 1;
const OP_DATA: u8 = 2;
const MAX_LITERAL: usize = 1 << 20;
/// Cap candidates per weak hash so long runs of identical blocks (zero
/// padding in tar files) don't make the scan quadratic.
const MAX_CANDIDATES: usize = 16;

/// Header fields of a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaHeader {
    pub block_size: u32,
    pub source_len: u64,
    pub source_sha256: [u8; 32],
    pub target_len: u64,
    pub target_sha256: [u8; 32],
}

impl DeltaHeader {
    fn write(&self, w: &mut impl Write) -> std::io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&self.block_size.to_le_bytes())?;
        w.write_all(&self.source_len.to_le_bytes())?;
        w.write_all(&self.source_sha256)?;
        w.write_all(&self.target_len.to_le_bytes())?;
        w.write_all(&self.target_sha256)
    }

    pub fn read(r: &mut impl Read) -> Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)
            .map_err(|_| UpdateError::BadDelta("truncated header".into()))?;
        if &magic != MAGIC {
            return Err(UpdateError::BadDelta("not an FPDELTA1 patch".into()));
        }
        let block_size = read_u32(r)?;
        let source_len = read_u64(r)?;
        let source_sha256 = read_32(r)?;
        let target_len = read_u64(r)?;
        let target_sha256 = read_32(r)?;
        Ok(Self {
            block_size,
            source_len,
            source_sha256,
            target_len,
            target_sha256,
        })
    }
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)
        .map_err(|_| UpdateError::BadDelta("truncated".into()))?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)
        .map_err(|_| UpdateError::BadDelta("truncated".into()))?;
    Ok(u64::from_le_bytes(b))
}
fn read_32(r: &mut impl Read) -> Result<[u8; 32]> {
    let mut b = [0u8; 32];
    r.read_exact(&mut b)
        .map_err(|_| UpdateError::BadDelta("truncated".into()))?;
    Ok(b)
}

/// rsync-style rolling checksum over a fixed window.
#[derive(Debug, Clone, Copy)]
struct Rolling {
    a: u32,
    b: u32,
    len: u32,
}

impl Rolling {
    fn new(window: &[u8]) -> Self {
        let len = window.len() as u32;
        let (mut a, mut b) = (0u32, 0u32);
        for (i, &x) in window.iter().enumerate() {
            a = a.wrapping_add(x as u32);
            b = b.wrapping_add((len - i as u32).wrapping_mul(x as u32));
        }
        Self { a, b, len }
    }

    fn roll(&mut self, out: u8, inp: u8) {
        self.a = self.a.wrapping_sub(out as u32).wrapping_add(inp as u32);
        self.b = self
            .b
            .wrapping_sub(self.len.wrapping_mul(out as u32))
            .wrapping_add(self.a);
    }

    fn digest(&self) -> u32 {
        (self.a & 0xffff) | (self.b << 16)
    }
}

/// Collects ops, merging adjacent copies and chunking literals.
struct OpWriter<W: Write> {
    w: W,
    pending_copy: Option<(u64, u64)>,
    literal: Vec<u8>,
}

impl<W: Write> OpWriter<W> {
    fn copy(&mut self, off: u64, len: u64) -> std::io::Result<()> {
        self.flush_literal()?;
        match &mut self.pending_copy {
            Some((o, l)) if *o + *l == off => *l += len,
            _ => {
                self.flush_copy()?;
                self.pending_copy = Some((off, len));
            }
        }
        Ok(())
    }

    fn data(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.flush_copy()?;
        self.literal.extend_from_slice(bytes);
        if self.literal.len() >= MAX_LITERAL {
            self.flush_literal()?;
        }
        Ok(())
    }

    fn flush_copy(&mut self) -> std::io::Result<()> {
        if let Some((o, l)) = self.pending_copy.take() {
            self.w.write_all(&[OP_COPY])?;
            self.w.write_all(&o.to_le_bytes())?;
            self.w.write_all(&l.to_le_bytes())?;
        }
        Ok(())
    }

    fn flush_literal(&mut self) -> std::io::Result<()> {
        for chunk in self.literal.chunks(MAX_LITERAL) {
            self.w.write_all(&[OP_DATA])?;
            self.w.write_all(&(chunk.len() as u32).to_le_bytes())?;
            self.w.write_all(chunk)?;
        }
        self.literal.clear();
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<W> {
        self.flush_literal()?;
        self.flush_copy()?;
        self.w.write_all(&[OP_END])?;
        Ok(self.w)
    }
}

/// Encode a patch turning `source` into `target`.
pub fn create(source: &[u8], target: &[u8], block_size: usize) -> Vec<u8> {
    let bs = block_size.max(16);
    let header = DeltaHeader {
        block_size: bs as u32,
        source_len: source.len() as u64,
        source_sha256: Sha256::digest(source).into(),
        target_len: target.len() as u64,
        target_sha256: Sha256::digest(target).into(),
    };
    let mut out = Vec::new();
    header.write(&mut out).expect("write to Vec");
    let gz = GzEncoder::new(out, flate2::Compression::best());
    let mut ops = OpWriter {
        w: gz,
        pending_copy: None,
        literal: Vec::new(),
    };
    encode_ops(source, target, bs, &mut ops).expect("write to Vec");
    ops.finish()
        .and_then(|gz| gz.finish())
        .expect("write to Vec")
}

fn encode_ops<W: Write>(
    source: &[u8],
    target: &[u8],
    bs: usize,
    ops: &mut OpWriter<W>,
) -> std::io::Result<()> {
    if source.len() < bs || target.len() < bs {
        return ops.data(target);
    }
    let mut index: HashMap<u32, Vec<usize>> = HashMap::with_capacity(source.len() / bs + 1);
    for off in (0..=source.len() - bs).step_by(bs) {
        let v = index
            .entry(Rolling::new(&source[off..off + bs]).digest())
            .or_default();
        if v.len() < MAX_CANDIDATES {
            v.push(off);
        }
    }

    let mut p = 0usize;
    let mut lit_start = 0usize;
    let mut roll = Rolling::new(&target[..bs]);
    while p + bs <= target.len() {
        let hit = index.get(&roll.digest()).and_then(|cands| {
            cands
                .iter()
                .copied()
                .find(|&off| source[off..off + bs] == target[p..p + bs])
        });
        if let Some(off) = hit {
            if lit_start < p {
                ops.data(&target[lit_start..p])?;
            }
            let mut len = bs;
            while off + len < source.len()
                && p + len < target.len()
                && source[off + len] == target[p + len]
            {
                len += 1;
            }
            ops.copy(off as u64, len as u64)?;
            p += len;
            lit_start = p;
            if p + bs <= target.len() {
                roll = Rolling::new(&target[p..p + bs]);
            }
        } else {
            if p + bs < target.len() {
                roll.roll(target[p], target[p + bs]);
            }
            p += 1;
        }
    }
    if lit_start < target.len() {
        ops.data(&target[lit_start..])?;
    }
    Ok(())
}

/// Apply a patch. Verifies the source hash before and the target hash after;
/// on any error the output must be discarded (use [`apply_files`] for that).
pub fn apply<S: Read + Seek, P: Read, O: Write>(
    mut source: S,
    patch: P,
    out: O,
) -> Result<DeltaHeader> {
    let mut patch = std::io::BufReader::new(patch);
    let header = DeltaHeader::read(&mut patch)?;

    // Pass 1: the source must be exactly the file the patch was made against.
    source.seek(SeekFrom::Start(0))?;
    let mut h = Sha256::new();
    let n = std::io::copy(
        &mut (&mut source).take(header.source_len + 1),
        &mut HashWriter(&mut h),
    )?;
    if n != header.source_len || <[u8; 32]>::from(h.finalize()) != header.source_sha256 {
        return Err(UpdateError::BadDelta(
            "source file does not match the patch (different base version?)".into(),
        ));
    }

    let mut body = GzDecoder::new(patch);
    let mut out = HashingOut {
        inner: out,
        hasher: Sha256::new(),
        written: 0,
    };
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let mut op = [0u8; 1];
        body.read_exact(&mut op)
            .map_err(|_| UpdateError::BadDelta("truncated op stream".into()))?;
        match op[0] {
            OP_END => break,
            OP_COPY => {
                let off = read_u64(&mut body)?;
                let len = read_u64(&mut body)?;
                if !matches!(off.checked_add(len), Some(e) if e <= header.source_len) {
                    return Err(UpdateError::BadDelta("copy outside source".into()));
                }
                source.seek(SeekFrom::Start(off))?;
                copy_exact(&mut source, &mut out, len, &mut buf)?;
            }
            OP_DATA => {
                let len = read_u32(&mut body)? as u64;
                copy_exact(&mut body, &mut out, len, &mut buf)?;
            }
            other => return Err(UpdateError::BadDelta(format!("unknown op {other:#x}"))),
        }
        if out.written > header.target_len {
            return Err(UpdateError::BadDelta("output exceeds target length".into()));
        }
    }
    out.inner.flush()?;
    if out.written != header.target_len
        || <[u8; 32]>::from(out.hasher.finalize()) != header.target_sha256
    {
        return Err(UpdateError::BadDelta(
            "patched output failed verification".into(),
        ));
    }
    Ok(header)
}

fn copy_exact(r: &mut impl Read, w: &mut impl Write, mut len: u64, buf: &mut [u8]) -> Result<()> {
    while len > 0 {
        let want = (buf.len() as u64).min(len) as usize;
        r.read_exact(&mut buf[..want])
            .map_err(|_| UpdateError::BadDelta("unexpected end of data".into()))?;
        w.write_all(&buf[..want])?;
        len -= want as u64;
    }
    Ok(())
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct HashingOut<W> {
    inner: W,
    hasher: Sha256,
    written: u64,
}
impl<W: Write> Write for HashingOut<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.hasher.update(&b[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Create a patch file from two files on disk (release tool).
pub fn create_files(
    source: &Path,
    target: &Path,
    patch_out: &Path,
    block_size: usize,
) -> Result<u64> {
    let patch = create(&std::fs::read(source)?, &std::fs::read(target)?, block_size);
    std::fs::write(patch_out, &patch)?;
    Ok(patch.len() as u64)
}

/// Apply a patch file, writing `out` atomically (temp file + rename).
pub fn apply_files(source: &Path, patch: &Path, out: &Path) -> Result<DeltaHeader> {
    let tmp = out.with_extension("delta-tmp");
    let res = (|| {
        let src = std::fs::File::open(source)?;
        let p = std::fs::File::open(patch)?;
        let o = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let header = apply(src, p, o)?;
        std::fs::rename(&tmp, out)?;
        Ok(header)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn roundtrip(src: &[u8], dst: &[u8], bs: usize) -> usize {
        let patch = create(src, dst, bs);
        let mut out = Vec::new();
        apply(Cursor::new(src), Cursor::new(&patch), &mut out).unwrap();
        assert_eq!(out, dst);
        patch.len()
    }

    #[test]
    fn rolling_matches_fresh_hash() {
        let data = pseudo_random(300, 1);
        let mut r = Rolling::new(&data[0..64]);
        for p in 1..200 {
            r.roll(data[p - 1], data[p + 63]);
            assert_eq!(
                r.digest(),
                Rolling::new(&data[p..p + 64]).digest(),
                "at {p}"
            );
        }
    }

    #[test]
    fn identical_files_tiny_patch() {
        let a = pseudo_random(1 << 20, 2);
        let size = roundtrip(&a, &a, 4096);
        assert!(size < 200, "patch for identical file is {size} bytes");
    }

    #[test]
    fn small_edits_produce_small_patch() {
        let a = pseudo_random(1 << 20, 3);
        let mut b = a.clone();
        b[1000..1010].copy_from_slice(b"0123456789"); // overwrite
        b.splice(500_000..500_000, pseudo_random(3000, 4)); // insert
        b.drain(800_000..801_234); // delete
        b.extend_from_slice(b"tail");
        let size = roundtrip(&a, &b, 4096);
        assert!(size < 20_000, "patch too large: {size}");
    }

    #[test]
    fn unrelated_and_edge_cases() {
        roundtrip(b"", b"", 4096);
        roundtrip(b"", b"new content", 4096);
        roundtrip(b"old content", b"", 4096);
        roundtrip(b"short", b"shorter", 4096);
        roundtrip(&pseudo_random(10_000, 5), &pseudo_random(20_000, 6), 64);
        let zeros = vec![0u8; 100_000];
        let mut z2 = zeros.clone();
        z2[50_000] = 1;
        roundtrip(&zeros, &z2, 512);
        // Literal data larger than one DATA op.
        roundtrip(b"", &pseudo_random(MAX_LITERAL * 2 + 17, 7), 4096);
    }

    #[test]
    fn reordered_blocks() {
        let a = pseudo_random(64 * 1024, 8);
        let mut b = a[32 * 1024..].to_vec();
        b.extend_from_slice(&a[..32 * 1024]);
        let size = roundtrip(&a, &b, 1024);
        assert!(size < 1000, "{size}");
    }

    #[test]
    fn wrong_source_rejected() {
        let a = pseudo_random(10_000, 9);
        let b = pseudo_random(10_000, 10);
        let patch = create(&a, &b, 256);
        let mut out = Vec::new();
        let err = apply(Cursor::new(&b), Cursor::new(&patch), &mut out).unwrap_err();
        assert!(matches!(err, UpdateError::BadDelta(_)));
    }

    #[test]
    fn corrupt_patch_rejected() {
        let a = pseudo_random(10_000, 11);
        let b = pseudo_random(10_000, 12);
        let mut patch = create(&a, &b, 256);
        assert!(apply(
            Cursor::new(&a),
            Cursor::new(b"NOTDELTA".as_slice()),
            &mut Vec::new()
        )
        .is_err());
        let last = patch.len() - 20;
        patch[last] ^= 0xff;
        assert!(apply(Cursor::new(&a), Cursor::new(&patch), &mut Vec::new()).is_err());
        patch.truncate(100);
        assert!(apply(Cursor::new(&a), Cursor::new(&patch), &mut Vec::new()).is_err());
    }

    #[test]
    fn file_helpers_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let (s, t, p, o) = (
            dir.path().join("s.tar"),
            dir.path().join("t.tar"),
            dir.path().join("p.fpd"),
            dir.path().join("o.tar"),
        );
        let a = pseudo_random(50_000, 13);
        let mut b = a.clone();
        b[100] ^= 1;
        std::fs::write(&s, &a).unwrap();
        std::fs::write(&t, &b).unwrap();
        create_files(&s, &t, &p, 1024).unwrap();
        apply_files(&s, &p, &o).unwrap();
        assert_eq!(std::fs::read(&o).unwrap(), b);
        // Wrong base: no output file left behind.
        let o2 = dir.path().join("o2.tar");
        assert!(apply_files(&t, &p, &o2).is_err());
        assert!(!o2.exists());
        assert!(!o2.with_extension("delta-tmp").exists());
    }
}
