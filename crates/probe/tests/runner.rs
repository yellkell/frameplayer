//! Runner isolation against the real binary: crash capture, timeouts,
//! panics and missing results in child processes.

use fp_probe::checks::registry;
use fp_probe::report::{Isolation, Mode, Report, Status};
use fp_probe::runner::{run_child, CheckContext, CheckSpec};
use std::path::Path;
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_frameplayer-probe");

fn spec(id: &str) -> CheckSpec {
    registry().into_iter().find(|s| s.id == id).unwrap()
}

fn ctx() -> CheckContext {
    CheckContext {
        mode: Mode::Headless,
        with_session: false,
        emit_partial: false,
    }
}

#[test]
fn child_ok() {
    let r = run_child(Path::new(EXE), &spec("selftest_ok"), &ctx());
    assert_eq!(r.status, Status::Pass, "{r:?}");
    assert_eq!(r.isolation, Isolation::Child);
    assert_eq!(r.data["answer"], 42);
    assert!(r.exit.is_none());
    assert!(r.stderr_tail.is_none());
}

#[test]
fn child_segfault_is_recorded_with_signal_and_partial_results() {
    let r = run_child(Path::new(EXE), &spec("selftest_crash"), &ctx());
    assert_eq!(r.status, Status::Crashed, "{r:?}");
    let exit = r.exit.as_ref().unwrap();
    assert_eq!(exit.signal, Some(libc::SIGSEGV));
    assert_eq!(exit.signal_name.as_deref(), Some("SIGSEGV"));
    assert!(r.summary.contains("SIGSEGV"), "{}", r.summary);
    assert_eq!(r.findings[0].id, "before_crash");
    assert!(r.summary.contains("kept"));
}

#[test]
fn child_hang_times_out() {
    let mut s = spec("selftest_hang");
    s.timeout = std::time::Duration::from_millis(1500);
    let t = std::time::Instant::now();
    let r = run_child(Path::new(EXE), &s, &ctx());
    assert!(t.elapsed().as_secs() < 10);
    assert_eq!(r.status, Status::Timeout, "{r:?}");
    assert_eq!(r.findings[0].id, "before_hang");
    assert_eq!(
        r.exit.as_ref().unwrap().signal_name.as_deref(),
        Some("SIGKILL")
    );
}

#[test]
fn child_panic_is_captured() {
    let r = run_child(Path::new(EXE), &spec("selftest_panic"), &ctx());
    // Debug builds unwind (caught in the child), release builds abort.
    assert_eq!(r.status, Status::Crashed, "{r:?}");
    let text = format!(
        "{} {}",
        r.summary,
        r.stderr_tail.clone().unwrap_or_default()
    );
    assert!(text.contains("deliberate self-test panic"), "{text}");
}

#[test]
fn child_exit_without_result() {
    let r = run_child(Path::new(EXE), &spec("selftest_exit"), &ctx());
    assert_eq!(r.status, Status::Fail);
    assert!(r.summary.contains("code 3"), "{}", r.summary);
    assert_eq!(r.exit.as_ref().unwrap().code, Some(3));
}

#[test]
fn full_cli_writes_report_files() {
    let dir = std::env::temp_dir().join(format!("fp-probe-test-{}", std::process::id()));
    let out = Command::new(EXE)
        .args([
            "--headless",
            "--only",
            "selftest_ok,selftest_crash",
            "--out",
        ])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FramePlayer self-test"), "{stdout}");
    assert!(stdout.contains("[CRASHED]"), "{stdout}");
    let json = std::fs::read_to_string(dir.join("frameplayer-probe-report.json")).unwrap();
    let txt = std::fs::read_to_string(dir.join("frameplayer-probe-report.txt")).unwrap();
    assert!(txt.contains("selftest_crash"));
    let report = Report::from_json(&json).unwrap();
    assert_eq!(report.mode, Mode::Headless);
    assert_eq!(
        report.check("selftest_crash").unwrap().status,
        Status::Crashed
    );
    assert_eq!(report.check("selftest_ok").unwrap().status, Status::Pass);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_option_fails() {
    let out = Command::new(EXE).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let list = Command::new(EXE).arg("--list").output().unwrap();
    let s = String::from_utf8_lossy(&list.stdout);
    assert!(s.contains("video_decode") && !s.contains("selftest"));
}
