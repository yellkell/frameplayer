//! Minimal logger: stderr plus `~/.local/share/frameplayer/frameplayer.log`,
//! so logs survive when launched from Steam. Each start moves the last
//! run's log to `frameplayer.log.old` (the one wanted after a hang or a
//! crash, once FramePlayer has been started again). Lines carry the seconds
//! since start.

use log::{LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

struct Logger {
    file: Option<Mutex<std::fs::File>>,
    level: LevelFilter,
    start: Instant,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!(
            "{:9.3} [{:5}] {}: {}\n",
            self.start.elapsed().as_secs_f64(),
            r.level(),
            r.target(),
            r.args()
        );
        #[cfg(not(target_os = "android"))]
        let _ = std::io::stderr().write_all(line.as_bytes());
        #[cfg(target_os = "android")]
        logcat(r.level(), &line);
        if let Some(f) = &self.file
            && let Ok(mut f) = f.lock()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
    fn flush(&self) {}
}

/// Android has no stderr anyone reads: lines go to logcat, tag FramePlayer.
#[cfg(target_os = "android")]
fn logcat(level: log::Level, line: &str) {
    use std::ffi::{CString, c_char, c_int};
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }
    let prio = match level {
        log::Level::Error => 6,
        log::Level::Warn => 5,
        log::Level::Info => 4,
        log::Level::Debug => 3,
        log::Level::Trace => 2,
    };
    if let Ok(text) = CString::new(line.trim_end()) {
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe { __android_log_write(prio, c"FramePlayer".as_ptr(), text.as_ptr()) };
    }
}

pub fn log_path() -> std::path::PathBuf {
    fp_core::dirs::data_dir().join("frameplayer.log")
}

pub fn init(verbose: bool) {
    let _ = std::fs::create_dir_all(fp_core::dirs::data_dir());
    let path = log_path();
    let _ = std::fs::rename(&path, path.with_extension("log.old"));
    let file = std::fs::File::create(&path).ok().map(Mutex::new);
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let logger = Box::leak(Box::new(Logger {
        file,
        level,
        start: Instant::now(),
    }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}
