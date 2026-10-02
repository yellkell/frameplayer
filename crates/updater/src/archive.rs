//! Release tarball handling: decompression (gzip via miniz_oxide, zstd via
//! ruzstd: both pure Rust) and hardened extraction.
//!
//! Extraction rules (an attacker who controls a tarball still cannot write
//! outside the destination, even though tarballs are hash-verified first):
//! * only regular files, directories and symlinks; hard links, devices and
//!   FIFOs are rejected;
//! * entry paths must be relative with no `..` components;
//! * no entry may be written *through* an already-extracted symlink;
//! * symlink targets must be relative and stay inside the tree;
//! * permission bits are reduced to `rwx` (no setuid/setgid/sticky);
//! * total unpacked size is capped.

use crate::manifest::ArchiveFormat;
use crate::{Result, UpdateError};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

/// Limits applied while extracting.
#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub max_total_bytes: u64,
    pub max_entries: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            max_total_bytes: 8 << 30,
            max_entries: 200_000,
        }
    }
}

/// What extraction produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractStats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes: u64,
}

/// Sniff the compression from magic bytes, falling back to the file name.
pub fn detect_format(path: &Path) -> Result<ArchiveFormat> {
    let mut magic = [0u8; 4];
    let n = fs::File::open(path)?.read(&mut magic)?;
    Ok(match &magic[..n] {
        [0x1f, 0x8b, ..] => ArchiveFormat::TarGz,
        [0x28, 0xb5, 0x2f, 0xfd] => ArchiveFormat::TarZst,
        _ => ArchiveFormat::from_name(&path.to_string_lossy()).unwrap_or(ArchiveFormat::Tar),
    })
}

/// Wrap a reader in the right decompressor.
pub fn decoder<'a, R: Read + 'a>(r: R, format: ArchiveFormat) -> Result<Box<dyn Read + 'a>> {
    Ok(match format {
        ArchiveFormat::Tar => Box::new(r),
        ArchiveFormat::TarGz => Box::new(flate2::read::MultiGzDecoder::new(r)),
        ArchiveFormat::TarZst => Box::new(
            ruzstd::decoding::StreamingDecoder::new(r)
                .map_err(|e| UpdateError::Io(std::io::Error::other(format!("zstd: {e}"))))?,
        ),
    })
}

/// Decompress a tarball into a plain `.tar`, returning `(size, sha256_hex)`
/// of the result. The plain tar is what delta patches operate on.
pub fn decompress_to_tar(src: &Path, format: ArchiveFormat, dest: &Path) -> Result<(u64, String)> {
    let input = BufReader::new(fs::File::open(src)?);
    let mut dec = decoder(input, format)?;
    let tmp = dest.with_extension("tar-tmp");
    let mut out = std::io::BufWriter::new(fs::File::create(&tmp)?);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    let res: Result<()> = (|| {
        loop {
            let n = dec.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            total += n as u64;
        }
        out.flush()?;
        Ok(())
    })();
    drop(out);
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dest)?;
    Ok((total, hex::encode(hasher.finalize())))
}

/// Extract a (possibly compressed) tarball file into `dest`.
pub fn extract_file(src: &Path, dest: &Path, opts: &ExtractOptions) -> Result<ExtractStats> {
    let format = detect_format(src)?;
    let dec = decoder(BufReader::new(fs::File::open(src)?), format)?;
    extract_tar(dec, dest, opts)
}

fn unsafe_entry(path: &Path, reason: &str) -> UpdateError {
    UpdateError::UnsafeArchive {
        path: path.display().to_string(),
        reason: reason.to_string(),
    }
}

/// Validate an entry path and return it as a clean relative path.
fn sanitize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => return Err(unsafe_entry(path, "contains '..'")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_entry(path, "absolute path"))
            }
        }
    }
    Ok(out)
}

/// A relative symlink target must not climb above the archive root when
/// resolved from the link's directory.
fn symlink_target_ok(link_rel: &Path, target: &Path) -> bool {
    if target.is_absolute() || target.as_os_str().is_empty() {
        return false;
    }
    let mut depth = link_rel.components().count() as i64 - 1;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Refuse to traverse any existing symlink between `root` and the entry.
fn check_no_symlink_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        cur.push(c);
        if let Ok(md) = fs::symlink_metadata(&cur) {
            if md.file_type().is_symlink() {
                return Err(unsafe_entry(rel, "parent directory is a symlink"));
            }
            if !md.is_dir() {
                return Err(unsafe_entry(rel, "parent is not a directory"));
            }
        }
    }
    Ok(())
}

/// Remove whatever currently sits at `p` (file or symlink; never follows).
fn clear_path(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => Err(unsafe_entry(p, "would replace a directory")),
        Ok(_) => Ok(fs::remove_file(p)?),
        Err(_) => Ok(()),
    }
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "symlinks unsupported here: {}",
        link.display()
    )))
}

/// Extract an uncompressed tar stream into `dest` (created if missing).
pub fn extract_tar<R: Read>(reader: R, dest: &Path, opts: &ExtractOptions) -> Result<ExtractStats> {
    fs::create_dir_all(dest)?;
    let mut ar = tar::Archive::new(reader);
    let mut stats = ExtractStats::default();
    for (i, entry) in ar.entries()?.enumerate() {
        if i >= opts.max_entries {
            return Err(unsafe_entry(dest, "too many entries"));
        }
        let mut entry = entry?;
        let raw_path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        let rel = sanitize(&raw_path)?;
        if rel.as_os_str().is_empty() {
            continue; // "./"
        }
        check_no_symlink_parents(dest, &rel)?;
        let out = dest.join(&rel);
        let mode = entry.header().mode().unwrap_or(0o644);
        match kind {
            tar::EntryType::Directory => {
                if fs::symlink_metadata(&out).is_ok_and(|m| !m.is_dir()) {
                    return Err(unsafe_entry(&rel, "directory collides with a file"));
                }
                fs::create_dir_all(&out)?;
                set_mode(&out, (mode | 0o700) & 0o755)?;
                stats.dirs += 1;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let size = entry.header().size()?;
                if stats.bytes + size > opts.max_total_bytes {
                    return Err(unsafe_entry(&rel, "archive exceeds size limit"));
                }
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                clear_path(&out)?;
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&out)?;
                let n = std::io::copy(&mut (&mut entry).take(size), &mut f)?;
                if n != size {
                    return Err(unsafe_entry(&rel, "truncated entry"));
                }
                drop(f);
                set_mode(&out, (mode | 0o600) & 0o755)?;
                stats.files += 1;
                stats.bytes += size;
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| unsafe_entry(&rel, "symlink without target"))?
                    .into_owned();
                if !symlink_target_ok(&rel, &target) {
                    return Err(unsafe_entry(&rel, "symlink points outside the archive"));
                }
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent)?;
                }
                clear_path(&out)?;
                make_symlink(&target, &out)?;
                stats.symlinks += 1;
            }
            tar::EntryType::Link => return Err(unsafe_entry(&rel, "hard links are not allowed")),
            other => {
                return Err(unsafe_entry(
                    &rel,
                    &format!("unsupported entry type {other:?}"),
                ));
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Append an entry with a raw (unchecked) path, for malicious fixtures.
    fn raw_entry(
        b: &mut tar::Builder<Vec<u8>>,
        path: &str,
        kind: tar::EntryType,
        link: Option<&str>,
        data: &[u8],
    ) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        let name = &mut h.as_old_mut().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        if let Some(l) = link {
            h.as_old_mut().linkname[..l.len()].copy_from_slice(l.as_bytes());
        }
        h.set_cksum();
        b.append(&h, data).unwrap();
    }

    /// A small, well-formed release-shaped tar.
    pub(crate) fn sample_tar(version: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut add = |path: &str, data: &[u8], mode: u32| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, path, data).unwrap();
        };
        add(
            "frameplayer.sh",
            b"#!/bin/sh\nexec ./current/bin/frameplayer\n",
            0o755,
        );
        add("RELEASE", format!("{version}\n").as_bytes(), 0o644);
        add(
            &format!("versions/{version}/bin/frameplayer"),
            format!("binary {version}").as_bytes(),
            0o4755,
        );
        add(
            &format!("versions/{version}/share/readme.txt"),
            b"hello",
            0o644,
        );
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(
            &mut h,
            format!("versions/{version}/lib/libfoo.so"),
            "libfoo.so.1",
        )
        .unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn extracts_well_formed_tar() {
        let dir = tempfile::tempdir().unwrap();
        let stats = extract_tar(
            sample_tar("1.0.0").as_slice(),
            dir.path(),
            &ExtractOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.files, 4);
        assert_eq!(stats.symlinks, 1);
        let bin = dir.path().join("versions/1.0.0/bin/frameplayer");
        assert_eq!(fs::read(&bin).unwrap(), b"binary 1.0.0");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o755, "setuid stripped, exec kept");
        }
        let link = dir.path().join("versions/1.0.0/lib/libfoo.so");
        assert_eq!(fs::read_link(link).unwrap(), PathBuf::from("libfoo.so.1"));
    }

    #[test]
    fn rejects_traversal_and_absolute() {
        for path in ["../evil", "a/../../evil", "/etc/evil", "./../x"] {
            let mut b = tar::Builder::new(Vec::new());
            raw_entry(&mut b, path, tar::EntryType::Regular, None, b"x");
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("out");
            let err = extract_tar(
                b.into_inner().unwrap().as_slice(),
                &target,
                &ExtractOptions::default(),
            );
            assert!(
                matches!(err, Err(UpdateError::UnsafeArchive { .. })),
                "accepted {path}"
            );
            assert!(!dir.path().join("evil").exists());
        }
    }

    #[test]
    fn rejects_escaping_symlinks_and_writes_through_links() {
        let cases: Vec<Vec<(&str, tar::EntryType, Option<&str>)>> = vec![
            vec![("link", tar::EntryType::Symlink, Some("/etc"))],
            vec![("link", tar::EntryType::Symlink, Some("../outside"))],
            vec![("a/link", tar::EntryType::Symlink, Some("../../x"))],
            // In-tree link to a dir, then a write through it.
            vec![
                ("d", tar::EntryType::Symlink, Some(".")),
                ("d/x", tar::EntryType::Symlink, Some("..")),
            ],
            vec![("h", tar::EntryType::Link, Some("frameplayer.sh"))],
            vec![("dev", tar::EntryType::Char, None)],
            vec![("fifo", tar::EntryType::Fifo, None)],
        ];
        for case in cases {
            let mut b = tar::Builder::new(Vec::new());
            for (p, k, l) in &case {
                raw_entry(&mut b, p, *k, *l, b"");
            }
            let dir = tempfile::tempdir().unwrap();
            let err = extract_tar(
                b.into_inner().unwrap().as_slice(),
                dir.path(),
                &ExtractOptions::default(),
            );
            assert!(
                matches!(err, Err(UpdateError::UnsafeArchive { .. })),
                "accepted {case:?}"
            );
        }
    }

    #[test]
    fn allows_in_tree_relative_symlink() {
        let mut b = tar::Builder::new(Vec::new());
        raw_entry(
            &mut b,
            "a/b/link",
            tar::EntryType::Symlink,
            Some("../../c"),
            b"",
        );
        let dir = tempfile::tempdir().unwrap();
        extract_tar(
            b.into_inner().unwrap().as_slice(),
            dir.path(),
            &ExtractOptions::default(),
        )
        .unwrap();
    }

    #[test]
    fn size_limit_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let opts = ExtractOptions {
            max_total_bytes: 10,
            ..Default::default()
        };
        assert!(extract_tar(sample_tar("1.0.0").as_slice(), dir.path(), &opts).is_err());
    }

    #[test]
    fn gzip_and_zstd_roundtrip() {
        let tar_bytes = sample_tar("2.0.0");
        let dir = tempfile::tempdir().unwrap();

        let gz_path = dir.path().join("r.tar.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        fs::write(&gz_path, gz.finish().unwrap()).unwrap();

        let zst_path = dir.path().join("r.tar.zst");
        let zst = ruzstd::encoding::compress_to_vec(
            tar_bytes.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        fs::write(&zst_path, zst).unwrap();

        for (p, fmt) in [
            (&gz_path, ArchiveFormat::TarGz),
            (&zst_path, ArchiveFormat::TarZst),
        ] {
            assert_eq!(detect_format(p).unwrap(), fmt);
            let plain = dir.path().join("plain.tar");
            let (size, sha) = decompress_to_tar(p, fmt, &plain).unwrap();
            assert_eq!(size, tar_bytes.len() as u64);
            assert_eq!(sha, crate::sha256_hex(&tar_bytes));
            let out = dir.path().join(format!("out-{}", fmt.extension()));
            extract_file(p, &out, &ExtractOptions::default()).unwrap();
            assert!(out.join("versions/2.0.0/bin/frameplayer").exists());
        }
    }

    #[test]
    fn sanitize_strips_curdir() {
        assert_eq!(
            sanitize(Path::new("./a/./b")).unwrap(),
            PathBuf::from("a/b")
        );
        assert!(symlink_target_ok(Path::new("x"), Path::new("y/z")));
        assert!(!symlink_target_ok(Path::new("x"), Path::new("..")));
        assert!(symlink_target_ok(Path::new("a/x"), Path::new("..")));
    }
}
