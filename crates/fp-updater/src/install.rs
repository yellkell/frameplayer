//! Safe extraction, validation, atomic swap and rollback of an installation
//! directory.
//!
//! Layout on disk, for `install_dir = ~/frameplayer`:
//!
//! ```text
//! ~/frameplayer        current version (binary, lib/, assets/, VERSION)
//! ~/frameplayer.old    the one previous version kept for rollback
//! ~/frameplayer.new    staging area while an install is in progress
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use semver::Version;

use crate::error::{Error, IoContext, Result};

/// Name of the main binary inside the install directory.
pub const BINARY_NAME: &str = "frameplayer";
/// Name of the launcher script inside the install directory.
pub const LAUNCHER_NAME: &str = "frameplayer.sh";
/// Name of the version file inside the install directory.
pub const VERSION_FILE: &str = "VERSION";
/// Directory inside the install holding Steam library artwork.
pub const STEAM_ASSETS_DIR: &str = "assets/steam";

/// Hard limits that keep a hostile archive from filling the disk.
const MAX_ENTRIES: usize = 100_000;
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// What [`inspect_zip`] learned about a release archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInfo {
    /// Version from the archive's `VERSION` file.
    pub version: Version,
    /// Single top-level directory every entry sits in (normally
    /// `frameplayer`), stripped on extraction.
    pub root: Option<String>,
    /// Number of entries.
    pub entries: usize,
    /// Sum of uncompressed sizes as declared by the archive.
    pub uncompressed_size: u64,
    /// Whether `frameplayer.sh` is present.
    pub has_launcher: bool,
    /// Files under `assets/steam/` (paths relative to the install root).
    pub steam_assets: Vec<String>,
}

/// Splits an archive entry name into path components, rejecting anything
/// that could escape the extraction directory: absolute paths, `..`,
/// Windows drive letters and backslashes, NUL bytes. `.` and empty
/// components are dropped.
pub fn safe_entry_components(name: &str) -> Result<Vec<String>> {
    let unsafe_path = || Error::UnsafeArchivePath(name.to_string());
    if name.is_empty() || name.contains('\0') || name.contains('\\') || name.starts_with('/') {
        return Err(unsafe_path());
    }
    let mut out = Vec::new();
    for comp in name.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return Err(unsafe_path()),
            c => {
                let b = c.as_bytes();
                if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                    return Err(unsafe_path());
                }
                // Defence in depth: the platform must agree it is one
                // ordinary component.
                let mut comps = Path::new(c).components();
                match (comps.next(), comps.next()) {
                    (Some(Component::Normal(_)), None) => {}
                    _ => return Err(unsafe_path()),
                }
                out.push(c.to_string());
            }
        }
    }
    if out.is_empty() {
        return Err(unsafe_path());
    }
    Ok(out)
}

/// File type bits from a Unix mode.
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;

struct Entry {
    comps: Vec<String>,
    is_dir: bool,
    mode: Option<u32>,
}

fn classify(file: &zip::read::ZipFile<'_, File>) -> Result<Entry> {
    let name = file.name().to_string();
    let comps = safe_entry_components(&name)?;
    let mode = file.unix_mode();
    if file.is_symlink() {
        return Err(Error::UnsupportedArchiveEntry(name));
    }
    if let Some(m) = mode {
        let kind = m & S_IFMT;
        if kind != 0 && kind != S_IFREG && kind != S_IFDIR {
            return Err(Error::UnsupportedArchiveEntry(name));
        }
    }
    Ok(Entry {
        comps,
        is_dir: file.is_dir(),
        mode,
    })
}

fn open_archive(zip_path: &Path) -> Result<zip::ZipArchive<File>> {
    let f = File::open(zip_path).ctx("cannot open", zip_path)?;
    let archive = zip::ZipArchive::new(f)?;
    if archive.len() > MAX_ENTRIES {
        return Err(Error::ArchiveTooLarge(format!(
            "{} entries (limit {MAX_ENTRIES})",
            archive.len()
        )));
    }
    Ok(archive)
}

/// The single top-level directory shared by every entry, if there is one
/// and at least one entry lies inside it.
fn common_root(entries: &[Entry]) -> Option<String> {
    let first = entries.first()?.comps.first()?.clone();
    let all_under = entries
        .iter()
        .all(|e| e.comps.first() == Some(&first) && (e.comps.len() > 1 || e.is_dir));
    let any_inside = entries.iter().any(|e| e.comps.len() > 1);
    (all_under && any_inside).then_some(first)
}

/// Validates every entry of a release zip without extracting it, and reads
/// its `VERSION`. Fails on unsafe paths, links, special files, size limits,
/// a missing binary or a malformed version.
pub fn inspect_zip(zip_path: &Path) -> Result<ArchiveInfo> {
    let mut archive = open_archive(zip_path)?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut total = 0u64;
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        total = total.saturating_add(file.size());
        entries.push(classify(&file)?);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(Error::ArchiveTooLarge(format!(
            "{total} bytes uncompressed"
        )));
    }
    let root = common_root(&entries);
    let strip = usize::from(root.is_some());
    let rel = |e: &Entry| e.comps[strip..].join("/");
    let mut version_idx = None;
    let mut has_binary = false;
    let mut has_launcher = false;
    let mut steam_assets = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir {
            continue;
        }
        let r = rel(e);
        match r.as_str() {
            VERSION_FILE => version_idx = Some(i),
            BINARY_NAME => has_binary = true,
            LAUNCHER_NAME => has_launcher = true,
            _ => {
                if r.starts_with(&format!("{STEAM_ASSETS_DIR}/")) {
                    steam_assets.push(r);
                }
            }
        }
    }
    if !has_binary {
        return Err(Error::InvalidInstall(format!(
            "archive has no {BINARY_NAME} binary"
        )));
    }
    let idx = version_idx
        .ok_or_else(|| Error::InvalidInstall(format!("archive has no {VERSION_FILE} file")))?;
    let mut text = String::new();
    archive
        .by_index(idx)?
        .take(1024)
        .read_to_string(&mut text)
        .map_err(|e| Error::InvalidInstall(format!("unreadable {VERSION_FILE}: {e}")))?;
    let version = parse_version_text(&text)?;
    Ok(ArchiveInfo {
        version,
        root,
        entries: entries.len(),
        uncompressed_size: total,
        has_launcher,
        steam_assets,
    })
}

fn parse_version_text(text: &str) -> Result<Version> {
    Version::parse(text.trim()).map_err(|e| {
        Error::InvalidInstall(format!(
            "{VERSION_FILE} {:?} is not semver: {e}",
            text.trim()
        ))
    })
}

/// Extracts `zip_path` into `dest`, which must not exist yet. A single
/// top-level directory (normally `frameplayer/`) is stripped. Files are
/// created with `create_new`, so duplicate entries fail rather than
/// overwrite; no symbolic links are ever created; permission bits are kept
/// minus setuid/setgid/sticky.
pub fn extract_zip(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut archive = open_archive(zip_path)?;
    let mut entries = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        entries.push(classify(&archive.by_index(i)?)?);
    }
    let strip = usize::from(common_root(&entries).is_some());
    fs::create_dir(dest).ctx("cannot create", dest)?;
    let mut budget = MAX_TOTAL_BYTES;
    for (i, e) in entries.iter().enumerate() {
        let rel = &e.comps[strip..];
        if rel.is_empty() {
            continue;
        }
        let path = rel.iter().fold(dest.to_path_buf(), |p, c| p.join(c));
        if e.is_dir {
            fs::create_dir_all(&path).ctx("cannot create", &path)?;
            set_mode(&path, e.mode, true)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ctx("cannot create", parent)?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ctx("cannot create", &path)?;
        let mut file = archive.by_index(i)?;
        let written = io::copy(&mut (&mut file).take(budget.saturating_add(1)), &mut out)
            .ctx("cannot extract", &path)?;
        if written > budget {
            return Err(Error::ArchiveTooLarge(
                "uncompressed data exceeds the limit".into(),
            ));
        }
        budget -= written;
        out.flush().ctx("cannot write", &path)?;
        drop(out);
        set_mode(&path, e.mode, false)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>, dir: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let base = if dir { 0o755 } else { 0o644 };
    let owner = if dir { 0o700 } else { 0o600 };
    let m = (mode.map_or(base, |m| m & 0o777)) | owner;
    fs::set_permissions(path, fs::Permissions::from_mode(m)).ctx("cannot set permissions on", path)
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>, _dir: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    true
}

/// Reads and parses `<dir>/VERSION`. `Ok(None)` when the directory or file
/// does not exist.
pub fn installed_version(dir: &Path) -> Result<Option<Version>> {
    let path = dir.join(VERSION_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => parse_version_text(&text).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io("cannot read", path, e)),
    }
}

/// Checks that `dir` holds a runnable FramePlayer: a regular, executable
/// `frameplayer` binary (not a link), an executable launcher if present,
/// and a semver `VERSION` (equal to `expected` when given). Returns the
/// version.
pub fn validate_install_dir(dir: &Path, expected: Option<&Version>) -> Result<Version> {
    let bin = dir.join(BINARY_NAME);
    let meta = fs::symlink_metadata(&bin)
        .map_err(|_| Error::InvalidInstall(format!("{BINARY_NAME} binary is missing")))?;
    if !meta.file_type().is_file() {
        return Err(Error::InvalidInstall(format!(
            "{BINARY_NAME} is not a regular file"
        )));
    }
    if !is_executable(&meta) {
        return Err(Error::InvalidInstall(format!(
            "{BINARY_NAME} is not executable"
        )));
    }
    let launcher = dir.join(LAUNCHER_NAME);
    if let Ok(m) = fs::symlink_metadata(&launcher)
        && (!m.file_type().is_file() || !is_executable(&m))
    {
        return Err(Error::InvalidInstall(format!(
            "{LAUNCHER_NAME} is not an executable regular file"
        )));
    }
    let version = installed_version(dir)?
        .ok_or_else(|| Error::InvalidInstall(format!("{VERSION_FILE} file is missing")))?;
    if let Some(exp) = expected
        && exp != &version
    {
        return Err(Error::VersionMismatch {
            expected: exp.to_string(),
            found: version.to_string(),
        });
    }
    Ok(version)
}

/// `<install_dir><suffix>` as a sibling path (`~/frameplayer.old`).
pub fn sibling(install_dir: &Path, suffix: &str) -> PathBuf {
    let mut name = install_dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "frameplayer".into());
    name.push(suffix);
    install_dir.with_file_name(name)
}

/// Removes a file, link or directory tree at `path` without following a
/// link at `path` itself. Missing paths are fine.
fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io("cannot inspect", path, e)),
        Ok(m) if m.file_type().is_dir() => fs::remove_dir_all(path).ctx("cannot remove", path),
        Ok(_) => fs::remove_file(path).ctx("cannot remove", path),
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn check_install_dir(install_dir: &Path) -> Result<()> {
    match install_dir.file_name() {
        Some(n) if !n.is_empty() => Ok(()),
        _ => Err(Error::InvalidInstall(format!(
            "install directory {} has no final path component",
            install_dir.display()
        ))),
    }
}

/// Installs the release zip at `zip_path` into `install_dir`.
///
/// Steps: extract into `<install_dir>.new`, validate it (binary present and
/// executable, `VERSION` is semver), then swap by renames:
/// `install_dir → install_dir.old`, `.new → install_dir`. Any older `.old`
/// is deleted first, so exactly one previous version is kept. If the second
/// rename fails, the first is undone. Nothing in `install_dir` is touched
/// until the new tree has been fully extracted and validated.
pub fn install(zip_path: &Path, install_dir: &Path) -> Result<Version> {
    install_impl(zip_path, install_dir, None)
}

/// Like [`install`] but also requires the archive's `VERSION` to equal
/// `expected` (the version from the signed manifest).
pub fn install_expecting(
    zip_path: &Path,
    install_dir: &Path,
    expected: &Version,
) -> Result<Version> {
    install_impl(zip_path, install_dir, Some(expected))
}

fn install_impl(
    zip_path: &Path,
    install_dir: &Path,
    expected: Option<&Version>,
) -> Result<Version> {
    check_install_dir(install_dir)?;
    if let Some(parent) = install_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).ctx("cannot create", parent)?;
    }
    let staging = sibling(install_dir, ".new");
    remove_any(&staging)?;
    let staged =
        extract_zip(zip_path, &staging).and_then(|()| validate_install_dir(&staging, expected));
    let version = match staged {
        Ok(v) => v,
        Err(e) => {
            let _ = remove_any(&staging);
            return Err(e);
        }
    };
    swap_in(&staging, install_dir)?;
    Ok(version)
}

fn swap_in(staging: &Path, install_dir: &Path) -> Result<()> {
    let old = sibling(install_dir, ".old");
    if exists(install_dir) {
        remove_any(&old)?;
        fs::rename(install_dir, &old).ctx("cannot move aside", install_dir)?;
        if let Err(e) = fs::rename(staging, install_dir) {
            let _ = fs::rename(&old, install_dir);
            return Err(Error::io("cannot move into place", staging, e));
        }
    } else {
        fs::rename(staging, install_dir).ctx("cannot move into place", staging)?;
    }
    Ok(())
}

/// Swaps the current installation with the kept previous version
/// (`<install_dir>.old`), after validating the previous one. The current
/// version becomes the new `.old`, so calling this twice rolls forward
/// again. Returns the version now installed.
pub fn rollback(install_dir: &Path) -> Result<Version> {
    check_install_dir(install_dir)?;
    let old = sibling(install_dir, ".old");
    if !fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir()) {
        return Err(Error::NoPreviousVersion(old));
    }
    let version = validate_install_dir(&old, None)?;
    let tmp = sibling(install_dir, ".rollback");
    remove_any(&tmp)?;
    let had_current = exists(install_dir);
    if had_current {
        fs::rename(install_dir, &tmp).ctx("cannot move aside", install_dir)?;
    }
    if let Err(e) = fs::rename(&old, install_dir) {
        if had_current {
            let _ = fs::rename(&tmp, install_dir);
        }
        return Err(Error::io("cannot restore", &old, e));
    }
    if had_current {
        fs::rename(&tmp, &old).ctx("cannot keep previous version", &tmp)?;
    }
    Ok(version)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    /// One entry for [`make_zip`].
    pub(crate) enum Item<'a> {
        File(&'a str, &'a [u8], u32),
        Dir(&'a str),
        Symlink(&'a str, &'a str),
    }

    pub(crate) fn make_zip(path: &Path, items: &[Item<'_>]) {
        let f = File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        for item in items {
            match item {
                Item::File(name, data, mode) => {
                    let opts = SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated)
                        .unix_permissions(*mode);
                    w.start_file(*name, opts).unwrap();
                    w.write_all(data).unwrap();
                }
                Item::Dir(name) => {
                    w.add_directory(*name, SimpleFileOptions::default())
                        .unwrap();
                }
                Item::Symlink(name, target) => {
                    w.add_symlink(*name, *target, SimpleFileOptions::default())
                        .unwrap();
                }
            }
        }
        w.finish().unwrap();
    }

    pub(crate) fn release_zip(path: &Path, version: &str) {
        make_zip(
            path,
            &[
                Item::Dir("frameplayer/"),
                Item::File("frameplayer/frameplayer", b"\x7fELF fake", 0o755),
                Item::File("frameplayer/frameplayer.sh", b"#!/bin/sh\n", 0o755),
                Item::File(
                    "frameplayer/VERSION",
                    format!("{version}\n").as_bytes(),
                    0o644,
                ),
                Item::File("frameplayer/lib/libavcodec.so.61", b"lib", 0o644),
                Item::File("frameplayer/assets/steam/portrait.png", b"png", 0o644),
            ],
        );
    }

    #[test]
    fn entry_name_rules() {
        assert_eq!(
            safe_entry_components("frameplayer/./lib//a.so").unwrap(),
            ["frameplayer", "lib", "a.so"]
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/..",
            "a\\..\\x",
            "C:/x",
            "c:x",
            "a\0b",
            "./",
        ] {
            assert!(safe_entry_components(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn inspect_reports_version_and_assets() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("r.zip");
        release_zip(&zip, "1.2.3");
        let info = inspect_zip(&zip).unwrap();
        assert_eq!(info.version, Version::new(1, 2, 3));
        assert_eq!(info.root.as_deref(), Some("frameplayer"));
        assert!(info.has_launcher);
        assert_eq!(info.steam_assets, ["assets/steam/portrait.png"]);
    }

    #[cfg(unix)]
    #[test]
    fn install_swap_and_rollback() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z1 = tmp.path().join("1.zip");
        let z2 = tmp.path().join("2.zip");
        let z3 = tmp.path().join("3.zip");
        release_zip(&z1, "1.0.0");
        release_zip(&z2, "1.1.0");
        release_zip(&z3, "1.2.0");

        assert!(matches!(rollback(&dir), Err(Error::NoPreviousVersion(_))));
        assert_eq!(install(&z1, &dir).unwrap(), Version::new(1, 0, 0));
        assert!(!sibling(&dir, ".old").exists());
        let mode = fs::metadata(dir.join("frameplayer"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(fs::read(dir.join("lib/libavcodec.so.61")).unwrap(), b"lib");

        install_expecting(&z2, &dir, &Version::new(1, 1, 0)).unwrap();
        assert_eq!(
            installed_version(&dir).unwrap(),
            Some(Version::new(1, 1, 0))
        );
        assert_eq!(
            installed_version(&sibling(&dir, ".old")).unwrap(),
            Some(Version::new(1, 0, 0))
        );

        install(&z3, &dir).unwrap();
        // Exactly one previous version kept.
        assert_eq!(
            installed_version(&sibling(&dir, ".old")).unwrap(),
            Some(Version::new(1, 1, 0))
        );
        let siblings: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("frameplayer"))
            .collect();
        assert_eq!(siblings.len(), 2, "{siblings:?}");

        assert_eq!(rollback(&dir).unwrap(), Version::new(1, 1, 0));
        assert_eq!(
            installed_version(&dir).unwrap(),
            Some(Version::new(1, 1, 0))
        );
        assert_eq!(
            installed_version(&sibling(&dir, ".old")).unwrap(),
            Some(Version::new(1, 2, 0))
        );
        // Rolling back again rolls forward.
        assert_eq!(rollback(&dir).unwrap(), Version::new(1, 2, 0));
    }

    #[test]
    fn version_mismatch_leaves_install_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z1 = tmp.path().join("1.zip");
        let z2 = tmp.path().join("2.zip");
        release_zip(&z1, "1.0.0");
        release_zip(&z2, "1.1.0");
        install(&z1, &dir).unwrap();
        let err = install_expecting(&z2, &dir, &Version::new(9, 9, 9)).unwrap_err();
        assert!(matches!(err, Error::VersionMismatch { .. }));
        assert_eq!(
            installed_version(&dir).unwrap(),
            Some(Version::new(1, 0, 0))
        );
        assert!(!sibling(&dir, ".new").exists());
        assert!(!sibling(&dir, ".old").exists());
    }

    #[test]
    fn zip_slip_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inst").join("frameplayer");
        for (i, evil) in ["frameplayer/../../evil", "/tmp/evil", "..\\evil", "C:/evil"]
            .iter()
            .enumerate()
        {
            let z = tmp.path().join(format!("evil{i}.zip"));
            make_zip(
                &z,
                &[
                    Item::File("frameplayer/frameplayer", b"bin", 0o755),
                    Item::File("frameplayer/VERSION", b"1.0.0", 0o644),
                    Item::File(evil, b"pwned", 0o644),
                ],
            );
            let err = install(&z, &dir).unwrap_err();
            assert!(
                matches!(err, Error::UnsafeArchivePath(_)),
                "{evil}: {err:?}"
            );
            assert!(inspect_zip(&z).is_err());
        }
        assert!(!tmp.path().join("evil").exists());
        assert!(!dir.exists());
        assert!(!sibling(&dir, ".new").exists());
    }

    #[test]
    fn symlinks_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z = tmp.path().join("link.zip");
        make_zip(
            &z,
            &[
                Item::File("frameplayer/frameplayer", b"bin", 0o755),
                Item::File("frameplayer/VERSION", b"1.0.0", 0o644),
                Item::Symlink("frameplayer/lib", "/etc"),
                Item::File("frameplayer/lib/passwd", b"x", 0o644),
            ],
        );
        let err = install(&z, &dir).unwrap_err();
        assert!(matches!(err, Error::UnsupportedArchiveEntry(_)), "{err:?}");
        assert!(!dir.exists());
    }

    #[test]
    fn duplicate_entries_do_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z = tmp.path().join("dup.zip");
        // ZipWriter refuses duplicate names, so build one with distinct
        // spellings that normalise to the same path.
        make_zip(
            &z,
            &[
                Item::File("frameplayer/frameplayer", b"bin", 0o755),
                Item::File("frameplayer/./frameplayer", b"evil", 0o755),
                Item::File("frameplayer/VERSION", b"1.0.0", 0o644),
            ],
        );
        assert!(matches!(install(&z, &dir), Err(Error::Io { .. })));
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_binary_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z = tmp.path().join("noexec.zip");
        make_zip(
            &z,
            &[
                Item::File("frameplayer/frameplayer", b"bin", 0o644),
                Item::File("frameplayer/VERSION", b"1.0.0", 0o644),
            ],
        );
        let err = install(&z, &dir).unwrap_err();
        assert!(matches!(err, Error::InvalidInstall(_)), "{err:?}");
    }

    #[test]
    fn missing_binary_or_version_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z = tmp.path().join("nobin.zip");
        make_zip(&z, &[Item::File("frameplayer/VERSION", b"1.0.0", 0o644)]);
        assert!(matches!(install(&z, &dir), Err(Error::InvalidInstall(_))));
        let z = tmp.path().join("nover.zip");
        make_zip(&z, &[Item::File("frameplayer/frameplayer", b"x", 0o755)]);
        assert!(matches!(install(&z, &dir), Err(Error::InvalidInstall(_))));
        let z = tmp.path().join("badver.zip");
        make_zip(
            &z,
            &[
                Item::File("frameplayer/frameplayer", b"x", 0o755),
                Item::File("frameplayer/VERSION", b"latest", 0o644),
            ],
        );
        assert!(matches!(install(&z, &dir), Err(Error::InvalidInstall(_))));
    }

    #[cfg(unix)]
    #[test]
    fn setuid_bits_are_stripped() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("frameplayer");
        let z = tmp.path().join("suid.zip");
        make_zip(
            &z,
            &[
                Item::File("frameplayer/frameplayer", b"x", 0o4755),
                Item::File("frameplayer/VERSION", b"1.0.0", 0o644),
            ],
        );
        install(&z, &dir).unwrap();
        let mode = fs::metadata(dir.join("frameplayer"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn flat_archive_without_root_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fp");
        let z = tmp.path().join("flat.zip");
        make_zip(
            &z,
            &[
                Item::File("frameplayer", b"x", 0o755),
                Item::File("VERSION", b"2.0.0", 0o644),
            ],
        );
        assert_eq!(install(&z, &dir).unwrap(), Version::new(2, 0, 0));
        assert!(dir.join("frameplayer").is_file());
    }
}
