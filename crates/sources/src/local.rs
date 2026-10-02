//! Local filesystem source (internal storage, microSD, USB drives) and the
//! removable-media mount watcher.
//!
//! SteamOS (and so presumably the Frame) mounts removable media through
//! udisks2 under `/run/media/<user>/<label>` (older images used
//! `/run/media/<device>`). Without udev/dbus bindings we watch
//! `/proc/self/mountinfo` by polling: it's cheap (a few KB) and catches every
//! mount regardless of who performed it.

use crate::config::SourceKind;
use crate::error::{Result, SourceError};
use crate::source::{Entry, RandomAccess, Source};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Convert a `file://` URI (or plain absolute path) to a path.
pub fn uri_to_path(uri: &str) -> Result<PathBuf> {
    if uri.starts_with('/') {
        return Ok(PathBuf::from(uri));
    }
    let u = url::Url::parse(uri)?;
    if u.scheme() != "file" {
        return Err(SourceError::InvalidUri(format!("not a file URI: {uri}")));
    }
    u.to_file_path()
        .map_err(|_| SourceError::InvalidUri(uri.to_string()))
}

pub fn path_to_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()))
}

/// Directory tree on a locally mounted filesystem.
#[derive(Debug, Clone)]
pub struct LocalSource {
    root: PathBuf,
    show_hidden: bool,
}

impl LocalSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalSource {
            root: root.into(),
            show_hidden: false,
        }
    }

    pub fn from_uri(uri: &str) -> Result<Self> {
        Ok(Self::new(uri_to_path(uri)?))
    }

    pub fn show_hidden(mut self, yes: bool) -> Self {
        self.show_hidden = yes;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Default roots worth offering on first launch: `~/Videos` (or home),
    /// plus every currently mounted removable volume.
    pub fn default_roots() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let videos = home.join("Videos");
            out.push(if videos.is_dir() { videos } else { home });
        }
        if let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") {
            out.extend(
                removable_mounts(&parse_mountinfo(&s))
                    .into_iter()
                    .map(|m| m.mount_point),
            );
        }
        out
    }
}

fn mtime_secs(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

#[async_trait]
impl Source for LocalSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Local
    }

    fn root_uri(&self) -> String {
        path_to_uri(&self.root)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let path = if dir.is_empty() {
            self.root.clone()
        } else {
            uri_to_path(dir)?
        };
        let show_hidden = self.show_hidden;
        tokio::task::spawn_blocking(move || -> Result<Vec<Entry>> {
            let mut out = Vec::new();
            for de in std::fs::read_dir(&path).map_err(|e| map_io(e, &path))? {
                let Ok(de) = de else { continue };
                let name = de.file_name().to_string_lossy().into_owned();
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                let p = de.path();
                // Follow symlinks; skip dangling ones.
                let Ok(md) = std::fs::metadata(&p) else {
                    continue;
                };
                out.push(Entry {
                    name,
                    uri: path_to_uri(&p),
                    is_dir: md.is_dir(),
                    size: md.is_file().then_some(md.len()),
                    mtime: mtime_secs(&md),
                    ..Default::default()
                });
            }
            out.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(out)
        })
        .await
        .map_err(|e| SourceError::Io(std::io::Error::other(e)))?
    }

    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
        Ok(Box::new(LocalFile::open(uri_to_path(uri)?).await?))
    }
}

fn map_io(e: std::io::Error, p: &Path) -> SourceError {
    if e.kind() == std::io::ErrorKind::NotFound {
        SourceError::NotFound(p.display().to_string())
    } else {
        SourceError::Io(e)
    }
}

/// An open local file; reads run on the blocking pool via `pread`.
pub struct LocalFile {
    file: Arc<std::fs::File>,
    size: u64,
}

impl LocalFile {
    pub async fn open(path: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            let f = std::fs::File::open(&path).map_err(|e| map_io(e, &path))?;
            let size = f.metadata()?.len();
            Ok(LocalFile {
                file: Arc::new(f),
                size,
            })
        })
        .await
        .map_err(|e| SourceError::Io(std::io::Error::other(e)))?
    }
}

#[async_trait]
impl RandomAccess for LocalFile {
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        if offset >= self.size || len == 0 {
            return Ok(Bytes::new());
        }
        let len = len.min((self.size - offset) as usize);
        let f = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                match f.read_at(&mut buf[done..], offset + done as u64) {
                    Ok(0) => break,
                    Ok(n) => done += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(SourceError::Io(e)),
                }
            }
            buf.truncate(done);
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(|e| SourceError::Io(std::io::Error::other(e)))?
    }

    fn size(&self) -> Option<u64> {
        Some(self.size)
    }
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub device: String,
}

impl MountInfo {
    /// Volume label as udisks names the mount directory.
    pub fn label(&self) -> String {
        self.mount_point
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// "microSD" for mmcblk devices, "USB drive" for sd*, else the label.
    pub fn kind_label(&self) -> &'static str {
        if self.device.contains("mmcblk") {
            "microSD"
        } else if self.device.starts_with("/dev/sd") {
            "USB drive"
        } else {
            "Drive"
        }
    }
}

/// Decode the octal escapes (`\040` = space) mountinfo uses.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `/proc/self/mountinfo` content.
pub fn parse_mountinfo(s: &str) -> Vec<MountInfo> {
    s.lines()
        .filter_map(|line| {
            // id parent maj:min root mount_point opts [optional...] - fstype source superopts
            let (left, right) = line.split_once(" - ")?;
            let mount_point = left.split(' ').nth(4)?;
            let mut r = right.split(' ');
            let fs_type = r.next()?.to_string();
            let device = unescape_mount(r.next().unwrap_or(""));
            Some(MountInfo {
                mount_point: PathBuf::from(unescape_mount(mount_point)),
                fs_type,
                device,
            })
        })
        .collect()
}

const REMOVABLE_FS: &[&str] = &[
    "vfat", "exfat", "ntfs", "ntfs3", "fuseblk", "ext4", "ext3", "ext2", "btrfs", "f2fs", "xfs",
    "hfsplus", "udf", "iso9660",
];

/// Mounts that look like user-visible removable media.
// [verify] Confirm the Frame mounts microSD/USB under /run/media/<user>/<label>
// like the Steam Deck (udisks2 via steamos-automount) rather than /media.
pub fn removable_mounts(all: &[MountInfo]) -> Vec<MountInfo> {
    all.iter()
        .filter(|m| {
            let p = m.mount_point.to_string_lossy();
            (p.starts_with("/run/media/") || p.starts_with("/media/") || p.starts_with("/mnt/"))
                && REMOVABLE_FS.contains(&m.fs_type.as_str())
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountEvent {
    Added(MountInfo),
    Removed(MountInfo),
}

/// Diff two snapshots keyed by mount point.
pub fn diff_mounts(old: &[MountInfo], new: &[MountInfo]) -> Vec<MountEvent> {
    let o: BTreeMap<_, _> = old.iter().map(|m| (&m.mount_point, m)).collect();
    let n: BTreeMap<_, _> = new.iter().map(|m| (&m.mount_point, m)).collect();
    let mut ev: Vec<MountEvent> = n
        .iter()
        .filter(|(k, _)| !o.contains_key(*k))
        .map(|(_, m)| MountEvent::Added((*m).clone()))
        .collect();
    ev.extend(
        o.iter()
            .filter(|(k, _)| !n.contains_key(*k))
            .map(|(_, m)| MountEvent::Removed((*m).clone())),
    );
    ev
}

/// Polls mountinfo and sends [`MountEvent`]s for removable volumes.
pub struct MountWatcher {
    task: tokio::task::JoinHandle<()>,
}

impl MountWatcher {
    /// Start watching on the current tokio runtime. Mounts present at start
    /// are reported as `Added` first so the UI can populate its list.
    pub fn spawn(interval: Duration) -> (MountWatcher, mpsc::Receiver<MountEvent>) {
        Self::spawn_with_path(PathBuf::from("/proc/self/mountinfo"), interval)
    }

    /// As [`spawn`](Self::spawn) but reading an arbitrary mountinfo file (tests).
    pub fn spawn_with_path(
        path: PathBuf,
        interval: Duration,
    ) -> (MountWatcher, mpsc::Receiver<MountEvent>) {
        let (tx, rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let mut current: Vec<MountInfo> = Vec::new();
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Ok(s) = tokio::fs::read_to_string(&path).await else {
                    continue;
                };
                let now = removable_mounts(&parse_mountinfo(&s));
                for ev in diff_mounts(&current, &now) {
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
                current = now;
            }
        });
        (MountWatcher { task }, rx)
    }
}

impl Drop for MountWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
35 22 0:31 / /proc rw,nosuid shared:12 - proc proc rw
120 22 179:1 / /run/media/deck/SD\\040Card rw,nosuid,nodev,relatime shared:60 - exfat /dev/mmcblk0p1 rw
121 22 8:1 / /run/media/deck/USB rw,nosuid shared:61 master:3 - vfat /dev/sda1 rw
122 22 0:50 / /run/media/deck/tmp rw - tmpfs tmpfs rw
";

    #[test]
    fn parse_and_filter_mounts() {
        let all = parse_mountinfo(MOUNTINFO);
        assert_eq!(all.len(), 5);
        let rem = removable_mounts(&all);
        assert_eq!(rem.len(), 2);
        assert_eq!(rem[0].mount_point, PathBuf::from("/run/media/deck/SD Card"));
        assert_eq!(rem[0].label(), "SD Card");
        assert_eq!(rem[0].kind_label(), "microSD");
        assert_eq!(rem[1].kind_label(), "USB drive");
    }

    #[test]
    fn mount_diff() {
        let all = removable_mounts(&parse_mountinfo(MOUNTINFO));
        let ev = diff_mounts(&all[..1], &all[1..]);
        assert_eq!(
            ev,
            vec![
                MountEvent::Added(all[1].clone()),
                MountEvent::Removed(all[0].clone())
            ]
        );
    }

    #[tokio::test]
    async fn watcher_emits_events() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mountinfo");
        std::fs::write(&p, "").unwrap();
        let (_w, mut rx) = MountWatcher::spawn_with_path(p.clone(), Duration::from_millis(10));
        std::fs::write(&p, MOUNTINFO).unwrap();
        let a = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(a, MountEvent::Added(_)));
        let _ = rx.recv().await;
        std::fs::write(&p, "").unwrap();
        let r = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(r, MountEvent::Removed(_)));
    }

    #[tokio::test]
    async fn list_and_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.mp4"), b"0123456789").unwrap();
        std::fs::write(dir.path().join(".hidden.mp4"), b"x").unwrap();
        let src = LocalSource::new(dir.path());
        let list = src.list("").await.unwrap();
        assert_eq!(
            list.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["sub", "b.mp4"]
        );
        assert!(list[0].is_dir);
        assert_eq!(list[1].size, Some(10));
        let f = src.open(&list[1].uri).await.unwrap();
        assert_eq!(f.size(), Some(10));
        assert_eq!(&f.read_at(3, 4).await.unwrap()[..], b"3456");
        assert_eq!(&f.read_at(8, 100).await.unwrap()[..], b"89");
        assert!(f.read_at(10, 1).await.unwrap().is_empty());
        assert!(matches!(
            src.open(&path_to_uri(&dir.path().join("nope.mp4"))).await,
            Err(SourceError::NotFound(_))
        ));
    }
}
