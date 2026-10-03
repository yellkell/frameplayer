//! The checks. Each is a plain function `fn(&CheckContext) -> CheckOutput`
//! registered in [`registry`]; the runner executes it in a child process.
//!
//! | id | answers (docs/platform-notes.md) |
//! |---|---|
//! | `video_decode` | P2, P3, P4 (V4L2 nodes, formats, real decodes) |
//! | `vulkan` | P4, P5, P9 (driver, extensions, modifiers, Vulkan Video) |
//! | `dmabuf_import` | P4 (decoder DMA-BUF imported into Vulkan, via fp-gfx) |
//! | `openxr` | P1, P6, P8, P10, P11, P12, I11 |
//! | `interactive` | P1, P10, P11 + hand tracking (needs the headset on) |
//! | `system` | P7, P9, P18, I3 |
//! | `audio` | P15 |
//! | `storage` | P14 |
//! | `network` | P16 |
//! | `devkit` | I1, I4, I5, I6, I10, I14 |

use crate::report::{CheckOutput, Status};
use crate::runner::{CheckContext, CheckSpec};
use crate::util::Out;
use std::time::Duration;

pub mod audio;
pub mod devkit;
pub mod dmabuf;
pub mod interactive;
pub mod network;
pub mod openxr;
pub mod storage;
pub mod system;
pub mod video;
pub mod vulkan;

/// All checks, in execution order (cheap and safe first). The report
/// orders them by importance instead (see `runner::IMPORTANCE`).
pub fn registry() -> Vec<CheckSpec> {
    let s = |id, title, secs, run| CheckSpec {
        id,
        title,
        timeout: Duration::from_secs(secs),
        interactive_only: false,
        hidden: false,
        run,
    };
    let mut v = vec![
        s(
            "system",
            "System, libraries and runtime environment",
            30,
            system::run,
        ),
        s("storage", "Storage and removable media", 20, storage::run),
        s(
            "devkit",
            "Developer-mode install environment",
            20,
            devkit::run,
        ),
        s(
            "network",
            "Network (LAN discovery, ports)",
            20,
            network::run,
        ),
        s("audio", "Audio output", 30, audio::run),
        s(
            "video_decode",
            "Hardware video decode (V4L2)",
            120,
            video::run,
        ),
        s("vulkan", "Vulkan driver capabilities", 60, vulkan::run),
        s(
            "dmabuf_import",
            "Decoder DMA-BUF into Vulkan (zero-copy)",
            90,
            dmabuf::run,
        ),
        s("openxr", "OpenXR runtime", 90, openxr::run),
    ];
    let mut inter = s(
        "interactive",
        "In-headset controller, hand and eye test",
        480,
        interactive::run,
    );
    inter.interactive_only = true;
    v.push(inter);
    for (id, run) in [
        (
            "selftest_ok",
            selftest_ok as fn(&CheckContext) -> CheckOutput,
        ),
        ("selftest_crash", selftest_crash),
        ("selftest_hang", selftest_hang),
        ("selftest_panic", selftest_panic),
        ("selftest_exit", selftest_exit),
    ] {
        let mut c = s(id, "Runner self-test", 3, run);
        c.hidden = true;
        v.push(c);
    }
    v
}

fn selftest_ok(_: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    o.finding("ok", Status::Pass, &[], "runner works");
    o.set("answer", 42);
    o.finish(Status::Pass, "self-test passed")
}

/// Deliberately dies with SIGSEGV after publishing a partial result.
fn selftest_crash(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    o.finding(
        "before_crash",
        Status::Pass,
        &[],
        "this was recorded before the crash",
    );
    ctx.partial(&o.snapshot(Status::Unknown, "about to crash"));
    // SAFETY: intentionally terminating this child process with a signal.
    unsafe {
        libc::signal(libc::SIGSEGV, libc::SIG_DFL);
        libc::raise(libc::SIGSEGV);
    }
    o.finish(Status::Fail, "unreachable")
}

fn selftest_hang(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    o.finding("before_hang", Status::Pass, &[], "recorded before hanging");
    ctx.partial(&o.snapshot(Status::Unknown, "about to hang"));
    std::thread::sleep(Duration::from_secs(3600));
    o.finish(Status::Fail, "unreachable")
}

fn selftest_panic(_: &CheckContext) -> CheckOutput {
    panic!("deliberate self-test panic")
}

fn selftest_exit(_: &CheckContext) -> CheckOutput {
    println!("some noise without a result line");
    std::process::exit(3)
}

/// Shorthand used by checks: combine finding statuses into a check status.
/// Any pass → pass unless `all_required` and something failed.
pub fn combine(o: &Out) -> Status {
    let any = |s: Status| o.findings.iter().any(|f| f.status == s);
    if any(Status::Fail) {
        Status::Fail
    } else if any(Status::Pass) {
        Status::Pass
    } else {
        Status::Unknown
    }
}
