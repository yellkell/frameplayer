//! Incremental scanner for local folders.
//!
//! Walks a folder recursively (hidden files and folders skipped), finds
//! videos with [`fp_core::source::is_video_name`], attaches side files
//! (haptic scripts, subtitles) and indexes everything in batched
//! transactions. Unchanged files (same size, mtime and side files) cause no
//! writes; files that disappeared are marked missing, never deleted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use fp_core::source::{is_script_name, is_subtitle_name, is_video_name};
use rusqlite::params;
use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::library::{Library, MediaUpsert, UpsertStatus, detect_name, upsert_conn};
use crate::record::stem_of;

/// Axis names recognised in multi-axis script names (`video.<axis>.funscript`):
/// MultiFunPlayer / OSR names and raw TCode channel ids.
pub const SCRIPT_AXES: &[&str] = &[
    "surge", "sway", "suck", "twist", "roll", "pitch", "vib", "pump", "lube", "valve", "stroke",
    "raw", "l0", "l1", "l2", "r0", "r1", "r2", "v0", "v1", "v2", "a0", "a1", "a2",
];

/// The axis of a multi-axis script location (`clip.roll.funscript` →
/// `Some("roll")`), `None` for a main script or anything else.
pub fn script_axis(location: &str) -> Option<String> {
    let name = crate::record::file_name_of(location);
    if !is_script_name(name) {
        return None;
    }
    let (_, axis) = stem_of(name).rsplit_once('.')?;
    let axis = axis.to_ascii_lowercase();
    SCRIPT_AXES.contains(&axis.as_str()).then_some(axis)
}

/// How a scan behaves.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanOptions {
    /// DeoVR-style fallback folder searched for `stem.funscript` (and axis
    /// scripts) when none sit next to a video. Default:
    /// [`fp_core::dirs::interactive_dir`].
    pub interactive_dir: Option<PathBuf>,
    /// Follow symbolic links to files and folders.
    pub follow_links: bool,
    /// Re-index every file as if it had changed, so the metadata worker
    /// probes and thumbnails it again.
    pub force: bool,
    /// Rows written per transaction; the library lock is released between
    /// batches so the UI stays responsive.
    pub batch_size: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            interactive_dir: Some(fp_core::dirs::interactive_dir()),
            follow_links: true,
            force: false,
            batch_size: 256,
        }
    }
}

/// Stage of a running scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    /// Walking the folder tree.
    Walking,
    /// Writing to the database.
    Indexing,
    /// Marking files that disappeared.
    MarkingMissing,
}

/// Progress passed to the scan callback.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanProgress {
    /// Current stage.
    pub phase: ScanPhase,
    /// Files looked at so far.
    pub files_seen: usize,
    /// Videos found so far.
    pub videos_found: usize,
    /// Videos indexed so far (during [`ScanPhase::Indexing`]).
    pub indexed: usize,
    /// The video just found or indexed.
    pub current: Option<PathBuf>,
}

/// A path the scan could not read.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanError {
    /// File or folder.
    pub path: PathBuf,
    /// What went wrong.
    pub message: String,
}

/// Summary of a finished scan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanReport {
    /// Absolute root that was scanned.
    pub root: PathBuf,
    /// Videos found on disk.
    pub videos: usize,
    /// New rows.
    pub added: usize,
    /// Rows whose file or side files changed.
    pub updated: usize,
    /// Rows left untouched.
    pub unchanged: usize,
    /// Rows that were missing and are back (also counted in `updated`).
    pub restored: usize,
    /// Rows newly marked missing.
    pub missing: usize,
    /// Unreadable paths. Rows below them are not marked missing.
    pub errors: Vec<ScanError>,
}

/// A video found on disk.
struct FoundVideo {
    path: PathBuf,
    location: String,
    name: String,
    size: u64,
    mtime: Option<i64>,
}

#[derive(Default)]
struct DirFiles {
    videos: Vec<FoundVideo>,
    /// Script and subtitle file names.
    side: Vec<String>,
}

/// How a side file relates to a video stem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideKind {
    MainScript,
    AxisScript,
    Subtitle,
}

/// Whether `file` (a side-file name) belongs to a video with stem `stem`.
fn side_kind(stem: &str, file: &str) -> Option<SideKind> {
    let stem_l = stem.to_lowercase();
    let file_l = file.to_lowercase();
    if is_script_name(file) {
        let base = stem_of(&file_l);
        if base == stem_l {
            return Some(SideKind::MainScript);
        }
        let axis = base.strip_prefix(&stem_l)?.strip_prefix('.')?;
        return SCRIPT_AXES.contains(&axis).then_some(SideKind::AxisScript);
    }
    if is_subtitle_name(file) {
        // `stem.srt`, `stem.en.srt`, `stem_eng.srt`, `stem - English.srt`;
        // but not `stem2.srt`, which belongs to another video.
        let rest = file_l.strip_prefix(&stem_l)?;
        return rest
            .chars()
            .next()
            .filter(|c| !c.is_alphanumeric())
            .map(|_| SideKind::Subtitle);
    }
    None
}

/// Scripts tagged with their kind, before sorting.
type KindedScripts = Vec<(SideKind, String)>;

/// For each video (by index into `stems`): scripts (main first, then axes
/// by name) and subtitles. A side file matching several videos goes to the
/// one with the longest stem (`a.b.srt` belongs to `a.b.mp4`, not `a.mp4`).
fn match_side_files(stems: &[&str], side: &[String]) -> Vec<(Vec<String>, Vec<String>)> {
    let mut out: Vec<(KindedScripts, Vec<String>)> =
        stems.iter().map(|_| (Vec::new(), Vec::new())).collect();
    for file in side {
        let best = stems
            .iter()
            .enumerate()
            .filter_map(|(i, s)| side_kind(s, file).map(|k| (i, s.len(), k)))
            .max_by_key(|(_, len, _)| *len);
        if let Some((i, _, kind)) = best {
            match kind {
                SideKind::Subtitle => out[i].1.push(file.clone()),
                k => out[i].0.push((k, file.clone())),
            }
        }
    }
    out.into_iter()
        .map(|(mut scripts, mut subs)| {
            scripts.sort_by(|a, b| {
                (a.0 != SideKind::MainScript, a.1.to_lowercase())
                    .cmp(&(b.0 != SideKind::MainScript, b.1.to_lowercase()))
            });
            subs.sort_by_key(|s| s.to_lowercase());
            (scripts.into_iter().map(|(_, s)| s).collect(), subs)
        })
        .collect()
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok(),
        Err(e) => i64::try_from(e.duration().as_secs()).ok().map(|s| -s),
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Script names in the interactive fallback folder.
fn list_interactive(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_script_name(n) && !n.starts_with('.'))
        .collect()
}

/// State of a row before the scan, for the incremental check.
struct Known {
    source_id: String,
    size: Option<i64>,
    mtime: Option<i64>,
    missing: bool,
    scripts: String,
    subtitles: String,
}

impl Library {
    /// Scans `root` recursively and indexes its videos under `source_id`.
    ///
    /// The file system is walked without holding the library lock; rows
    /// are then written in batches of [`ScanOptions::batch_size`]. Rows of
    /// `source_id` below `root` that were not found are marked missing
    /// (user data is kept, and they come back when the file reappears),
    /// except below folders that could not be read. Fails when `root` is
    /// not a readable folder, so an unmounted drive never marks a whole
    /// library missing.
    pub fn scan_folder(
        &self,
        root: impl AsRef<Path>,
        source_id: &str,
        options: &ScanOptions,
        mut progress: impl FnMut(&ScanProgress),
    ) -> Result<ScanReport> {
        let root = std::path::absolute(root.as_ref()).map_err(|e| Error::io(root.as_ref(), e))?;
        let meta = std::fs::metadata(&root).map_err(|e| Error::io(&root, e))?;
        if !meta.is_dir() {
            return Err(Error::io(
                &root,
                std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a folder"),
            ));
        }
        std::fs::read_dir(&root).map_err(|e| Error::io(&root, e))?;

        let mut report = ScanReport {
            root: root.clone(),
            ..ScanReport::default()
        };
        let mut status = ScanProgress {
            phase: ScanPhase::Walking,
            files_seen: 0,
            videos_found: 0,
            indexed: 0,
            current: None,
        };

        // 1. Walk.
        let mut dirs: BTreeMap<PathBuf, DirFiles> = BTreeMap::new();
        let mut unreadable: Vec<String> = Vec::new();
        let walker = WalkDir::new(&root)
            .follow_links(options.follow_links)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    let path = err.path().map(Path::to_path_buf).unwrap_or(root.clone());
                    unreadable.push(path_string(&path));
                    report.errors.push(ScanError {
                        path,
                        message: err.to_string(),
                    });
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            status.files_seen += 1;
            let Some(name) = entry.file_name().to_str() else {
                report.errors.push(ScanError {
                    path: entry.path().to_path_buf(),
                    message: "file name is not valid UTF-8".into(),
                });
                continue;
            };
            let name = name.to_string();
            let parent = entry.path().parent().unwrap_or(&root).to_path_buf();
            if is_video_name(&name) {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(err) => {
                        unreadable.push(path_string(entry.path()));
                        report.errors.push(ScanError {
                            path: entry.path().to_path_buf(),
                            message: err.to_string(),
                        });
                        continue;
                    }
                };
                let Some(location) = entry.path().to_str().map(str::to_string) else {
                    report.errors.push(ScanError {
                        path: entry.path().to_path_buf(),
                        message: "path is not valid UTF-8".into(),
                    });
                    continue;
                };
                status.videos_found += 1;
                status.current = Some(entry.path().to_path_buf());
                progress(&status);
                dirs.entry(parent).or_default().videos.push(FoundVideo {
                    path: entry.path().to_path_buf(),
                    location,
                    name,
                    size: meta.len(),
                    mtime: mtime_secs(&meta),
                });
            } else if is_script_name(&name) || is_subtitle_name(&name) {
                dirs.entry(parent).or_default().side.push(name);
            }
        }
        report.videos = status.videos_found;

        // 2. Attach side files.
        let interactive: Option<(PathBuf, Vec<String>)> = options
            .interactive_dir
            .as_ref()
            .map(|d| (d.clone(), list_interactive(d)))
            .filter(|(_, names)| !names.is_empty());
        let mut found: Vec<(FoundVideo, Vec<String>, Vec<String>)> = Vec::new();
        for (dir, files) in dirs {
            let stems: Vec<&str> = files.videos.iter().map(|v| stem_of(&v.name)).collect();
            let matched = match_side_files(&stems, &files.side);
            let fallback: Vec<(Vec<String>, Vec<String>)> = match &interactive {
                Some((_, names)) => match_side_files(&stems, names),
                None => Vec::new(),
            };
            for (i, ((scripts, subs), video)) in matched.into_iter().zip(files.videos).enumerate() {
                let mut scripts: Vec<String> = scripts
                    .into_iter()
                    .map(|s| path_string(&dir.join(s)))
                    .collect();
                if scripts.is_empty() {
                    if let (Some((idir, _)), Some((fs, _))) = (&interactive, fallback.get(i)) {
                        scripts = fs.iter().map(|s| path_string(&idir.join(s))).collect();
                    }
                }
                let subs = subs
                    .into_iter()
                    .map(|s| path_string(&dir.join(s)))
                    .collect();
                found.push((video, scripts, subs));
            }
        }

        // 3. Compare with the database and write changes.
        let prefix = format!("{}/", path_string(&root).trim_end_matches('/'));
        let known: HashMap<String, Known> = self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT location, source_id, size, mtime, missing, scripts, subtitles FROM media
                 WHERE substr(location, 1, length(?1)) = ?1",
            )?;
            let rows = stmt.query_map([&prefix], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Known {
                        source_id: r.get(1)?,
                        size: r.get(2)?,
                        mtime: r.get(3)?,
                        missing: r.get(4)?,
                        scripts: r.get(5)?,
                        subtitles: r.get(6)?,
                    },
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?;

        status.phase = ScanPhase::Indexing;
        let mut seen: HashSet<String> = HashSet::with_capacity(found.len());
        let mut pending: Vec<(PathBuf, MediaUpsert)> = Vec::new();
        for (video, scripts, subtitles) in found {
            seen.insert(video.location.clone());
            let size = i64::try_from(video.size).ok();
            let scripts_json = serde_json::to_string(&scripts)?;
            let subs_json = serde_json::to_string(&subtitles)?;
            let unchanged = !options.force
                && known.get(&video.location).is_some_and(|k| {
                    k.source_id == source_id
                        && !k.missing
                        && k.size == size
                        && k.mtime == video.mtime
                        && k.scripts == scripts_json
                        && k.subtitles == subs_json
                });
            if unchanged {
                report.unchanged += 1;
                status.indexed += 1;
                continue;
            }
            let mut u = MediaUpsert::new(video.location, source_id);
            u.title = stem_of(&video.name).to_string();
            u.detected = detect_name(&video.name);
            u.size = Some(video.size);
            u.mtime = video.mtime;
            u.scripts = scripts;
            u.subtitles = subtitles;
            u.reset_metadata = options.force;
            pending.push((video.path, u));
        }
        for batch in pending.chunks(options.batch_size.max(1)) {
            let outcomes = self.in_tx(|c| {
                batch
                    .iter()
                    .map(|(_, u)| upsert_conn(c, u))
                    .collect::<Result<Vec<_>>>()
            })?;
            for ((path, _), out) in batch.iter().zip(outcomes) {
                match out.status {
                    UpsertStatus::Added => report.added += 1,
                    UpsertStatus::Updated => report.updated += 1,
                    UpsertStatus::Unchanged => report.unchanged += 1,
                }
                if out.restored {
                    report.restored += 1;
                }
                status.indexed += 1;
                status.current = Some(path.clone());
                progress(&status);
            }
        }

        // 4. Mark what disappeared.
        status.phase = ScanPhase::MarkingMissing;
        status.current = None;
        progress(&status);
        let protected = |loc: &str| {
            unreadable.iter().any(|u| {
                loc == u
                    || loc
                        .strip_prefix(u.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            })
        };
        let gone: Vec<&String> = known
            .iter()
            .filter(|(loc, k)| {
                k.source_id == source_id && !k.missing && !seen.contains(*loc) && !protected(loc)
            })
            .map(|(loc, _)| loc)
            .collect();
        if !gone.is_empty() {
            report.missing = self.in_tx(|c| {
                let mut stmt =
                    c.prepare_cached("UPDATE media SET missing = 1 WHERE location = ?1")?;
                let mut n = 0;
                for loc in &gone {
                    n += stmt.execute(params![loc])?;
                }
                Ok(n)
            })?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::Projection;
    use fp_core::format::Evidence;
    use std::fs;

    fn touch(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn opts(interactive: Option<PathBuf>) -> ScanOptions {
        ScanOptions {
            interactive_dir: interactive,
            ..ScanOptions::default()
        }
    }

    fn names(paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .map(|p| crate::record::file_name_of(p).to_string())
            .collect()
    }

    #[test]
    fn side_file_rules() {
        assert_eq!(
            side_kind("Clip", "clip.funscript"),
            Some(SideKind::MainScript)
        );
        assert_eq!(
            side_kind("Clip", "Clip.roll.funscript"),
            Some(SideKind::AxisScript)
        );
        assert_eq!(
            side_kind("Clip", "Clip.L1.funscript"),
            Some(SideKind::AxisScript)
        );
        assert_eq!(side_kind("Clip", "Clip.other.funscript"), None);
        assert_eq!(side_kind("Clip", "Clip2.funscript"), None);
        assert_eq!(side_kind("Clip", "Clip.srt"), Some(SideKind::Subtitle));
        assert_eq!(side_kind("Clip", "clip.en.vtt"), Some(SideKind::Subtitle));
        assert_eq!(
            side_kind("Clip", "Clip - English.ass"),
            Some(SideKind::Subtitle)
        );
        assert_eq!(side_kind("Clip", "Clip2.srt"), None);
        assert_eq!(side_kind("Clip", "Clip.txt"), None);
        assert_eq!(
            script_axis("/x/a.b.twist.funscript").as_deref(),
            Some("twist")
        );
        assert_eq!(script_axis("/x/a.funscript"), None);
        assert_eq!(script_axis("/x/a.roll.mp4"), None);
    }

    #[test]
    fn longest_stem_wins() {
        let side = vec![
            "a.srt".to_string(),
            "a.b.srt".to_string(),
            "a.b.funscript".to_string(),
            "a.funscript".to_string(),
            "a.pitch.funscript".to_string(),
            "a.b.roll.funscript".to_string(),
        ];
        let m = match_side_files(&["a", "a.b"], &side);
        assert_eq!(m[0].0, vec!["a.funscript", "a.pitch.funscript"]);
        assert_eq!(m[0].1, vec!["a.srt"]);
        assert_eq!(m[1].0, vec!["a.b.funscript", "a.b.roll.funscript"]);
        assert_eq!(m[1].1, vec!["a.b.srt"]);
    }

    #[test]
    fn scan_finds_videos_and_side_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        touch(&root.join("Scene_180_LR.mp4"), b"video1");
        touch(&root.join("Scene_180_LR.funscript"), b"{}");
        touch(&root.join("Scene_180_LR.roll.funscript"), b"{}");
        touch(&root.join("Scene_180_LR.twist.funscript"), b"{}");
        touch(&root.join("scene_180_lr.en.srt"), b"1");
        touch(&root.join("Scene_180_LR_es.vtt"), b"1");
        touch(&root.join("notes.txt"), b"x");
        touch(&root.join("sub/deeper/Trip_360.MKV"), b"video2");
        touch(&root.join("sub/Other.mp4"), b"video3");
        touch(&root.join(".hidden/secret.mp4"), b"no");
        touch(&root.join("._Scene_180_LR.mp4"), b"appledouble");
        let interactive = tmp.path().join("Interactive");
        touch(&interactive.join("Other.funscript"), b"{}");
        touch(&interactive.join("Other.surge.funscript"), b"{}");
        touch(&interactive.join("Trip_360.funscript.bak"), b"{}");

        let lib = Library::open_in_memory().unwrap();
        let mut calls = Vec::new();
        let report = lib
            .scan_folder(&root, "local", &opts(Some(interactive.clone())), |p| {
                calls.push(p.clone())
            })
            .unwrap();
        assert_eq!(report.videos, 3);
        assert_eq!(report.added, 3);
        assert_eq!(
            (report.updated, report.unchanged, report.missing),
            (0, 0, 0)
        );
        assert!(report.errors.is_empty());
        assert!(calls.iter().any(|p| p.phase == ScanPhase::Walking));
        assert_eq!(
            calls
                .iter()
                .filter(|p| p.phase == ScanPhase::Indexing)
                .count(),
            3
        );
        assert_eq!(calls.last().unwrap().phase, ScanPhase::MarkingMissing);

        let loc = |p: &Path| p.to_str().unwrap().to_string();
        let scene = lib
            .get_by_location(&loc(&root.join("Scene_180_LR.mp4")))
            .unwrap()
            .unwrap();
        assert_eq!(
            names(&scene.scripts),
            vec![
                "Scene_180_LR.funscript",
                "Scene_180_LR.roll.funscript",
                "Scene_180_LR.twist.funscript"
            ]
        );
        assert_eq!(
            names(&scene.subtitles),
            vec!["scene_180_lr.en.srt", "Scene_180_LR_es.vtt"]
        );
        assert_eq!(scene.detected.evidence, Evidence::FileName);
        assert_eq!(scene.detected.format.projection, Projection::EQUIRECT_180);
        assert_eq!(scene.size, Some(6));
        assert!(scene.mtime.is_some());

        let other = lib
            .get_by_location(&loc(&root.join("sub/Other.mp4")))
            .unwrap()
            .unwrap();
        assert_eq!(
            other.scripts,
            vec![
                loc(&interactive.join("Other.funscript")),
                loc(&interactive.join("Other.surge.funscript"))
            ]
        );
        let trip = lib
            .get_by_location(&loc(&root.join("sub/deeper/Trip_360.MKV")))
            .unwrap()
            .unwrap();
        assert!(trip.scripts.is_empty());
        assert_eq!(trip.detected.format.projection, Projection::EQUIRECT_360);
    }

    #[test]
    fn incremental_rescan_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("media");
        touch(&root.join("a.mp4"), b"aaaa");
        touch(&root.join("b.mp4"), b"bbbb");
        touch(&root.join("dir/c.mp4"), b"cccc");
        let lib = Library::open_in_memory().unwrap();
        let o = opts(None);
        let r1 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!(r1.added, 3);

        let loc = |n: &str| root.join(n).to_str().unwrap().to_string();
        let b = lib.get_by_location(&loc("b.mp4")).unwrap().unwrap();
        lib.set_rating(b.id, 4).unwrap();
        lib.set_tags(b.id, &["keep"]).unwrap();

        // Nothing changed: no writes.
        let r2 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!(
            (r2.added, r2.updated, r2.unchanged, r2.missing),
            (0, 0, 3, 0)
        );

        // Modify a, add a script to c, remove b, add d.
        touch(&root.join("a.mp4"), b"aaaaaaaa");
        touch(&root.join("dir/c.funscript"), b"{}");
        fs::remove_file(root.join("b.mp4")).unwrap();
        touch(&root.join("d.webm"), b"d");
        let r3 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!(
            (r3.added, r3.updated, r3.unchanged, r3.missing),
            (1, 2, 0, 1)
        );
        let a = lib.get_by_location(&loc("a.mp4")).unwrap().unwrap();
        assert_eq!(a.size, Some(8));
        let c = lib.get_by_location(&loc("dir/c.mp4")).unwrap().unwrap();
        assert_eq!(c.scripts.len(), 1);
        let b2 = lib.get(b.id).unwrap().unwrap();
        assert!(b2.missing);
        assert_eq!(b2.rating, 4, "user data kept while missing");
        assert_eq!(lib.count(&crate::Query::default()).unwrap(), 3);

        // Missing again is not counted twice.
        let r4 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!((r4.missing, r4.unchanged), (0, 3));

        // b comes back with its data.
        touch(&root.join("b.mp4"), b"bbbb");
        let r5 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!((r5.restored, r5.updated), (1, 1));
        let b3 = lib.get(b.id).unwrap().unwrap();
        assert!(!b3.missing);
        assert_eq!(b3.tags, vec!["keep"]);

        // Rows of other sources or outside the root are left alone.
        lib.upsert(&MediaUpsert::new("/elsewhere/x.mp4", "local"))
            .unwrap();
        lib.upsert(&MediaUpsert::new(loc("foreign.mp4"), "nas"))
            .unwrap();
        let r6 = lib.scan_folder(&root, "local", &o, |_| {}).unwrap();
        assert_eq!(r6.missing, 0);

        // Force re-indexes everything.
        let r7 = lib
            .scan_folder(
                &root,
                "local",
                &ScanOptions {
                    force: true,
                    ..o.clone()
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(r7.updated, 4);
    }

    #[test]
    fn scanning_a_subfolder_only_marks_inside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("m");
        touch(&root.join("a.mp4"), b"a");
        touch(&root.join("ab/b.mp4"), b"b");
        let lib = Library::open_in_memory().unwrap();
        lib.scan_folder(&root, "local", &opts(None), |_| {})
            .unwrap();
        // Prefix "m/a" must not match "m/ab/...".
        fs::remove_file(root.join("a.mp4")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        let r = lib
            .scan_folder(root.join("a"), "local", &opts(None), |_| {})
            .unwrap();
        assert_eq!(r.missing, 0);
        let r = lib
            .scan_folder(&root, "local", &opts(None), |_| {})
            .unwrap();
        assert_eq!(r.missing, 1);
    }

    #[test]
    fn missing_root_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        touch(&tmp.path().join("m/a.mp4"), b"a");
        lib.scan_folder(tmp.path().join("m"), "local", &opts(None), |_| {})
            .unwrap();
        fs::remove_dir_all(tmp.path().join("m")).unwrap();
        assert!(matches!(
            lib.scan_folder(tmp.path().join("m"), "local", &opts(None), |_| {}),
            Err(Error::Io { .. })
        ));
        let file = tmp.path().join("f.mp4");
        touch(&file, b"x");
        assert!(
            lib.scan_folder(&file, "local", &opts(None), |_| {})
                .is_err()
        );
        // Nothing was marked missing by the failed scans.
        assert_eq!(lib.count(&crate::Query::default()).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folder_is_reported_and_protected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("m");
        touch(&root.join("locked/a.mp4"), b"a");
        touch(&root.join("b.mp4"), b"b");
        let lib = Library::open_in_memory().unwrap();
        lib.scan_folder(&root, "local", &opts(None), |_| {})
            .unwrap();
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything; only check when permissions bite.
        let bites = fs::read_dir(&locked).is_err();
        let r = lib
            .scan_folder(&root, "local", &opts(None), |_| {})
            .unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if bites {
            assert_eq!(r.errors.len(), 1);
            assert_eq!(r.missing, 0);
        }
    }
}
