//! Logging setup.
//!
//! Level precedence: `RUST_LOG` > `--log-level` > `general.log_level`.
//! When the launcher exports `FP_LOG_DIR` (see `dist/frameplayer.sh`), logs
//! go to `$FP_LOG_DIR/frameplayer.log` (rotated to `.old` above 5 MB) so
//! `tools/frame.sh logs` finds them; otherwise to stderr. File output goes
//! through a writer thread so logging never blocks the render thread on disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

enum LogMsg {
    Line(Vec<u8>),
    Flush(std::sync::mpsc::SyncSender<()>),
}

static FILE_SINK: OnceLock<crossbeam_channel::Sender<LogMsg>> = OnceLock::new();

/// `MakeWriter` handing formatted lines to the writer thread (lines are
/// dropped rather than blocking if the disk can't keep up).
#[derive(Clone)]
struct ChannelMakeWriter(crossbeam_channel::Sender<LogMsg>);

struct ChannelWriter(crossbeam_channel::Sender<LogMsg>);

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.try_send(LogMsg::Line(buf.to_vec()));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ChannelMakeWriter {
    type Writer = ChannelWriter;
    fn make_writer(&'a self) -> ChannelWriter {
        ChannelWriter(self.0.clone())
    }
}

fn spawn_writer(file: std::fs::File) -> std::io::Result<crossbeam_channel::Sender<LogMsg>> {
    let (tx, rx) = crossbeam_channel::bounded::<LogMsg>(8192);
    std::thread::Builder::new()
        .name("fp-log".into())
        .spawn(move || {
            let mut out = std::io::BufWriter::new(file);
            while let Ok(msg) = rx.recv() {
                match msg {
                    LogMsg::Line(l) => {
                        let _ = out.write_all(&l);
                    }
                    LogMsg::Flush(ack) => {
                        let _ = out.flush();
                        let _ = ack.send(());
                    }
                }
                if rx.is_empty() {
                    let _ = out.flush();
                }
            }
        })?;
    Ok(tx)
}

/// Wait (briefly) until queued log lines reached the file.
pub fn flush() {
    if let Some(tx) = FILE_SINK.get() {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        if tx.send(LogMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}

const ROTATE_BYTES: u64 = 5_000_000;
/// Chatty dependencies kept at warn unless RUST_LOG says otherwise.
const QUIET: &[&str] = &[
    "hyper",
    "hyper_util",
    "reqwest",
    "rustls",
    "h2",
    "tungstenite",
    "tokio_tungstenite",
    "naga",
];

/// The filter directive string for a base level.
pub fn directives(level: &str) -> String {
    let level = match level.trim().to_ascii_lowercase().as_str() {
        l @ ("trace" | "debug" | "info" | "warn" | "error" | "off") => l.to_string(),
        _ => "info".to_string(),
    };
    let mut d = level;
    for q in QUIET {
        d.push_str(&format!(",{q}=warn"));
    }
    d
}

fn open_log(dir: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("frameplayer.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let _ = std::fs::rename(&path, dir.join("frameplayer.log.old"));
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    Ok((f, path))
}

/// Install the global subscriber. Returns the log file, if logging to one.
pub fn init(level: &str, log_dir: Option<&Path>) -> Option<PathBuf> {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(directives(level)));
    if let Some(dir) = log_dir {
        match open_log(dir) {
            Ok((file, path)) => match spawn_writer(file) {
                Ok(tx) => {
                    let ok = tracing_subscriber::fmt()
                        .with_env_filter(filter)
                        .with_ansi(false)
                        .with_writer(ChannelMakeWriter(tx.clone()))
                        .try_init()
                        .is_ok();
                    if ok {
                        let _ = FILE_SINK.set(tx);
                    }
                    return ok.then_some(path);
                }
                Err(e) => eprintln!("FramePlayer: log writer thread: {e}"),
            },
            Err(e) => eprintln!("FramePlayer: cannot open log in {}: {e}", dir.display()),
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_strings() {
        assert!(directives("DEBUG").starts_with("debug,"));
        assert!(directives("bogus").starts_with("info,"));
        assert!(directives("info").contains("hyper=warn"));
        assert!(EnvFilter::try_new(directives("trace")).is_ok());
    }

    #[test]
    fn writer_thread_appends_lines() {
        let dir = tempfile::tempdir().unwrap();
        let (file, path) = open_log(dir.path()).unwrap();
        let tx = spawn_writer(file).unwrap();
        let mw = ChannelMakeWriter(tx.clone());
        mw.make_writer().write_all(b"hello\n").unwrap();
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        tx.send(LogMsg::Flush(ack_tx)).unwrap();
        ack_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hello\n");
    }

    #[test]
    fn rotates_large_logs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("frameplayer.log"),
            vec![b'x'; ROTATE_BYTES as usize + 1],
        )
        .unwrap();
        let (_f, p) = open_log(dir.path()).unwrap();
        assert!(dir.path().join("frameplayer.log.old").exists());
        assert_eq!(std::fs::metadata(p).unwrap().len(), 0);
    }
}
