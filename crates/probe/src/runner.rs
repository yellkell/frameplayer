//! Check runner: every check runs isolated in a child process of this
//! binary (`frameplayer-probe --check <id>`) under a time limit, so a
//! segfault or hang inside a GPU/XR/video driver only costs that one check.
//!
//! Protocol: the child prints `@@FPPROBE-PARTIAL@@ <json>` snapshots while
//! it works and one `@@FPPROBE-RESULT@@ <json>` line (a [`CheckOutput`])
//! at the end, then `_exit(0)`s without running C atexit handlers. Any
//! other stdout/stderr output (drivers are chatty) is ignored, except a
//! stderr tail kept for checks that did not pass. When a child crashes or
//! times out, its last snapshot is kept so partial answers survive.
//!
//! If re-executing ourselves fails, checks run in-process on a worker
//! thread with `catch_unwind` (only effective with `panic = "unwind"`).

use crate::report::{CheckOutput, CheckResult, ExitInfo, Isolation, Mode, Status};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const RESULT_MARKER: &str = "@@FPPROBE-RESULT@@ ";
pub const PARTIAL_MARKER: &str = "@@FPPROBE-PARTIAL@@ ";

/// Report order of checks (most important first).
pub const IMPORTANCE: &[&str] = &[
    "video_decode",
    "vulkan",
    "dmabuf_import",
    "openxr",
    "interactive",
    "system",
    "audio",
    "storage",
    "network",
    "devkit",
];

/// Position of a check in [`IMPORTANCE`] (unknown ids sort last).
pub fn importance(id: &str) -> usize {
    IMPORTANCE
        .iter()
        .position(|&i| i == id)
        .unwrap_or(IMPORTANCE.len())
}

/// What a check body sees.
#[derive(Debug, Clone)]
pub struct CheckContext {
    pub mode: Mode,
    /// Create an XR session even in headless mode.
    pub with_session: bool,
    /// Running as a child: partial snapshots go to stdout.
    pub emit_partial: bool,
}

impl CheckContext {
    /// Allowed to create an OpenXR session (may take over the display).
    pub fn session_allowed(&self) -> bool {
        self.mode == Mode::Interactive || self.with_session
    }

    /// Publish a snapshot the parent keeps if this process dies later.
    pub fn partial(&self, out: &CheckOutput) {
        if !self.emit_partial {
            return;
        }
        if let Ok(j) = serde_json::to_string(out) {
            let mut so = std::io::stdout().lock();
            let _ = writeln!(so, "\n{PARTIAL_MARKER}{j}");
            let _ = so.flush();
        }
    }
}

/// A check the probe can run.
#[derive(Clone)]
pub struct CheckSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub timeout: Duration,
    /// Only runs in interactive (default) mode.
    pub interactive_only: bool,
    /// Not part of a normal run (self-tests of the runner).
    pub hidden: bool,
    pub run: fn(&CheckContext) -> CheckOutput,
}

impl std::fmt::Debug for CheckSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckSpec")
            .field("id", &self.id)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Name of a POSIX signal number.
pub fn signal_name(sig: i32) -> String {
    let n = match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSYS => "SIGSYS",
        _ => return format!("signal {sig}"),
    };
    n.to_string()
}

/// Keep the last `max` bytes of `buf` as text, starting on a line boundary.
pub fn tail_text(buf: &[u8], max: usize) -> String {
    let start = buf.len().saturating_sub(max);
    let mut s = String::from_utf8_lossy(&buf[start..]).into_owned();
    if start > 0 {
        if let Some(i) = s.find('\n') {
            s = s[i + 1..].to_string();
        }
    }
    s.trim_end().to_string()
}

/// Last final result and last partial snapshot found in a child's stdout.
pub fn parse_child_stdout(out: &str) -> (Option<CheckOutput>, Option<CheckOutput>) {
    let mut result = None;
    let mut partial = None;
    for line in out.lines() {
        if let Some(j) = line.strip_prefix(RESULT_MARKER) {
            if let Ok(o) = serde_json::from_str(j) {
                result = Some(o);
            }
        } else if let Some(j) = line.strip_prefix(PARTIAL_MARKER) {
            if let Ok(o) = serde_json::from_str(j) {
                partial = Some(o);
            }
        }
    }
    (result, partial)
}

const STDERR_TAIL: usize = 1500;
const STDOUT_CAP: usize = 8 << 20;

/// Run `spec` in a child process of `exe`.
pub fn run_child(exe: &Path, spec: &CheckSpec, ctx: &CheckContext) -> CheckResult {
    let start = Instant::now();
    let mut cmd = Command::new(exe);
    cmd.arg("--check")
        .arg(spec.id)
        .arg("--mode")
        .arg(ctx.mode.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if ctx.with_session {
        cmd.arg("--with-session");
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut r = run_in_process(spec, ctx);
            r.summary = format!("{} (could not start child process: {e})", r.summary);
            return r;
        }
    };
    // Reader threads forward chunks; they may outlive the child when a
    // grandchild (e.g. a runtime server it launched) inherits the pipes, so
    // they are never joined.
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_out, pipe) in [
        (
            true,
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        let Some(mut pipe) = pipe else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((is_out, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(tx);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut sink = |(is_out, chunk): (bool, Vec<u8>)| {
        if is_out {
            if stdout.len() < STDOUT_CAP {
                stdout.extend_from_slice(&chunk);
            }
        } else {
            stderr.extend_from_slice(&chunk);
            if stderr.len() > 64 << 10 {
                stderr.drain(..stderr.len() - (16 << 10));
            }
        }
    };
    let mut timed_out = false;
    let status = loop {
        while let Ok(c) = rx.try_recv() {
            sink(c);
        }
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(_) => break None,
        }
        if start.elapsed() > spec.timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        if let Ok(c) = rx.recv_timeout(Duration::from_millis(20)) {
            sink(c);
        }
    };
    let drain_until = Instant::now() + Duration::from_millis(300);
    while let Some(left) = drain_until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(c) => sink(c),
            Err(_) => break,
        }
    }
    let duration_ms = start.elapsed().as_millis() as u64;
    let (result, partial) = parse_child_stdout(&String::from_utf8_lossy(&stdout));
    let signal = status.and_then(|s| s.signal());
    let code = status.and_then(|s| s.code());

    let mut r = match (result, timed_out, signal) {
        (Some(out), _, _) => {
            let mut r = CheckResult::from_output(spec.id, spec.title, out, duration_ms);
            if timed_out {
                r.summary
                    .push_str(" (process hung while exiting and was killed)");
            }
            r
        }
        (None, true, _) => {
            let mut r = from_partial(spec, partial, duration_ms);
            r.status = Status::Timeout;
            r.summary = format!(
                "did not finish within {} s and was stopped{}",
                spec.timeout.as_secs(),
                partial_note(&r)
            );
            r
        }
        (None, false, Some(sig)) => {
            let mut r = from_partial(spec, partial, duration_ms);
            r.status = Status::Crashed;
            r.summary = format!(
                "crashed with {} (most likely inside a system driver){}",
                signal_name(sig),
                partial_note(&r)
            );
            r
        }
        (None, false, None) => {
            let mut r = from_partial(spec, partial, duration_ms);
            r.status = Status::Fail;
            r.summary = match code {
                Some(c) => format!("exited with code {c} without reporting{}", partial_note(&r)),
                None => "child process status unavailable".into(),
            };
            r
        }
    };
    r.isolation = Isolation::Child;
    if r.status != Status::Pass && (signal.is_some() || timed_out || code != Some(0)) {
        r.exit = Some(ExitInfo {
            code,
            signal,
            signal_name: signal.map(signal_name),
        });
    }
    if r.status != Status::Pass {
        let t = tail_text(&stderr, STDERR_TAIL);
        if !t.is_empty() {
            r.stderr_tail = Some(t);
        }
    }
    r
}

fn from_partial(spec: &CheckSpec, partial: Option<CheckOutput>, ms: u64) -> CheckResult {
    let out = partial.unwrap_or(CheckOutput {
        status: Status::Unknown,
        summary: String::new(),
        findings: Vec::new(),
        data: serde_json::Value::Null,
    });
    CheckResult::from_output(spec.id, spec.title, out, ms)
}

fn partial_note(r: &CheckResult) -> &'static str {
    if r.findings.is_empty() && r.data.is_null() {
        ""
    } else {
        "; results gathered before that are kept"
    }
}

/// Run `spec` on a worker thread of this process (fallback isolation).
pub fn run_in_process(spec: &CheckSpec, ctx: &CheckContext) -> CheckResult {
    let start = Instant::now();
    let (tx, rx) = mpsc::channel();
    let run = spec.run;
    let mut ctx = ctx.clone();
    ctx.emit_partial = false;
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&ctx)));
        let _ = tx.send(r);
    });
    let mut r = match rx.recv_timeout(spec.timeout) {
        Ok(Ok(out)) => CheckResult::from_output(spec.id, spec.title, out, 0),
        Ok(Err(p)) => {
            let mut r = CheckResult::skipped(spec.id, spec.title, "");
            r.status = Status::Crashed;
            r.summary = format!("panicked: {}", panic_message(&*p));
            r
        }
        Err(_) => {
            let mut r = CheckResult::skipped(spec.id, spec.title, "");
            r.status = Status::Timeout;
            r.summary = format!("did not finish within {} s", spec.timeout.as_secs());
            r
        }
    };
    r.duration_ms = start.elapsed().as_millis() as u64;
    r.isolation = Isolation::InProcess;
    r
}

/// Text of a panic payload.
pub fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

/// Entry point of `frameplayer-probe --check <id>`: run, print the result
/// line, and leave without running atexit handlers.
pub fn child_main(spec: &CheckSpec, ctx: &CheckContext) -> ! {
    let out = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (spec.run)(ctx))) {
        Ok(o) => o,
        Err(p) => CheckOutput {
            status: Status::Crashed,
            summary: format!("panicked: {}", panic_message(&*p)),
            findings: Vec::new(),
            data: serde_json::Value::Null,
        },
    };
    let j = serde_json::to_string(&out).unwrap_or_else(|_| "{}".into());
    {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "\n{RESULT_MARKER}{j}");
        let _ = so.flush();
    }
    let _ = std::io::stderr().flush();
    // SAFETY: terminating the process; skips C atexit handlers of drivers
    // that are known to hang or crash on teardown.
    unsafe { libc::_exit(0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importance_order() {
        assert!(importance("video_decode") < importance("vulkan"));
        assert!(importance("network") < importance("devkit"));
        assert_eq!(importance("selftest_crash"), IMPORTANCE.len());
    }

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(libc::SIGSEGV), "SIGSEGV");
        assert_eq!(signal_name(libc::SIGABRT), "SIGABRT");
        assert_eq!(signal_name(99), "signal 99");
    }

    #[test]
    fn stdout_parsing_picks_last_markers() {
        let p1 = serde_json::to_string(&CheckOutput {
            status: Status::Unknown,
            summary: "step 1".into(),
            findings: vec![],
            data: serde_json::Value::Null,
        })
        .unwrap();
        let fin = p1.replace("step 1", "done").replace("unknown", "pass");
        let text = format!(
            "driver noise\n{PARTIAL_MARKER}{p1}\n{PARTIAL_MARKER}{{broken\nmore noise{RESULT_MARKER}x\n{RESULT_MARKER}{fin}\n"
        );
        let (r, p) = parse_child_stdout(&text);
        assert_eq!(r.unwrap().summary, "done");
        assert_eq!(p.unwrap().summary, "step 1");
        assert_eq!(parse_child_stdout("nothing"), (None, None));
    }

    #[test]
    fn tails() {
        let s = b"line1\nline2\nline3\n";
        assert_eq!(tail_text(s, 100), "line1\nline2\nline3");
        assert_eq!(tail_text(s, 8), "line3");
        assert_eq!(tail_text(b"", 8), "");
    }

    fn ok_check(_: &CheckContext) -> CheckOutput {
        CheckOutput {
            status: Status::Pass,
            summary: "fine".into(),
            findings: vec![],
            data: serde_json::json!({"x": 1}),
        }
    }
    fn panicking_check(_: &CheckContext) -> CheckOutput {
        panic!("boom")
    }
    fn slow_check(_: &CheckContext) -> CheckOutput {
        std::thread::sleep(Duration::from_secs(5));
        ok_check(&ctx())
    }
    fn ctx() -> CheckContext {
        CheckContext {
            mode: Mode::Headless,
            with_session: false,
            emit_partial: false,
        }
    }
    fn spec(run: fn(&CheckContext) -> CheckOutput, timeout_ms: u64) -> CheckSpec {
        CheckSpec {
            id: "t",
            title: "T",
            timeout: Duration::from_millis(timeout_ms),
            interactive_only: false,
            hidden: true,
            run,
        }
    }

    #[test]
    fn in_process_outcomes() {
        let r = run_in_process(&spec(ok_check, 5000), &ctx());
        assert_eq!(r.status, Status::Pass);
        assert_eq!(r.isolation, Isolation::InProcess);
        assert_eq!(r.data["x"], 1);
        let r = run_in_process(&spec(panicking_check, 5000), &ctx());
        assert_eq!(r.status, Status::Crashed);
        assert!(r.summary.contains("boom"), "{}", r.summary);
        let r = run_in_process(&spec(slow_check, 100), &ctx());
        assert_eq!(r.status, Status::Timeout);
    }

    #[test]
    fn session_policy() {
        let mut c = ctx();
        assert!(!c.session_allowed());
        c.with_session = true;
        assert!(c.session_allowed());
        c.with_session = false;
        c.mode = Mode::Interactive;
        assert!(c.session_allowed());
    }
}
