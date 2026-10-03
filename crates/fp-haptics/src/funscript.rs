//! Funscript parsing (single-axis and combined multi-axis files) and script
//! discovery next to a video.
//!
//! The format is JSON:
//!
//! ```json
//! {"version": "1.0", "inverted": false, "range": 100,
//!  "actions": [{"at": 0, "pos": 10}, {"at": 500, "pos": 90}],
//!  "metadata": {"creator": "..."},
//!  "axes": [{"id": "R1", "actions": [...]}]}
//! ```
//!
//! Parsing is tolerant: every field is optional, numbers may be floats or
//! numeric strings, actions that cannot be read are skipped, positions are
//! clamped to 0–100, and actions are sorted and de-duplicated by time.
//!
//! `inverted: true` flips positions (`100 - pos`). `range` (1–100, default
//! 100) is the original funscript "range of movement in percent": positions
//! are scaled by `range / 100` towards 0. The optional `axes` array is the
//! combined multi-axis layout; each entry may carry its own `inverted` and
//! `range`.

use crate::axis::{Axis, split_script_name};
use crate::error::{Error, Result};
use crate::script::{Action, Script};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A parsed funscript file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Funscript {
    /// The `version` field, if present.
    pub version: Option<String>,
    /// The `inverted` field (already applied to [`Funscript::script`]).
    pub inverted: bool,
    /// The `range` field (already applied to [`Funscript::script`]).
    pub range: Option<f64>,
    /// The `metadata` object as-is (`Null` when absent).
    pub metadata: Value,
    /// The main `actions`, normalised.
    pub script: Script,
    /// Extra axes from a combined multi-axis file's `axes` array.
    pub axes: Vec<(Axis, Script)>,
}

/// Raw layout, every field optional and loosely typed.
#[derive(Deserialize)]
struct RawFunscript {
    #[serde(default)]
    version: Option<Value>,
    #[serde(default)]
    inverted: Option<Value>,
    #[serde(default)]
    range: Option<Value>,
    #[serde(default)]
    actions: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    axes: Option<Value>,
}

impl Funscript {
    /// Parses a funscript from raw bytes (a UTF-8 byte-order mark is
    /// skipped).
    pub fn parse(bytes: &[u8]) -> Result<Funscript> {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let value: Value =
            serde_json::from_slice(bytes).map_err(|e| Error::Script(format!("not JSON: {e}")))?;
        if !value.is_object() {
            return Err(Error::Script("not a JSON object".into()));
        }
        let raw: RawFunscript = serde_json::from_value(value)
            .map_err(|e| Error::Script(format!("not a funscript: {e}")))?;
        let inverted = raw.inverted.as_ref().map(value_bool).unwrap_or(false);
        let range = raw.range.as_ref().and_then(value_f64);
        let script = parse_actions(raw.actions.as_ref(), inverted, range);
        let mut axes = Vec::new();
        if let Some(Value::Array(entries)) = &raw.axes {
            for entry in entries {
                let Some(axis) = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<Axis>().ok())
                else {
                    continue;
                };
                let inv = entry.get("inverted").map(value_bool).unwrap_or(false);
                let rng = entry.get("range").and_then(value_f64);
                axes.push((axis, parse_actions(entry.get("actions"), inv, rng)));
            }
        }
        Ok(Funscript {
            version: raw.version.and_then(|v| match v {
                Value::String(s) => Some(s),
                Value::Null => None,
                other => Some(other.to_string()),
            }),
            inverted,
            range,
            metadata: raw.metadata.unwrap_or(Value::Null),
            script,
            axes,
        })
    }

    /// Every non-empty axis in the file: the main actions on `main_axis`,
    /// then the extra axes. When an axis appears twice the first wins.
    pub fn into_axes(self, main_axis: Axis) -> Vec<(Axis, Script)> {
        let mut out: Vec<(Axis, Script)> = Vec::new();
        for (axis, script) in std::iter::once((main_axis, self.script)).chain(self.axes) {
            if !script.is_empty() && !out.iter().any(|(a, _)| *a == axis) {
                out.push((axis, script));
            }
        }
        out
    }
}

fn value_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

fn parse_actions(actions: Option<&Value>, inverted: bool, range: Option<f64>) -> Script {
    let Some(Value::Array(items)) = actions else {
        return Script::default();
    };
    let scale = range
        .filter(|r| *r > 0.0)
        .map(|r| r.min(100.0) / 100.0)
        .unwrap_or(1.0);
    let parsed = items
        .iter()
        .filter_map(|item| {
            let at = value_f64(item.get("at")?)?;
            let pos = value_f64(item.get("pos")?)?.clamp(0.0, 100.0);
            let pos = if inverted { 100.0 - pos } else { pos };
            Some(Action::new(at.round() as i64, (pos / 100.0 * scale) as f32))
        })
        .collect();
    Script::new(parsed)
}

/// Parses a funscript and assigns its main actions to the axis named by
/// `file_name`'s suffix (see [`split_script_name`]).
pub fn parse_script_file(file_name: &str, bytes: &[u8]) -> Result<Vec<(Axis, Script)>> {
    let axis = split_script_name(file_name)
        .map(|(_, a)| a)
        .unwrap_or(Axis::L0);
    Ok(Funscript::parse(bytes)?.into_axes(axis))
}

/// Reads and parses one funscript file (see [`parse_script_file`]).
pub fn load_script_file(path: &Path) -> Result<Vec<(Axis, Script)>> {
    let bytes = std::fs::read(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    parse_script_file(&name, &bytes)
}

/// Files that failed to load, with the reason.
pub type LoadFailures = Vec<(PathBuf, Error)>;

/// Reads several funscript files and merges their axes. When two files
/// provide the same axis, the first one listed wins. Files that fail to
/// load are returned separately so the caller can report them.
pub fn load_script_files(paths: &[PathBuf]) -> (Vec<(Axis, Script)>, LoadFailures) {
    let mut scripts: Vec<(Axis, Script)> = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match load_script_file(path) {
            Ok(axes) => {
                for (axis, script) in axes {
                    if !scripts.iter().any(|(a, _)| *a == axis) {
                        scripts.push((axis, script));
                    }
                }
            }
            Err(e) => errors.push((path.clone(), e)),
        }
    }
    (scripts, errors)
}

/// Finds the funscripts that belong to a local video: files named
/// `<video stem>.funscript` or `<video stem>.<axis>.funscript` in the
/// video's directory, then in each of `fallback_dirs` (for example
/// [`fp_core::dirs::interactive_dir`]). The first directory that has a
/// script for an axis wins. Name matching ignores ASCII case. Results are
/// sorted by axis.
pub fn find_scripts(video: &Path, fallback_dirs: &[PathBuf]) -> Vec<(Axis, PathBuf)> {
    let Some(stem) = video.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    let dirs = video
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .chain(fallback_dirs.iter().cloned());
    let mut found: Vec<(Axis, PathBuf)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut here: Vec<(Axis, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (base, axis) = split_script_name(&name)?;
                base.eq_ignore_ascii_case(&stem).then(|| (axis, e.path()))
            })
            .collect();
        here.sort();
        for (axis, path) in here {
            if !found.iter().any(|(a, _)| *a == axis) {
                found.push((axis, path));
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_funscript() {
        let json = br#"{"version":"1.0","inverted":false,"range":100,
            "actions":[{"at":500,"pos":100},{"at":0,"pos":0},{"at":500,"pos":50},{"at":"750","pos":"25.5"},
                       {"at":1000},{"pos":3},{"at":1200,"pos":150},{"at":1300.4,"pos":-5}],
            "metadata":{"creator":"someone","duration":12}}"#;
        let f = Funscript::parse(json).unwrap();
        assert_eq!(f.version.as_deref(), Some("1.0"));
        assert_eq!(f.metadata["creator"], "someone");
        let a = f.script.actions();
        assert_eq!(a.len(), 5);
        assert_eq!(a[0], Action::new(0, 0.0));
        assert_eq!(a[1], Action::new(500, 0.5));
        assert_eq!(a[2], Action::new(750, 0.255));
        assert_eq!(a[3], Action::new(1200, 1.0));
        assert_eq!(a[4], Action::new(1300, 0.0));
    }

    #[test]
    fn applies_inverted_and_range() {
        let f = Funscript::parse(
            br#"{"inverted":true,"range":50,"actions":[{"at":0,"pos":0},{"at":1,"pos":80}]}"#,
        )
        .unwrap();
        assert!(f.inverted);
        let a = f.script.actions();
        assert!((a[0].pos - 0.5).abs() < 1e-6);
        assert!((a[1].pos - 0.1).abs() < 1e-6);
        // Bogus range values are ignored or capped.
        let f = Funscript::parse(br#"{"range":0,"actions":[{"at":0,"pos":80}]}"#).unwrap();
        assert!((f.script.actions()[0].pos - 0.8).abs() < 1e-6);
        let f = Funscript::parse(br#"{"range":400,"actions":[{"at":0,"pos":80}]}"#).unwrap();
        assert!((f.script.actions()[0].pos - 0.8).abs() < 1e-6);
        let f = Funscript::parse(br#"{"inverted":"true","actions":[{"at":0,"pos":80}]}"#).unwrap();
        assert!((f.script.actions()[0].pos - 0.2).abs() < 1e-6);
    }

    #[test]
    fn tolerates_odd_files() {
        let f = Funscript::parse(b"\xEF\xBB\xBF{}").unwrap();
        assert!(f.script.is_empty());
        assert_eq!(f.metadata, Value::Null);
        assert!(Funscript::parse(br#"{"actions":null,"version":1}"#).is_ok());
        assert!(Funscript::parse(b"[1,2]").is_err());
        assert!(Funscript::parse(b"not json").is_err());
    }

    #[test]
    fn parses_multi_axis() {
        let json = br#"{"actions":[{"at":0,"pos":10}],
            "axes":[{"id":"R1","actions":[{"at":0,"pos":20}]},
                    {"id":"twist","inverted":true,"actions":[{"at":0,"pos":30}]},
                    {"id":"L0","actions":[{"at":0,"pos":99}]},
                    {"id":"??","actions":[{"at":0,"pos":40}]},
                    {"id":"V0","actions":[]}]}"#;
        let axes = parse_script_file("clip.funscript", json).unwrap();
        let ids: Vec<Axis> = axes.iter().map(|(a, _)| *a).collect();
        assert_eq!(ids, vec![Axis::L0, Axis::R1, Axis::R0]);
        assert!((axes[0].1.actions()[0].pos - 0.1).abs() < 1e-6);
        assert!((axes[2].1.actions()[0].pos - 0.7).abs() < 1e-6);

        let axes =
            parse_script_file("clip.roll.funscript", br#"{"actions":[{"at":0,"pos":1}]}"#).unwrap();
        assert_eq!(axes[0].0, Axis::R1);
    }

    #[test]
    fn finds_scripts_next_to_video_and_in_fallback() {
        let root = std::env::temp_dir().join(format!("fp-haptics-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let videos = root.join("videos");
        let interactive = root.join("Interactive");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::create_dir_all(&interactive).unwrap();
        let body = br#"{"actions":[{"at":0,"pos":0},{"at":100,"pos":100}]}"#;
        for (dir, name) in [
            (&videos, "Clip_180_LR.funscript"),
            (&videos, "clip_180_lr.roll.funscript"),
            (&videos, "Other.funscript"),
            (&interactive, "Clip_180_LR.funscript"),
            (&interactive, "Clip_180_LR.twist.funscript"),
            (&interactive, "Clip_180_LR.broken.surge.funscript"),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }
        std::fs::write(interactive.join("Clip_180_LR.pitch.funscript"), b"oops").unwrap();
        let found = find_scripts(
            &videos.join("Clip_180_LR.mp4"),
            std::slice::from_ref(&interactive),
        );
        let summary: Vec<(Axis, PathBuf)> = found.clone();
        assert_eq!(
            summary,
            vec![
                (Axis::L0, videos.join("Clip_180_LR.funscript")),
                (Axis::R0, interactive.join("Clip_180_LR.twist.funscript")),
                (Axis::R1, videos.join("clip_180_lr.roll.funscript")),
                (Axis::R2, interactive.join("Clip_180_LR.pitch.funscript")),
            ]
        );
        let paths: Vec<PathBuf> = found.into_iter().map(|(_, p)| p).collect();
        let (scripts, errors) = load_script_files(&paths);
        assert_eq!(scripts.len(), 3);
        assert_eq!(errors.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
