//! Matching haptic scripts and subtitles to a video by file name.
//!
//! For `Scene.mp4` in a folder:
//! - `Scene.funscript` is the main script, `Scene.<axis>.funscript` an extra
//!   axis of a multi-axis script (`Scene.roll.funscript`, `Scene.twist.funscript`,
//!   `Scene.L1.funscript`, ...);
//! - `Scene.srt` is a subtitle with no language, `Scene.<lang>.srt` one with a
//!   language tag (`Scene.en.srt`, `Scene.de.forced.ass`).
//!
//! Names compare case-insensitively. A file that matches another video in
//! the folder exactly (`Scene.part2.funscript` next to `Scene.part2.mp4`)
//! belongs to that video, not to `Scene.mp4`.

use fp_core::source::{Entry, EntryKind, is_script_name, is_subtitle_name, is_video_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A haptic script belonging to a video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFile {
    /// File name or title.
    pub name: String,
    /// Location to open with the same source (or a URL for feeds).
    pub location: String,
    /// Axis of a multi-axis script (`roll`, `twist`, `L1`, ...); `None` for
    /// the main stroke script.
    #[serde(default)]
    pub axis: Option<String>,
}

/// A subtitle file belonging to a video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtitleFile {
    /// File name or title.
    pub name: String,
    /// Location to open with the same source (or a URL for feeds).
    pub location: String,
    /// Language tag from the name or the feed (`en`, `de.forced`).
    #[serde(default)]
    pub language: Option<String>,
}

/// Scripts and subtitles found for one video.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecars {
    /// Main script first, then extra axes by name.
    pub scripts: Vec<ScriptFile>,
    /// Untagged subtitle first, then by language.
    pub subtitles: Vec<SubtitleFile>,
}

fn strip_ext(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// If `candidate` is `stem` or `stem.<suffix>` (case-insensitive), returns
/// `Some(None)` or `Some(Some(suffix))`.
fn suffix_after_stem<'a>(candidate: &'a str, stem: &str) -> Option<Option<&'a str>> {
    if candidate.eq_ignore_ascii_case(stem) {
        return Some(None);
    }
    let head = candidate.get(..stem.len())?;
    let rest = candidate.get(stem.len()..)?;
    if head.eq_ignore_ascii_case(stem) {
        if let Some(sfx) = rest.strip_prefix('.') {
            if !sfx.is_empty() {
                return Some(Some(sfx));
            }
        }
    }
    None
}

impl Sidecars {
    /// Wraps the script and subtitle locations an entry already carries
    /// (from a feed or DLNA metadata), deriving axis and language from the
    /// file names.
    pub fn from_entry(video: &Entry) -> Sidecars {
        let video_name = crate::urlutil::last_segment(&video.location);
        let stem = strip_ext(if video_name.is_empty() {
            &video.name
        } else {
            &video_name
        })
        .to_string();
        let mut out = Sidecars::default();
        for loc in &video.scripts {
            let name = crate::urlutil::last_segment(loc);
            let axis = suffix_after_stem(strip_ext(&name), &stem)
                .flatten()
                .filter(|a| !a.contains('.'))
                .map(str::to_string);
            out.scripts.push(ScriptFile {
                name,
                location: loc.clone(),
                axis,
            });
        }
        for loc in &video.subtitles {
            let name = crate::urlutil::last_segment(loc);
            let language = suffix_after_stem(strip_ext(&name), &stem)
                .flatten()
                .map(str::to_string);
            out.subtitles.push(SubtitleFile {
                name,
                location: loc.clone(),
                language,
            });
        }
        out
    }

    /// True when nothing was found.
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty() && self.subtitles.is_empty()
    }

    /// Adds the files of `other` that are not already present (by location).
    pub fn merge(&mut self, other: Sidecars) {
        for s in other.scripts {
            if !self.scripts.iter().any(|x| x.location == s.location) {
                self.scripts.push(s);
            }
        }
        for s in other.subtitles {
            if !self.subtitles.iter().any(|x| x.location == s.location) {
                self.subtitles.push(s);
            }
        }
        self.sort();
    }

    fn sort(&mut self) {
        self.scripts.sort_by(|a, b| {
            a.axis
                .is_some()
                .cmp(&b.axis.is_some())
                .then_with(|| a.axis.cmp(&b.axis))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.subtitles.sort_by(|a, b| {
            a.language
                .is_some()
                .cmp(&b.language.is_some())
                .then_with(|| a.language.cmp(&b.language))
                .then_with(|| a.name.cmp(&b.name))
        });
    }
}

/// Picks the scripts and subtitles for the video named `video_name` among
/// the entries of its folder.
pub fn match_sidecars(video_name: &str, siblings: &[Entry]) -> Sidecars {
    let stem = strip_ext(video_name);
    // Stems of the other videos: their exact sidecars are not ours.
    let other_stems: HashSet<String> = siblings
        .iter()
        .filter(|e| e.kind != EntryKind::Directory && is_video_name(&e.name))
        .map(|e| strip_ext(&e.name).to_lowercase())
        .filter(|s| !s.eq_ignore_ascii_case(stem))
        .collect();
    let mut out = Sidecars::default();
    for e in siblings.iter().filter(|e| e.kind != EntryKind::Directory) {
        let cand = strip_ext(&e.name);
        let Some(suffix) = suffix_after_stem(cand, stem) else {
            continue;
        };
        if suffix.is_some() && other_stems.contains(&cand.to_lowercase()) {
            continue;
        }
        if is_script_name(&e.name) {
            // Axis names are single tokens; `a.b.c.funscript` is not ours.
            if suffix.is_some_and(|s| s.contains('.')) {
                continue;
            }
            out.scripts.push(ScriptFile {
                name: e.name.clone(),
                location: e.location.clone(),
                axis: suffix.map(str::to_string),
            });
        } else if is_subtitle_name(&e.name) {
            out.subtitles.push(SubtitleFile {
                name: e.name.clone(),
                location: e.location.clone(),
                language: suffix.map(str::to_string),
            });
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_entry;

    fn folder(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| file_entry(*n, format!("/v/{n}"), None, None))
            .collect()
    }

    #[test]
    fn single_and_multi_axis_scripts() {
        let f = folder(&[
            "Scene_180_LR.mp4",
            "Scene_180_LR.funscript",
            "scene_180_lr.roll.funscript",
            "Scene_180_LR.twist.funscript",
            "Scene_180_LR.L1.funscript",
            "Scene_180_LR.a.b.funscript",
            "Other.funscript",
            "Scene_180_LR_extra.funscript",
        ]);
        let s = match_sidecars("Scene_180_LR.mp4", &f);
        let got: Vec<_> = s
            .scripts
            .iter()
            .map(|x| (x.name.as_str(), x.axis.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Scene_180_LR.funscript", None),
                ("Scene_180_LR.L1.funscript", Some("L1")),
                ("scene_180_lr.roll.funscript", Some("roll")),
                ("Scene_180_LR.twist.funscript", Some("twist")),
            ]
        );
        assert_eq!(s.scripts[0].location, "/v/Scene_180_LR.funscript");
    }

    #[test]
    fn subtitles_with_languages() {
        let f = folder(&[
            "Movie.mkv",
            "Movie.srt",
            "Movie.en.srt",
            "movie.de.forced.ass",
            "Movie.vtt",
            "Moviex.srt",
            "Movie.nfo",
        ]);
        let s = match_sidecars("Movie.mkv", &f);
        let got: Vec<_> = s
            .subtitles
            .iter()
            .map(|x| (x.name.as_str(), x.language.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("Movie.srt", None),
                ("Movie.vtt", None),
                ("movie.de.forced.ass", Some("de.forced")),
                ("Movie.en.srt", Some("en")),
            ]
        );
        assert!(s.scripts.is_empty());
    }

    #[test]
    fn other_videos_keep_their_sidecars() {
        let f = folder(&[
            "Scene.mp4",
            "Scene.part2.mp4",
            "Scene.funscript",
            "Scene.part2.funscript",
            "Scene.part2.srt",
            "Scene.roll.funscript",
        ]);
        let s = match_sidecars("Scene.mp4", &f);
        let names: Vec<_> = s.scripts.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Scene.funscript", "Scene.roll.funscript"]);
        assert!(s.subtitles.is_empty());
        let s = match_sidecars("Scene.part2.mp4", &f);
        let names: Vec<_> = s.scripts.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Scene.part2.funscript"]);
        assert_eq!(s.subtitles.len(), 1);
    }

    #[test]
    fn unicode_names() {
        let f = folder(&[
            "Szene ü 日本.mp4",
            "Szene ü 日本.funscript",
            "Szene ü 日本.ja.srt",
        ]);
        let s = match_sidecars("Szene ü 日本.mp4", &f);
        assert_eq!(s.scripts.len(), 1);
        assert_eq!(s.subtitles[0].language.as_deref(), Some("ja"));
    }

    #[test]
    fn from_entry_and_merge() {
        let mut e = file_entry("Clip", "http://h/v/Clip.mp4", None, None);
        e.scripts = vec![
            "http://h/v/Clip.funscript".into(),
            "http://h/v/Clip.surge.funscript".into(),
        ];
        e.subtitles = vec!["http://h/v/Clip.en.srt".into()];
        let mut s = Sidecars::from_entry(&e);
        assert_eq!(s.scripts[1].axis.as_deref(), Some("surge"));
        assert_eq!(s.subtitles[0].language.as_deref(), Some("en"));
        let extra = Sidecars {
            scripts: vec![ScriptFile {
                name: "Clip.funscript".into(),
                location: "http://h/v/Clip.funscript".into(),
                axis: None,
            }],
            subtitles: vec![SubtitleFile {
                name: "Clip.srt".into(),
                location: "http://h/v/Clip.srt".into(),
                language: None,
            }],
        };
        s.merge(extra);
        assert_eq!(s.scripts.len(), 2);
        assert_eq!(s.subtitles[0].name, "Clip.srt");
        assert!(!s.is_empty());
    }
}
