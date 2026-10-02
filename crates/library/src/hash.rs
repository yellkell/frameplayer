//! Fast partial content hash: SHA-256 over the file size (u64 LE) and the
//! first and last 64 KiB. Cheap enough to compute over SMB/HTTP (two small
//! reads), stable across renames/moves, and in practice unique for video
//! files (container headers + trailing index differ between encodes).

use fp_sources::RandomAccess;
use sha2::{Digest, Sha256};

pub const EDGE: usize = 64 * 1024;

/// Hash from already-read parts. `last` is the final `min(EDGE, size)`
/// bytes (it may overlap `first` for small files).
pub fn hash_parts(size: u64, first: &[u8], last: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(size.to_le_bytes());
    h.update(first);
    h.update(last);
    hex::encode(h.finalize())
}

/// Compute the hash through any source's random-access reader. When the
/// size is unknown only the first block is hashed (with size 0).
pub async fn content_hash(ra: &dyn RandomAccess) -> fp_sources::Result<String> {
    let size = ra.size();
    let first = ra.read_at(0, EDGE).await?;
    let last = match size {
        Some(s) if s > 0 => {
            let off = s.saturating_sub(EDGE as u64);
            if off == 0 {
                first.clone()
            } else {
                ra.read_at(off, EDGE).await?
            }
        }
        _ => Default::default(),
    };
    Ok(hash_parts(size.unwrap_or(0), &first, &last))
}

/// Synchronous variant for local paths.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let read = |off: u64, len: usize| -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            match f.read_at(&mut buf[done..], off + done as u64)? {
                0 => break,
                n => done += n,
            }
        }
        buf.truncate(done);
        Ok(buf)
    };
    let first = read(0, EDGE)?;
    let off = size.saturating_sub(EDGE as u64);
    let last = if off == 0 {
        first.clone()
    } else {
        read(off, EDGE)?
    };
    Ok(hash_parts(size, &first, &last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_sources::MemoryFile;

    #[tokio::test]
    async fn async_and_sync_agree() {
        let dir = tempfile::tempdir().unwrap();
        for n in [0usize, 10, EDGE, EDGE + 1, 3 * EDGE + 17] {
            let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let p = dir.path().join(format!("f{n}"));
            std::fs::write(&p, &data).unwrap();
            let a = content_hash(&MemoryFile::new(data.clone())).await.unwrap();
            assert_eq!(a, hash_file(&p).unwrap(), "size {n}");
        }
    }

    #[tokio::test]
    async fn middle_changes_ignored_edges_not() {
        let mut data = vec![0u8; 4 * EDGE];
        let base = content_hash(&MemoryFile::new(data.clone())).await.unwrap();
        data[2 * EDGE] = 1;
        assert_eq!(
            content_hash(&MemoryFile::new(data.clone())).await.unwrap(),
            base
        );
        data[10] = 1;
        assert_ne!(
            content_hash(&MemoryFile::new(data.clone())).await.unwrap(),
            base
        );
        let mut longer = vec![0u8; 4 * EDGE + 1];
        longer[0] = 0;
        assert_ne!(content_hash(&MemoryFile::new(longer)).await.unwrap(), base);
    }
}
