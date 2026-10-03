//! fp-probe: `frameplayer-probe`, the on-device self-test.
//!
//! Answers as many of the `[verify]` questions in `docs/platform-notes.md`
//! as can be answered automatically and writes one shareable report
//! (`~/frameplayer-probe-report.json` + `.txt`). Two modes:
//!
//! * `--headless` (over SSH from `frameplayer-install`, nobody wearing the
//!   headset): no XR session unless `--with-session`, nothing interactive.
//! * default (launched from the Steam library): adds the XR session checks
//!   and a guided controller / hand / eye test.
//!
//! Modules: [`report`] (schema), [`summary`] (text), [`redact`] (privacy +
//! size budget), [`runner`] (child-process isolation, timeouts, crash
//! capture), [`checks`] (the checks), [`parse`] (pure parsers), [`clips`]
//! (embedded test videos), [`util`].

pub mod checks;
pub mod clips;
pub mod parse;
pub mod redact;
pub mod report;
pub mod runner;
pub mod summary;
pub mod util;

/// Version of the probe (workspace version).
pub const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Report file names, written to `$HOME` (or `--out <dir>`).
pub const REPORT_JSON: &str = "frameplayer-probe-report.json";
pub const REPORT_TXT: &str = "frameplayer-probe-report.txt";
