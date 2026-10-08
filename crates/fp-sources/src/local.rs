//! Local folders: internal storage, microSD cards, USB drives.
//!
//! Locations are absolute paths. Files open as [`fp_core::source::FileSource`].

use crate::config::{LocalConfig, SourceKind};
use crate::error::{Error, Result};
use crate::timeutil::system_time_to_unix;
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry, FileSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A local folder.
#[derive(Clone, Debug)]
pub struct LocalSource {
    id: String,
    name: String,
    root: PathBuf,
}

impl LocalSource {
    /// Creates a source rooted at `config.root`.
    pub fn new(config: &LocalConfig) -> LocalSource {
        LocalSource {
            id: config.id.clone(),
            name: config.name.clone(),
            root: config.root.clone(),
        }
    }

    /// Folder listed by `list(None)`.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn not_found_or_io(e: std::io::Error, path: &Path) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::NotFound(path.display().to_string())
    } else {
        Error::Io(e)
    }
}

impl Source for LocalSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Local
    }

    fn describe(&self) -> String {
        format!("local folder {}", self.root.display())
    }

    /// Lists a folder, following symbolic links and skipping hidden files
    /// and names that are not valid UTF-8.
    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let dir = location.map_or_else(|| self.root.clone(), PathBuf::from);
        let read = std::fs::read_dir(&dir).map_err(|e| not_found_or_io(e, &dir))?;
        let mut out = Vec::new();
        for item in read {
            let item = match item {
                Ok(i) => i,
                Err(e) => {
                    log::debug!("skipping unreadable entry in {}: {e}", dir.display());
                    continue;
                }
            };
            let Some(name) = item.file_name().to_str().map(str::to_string) else {
                log::debug!("skipping non-UTF-8 name in {}", dir.display());
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = item.path();
            // `metadata` follows symlinks; broken links are skipped.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Some(location) = path.to_str().map(str::to_string) else {
                continue;
            };
            let modified = meta.modified().ok().map(system_time_to_unix);
            if meta.is_dir() {
                out.push(dir_entry(name, location, modified));
            } else if meta.is_file() {
                out.push(file_entry(name, location, Some(meta.len()), modified));
            }
        }
        sort_entries(&mut out);
        Ok(out)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        let path = Path::new(location);
        let file = FileSource::open(path).map_err(|e| not_found_or_io(e, path))?;
        Ok(Arc::new(file))
    }

    fn parent(&self, location: &str) -> Option<String> {
        Path::new(location)
            .parent()
            .and_then(Path::to_str)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::Projection;
    use fp_core::source::EntryKind;

    fn source(root: &Path) -> LocalSource {
        LocalSource::new(&LocalConfig {
            id: "l".into(),
            name: "Local".into(),
            root: root.to_path_buf(),
        })
    }

    #[test]
    fn lists_reads_and_finds_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("Sub Folder")).unwrap();
        std::fs::write(root.join("Trip_360_TB.mkv"), b"0123456789").unwrap();
        std::fs::write(root.join("Trip_360_TB.funscript"), b"{}").unwrap();
        std::fs::write(root.join("Trip_360_TB.pitch.funscript"), b"{}").unwrap();
        std::fs::write(root.join("Trip_360_TB.en.srt"), b"1").unwrap();
        std::fs::write(root.join(".hidden.mp4"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join("broken.mp4")).unwrap();
        let src = source(root);

        let list = src.list(None).unwrap();
        let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Sub Folder",
                "Trip_360_TB.en.srt",
                "Trip_360_TB.funscript",
                "Trip_360_TB.mkv",
                "Trip_360_TB.pitch.funscript",
            ]
        );
        assert_eq!(list[0].kind, EntryKind::Directory);
        let video = list.iter().find(|e| e.kind == EntryKind::Video).unwrap();
        assert_eq!(video.size, Some(10));
        assert!(video.modified.unwrap() > 1_600_000_000);
        assert_eq!(video.format.unwrap().projection, Projection::EQUIRECT_360);
        assert_eq!(list[1].kind, EntryKind::Other);

        let f = src.open(&video.location).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(f.read_at(6, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"6789");
        assert_eq!(f.size(), Some(10));

        let sc = src.sidecars(video).unwrap();
        assert_eq!(sc.scripts.len(), 2);
        assert_eq!(sc.scripts[1].axis.as_deref(), Some("pitch"));
        assert_eq!(sc.subtitles[0].language.as_deref(), Some("en"));

        let sub = src.list(Some(&list[0].location)).unwrap();
        assert!(sub.is_empty());
        assert_eq!(src.parent(&video.location).as_deref(), root.to_str());
    }

    #[test]
    fn missing_paths_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path());
        let missing = dir.path().join("nope");
        assert!(matches!(
            src.list(missing.to_str()),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            src.open(missing.to_str().unwrap()),
            Err(Error::NotFound(_))
        ));
        assert_eq!(src.kind(), SourceKind::Local);
        assert!(src.describe().contains(dir.path().to_str().unwrap()));
    }
}
