//! `frameplayer-probe`: see the crate docs (`fp_probe`) and README.md.

use fp_probe::checks;
use fp_probe::redact::{self, RedactContext};
use fp_probe::report::{CheckResult, Mode, Report};
use fp_probe::runner::{self, CheckContext};
use fp_probe::{util, REPORT_JSON, REPORT_TXT};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

const USAGE: &str = "\
frameplayer-probe: FramePlayer self-test for the Steam Frame.

Usage: frameplayer-probe [--headless] [--with-session] [--only a,b] [--out DIR] [--summary-only]

  (no options)     full test: system, video, Vulkan, OpenXR session and a
                   guided controller test in the headset
  --headless       no headset interaction and no XR session (for SSH)
  --with-session   create an OpenXR session even with --headless
  --only IDS       run only these checks (comma separated)
  --out DIR        write the report there instead of $HOME
  --summary-only   print only the text summary, not the JSON
  --list           list check ids

Writes ~/frameplayer-probe-report.json and ~/frameplayer-probe-report.txt.
Nothing is uploaded anywhere.
";

struct Args {
    mode: Mode,
    with_session: bool,
    only: Option<Vec<String>>,
    out: Option<PathBuf>,
    summary_only: bool,
    child: Option<String>,
    list: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        mode: Mode::Interactive,
        with_session: false,
        only: None,
        out: None,
        summary_only: false,
        child: None,
        list: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--headless" => a.mode = Mode::Headless,
            "--interactive" => a.mode = Mode::Interactive,
            "--with-session" => a.with_session = true,
            "--summary-only" => a.summary_only = true,
            "--list" => a.list = true,
            "--only" => {
                let v = it.next().ok_or("--only needs a value")?;
                a.only = Some(v.split(',').map(|s| s.trim().to_string()).collect());
            }
            "--out" => a.out = Some(it.next().ok_or("--out needs a directory")?.into()),
            "--check" => a.child = Some(it.next().ok_or("--check needs an id")?),
            "--mode" => {
                let v = it.next().ok_or("--mode needs a value")?;
                a.mode = Mode::parse(&v).ok_or_else(|| format!("bad mode {v}"))?;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}\n");
            }
            eprint!("{USAGE}");
            std::process::exit(if e.is_empty() { 0 } else { 2 });
        }
    };
    let registry = checks::registry();
    if args.list {
        for s in &registry {
            if !s.hidden {
                println!("{:<14} {}", s.id, s.title);
            }
        }
        return;
    }
    if let Some(id) = &args.child {
        let Some(spec) = registry.iter().find(|s| s.id == id) else {
            eprintln!("unknown check {id}");
            std::process::exit(2);
        };
        let ctx = CheckContext {
            mode: args.mode,
            with_session: args.with_session,
            emit_partial: true,
        };
        runner::child_main(spec, &ctx);
    }
    std::process::exit(run_all(&args, &registry));
}

fn run_all(args: &Args, registry: &[runner::CheckSpec]) -> i32 {
    let start = Instant::now();
    let generated_at = fp_probe::report::format_utc(util::unix_now());
    let ctx = CheckContext {
        mode: args.mode,
        with_session: args.with_session,
        emit_partial: false,
    };
    let selected: Vec<&runner::CheckSpec> = registry
        .iter()
        .filter(|s| match &args.only {
            Some(only) => only.iter().any(|o| o == s.id),
            None => !s.hidden,
        })
        .collect();
    if selected.is_empty() {
        eprintln!("no checks selected (see --list)");
        return 2;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let out_dir = args.out.clone().unwrap_or_else(util::home);
    let redact_ctx = RedactContext::from_env();
    eprintln!(
        "FramePlayer self-test ({} mode). This takes up to a few minutes; results go to {}/{REPORT_JSON}",
        args.mode.as_str(),
        redact::tilde(&out_dir.display().to_string(), redact_ctx.home.as_deref())
    );
    let mut results: Vec<CheckResult> = Vec::new();
    let n = selected.len();
    for (i, spec) in selected.iter().enumerate() {
        eprint!("[{}/{n}] {:<14} ", i + 1, spec.id);
        let _ = std::io::stderr().flush();
        let r = if spec.interactive_only && args.mode == Mode::Headless {
            CheckResult::skipped(
                spec.id,
                spec.title,
                "needs someone wearing the headset; run without --headless",
            )
        } else {
            runner::run_child(&exe, spec, &ctx)
        };
        eprintln!(
            "{:<8} {:>5.1} s  {}",
            r.status.label(),
            r.duration_ms as f64 / 1000.0,
            r.summary.chars().take(100).collect::<String>()
        );
        results.push(r);
        // Save after every check so a hard crash of the whole run still
        // leaves the answers gathered so far.
        let _ = write_report(
            args,
            &results,
            &generated_at,
            start,
            &out_dir,
            &redact_ctx,
            false,
        );
    }
    match write_report(
        args,
        &results,
        &generated_at,
        start,
        &out_dir,
        &redact_ctx,
        true,
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("could not write the report to {}: {e}", out_dir.display());
            1
        }
    }
}

fn write_report(
    args: &Args,
    results: &[CheckResult],
    generated_at: &str,
    start: Instant,
    out_dir: &std::path::Path,
    redact_ctx: &RedactContext,
    last: bool,
) -> std::io::Result<()> {
    let mut checks = results.to_vec();
    checks.sort_by_key(|c| runner::importance(&c.id));
    let report = Report {
        probe_version: fp_probe::PROBE_VERSION.into(),
        generated_at: generated_at.into(),
        mode: args.mode,
        arch: std::env::consts::ARCH.into(),
        duration_ms: start.elapsed().as_millis() as u64,
        checks,
    };
    let f = redact::finalize(report, redact_ctx);
    std::fs::create_dir_all(out_dir)?;
    write_atomic(&out_dir.join(REPORT_JSON), &f.json)?;
    write_atomic(&out_dir.join(REPORT_TXT), &f.text)?;
    if last {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{}", f.text);
        if !args.summary_only {
            let _ = writeln!(so, "----- {REPORT_JSON} -----");
            let _ = writeln!(so, "{}", f.json);
        }
        eprintln!(
            "Report saved: {}/{REPORT_JSON} ({:.1} KB) and {REPORT_TXT}",
            redact::tilde(&out_dir.display().to_string(), redact_ctx.home.as_deref()),
            f.json.len() as f64 / 1024.0
        );
    }
    Ok(())
}

fn write_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("report");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}
