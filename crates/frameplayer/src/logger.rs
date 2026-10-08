//! Minimal logger: stderr plus `~/.local/share/frameplayer/frameplayer.log`
//! (truncated at start), so logs survive when launched from Steam.

use log::{LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<Mutex<std::fs::File>>,
    level: LevelFilter,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("[{:5}] {}: {}\n", r.level(), r.target(), r.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(f) = &self.file
            && let Ok(mut f) = f.lock()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

pub fn log_path() -> std::path::PathBuf {
    fp_core::dirs::data_dir().join("frameplayer.log")
}

pub fn init(verbose: bool) {
    let _ = std::fs::create_dir_all(fp_core::dirs::data_dir());
    let file = std::fs::File::create(log_path()).ok().map(Mutex::new);
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let logger = Box::leak(Box::new(Logger { file, level }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
