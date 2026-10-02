//! Funscript discovery for a video.
//!
//! Conventions (DeoVR / HereSphere / MultiFunPlayer):
//! - `<stem>.funscript` next to the video is the main (stroke / L0) script;
//! - `<stem>.<axis>.funscript` are extra axes (`surge`, `sway`, `twist`,
//!   `roll`, `pitch`, `vib`, ... or raw TCode channel names like `L1`, `R2`);
//! - if nothing is found next to the video, an `Interactive/` subfolder of
//!   the video's folder is searched the same way.
//!
//! Stems compare case-insensitively.

use fp_sources::{Entry, Source};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Known axis names (lowercase). TCode channel ids are accepted too.
pub const AXES: &[&str] = &[
    "surge", "sway", "twist", "roll", "pitch", "vib", "vibe", "pump", "suck", "valve", "lube",
    "stroke", "l0", "l1", "l2", "r0", "r1", "r2", "v0", "v1", "v2", "a0", "a1", "a2",
];

/// A discovered script: `axis` is `"main"` or an axis name from [`AXES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRef {
    pub axis: String,
    pub uri: String,
}

fn stem_of(name: &str) -> &str {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    }
}

/// Axis for a script file name if it is `<anything>.<axis>.funscript`,
/// `Some("main")` for a plain `.funscript`, `None` if not a funscript.
pub fn axis_from_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".funscript")?;
    match base.rsplit_once('.') {
        Some((_, ax)) if AXES.contains(&ax) => Some(normalize_axis(ax).to_string()),
        _ => Some("main".to_string()),
    }
}

fn normalize_axis(ax: &str) -> &str {
    match ax {
        "stroke" | "l0" => "main",
        "vibe" => "vib",
        other => other,
    }
}

/// Match scripts for `video_name` among `candidates` (entries of one
/// directory).
pub fn match_scripts(video_name: &str, candidates: &[Entry]) -> Vec<ScriptRef> {
    let stem = stem_of(video_name).to_lowercase();
    let mut out: Vec<ScriptRef> = Vec::new();
    for e in candidates.iter().filter(|e| !e.is_dir) {
        let lower = e.name.to_lowercase();
        let Some(base) = lower.strip_suffix(".funscript") else {
            continue;
        };
        let axis = if base == stem {
            "main".to_string()
        } else if let Some(ax) = base.strip_prefix(&stem).and_then(|r| r.strip_prefix('.')) {
            if !AXES.contains(&ax) {
                continue;
            }
            normalize_axis(ax).to_string()
        } else {
            continue;
        };
        if !out.iter().any(|s| s.axis == axis) {
            out.push(ScriptRef {
                axis,
                uri: e.uri.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        (a.axis != "main")
            .cmp(&(b.axis != "main"))
            .then_with(|| a.axis.cmp(&b.axis))
    });
    out
}

/// Parent "directory" of a URI (everything before the last `/`).
pub fn parent_uri(uri: &str) -> &str {
    let trimmed = uri.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[..i],
        None => "",
    }
}

/// Discover scripts from a full recursive listing (as produced by
/// [`fp_sources::walk`]), grouped by parent URI. Uses the same-folder
/// rule first, then `Interactive/`.
pub fn discover_in_listing(video: &Entry, by_dir: &HashMap<String, Vec<Entry>>) -> Vec<ScriptRef> {
    let dir = parent_uri(&video.uri);
    if let Some(siblings) = by_dir.get(dir) {
        let found = match_scripts(&video.name, siblings);
        if !found.is_empty() {
            return found;
        }
    }
    let interactive = format!("{dir}/interactive");
    by_dir
        .iter()
        .find(|(k, _)| k.to_lowercase() == interactive.to_lowercase())
        .map(|(_, v)| match_scripts(&video.name, v))
        .unwrap_or_default()
}

/// Group a recursive listing by parent directory URI.
pub fn group_by_dir(entries: &[Entry]) -> HashMap<String, Vec<Entry>> {
    let mut m: HashMap<String, Vec<Entry>> = HashMap::new();
    for e in entries {
        m.entry(parent_uri(&e.uri).to_string())
            .or_default()
            .push(e.clone());
    }
    m
}

/// Discover scripts for one video by listing its folder (and
/// `Interactive/` when needed) through the source.
pub async fn discover(source: &dyn Source, video: &Entry) -> fp_sources::Result<Vec<ScriptRef>> {
    let dir = parent_uri(&video.uri).to_string();
    let siblings = source.list(&dir).await?;
    let found = match_scripts(&video.name, &siblings);
    if !found.is_empty() {
        return Ok(found);
    }
    if let Some(sub) = siblings
        .iter()
        .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("interactive"))
    {
        let inner = source.list(&sub.uri).await?;
        return Ok(match_scripts(&video.name, &inner));
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(dir: &str, name: &str) -> Entry {
        Entry {
            name: name.into(),
            uri: format!("{dir}/{name}"),
            ..Default::default()
        }
    }

    #[test]
    fn same_dir_and_multi_axis() {
        let d = "file:///v";
        let c = vec![
            e(d, "Scene.mp4"),
            e(d, "scene.funscript"),
            e(d, "Scene.twist.funscript"),
            e(d, "Scene.Roll.funscript"),
            e(d, "Scene.notanaxis.funscript"),
            e(d, "Scene2.funscript"),
            e(d, "Scene.L0.funscript"),
            e(d, "Scene.vibe.funscript"),
        ];
        let s = match_scripts("Scene.mp4", &c);
        let axes: Vec<_> = s.iter().map(|s| s.axis.as_str()).collect();
        assert_eq!(axes, ["main", "roll", "twist", "vib"]);
        assert_eq!(s[0].uri, "file:///v/scene.funscript");
        // Video names with dots in the stem.
        let c = vec![e(d, "a.180.LR.funscript"), e(d, "a.180.LR.surge.funscript")];
        assert_eq!(match_scripts("a.180.LR.mp4", &c).len(), 2);
    }

    #[test]
    fn interactive_fallback() {
        let d = "smb://nas/share/vr";
        let video = e(d, "Clip.mkv");
        let listing = vec![
            video.clone(),
            e(&format!("{d}/Interactive"), "Clip.funscript"),
            e(&format!("{d}/Interactive"), "Clip.pitch.funscript"),
            e(&format!("{d}/other"), "Clip.funscript"),
        ];
        let by_dir = group_by_dir(&listing);
        let s = discover_in_listing(&video, &by_dir);
        assert_eq!(s.len(), 2);
        assert!(s[0].uri.contains("/Interactive/"));
        // Same-folder script wins over Interactive/.
        let mut listing2 = listing.clone();
        listing2.push(e(d, "clip.funscript"));
        let s = discover_in_listing(&video, &group_by_dir(&listing2));
        assert_eq!(
            s,
            vec![ScriptRef {
                axis: "main".into(),
                uri: format!("{d}/clip.funscript")
            }]
        );
        assert!(discover_in_listing(&e(d, "None.mp4"), &by_dir).is_empty());
    }

    #[test]
    fn axis_names() {
        assert_eq!(axis_from_name("X.funscript").as_deref(), Some("main"));
        assert_eq!(
            axis_from_name("X.SURGE.funscript").as_deref(),
            Some("surge")
        );
        assert_eq!(
            axis_from_name("X.stroke.funscript").as_deref(),
            Some("main")
        );
        assert_eq!(axis_from_name("X.mp4"), None);
        assert_eq!(parent_uri("file:///a/b/c.mp4"), "file:///a/b");
    }

    #[tokio::test]
    async fn discover_through_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Interactive")).unwrap();
        std::fs::write(dir.path().join("v.mp4"), b"x").unwrap();
        std::fs::write(dir.path().join("Interactive/v.funscript"), b"{}").unwrap();
        let src = fp_sources::local::LocalSource::new(dir.path());
        let video = src
            .list("")
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.name == "v.mp4")
            .unwrap();
        let s = discover(&src, &video).await.unwrap();
        assert_eq!(s.len(), 1);
        assert!(s[0].uri.ends_with("/Interactive/v.funscript"));
    }
}
