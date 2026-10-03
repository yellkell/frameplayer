//! Frame-timing capture for `tools/perf-capture.sh`.
//!
//! Protocol (documented in the script): when
//! `$XDG_DATA_HOME/frameplayer/perf/ENABLE` exists or `FP_PERF_LOG=1`, one
//! CSV row per presented frame goes to `perf/frame_timing-<unix>.csv`:
//!
//! ```text
//! t_ns,display_period_ns,cpu_ms,gpu_ms,decode_queue,dropped
//! ```
//!
//! `display_period_ns` is the measured interval between consecutive
//! predicted display times (what the summary turns into frame rate),
//! `dropped` the number of display refreshes missed before this frame.
//! Rows are written by a helper thread so the render thread never touches
//! the filesystem. A `perf/OPEN` file names a video to open at start (the
//! script's `--video`).

use std::io::Write;
use std::path::Path;

pub const HEADER: &str = "t_ns,display_period_ns,cpu_ms,gpu_ms,decode_queue,dropped";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerfRow {
    pub t_ns: i64,
    pub display_period_ns: i64,
    pub cpu_ms: f32,
    pub gpu_ms: f32,
    pub decode_queue: usize,
    pub dropped: u32,
}

impl PerfRow {
    pub fn to_csv(self) -> String {
        format!(
            "{},{},{:.3},{:.3},{},{}",
            self.t_ns,
            self.display_period_ns,
            self.cpu_ms,
            self.gpu_ms,
            self.decode_queue,
            self.dropped
        )
    }
}

/// Is capture requested?
pub fn enabled(perf_dir: &Path) -> bool {
    std::env::var("FP_PERF_LOG").is_ok_and(|v| v == "1") || perf_dir.join("ENABLE").exists()
}

/// Consume a pending `perf/OPEN` request.
pub fn take_open_request(perf_dir: &Path) -> Option<String> {
    let p = perf_dir.join("OPEN");
    let s = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    let line = s.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Missed refreshes given the measured interval and the nominal period.
pub fn dropped_frames(interval_ns: i64, period_ns: i64) -> u32 {
    if period_ns <= 0 || interval_ns <= 0 {
        return 0;
    }
    let ratio = interval_ns as f64 / period_ns as f64;
    (ratio.round() as i64 - 1).max(0) as u32
}

/// Per-frame recorder.
pub struct PerfLog {
    tx: crossbeam_channel::Sender<PerfRow>,
    last_t: Option<i64>,
}

impl PerfLog {
    pub fn start(perf_dir: &Path) -> std::io::Result<PerfLog> {
        std::fs::create_dir_all(perf_dir)?;
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = perf_dir.join(format!("frame_timing-{unix}.csv"));
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        writeln!(file, "{HEADER}")?;
        let (tx, rx) = crossbeam_channel::bounded::<PerfRow>(4096);
        std::thread::Builder::new()
            .name("fp-perf".into())
            .spawn(move || {
                let mut n = 0u32;
                while let Ok(row) = rx.recv() {
                    if writeln!(file, "{}", row.to_csv()).is_err() {
                        break;
                    }
                    n += 1;
                    if n.is_multiple_of(256) {
                        let _ = file.flush();
                    }
                }
                let _ = file.flush();
            })?;
        tracing::info!("perf capture → {}", path.display());
        Ok(PerfLog { tx, last_t: None })
    }

    /// Record a presented frame (never blocks; rows are dropped if the
    /// writer falls behind).
    pub fn record(&mut self, t_ns: i64, period_ns: i64, cpu_ms: f32, gpu_ms: f32, queue: usize) {
        let interval = self.last_t.map_or(period_ns, |l| t_ns - l);
        self.last_t = Some(t_ns);
        let _ = self.tx.try_send(PerfRow {
            t_ns,
            display_period_ns: interval,
            cpu_ms,
            gpu_ms,
            decode_queue: queue,
            dropped: dropped_frames(interval, period_ns),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_rows() {
        let r = PerfRow {
            t_ns: 1000,
            display_period_ns: 13_888_889,
            cpu_ms: 2.5,
            gpu_ms: 0.0,
            decode_queue: 3,
            dropped: 0,
        };
        assert_eq!(r.to_csv(), "1000,13888889,2.500,0.000,3,0");
        assert_eq!(HEADER.split(',').count(), r.to_csv().split(',').count());
    }

    #[test]
    fn dropped_counting() {
        let p = 13_888_889;
        assert_eq!(dropped_frames(p, p), 0);
        assert_eq!(dropped_frames(p * 2, p), 1);
        assert_eq!(dropped_frames(p * 3 + 100, p), 2);
        assert_eq!(dropped_frames(p + p / 3, p), 0);
        assert_eq!(dropped_frames(0, p), 0);
    }

    #[test]
    fn writes_file_and_open_request() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!enabled(dir.path()) || std::env::var("FP_PERF_LOG").is_ok());
        std::fs::write(dir.path().join("ENABLE"), "").unwrap();
        assert!(enabled(dir.path()));
        let mut log = PerfLog::start(dir.path()).unwrap();
        log.record(0, 1000, 1.0, 0.0, 2);
        log.record(2000, 1000, 1.0, 0.0, 2);
        drop(log);
        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "csv"))
            .unwrap();
        let mut text = String::new();
        for _ in 0..100 {
            text = std::fs::read_to_string(&path).unwrap();
            if text.lines().count() == 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], HEADER);
        assert_eq!(lines[2], "2000,2000,1.000,0.000,2,1");
        std::fs::write(dir.path().join("OPEN"), "/home/deck/clip.mp4\n").unwrap();
        assert_eq!(
            take_open_request(dir.path()).as_deref(),
            Some("/home/deck/clip.mp4")
        );
        assert_eq!(take_open_request(dir.path()), None);
    }
}
