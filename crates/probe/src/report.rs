//! Report schema.
//!
//! One [`Report`] per probe run, serialized as JSON with a fixed key order:
//! metadata, `summary`, then one object per check in order of importance
//! (see [`crate::runner::IMPORTANCE`]). Bump [`SCHEMA_VERSION`] whenever a
//! field is renamed or its meaning changes; adding fields is compatible.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Identifies the file format to tools that read reports.
pub const SCHEMA: &str = "frameplayer-probe-report";
/// Version of the report layout.
pub const SCHEMA_VERSION: u32 = 1;

/// Outcome of a check or finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The question was answered and the answer is good for FramePlayer.
    Pass,
    /// The question was answered and the answer is bad (or the operation failed).
    Fail,
    /// Could not be determined here (library / device / runtime not available).
    Unknown,
    /// Not run in this mode.
    Skipped,
    /// The isolated child process died (signal) before reporting.
    Crashed,
    /// The check exceeded its time limit and was killed.
    Timeout,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Pass,
        Status::Fail,
        Status::Unknown,
        Status::Skipped,
        Status::Crashed,
        Status::Timeout,
    ];

    /// Fixed-width upper-case label for the text summary.
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Unknown => "UNKNOWN",
            Status::Skipped => "SKIPPED",
            Status::Crashed => "CRASHED",
            Status::Timeout => "TIMEOUT",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Unknown => "unknown",
            Status::Skipped => "skipped",
            Status::Crashed => "crashed",
            Status::Timeout => "timeout",
        }
    }
}

/// One answered question inside a check, optionally tied to rows of
/// `docs/platform-notes.md` (`"P2"`, `"I4"`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub status: Status,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// What a check body returns (also the payload a child process prints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutput {
    pub status: Status,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub data: Value,
}

/// How a check was isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Ran in a child process (`frameplayer-probe --check <id>`).
    Child,
    /// Ran inside the main process (fallback when re-executing failed).
    InProcess,
}

/// How a child process ended when it did not report normally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_name: Option<String>,
}

/// A finished check as it appears in the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub title: String,
    pub status: Status,
    pub summary: String,
    pub duration_ms: u64,
    pub isolation: Isolation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    /// Last lines of the child's stderr, kept only for non-passing checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub data: Value,
    /// Check id (the key of this object in the report).
    #[serde(skip)]
    pub id: String,
}

impl CheckResult {
    pub fn from_output(id: &str, title: &str, out: CheckOutput, duration_ms: u64) -> Self {
        CheckResult {
            id: id.into(),
            title: title.into(),
            status: out.status,
            summary: out.summary,
            duration_ms,
            isolation: Isolation::Child,
            exit: None,
            stderr_tail: None,
            findings: out.findings,
            data: out.data,
        }
    }

    /// A check that did not run.
    pub fn skipped(id: &str, title: &str, why: &str) -> Self {
        CheckResult::from_output(
            id,
            title,
            CheckOutput {
                status: Status::Skipped,
                summary: why.into(),
                findings: Vec::new(),
                data: Value::Null,
            },
            0,
        )
    }
}

/// Probe mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No XR session unless `--with-session`; nothing interactive.
    Headless,
    /// Adds the XR session checks and the controller walk-through.
    Interactive,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Headless => "headless",
            Mode::Interactive => "interactive",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "headless" => Some(Mode::Headless),
            "interactive" => Some(Mode::Interactive),
            _ => None,
        }
    }
}

/// Combined verdict for one platform-notes row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    #[serde(rename = "ref")]
    pub reference: String,
    /// `pass`, `fail`, `partial` (some findings pass, some fail) or `unknown`.
    pub status: String,
    pub summary: String,
}

/// Top of the report: counts, one line per check, answers per row.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Summary {
    pub counts: BTreeMap<String, u32>,
    /// `"<check>: <STATUS> <one-line summary>"`, in report order.
    pub checks: Vec<String>,
    pub answers: Vec<Answer>,
}

impl Summary {
    pub fn build(checks: &[CheckResult]) -> Summary {
        let mut counts = BTreeMap::new();
        for s in Status::ALL {
            let n = checks.iter().filter(|c| c.status == s).count() as u32;
            if n > 0 {
                counts.insert(s.as_str().to_string(), n);
            }
        }
        Summary {
            counts,
            checks: checks
                .iter()
                .map(|c| format!("{}: {} {}", c.id, c.status.label(), c.summary))
                .collect(),
            answers: answers(checks),
        }
    }
}

/// Group findings by platform-notes row.
pub fn answers(checks: &[CheckResult]) -> Vec<Answer> {
    let mut by_ref: BTreeMap<String, Vec<&Finding>> = BTreeMap::new();
    for c in checks {
        for f in &c.findings {
            for r in &f.refs {
                by_ref.entry(r.clone()).or_default().push(f);
            }
        }
    }
    let mut out: Vec<Answer> = by_ref
        .into_iter()
        .map(|(r, fs)| {
            let decided: Vec<&&Finding> = fs
                .iter()
                .filter(|f| matches!(f.status, Status::Pass | Status::Fail))
                .collect();
            let pass = decided.iter().any(|f| f.status == Status::Pass);
            let fail = decided.iter().any(|f| f.status == Status::Fail)
                || fs
                    .iter()
                    .any(|f| matches!(f.status, Status::Crashed | Status::Timeout));
            let status = match (pass, fail) {
                (true, false) => "pass",
                (true, true) => "partial",
                (false, true) => "fail",
                (false, false) => "unknown",
            };
            let summary = fs
                .iter()
                .map(|f| f.summary.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            Answer {
                reference: r,
                status: status.into(),
                summary,
            }
        })
        .collect();
    out.sort_by_key(|a| ref_sort_key(&a.reference));
    out
}

/// `P2` < `P10` < `I1`: platform rows first, numeric order.
fn ref_sort_key(r: &str) -> (u8, u32, String) {
    let (prefix, num) = r.split_at(r.find(|c: char| c.is_ascii_digit()).unwrap_or(r.len()));
    let group = match prefix {
        "P" => 0,
        "I" => 1,
        _ => 2,
    };
    (group, num.parse().unwrap_or(u32::MAX), r.to_string())
}

/// The whole report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub probe_version: String,
    pub generated_at: String,
    pub mode: Mode,
    pub arch: String,
    pub duration_ms: u64,
    /// Checks in report order (most important first).
    pub checks: Vec<CheckResult>,
}

impl Report {
    pub fn summary(&self) -> Summary {
        Summary::build(&self.checks)
    }

    pub fn check(&self, id: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|c| c.id == id)
    }

    /// Pretty JSON (key order preserved, see module docs). Not redacted;
    /// use [`crate::redact::finalize`] for the file that gets shared.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serializes")
    }

    /// Parse a report produced by [`Report::to_json_pretty`]. Checks come
    /// back in importance order.
    pub fn from_json(s: &str) -> Result<Report, String> {
        let v: Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        let Value::Object(mut m) = v else {
            return Err("report is not an object".into());
        };
        let take_str = |m: &mut Map<String, Value>, k: &str| -> Result<String, String> {
            match m.remove(k) {
                Some(Value::String(s)) => Ok(s),
                _ => Err(format!("missing string field {k}")),
            }
        };
        if take_str(&mut m, "schema")? != SCHEMA {
            return Err("not a frameplayer-probe report".into());
        }
        let ver = m
            .remove("schema_version")
            .and_then(|v| v.as_u64())
            .ok_or("missing schema_version")?;
        if ver != SCHEMA_VERSION as u64 {
            return Err(format!("unsupported schema_version {ver}"));
        }
        let probe_version = take_str(&mut m, "probe_version")?;
        let generated_at = take_str(&mut m, "generated_at")?;
        let mode = Mode::parse(&take_str(&mut m, "mode")?).ok_or("bad mode")?;
        let arch = take_str(&mut m, "arch")?;
        let duration_ms = m
            .remove("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        m.remove("summary");
        let mut checks = Vec::new();
        for (id, v) in m {
            let mut c: CheckResult =
                serde_json::from_value(v).map_err(|e| format!("check {id}: {e}"))?;
            c.id = id;
            checks.push(c);
        }
        checks.sort_by_key(|c| crate::runner::importance(&c.id));
        Ok(Report {
            probe_version,
            generated_at,
            mode,
            arch,
            duration_ms,
            checks,
        })
    }
}

impl Serialize for Report {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("schema", SCHEMA)?;
        m.serialize_entry("schema_version", &SCHEMA_VERSION)?;
        m.serialize_entry("probe_version", &self.probe_version)?;
        m.serialize_entry("generated_at", &self.generated_at)?;
        m.serialize_entry("mode", self.mode.as_str())?;
        m.serialize_entry("arch", &self.arch)?;
        m.serialize_entry("duration_ms", &self.duration_ms)?;
        m.serialize_entry("summary", &self.summary())?;
        for c in &self.checks {
            m.serialize_entry(&c.id, c)?;
        }
        m.end()
    }
}

/// Seconds since the Unix epoch → `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn finding(id: &str, status: Status, refs: &[&str]) -> Finding {
        Finding {
            id: id.into(),
            status,
            summary: format!("{id} is {}", status.as_str()),
            refs: refs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sample() -> Report {
        let mut video = CheckResult::from_output(
            "video_decode",
            "Hardware video decode",
            CheckOutput {
                status: Status::Pass,
                summary: "HEVC decodes".into(),
                findings: vec![
                    finding("hevc", Status::Pass, &["P2"]),
                    finding("av1", Status::Fail, &["P2"]),
                    finding("nodes", Status::Pass, &["P3"]),
                ],
                data: json!({"devices": []}),
            },
            1234,
        );
        video.stderr_tail = None;
        let mut sys = CheckResult::skipped("system", "System", "not run");
        sys.findings
            .push(finding("glibc", Status::Unknown, &["P7"]));
        let mut crashed = CheckResult::skipped("vulkan", "Vulkan", "child died");
        crashed.status = Status::Crashed;
        crashed.exit = Some(ExitInfo {
            code: None,
            signal: Some(11),
            signal_name: Some("SIGSEGV".into()),
        });
        crashed
            .findings
            .push(finding("vk", Status::Crashed, &["P10"]));
        Report {
            probe_version: "0.1.0".into(),
            generated_at: format_utc(0),
            mode: Mode::Headless,
            arch: "aarch64".into(),
            duration_ms: 5,
            checks: vec![video, crashed, sys],
        }
    }

    #[test]
    fn serializes_in_fixed_key_order() {
        let r = sample();
        let s = r.to_json_pretty();
        let pos = |k: &str| s.find(&format!("\"{k}\":")).unwrap_or(usize::MAX);
        let order = [
            "schema",
            "schema_version",
            "probe_version",
            "generated_at",
            "mode",
            "arch",
            "summary",
            "video_decode",
            "vulkan",
            "system",
        ];
        for w in order.windows(2) {
            assert!(pos(w[0]) < pos(w[1]), "{} before {}", w[0], w[1]);
        }
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["schema_version"], SCHEMA_VERSION);
        assert_eq!(v["vulkan"]["exit"]["signal_name"], "SIGSEGV");
        assert_eq!(v["video_decode"]["findings"][0]["refs"][0], "P2");
        assert!(v["system"]["findings"][0].get("refs").is_some());
        // Absent optional fields are omitted, not null.
        assert!(v["video_decode"].get("exit").is_none());
        assert_eq!(v["summary"]["counts"]["pass"], 1);
        assert_eq!(v["summary"]["counts"]["crashed"], 1);
    }

    #[test]
    fn round_trips() {
        let r = sample();
        let back = Report::from_json(&r.to_json_pretty()).unwrap();
        assert_eq!(back.probe_version, r.probe_version);
        assert_eq!(back.mode, Mode::Headless);
        let ids: Vec<&str> = back.checks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["video_decode", "vulkan", "system"]);
        assert_eq!(back.checks, r.checks);
        assert!(Report::from_json("{\"schema\":\"other\"}").is_err());
        assert!(Report::from_json("[]").is_err());
    }

    #[test]
    fn answers_combine_findings() {
        let r = sample();
        let a = r.summary().answers;
        let get = |k: &str| a.iter().find(|x| x.reference == k).unwrap();
        assert_eq!(get("P2").status, "partial");
        assert_eq!(get("P3").status, "pass");
        assert_eq!(get("P7").status, "unknown");
        assert_eq!(get("P10").status, "fail");
        assert!(get("P2").summary.contains("hevc is pass"));
        let order: Vec<&str> = a.iter().map(|x| x.reference.as_str()).collect();
        assert_eq!(order, ["P2", "P3", "P7", "P10"]);
    }

    #[test]
    fn ref_ordering() {
        let mut v = vec!["I2", "P10", "P2", "X", "I10"];
        v.sort_by_key(|r| ref_sort_key(r));
        assert_eq!(v, ["P2", "P10", "I2", "I10", "X"]);
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_791_000_000), "2026-10-03T04:00:00Z");
        assert_eq!(format_utc(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn status_strings() {
        for s in Status::ALL {
            let j = serde_json::to_string(&s).unwrap();
            assert_eq!(j, format!("\"{}\"", s.as_str()));
            assert_eq!(s.label().to_lowercase(), s.as_str());
        }
    }
}
