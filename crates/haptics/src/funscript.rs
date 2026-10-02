//! Funscript parsing and querying.
//!
//! A funscript is JSON: `{"version", "inverted", "range", "actions": [{"at": ms, "pos": 0-100}],
//! "metadata": {...}}`. Real-world files are messy, so the parser is lenient: unknown fields are
//! ignored, numbers may be floats or numeric strings, actions may be unsorted or duplicated, and
//! OpenFunscripter (OFS) chapters/bookmarks in `metadata` are understood. OFS multi-axis files
//! (`"axes": [{"id": "R0", "actions": [...]}]`) are supported as well.
//!
//! After parsing, [`Script::actions`] are sorted by time and already have `range` and `inverted`
//! applied, so every query works in plain 0–100 "funscript units".

use crate::HapticsError;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Movement axes, named after their TCode identifiers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize,
)]
pub enum Axis {
    /// Stroke (up/down). The default axis for a plain `.funscript`.
    L0,
    /// Surge (forward/back).
    L1,
    /// Sway (left/right).
    L2,
    /// Twist.
    R0,
    /// Roll.
    R1,
    /// Pitch.
    R2,
    /// Vibration.
    V0,
    /// Secondary vibration / pump.
    V1,
    /// Valve.
    A0,
    /// Suction.
    A1,
    /// Lube.
    A2,
}

impl Axis {
    pub const ALL: [Axis; 11] = [
        Axis::L0,
        Axis::L1,
        Axis::L2,
        Axis::R0,
        Axis::R1,
        Axis::R2,
        Axis::V0,
        Axis::V1,
        Axis::A0,
        Axis::A1,
        Axis::A2,
    ];

    /// The TCode channel name (`"L0"`, `"R2"`, ...).
    pub fn tcode(self) -> &'static str {
        match self {
            Axis::L0 => "L0",
            Axis::L1 => "L1",
            Axis::L2 => "L2",
            Axis::R0 => "R0",
            Axis::R1 => "R1",
            Axis::R2 => "R2",
            Axis::V0 => "V0",
            Axis::V1 => "V1",
            Axis::A0 => "A0",
            Axis::A1 => "A1",
            Axis::A2 => "A2",
        }
    }

    /// Human-readable name, also the canonical file suffix (`video.surge.funscript`).
    pub fn name(self) -> &'static str {
        match self {
            Axis::L0 => "stroke",
            Axis::L1 => "surge",
            Axis::L2 => "sway",
            Axis::R0 => "twist",
            Axis::R1 => "roll",
            Axis::R2 => "pitch",
            Axis::V0 => "vib",
            Axis::V1 => "pump",
            Axis::A0 => "valve",
            Axis::A1 => "suck",
            Axis::A2 => "lube",
        }
    }

    /// Map a multi-axis file suffix (the part between the video stem and `.funscript`) to an
    /// axis. Accepts the MultiFunPlayer / OFS names and raw TCode names, case-insensitively.
    pub fn from_suffix(suffix: &str) -> Option<Axis> {
        let s = suffix.to_ascii_lowercase();
        let axis = match s.as_str() {
            "l0" | "stroke" | "up" => Axis::L0,
            "l1" | "surge" | "forward" => Axis::L1,
            "l2" | "sway" | "left" => Axis::L2,
            "r0" | "twist" | "rotate" => Axis::R0,
            "r1" | "roll" => Axis::R1,
            "r2" | "pitch" => Axis::R2,
            "v0" | "vib" | "vibe" | "vibrate" | "vibration" => Axis::V0,
            "v1" | "pump" => Axis::V1,
            "a0" | "valve" => Axis::A0,
            "a1" | "suck" | "suction" => Axis::A1,
            "a2" | "lube" => Axis::A2,
            _ => return None,
        };
        Some(axis)
    }

    /// Whether the axis is a linear/rotational *position* (as opposed to an intensity such as
    /// vibration). Position axes rest at 50 in TCode; intensity axes rest at 0.
    pub fn is_position(self) -> bool {
        matches!(
            self,
            Axis::L0 | Axis::L1 | Axis::L2 | Axis::R0 | Axis::R1 | Axis::R2
        )
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tcode())
    }
}

impl FromStr for Axis {
    type Err = HapticsError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Axis::from_suffix(s).ok_or_else(|| HapticsError::Parse(format!("unknown axis {s:?}")))
    }
}

/// Split a funscript file name into `(video stem, axis)`.
///
/// `clip.funscript` -> `("clip", L0)`, `clip.surge.funscript` -> `("clip", L1)`,
/// `My.Clip.2023.funscript` -> `("My.Clip.2023", L0)` (unknown suffixes belong to the stem).
pub fn split_script_name(file_name: &str) -> Option<(&str, Axis)> {
    let lower = file_name.to_ascii_lowercase();
    if !lower.ends_with(".funscript") {
        return None;
    }
    let base = &file_name[..file_name.len() - ".funscript".len()];
    if let Some(dot) = base.rfind('.') {
        if let Some(axis) = Axis::from_suffix(&base[dot + 1..]) {
            return Some((&base[..dot], axis));
        }
    }
    Some((base, Axis::L0))
}

/// One script point: time in milliseconds and position in 0–100.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Action {
    pub at: i64,
    pub pos: f64,
}

/// An OFS chapter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chapter {
    pub name: String,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
}

/// An OFS bookmark.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bookmark {
    pub name: String,
    pub time_ms: i64,
}

/// Script metadata (OFS layout). Unknown keys are kept in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Metadata {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub notes: Option<String>,
    pub script_url: Option<String>,
    pub video_url: Option<String>,
    pub script_type: Option<String>,
    /// Duration of the video in seconds, when the script states it.
    pub duration_secs: Option<f64>,
    pub performers: Vec<String>,
    pub tags: Vec<String>,
    pub chapters: Vec<Chapter>,
    pub bookmarks: Vec<Bookmark>,
    pub extra: Map<String, Value>,
}

/// A single-axis script with actions sorted and normalised to 0–100.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Script {
    pub version: Option<String>,
    /// `inverted` flag as stated in the file (already applied to `actions`).
    pub inverted: bool,
    /// `range` as stated in the file (already applied to `actions`).
    pub range: f64,
    pub actions: Vec<Action>,
    pub metadata: Metadata,
}

/// Reference speed (funscript units per second) that maps to heat 1.0 with
/// [`HeatmapScale::Fixed`]'s default. 400 u/s is roughly the Handy's top speed.
pub const DEFAULT_MAX_HEAT_SPEED: f32 = 400.0;

/// How [`Script::heatmap`] normalises bucket speeds into 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HeatmapScale {
    /// Divide by a fixed speed in units/s and clamp; comparable across scripts.
    Fixed(f32),
    /// Divide by the fastest bucket of this script.
    Relative,
}

impl Default for HeatmapScale {
    fn default() -> Self {
        HeatmapScale::Fixed(DEFAULT_MAX_HEAT_SPEED)
    }
}

impl Script {
    /// Parse a funscript JSON document. For OFS multi-axis files only the primary actions are
    /// returned; use [`ScriptSet::parse_multi`] to get every axis.
    pub fn parse(json: &str) -> Result<Script, HapticsError> {
        let v: Value =
            serde_json::from_str(json).map_err(|e| HapticsError::Parse(e.to_string()))?;
        Script::from_value(&v)
    }

    /// Build from an already-parsed JSON value.
    pub fn from_value(v: &Value) -> Result<Script, HapticsError> {
        let obj = v
            .as_object()
            .ok_or_else(|| HapticsError::Parse("funscript root is not an object".into()))?;
        let inverted = obj.get("inverted").map(lenient_bool).unwrap_or(false);
        let range = obj
            .get("range")
            .and_then(lenient_f64)
            .filter(|r| *r > 0.0 && r.is_finite())
            .unwrap_or(100.0);
        let version = obj.get("version").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        let raw_actions = obj.get("actions").and_then(Value::as_array);
        if raw_actions.is_none() && obj.get("axes").is_none() {
            return Err(HapticsError::Parse(
                "funscript has no \"actions\" array".into(),
            ));
        }
        let actions = parse_actions(
            raw_actions.map(Vec::as_slice).unwrap_or(&[]),
            inverted,
            range,
        );
        let metadata = obj.get("metadata").map(parse_metadata).unwrap_or_default();
        Ok(Script {
            version,
            inverted,
            range,
            actions,
            metadata,
        })
    }

    /// Construct from raw `(at, pos)` pairs (sorted and deduplicated for you).
    pub fn from_actions(actions: impl IntoIterator<Item = (i64, f64)>) -> Script {
        let mut actions: Vec<Action> = actions
            .into_iter()
            .filter(|(_, p)| p.is_finite())
            .map(|(at, pos)| Action {
                at,
                pos: pos.clamp(0.0, 100.0),
            })
            .collect();
        normalize_order(&mut actions);
        Script {
            range: 100.0,
            actions,
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Time of the last action in ms (0 if empty).
    pub fn end_ms(&self) -> i64 {
        self.actions.last().map(|a| a.at).unwrap_or(0)
    }

    /// Linearly interpolated position (0–100) at `t_ms`. Before the first action the first
    /// position holds, after the last the last one does. `None` for an empty script.
    pub fn position_at(&self, t_ms: f64) -> Option<f64> {
        let a = &self.actions;
        let first = a.first()?;
        let last = a.last()?;
        if t_ms <= first.at as f64 {
            return Some(first.pos);
        }
        if t_ms >= last.at as f64 {
            return Some(last.pos);
        }
        // First index with at > t; guaranteed 1..len by the bounds checks above.
        let i = a.partition_point(|x| (x.at as f64) <= t_ms);
        let (p, n) = (a[i - 1], a[i]);
        let span = (n.at - p.at) as f64;
        if span <= 0.0 {
            return Some(n.pos);
        }
        let f = (t_ms - p.at as f64) / span;
        Some(p.pos + (n.pos - p.pos) * f)
    }

    /// Index of the first action strictly after `t_ms`.
    pub fn next_index_after(&self, t_ms: f64) -> Option<usize> {
        let i = self.actions.partition_point(|x| (x.at as f64) <= t_ms);
        (i < self.actions.len()).then_some(i)
    }

    /// The first action strictly after `t_ms` (the "next target" for direct-position devices).
    pub fn next_action_after(&self, t_ms: f64) -> Option<&Action> {
        self.next_index_after(t_ms).map(|i| &self.actions[i])
    }

    /// All actions with `t_ms < at <= t_ms + window_ms` (lookahead window).
    pub fn actions_in_window(&self, t_ms: f64, window_ms: f64) -> &[Action] {
        let start = self.actions.partition_point(|x| (x.at as f64) <= t_ms);
        let end = self
            .actions
            .partition_point(|x| (x.at as f64) <= t_ms + window_ms);
        &self.actions[start..end.max(start)]
    }

    /// Instantaneous speed (units/s) of the segment containing `t_ms`.
    pub fn speed_at(&self, t_ms: f64) -> f64 {
        let a = &self.actions;
        if a.len() < 2 || t_ms < a[0].at as f64 || t_ms >= a[a.len() - 1].at as f64 {
            return 0.0;
        }
        let i = a.partition_point(|x| (x.at as f64) <= t_ms);
        segment_speed(a[i - 1], a[i])
    }

    /// Shift every action by `offset_ms` (positive = later).
    pub fn shift(&mut self, offset_ms: i64) {
        for a in &mut self.actions {
            a.at += offset_ms;
        }
    }

    /// Mirror positions (`pos -> 100 - pos`).
    pub fn invert(&mut self) {
        for a in &mut self.actions {
            a.pos = 100.0 - a.pos;
        }
    }

    /// Remap 0–100 into `min..=max` (both 0–100). Used for per-axis user stroke limits.
    pub fn remap_range(&mut self, min: f64, max: f64) {
        let (min, max) = (min.clamp(0.0, 100.0), max.clamp(0.0, 100.0));
        for a in &mut self.actions {
            a.pos = min + a.pos / 100.0 * (max - min);
        }
    }

    /// Enforce a maximum speed in units/s. Movements that are too fast are shortened so that
    /// the device reaches as far as it physically can in the time available; the following
    /// segment starts from that clipped position, exactly as the hardware would behave.
    pub fn limit_speed(&mut self, max_units_per_sec: f64) {
        if max_units_per_sec <= 0.0 || !max_units_per_sec.is_finite() || self.actions.len() < 2 {
            return;
        }
        for i in 1..self.actions.len() {
            let prev = self.actions[i - 1];
            let cur = &mut self.actions[i];
            let dt = (cur.at - prev.at) as f64 / 1000.0;
            let max_delta = max_units_per_sec * dt;
            let delta = cur.pos - prev.pos;
            if delta.abs() > max_delta {
                cur.pos = prev.pos + max_delta.copysign(delta);
            }
        }
    }

    /// Per-bucket average speed over `[0, duration_ms)`, normalised to 0..=1, for the timeline
    /// "heat" strip. Each bucket's value is the time-weighted mean of the segment speeds that
    /// overlap it (gaps count as zero speed).
    pub fn heatmap(&self, buckets: usize, duration_ms: i64, scale: HeatmapScale) -> Vec<f32> {
        let mut out = vec![0f32; buckets];
        if buckets == 0 || duration_ms <= 0 || self.actions.len() < 2 {
            return out;
        }
        let bucket_ms = duration_ms as f64 / buckets as f64;
        for w in self.actions.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b.at <= a.at {
                continue;
            }
            let speed = segment_speed(a, b);
            let (s, e) = (
                (a.at as f64).max(0.0),
                (b.at as f64).min(duration_ms as f64),
            );
            if e <= s {
                continue;
            }
            let first = (s / bucket_ms).floor() as usize;
            let last = ((e / bucket_ms).ceil() as usize).min(buckets);
            for (bi, slot) in out.iter_mut().enumerate().take(last).skip(first) {
                let bs = bi as f64 * bucket_ms;
                let overlap = e.min(bs + bucket_ms) - s.max(bs);
                if overlap > 0.0 {
                    *slot += (speed * overlap / bucket_ms) as f32;
                }
            }
        }
        let div = match scale {
            HeatmapScale::Fixed(max) => max.max(f32::EPSILON),
            HeatmapScale::Relative => out.iter().copied().fold(0.0, f32::max).max(f32::EPSILON),
        };
        for v in &mut out {
            *v = (*v / div).clamp(0.0, 1.0);
        }
        out
    }

    /// Serialise back to funscript JSON (used to upload to Handy hosting).
    pub fn to_funscript_json(&self) -> String {
        let actions: Vec<Value> = self
            .actions
            .iter()
            .map(|a| serde_json::json!({ "at": a.at, "pos": a.pos.round() as i64 }))
            .collect();
        serde_json::json!({
            "version": "1.0",
            "inverted": false,
            "range": 100,
            "actions": actions,
        })
        .to_string()
    }
}

fn segment_speed(a: Action, b: Action) -> f64 {
    let dt = (b.at - a.at) as f64 / 1000.0;
    if dt <= 0.0 {
        0.0
    } else {
        (b.pos - a.pos).abs() / dt
    }
}

/// Sort by time and collapse duplicate timestamps (last one wins, matching OFS).
fn normalize_order(actions: &mut Vec<Action>) {
    actions.sort_by_key(|a| a.at);
    let mut out: Vec<Action> = Vec::with_capacity(actions.len());
    for a in actions.drain(..) {
        match out.last_mut() {
            Some(l) if l.at == a.at => *l = a,
            _ => out.push(a),
        }
    }
    *actions = out;
}

fn parse_actions(raw: &[Value], inverted: bool, range: f64) -> Vec<Action> {
    let mut actions: Vec<Action> = raw
        .iter()
        .filter_map(|a| {
            let o = a.as_object()?;
            let at = o.get("at").and_then(lenient_f64)?;
            let pos = o.get("pos").and_then(lenient_f64)?;
            if !at.is_finite() || !pos.is_finite() {
                return None;
            }
            let mut pos = (pos * 100.0 / range).clamp(0.0, 100.0);
            if inverted {
                pos = 100.0 - pos;
            }
            Some(Action {
                at: at.round() as i64,
                pos,
            })
        })
        .collect();
    normalize_order(&mut actions);
    actions
}

fn lenient_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn lenient_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

fn lenient_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn lenient_string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(lenient_string).collect(),
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse an OFS timestamp: `"HH:MM:SS.mmm"`, `"MM:SS"`, `"SS.mmm"`, or a number (milliseconds).
pub fn parse_timestamp_ms(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|x| x.round() as i64),
        Value::String(s) => {
            let mut total = 0f64;
            for part in s.trim().split(':') {
                let x: f64 = part.trim().parse().ok()?;
                total = total * 60.0 + x;
            }
            Some((total * 1000.0).round() as i64)
        }
        _ => None,
    }
}

fn parse_metadata(v: &Value) -> Metadata {
    let Some(o) = v.as_object() else {
        return Metadata::default();
    };
    let mut m = Metadata::default();
    let mut extra = Map::new();
    for (k, val) in o {
        match k.as_str() {
            "title" => m.title = lenient_string(val),
            "creator" => m.creator = lenient_string(val),
            "description" => m.description = lenient_string(val),
            "license" => m.license = lenient_string(val),
            "notes" => m.notes = lenient_string(val),
            "script_url" => m.script_url = lenient_string(val),
            "video_url" => m.video_url = lenient_string(val),
            "type" => m.script_type = lenient_string(val),
            "duration" => m.duration_secs = lenient_f64(val),
            "performers" => m.performers = lenient_string_list(val),
            "tags" => m.tags = lenient_string_list(val),
            "chapters" => {
                m.chapters = val
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| {
                                let start_ms = c
                                    .get("startTime")
                                    .or_else(|| c.get("start"))
                                    .and_then(parse_timestamp_ms)?;
                                Some(Chapter {
                                    name: c
                                        .get("name")
                                        .and_then(lenient_string)
                                        .unwrap_or_default(),
                                    start_ms,
                                    end_ms: c
                                        .get("endTime")
                                        .or_else(|| c.get("end"))
                                        .and_then(parse_timestamp_ms),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                m.chapters.sort_by_key(|c| c.start_ms);
            }
            "bookmarks" => {
                m.bookmarks = val
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| {
                                Some(Bookmark {
                                    name: b
                                        .get("name")
                                        .and_then(lenient_string)
                                        .unwrap_or_default(),
                                    time_ms: b
                                        .get("time")
                                        .or_else(|| b.get("at"))
                                        .and_then(parse_timestamp_ms)?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                m.bookmarks.sort_by_key(|b| b.time_ms);
            }
            _ => {
                extra.insert(k.clone(), val.clone());
            }
        }
    }
    m.extra = extra;
    m
}

/// The scripts for one video, keyed by axis.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScriptSet {
    pub axes: BTreeMap<Axis, Script>,
}

impl ScriptSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.axes.values().all(Script::is_empty)
    }

    pub fn insert(&mut self, axis: Axis, script: Script) {
        self.axes.insert(axis, script);
    }

    pub fn get(&self, axis: Axis) -> Option<&Script> {
        self.axes.get(&axis)
    }

    /// The stroke script, falling back to whichever axis exists.
    pub fn primary(&self) -> Option<&Script> {
        self.axes
            .get(&Axis::L0)
            .or_else(|| self.axes.values().next())
    }

    /// Parse one file that may contain several axes (OFS `"axes"`). The top-level `actions`
    /// go to `default_axis`.
    pub fn parse_multi(json: &str, default_axis: Axis) -> Result<ScriptSet, HapticsError> {
        let v: Value =
            serde_json::from_str(json).map_err(|e| HapticsError::Parse(e.to_string()))?;
        let primary = Script::from_value(&v)?;
        let mut set = ScriptSet::new();
        if let Some(axes) = v.get("axes").and_then(Value::as_array) {
            for ax in axes {
                let Some(axis) = ax
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(Axis::from_suffix)
                else {
                    continue;
                };
                let Some(raw) = ax.get("actions").and_then(Value::as_array) else {
                    continue;
                };
                let actions = parse_actions(raw, primary.inverted, primary.range);
                set.insert(
                    axis,
                    Script {
                        actions,
                        ..primary.clone()
                    },
                );
            }
        }
        if !primary.actions.is_empty() || set.axes.is_empty() {
            set.insert(default_axis, primary);
        }
        Ok(set)
    }

    /// Add the contents of a script file, deriving the axis from its file name.
    pub fn add_file(&mut self, file_name: &str, json: &str) -> Result<(), HapticsError> {
        let axis = split_script_name(file_name)
            .map(|(_, a)| a)
            .unwrap_or(Axis::L0);
        let set = ScriptSet::parse_multi(json, axis)?;
        self.axes.extend(set.axes);
        Ok(())
    }

    /// Load every script found by [`discover_scripts`] for `video`. Blocking file I/O: call it
    /// from `spawn_blocking` or a loader thread.
    pub fn load_for_video(video: &Path) -> Result<ScriptSet, HapticsError> {
        let mut set = ScriptSet::new();
        for (axis, path) in discover_scripts(video) {
            let text = std::fs::read_to_string(&path)?;
            let mut file_set = ScriptSet::parse_multi(&text, axis)?;
            // The file-name axis wins over whatever the file itself claims as primary.
            if let Some(s) = file_set.axes.remove(&axis) {
                set.insert(axis, s);
            }
            for (a, s) in file_set.axes {
                set.axes.entry(a).or_insert(s);
            }
        }
        Ok(set)
    }

    /// Download a script from a URL (DeoVR feeds publish `fleshlight`/funscript URLs).
    pub async fn fetch(
        client: &reqwest::Client,
        url: &str,
        axis: Axis,
    ) -> Result<ScriptSet, HapticsError> {
        let resp = client.get(url).send().await?.error_for_status()?;
        let text = resp.text().await?;
        ScriptSet::parse_multi(&text, axis)
    }

    /// Latest action time across axes.
    pub fn end_ms(&self) -> i64 {
        self.axes.values().map(Script::end_ms).max().unwrap_or(0)
    }

    /// Chapters from whichever script carries them (usually the stroke script).
    pub fn chapters(&self) -> &[Chapter] {
        self.primary()
            .map(|s| s.metadata.chapters.as_slice())
            .unwrap_or(&[])
    }
}

/// Find the funscripts belonging to `video`: first next to it, then in an `Interactive/`
/// subdirectory, then in a sibling `../Interactive/` directory. Matching is on the file stem
/// (case-insensitive). The first directory that yields any script wins.
pub fn discover_scripts(video: &Path) -> Vec<(Axis, PathBuf)> {
    let Some(stem) = video.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let dir = video.parent().unwrap_or_else(|| Path::new("."));
    let mut candidates = vec![
        dir.to_path_buf(),
        dir.join("Interactive"),
        dir.join("interactive"),
    ];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.join("Interactive"));
    }
    for d in candidates {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut found: Vec<(Axis, PathBuf)> = rd
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_owned();
                let (s, axis) = split_script_name(&name)?;
                s.eq_ignore_ascii_case(stem).then(|| (axis, e.path()))
            })
            .collect();
        if !found.is_empty() {
            found.sort();
            found.dedup_by_key(|(a, _)| *a);
            return found;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "version": "1.0", "inverted": false, "range": 100, "someTool": {"x": 1},
        "actions": [
            {"at": 1000, "pos": 100, "extra": true},
            {"at": 0, "pos": 0},
            {"at": 500, "pos": 50},
            {"at": 500, "pos": 60},
            {"at": "1500", "pos": 20.5},
            {"at": 2000},
            "garbage"
        ],
        "metadata": {
            "creator": "someone", "duration": 125.5, "performers": ["A", "B"], "tags": "x, y",
            "chapters": [{"name": "Two", "startTime": "00:01:00.000", "endTime": "00:02:00.500"},
                         {"name": "One", "startTime": "00:00:01.250"}],
            "bookmarks": [{"name": "bm", "time": "00:00:30.000"}],
            "custom": 42
        }
    }"#;

    #[test]
    fn parses_lenient_file() {
        let s = Script::parse(SAMPLE).unwrap();
        assert_eq!(s.version.as_deref(), Some("1.0"));
        let at: Vec<i64> = s.actions.iter().map(|a| a.at).collect();
        assert_eq!(at, vec![0, 500, 1000, 1500]);
        assert_eq!(s.actions[1].pos, 60.0, "duplicate timestamp: last wins");
        assert_eq!(s.actions[3].pos, 20.5);
        let m = &s.metadata;
        assert_eq!(m.creator.as_deref(), Some("someone"));
        assert_eq!(m.duration_secs, Some(125.5));
        assert_eq!(m.performers, vec!["A", "B"]);
        assert_eq!(m.tags, vec!["x", "y"]);
        assert_eq!(
            m.chapters[0],
            Chapter {
                name: "One".into(),
                start_ms: 1250,
                end_ms: None
            }
        );
        assert_eq!(m.chapters[1].end_ms, Some(120_500));
        assert_eq!(m.bookmarks[0].time_ms, 30_000);
        assert_eq!(m.extra.get("custom"), Some(&Value::from(42)));
    }

    #[test]
    fn rejects_non_funscript() {
        assert!(Script::parse("[1,2]").is_err());
        assert!(Script::parse("{\"foo\": 1}").is_err());
        assert!(Script::parse("not json").is_err());
    }

    #[test]
    fn inverted_and_range() {
        let s = Script::parse(r#"{"inverted": true, "range": 50, "actions": [{"at":0,"pos":0},{"at":10,"pos":25},{"at":20,"pos":90}]}"#)
            .unwrap();
        let p: Vec<f64> = s.actions.iter().map(|a| a.pos).collect();
        assert_eq!(p, vec![100.0, 50.0, 0.0]);
    }

    #[test]
    fn interpolation() {
        let s = Script::from_actions([(1000, 0.0), (2000, 100.0), (3000, 50.0)]);
        assert_eq!(s.position_at(0.0), Some(0.0));
        assert_eq!(s.position_at(1500.0), Some(50.0));
        assert_eq!(s.position_at(2000.0), Some(100.0));
        assert_eq!(s.position_at(2500.0), Some(75.0));
        assert_eq!(s.position_at(9999.0), Some(50.0));
        assert_eq!(Script::default().position_at(0.0), None);
    }

    #[test]
    fn next_action_and_window() {
        let s = Script::from_actions([(0, 0.0), (100, 10.0), (200, 20.0), (300, 30.0)]);
        assert_eq!(s.next_action_after(-5.0).unwrap().at, 0);
        assert_eq!(s.next_action_after(0.0).unwrap().at, 100);
        assert_eq!(s.next_action_after(150.0).unwrap().at, 200);
        assert!(s.next_action_after(300.0).is_none());
        let w: Vec<i64> = s
            .actions_in_window(50.0, 200.0)
            .iter()
            .map(|a| a.at)
            .collect();
        assert_eq!(w, vec![100, 200]);
        assert!(s.actions_in_window(400.0, 100.0).is_empty());
        assert_eq!(s.speed_at(50.0), 100.0);
    }

    #[test]
    fn speed_limit_clips_and_propagates() {
        // 0 -> 100 in 100 ms is 1000 u/s; limit to 400 u/s => reaches 40.
        let mut s = Script::from_actions([(0, 0.0), (100, 100.0), (200, 0.0), (1200, 100.0)]);
        s.limit_speed(400.0);
        let p: Vec<f64> = s.actions.iter().map(|a| a.pos).collect();
        assert!((p[1] - 40.0).abs() < 1e-9);
        assert!((p[2] - 0.0).abs() < 1e-9);
        assert!((p[3] - 100.0).abs() < 1e-9);
    }

    #[test]
    fn heatmap_buckets() {
        // 0-1000 ms: 100 u/s; 1000-2000 ms: idle; 2000-2500: 200 u/s, 2500-3000 idle.
        let s = Script::from_actions([
            (0, 0.0),
            (1000, 100.0),
            (2000, 100.0),
            (2500, 0.0),
            (3000, 0.0),
        ]);
        let h = s.heatmap(6, 3000, HeatmapScale::Fixed(200.0));
        let expect = [0.5, 0.5, 0.0, 0.0, 1.0, 0.0];
        for (a, b) in h.iter().zip(expect) {
            assert!((a - b).abs() < 1e-6, "{h:?}");
        }
        let r = s.heatmap(3, 3000, HeatmapScale::Relative);
        assert!(
            (r[0] - 1.0).abs() < 1e-6 && (r[2] - 1.0).abs() < 1e-6 && r[1] == 0.0,
            "{r:?}"
        );
        assert!(Script::default()
            .heatmap(4, 1000, HeatmapScale::default())
            .iter()
            .all(|v| *v == 0.0));
    }

    #[test]
    fn heatmap_partial_overlap() {
        // One 1000 ms segment at 100 u/s straddling bucket borders.
        let s = Script::from_actions([(250, 0.0), (1250, 100.0)]);
        let h = s.heatmap(2, 2000, HeatmapScale::Fixed(100.0));
        assert!(
            (h[0] - 0.75).abs() < 1e-6 && (h[1] - 0.25).abs() < 1e-6,
            "{h:?}"
        );
    }

    #[test]
    fn file_name_axes() {
        assert_eq!(
            split_script_name("clip.funscript"),
            Some(("clip", Axis::L0))
        );
        assert_eq!(
            split_script_name("clip.surge.funscript"),
            Some(("clip", Axis::L1))
        );
        assert_eq!(
            split_script_name("clip.R2.funscript"),
            Some(("clip", Axis::R2))
        );
        assert_eq!(
            split_script_name("clip.vib.Funscript"),
            Some(("clip", Axis::V0))
        );
        assert_eq!(
            split_script_name("My.Clip.2023.funscript"),
            Some(("My.Clip.2023", Axis::L0))
        );
        assert_eq!(split_script_name("clip.mp4"), None);
        assert_eq!("twist".parse::<Axis>().unwrap(), Axis::R0);
        for a in Axis::ALL {
            assert_eq!(Axis::from_suffix(a.name()), Some(a));
            assert_eq!(Axis::from_suffix(a.tcode()), Some(a));
        }
    }

    #[test]
    fn ofs_multi_axis() {
        let j = r#"{"actions":[{"at":0,"pos":10}],"axes":[{"id":"R0","actions":[{"at":5,"pos":70}]},{"id":"zz","actions":[]}]}"#;
        let set = ScriptSet::parse_multi(j, Axis::L0).unwrap();
        assert_eq!(set.get(Axis::L0).unwrap().actions[0].pos, 10.0);
        assert_eq!(set.get(Axis::R0).unwrap().actions[0].at, 5);
        assert_eq!(set.axes.len(), 2);
    }

    #[test]
    fn discover_in_dir_and_interactive() {
        let dir = std::env::temp_dir().join(format!("fp-haptics-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Interactive")).unwrap();
        let script = r#"{"actions":[{"at":0,"pos":0},{"at":100,"pos":100}]}"#;
        std::fs::write(dir.join("Interactive/Movie.funscript"), script).unwrap();
        std::fs::write(dir.join("Interactive/movie.twist.funscript"), script).unwrap();
        std::fs::write(dir.join("Interactive/other.funscript"), script).unwrap();
        let video = dir.join("Movie.mp4");
        let found = discover_scripts(&video);
        assert_eq!(
            found.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            vec![Axis::L0, Axis::R0]
        );
        let set = ScriptSet::load_for_video(&video).unwrap();
        assert_eq!(set.axes.len(), 2);
        // A script next to the video takes precedence over Interactive/.
        std::fs::write(dir.join("Movie.funscript"), script).unwrap();
        assert_eq!(discover_scripts(&video).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn roundtrip_json() {
        let s = Script::from_actions([(0, 0.0), (100, 99.6)]);
        let back = Script::parse(&s.to_funscript_json()).unwrap();
        assert_eq!(
            back.actions[1],
            Action {
                at: 100,
                pos: 100.0
            }
        );
    }
}
