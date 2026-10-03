//! Human-readable summary placed at the top of the shared report.

use crate::report::{Report, Status};
use std::fmt::Write;

/// Longest line we emit for a single finding.
const MAX_LINE: usize = 160;

fn clip(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One-line result counts, e.g. `3 pass, 1 fail, 2 unknown`.
pub fn counts_line(report: &Report) -> String {
    let parts: Vec<String> = Status::ALL
        .iter()
        .filter_map(|&s| {
            let n = report.checks.iter().filter(|c| c.status == s).count();
            (n > 0).then(|| format!("{n} {}", s.as_str()))
        })
        .collect();
    if parts.is_empty() {
        "no checks ran".into()
    } else {
        parts.join(", ")
    }
}

/// Render the text summary. `json_bytes` is the size of the JSON file and
/// `notes` lists anything dropped to fit the size budget.
pub fn render(report: &Report, json_bytes: usize, notes: &[String]) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "FramePlayer self-test (frameplayer-probe {}, report schema {})",
        report.probe_version,
        crate::report::SCHEMA_VERSION
    );
    let _ = writeln!(
        o,
        "Generated {} | mode: {} | arch: {} | took {:.1} s",
        report.generated_at,
        report.mode.as_str(),
        report.arch,
        report.duration_ms as f64 / 1000.0
    );
    let _ = writeln!(o, "Result: {}", counts_line(report));
    let _ = writeln!(o);
    let _ = writeln!(o, "Checks:");
    let w = report.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    for c in &report.checks {
        let _ = writeln!(
            o,
            "  [{:<7}] {:<w$}  {}",
            c.status.label(),
            c.id,
            clip(&c.summary, MAX_LINE),
        );
    }
    let answers = report.summary().answers;
    if !answers.is_empty() {
        let _ = writeln!(o);
        let _ = writeln!(o, "Answers for docs/platform-notes.md:");
        for a in &answers {
            let _ = writeln!(
                o,
                "  {:<4} [{:<7}] {}",
                a.reference,
                a.status.to_uppercase(),
                clip(&a.summary, MAX_LINE)
            );
        }
    }
    let _ = writeln!(o);
    let _ = writeln!(o, "Details:");
    for c in &report.checks {
        if c.findings.is_empty() {
            continue;
        }
        let _ = writeln!(o, "  {} ({}):", c.id, c.title);
        for f in &c.findings {
            let refs = if f.refs.is_empty() {
                String::new()
            } else {
                format!(" ({})", f.refs.join(","))
            };
            let _ = writeln!(
                o,
                "    [{:<7}] {}{}: {}",
                f.status.label(),
                f.id,
                refs,
                clip(&f.summary, MAX_LINE)
            );
        }
    }
    let _ = writeln!(o);
    for n in notes {
        let _ = writeln!(o, "Note: {n}");
    }
    let _ = writeln!(
        o,
        "Full details: frameplayer-probe-report.json ({:.1} KB). Nothing was uploaded anywhere.",
        json_bytes as f64 / 1024.0
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{CheckOutput, CheckResult, Finding, Mode};
    use serde_json::Value;

    fn report() -> Report {
        let mut a = CheckResult::from_output(
            "video_decode",
            "Hardware video decode",
            CheckOutput {
                status: Status::Fail,
                summary: "no V4L2 decoder devices".into(),
                findings: vec![Finding {
                    id: "v4l2_nodes".into(),
                    status: Status::Fail,
                    summary: "no /dev/video* nodes".into(),
                    refs: vec!["P2".into(), "P3".into()],
                }],
                data: Value::Null,
            },
            10,
        );
        a.isolation = crate::report::Isolation::Child;
        let b = CheckResult::skipped("interactive", "Controller test", "headless mode");
        Report {
            probe_version: "0.1.0".into(),
            generated_at: "2026-10-03T00:00:00Z".into(),
            mode: Mode::Headless,
            arch: "x86_64".into(),
            duration_ms: 1500,
            checks: vec![a, b],
        }
    }

    #[test]
    fn renders_all_sections() {
        let t = render(&report(), 2048, &["dropped details of network".into()]);
        assert!(t.starts_with("FramePlayer self-test (frameplayer-probe 0.1.0"));
        assert!(t.contains("Result: 1 fail, 1 skipped"), "{t}");
        assert!(
            t.contains("[FAIL   ] video_decode  no V4L2 decoder devices"),
            "{t}"
        );
        assert!(t.contains("[SKIPPED] interactive   headless mode"), "{t}");
        assert!(t.contains("P2   [FAIL   ] no /dev/video* nodes"), "{t}");
        assert!(
            t.contains("[FAIL   ] v4l2_nodes (P2,P3): no /dev/video* nodes"),
            "{t}"
        );
        assert!(t.contains("Note: dropped details of network"));
        assert!(t.contains("(2.0 KB)"));
        assert!(t.contains("took 1.5 s"));
    }

    #[test]
    fn clips_long_lines() {
        let long = "x".repeat(500);
        let c = clip(&long, 20);
        assert_eq!(c.chars().count(), 20);
        assert!(c.ends_with('…'));
        assert_eq!(clip("a\nb", 20), "a b");
    }

    #[test]
    fn empty_report() {
        let mut r = report();
        r.checks.clear();
        assert_eq!(counts_line(&r), "no checks ran");
        assert!(render(&r, 0, &[]).contains("Checks:"));
    }
}
