//! Terminal front end of the [`crate::wizard`]: the console [`Ui`], the
//! real [`Backend`] over [`Installer`], and the debug log every wizard run
//! keeps (in memory for the failure screen, and on disk under
//! `<config dir>/logs/`).

use crate::config::{Device, Paths};
use crate::discovery;
use crate::installer::{
    inspect_tarball, Event, FetchedReports, InstallOptions, InstallSource, Installer, PairOptions,
    Reporter,
};
use crate::remote::ProbeState;
use crate::transport::{LineSink, TransportKind};
use crate::wizard::{Backend, Found, Outcome, Tarball, Ui, Wizard, WizardConfig};
use anyhow::Result;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// In-memory tail of the debug log (plus an optional file copy).
pub struct LogSink {
    inner: Mutex<(String, Option<std::fs::File>)>,
}

/// Keep at most this much log text in memory.
const LOG_MEMORY: usize = 4 << 20;

impl LogSink {
    pub fn new(file: Option<std::fs::File>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new((String::new(), file)),
        })
    }

    pub fn append(&self, s: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = g.1.as_mut() {
            let _ = f.write_all(s.as_bytes());
        }
        g.0.push_str(s);
        if g.0.len() > LOG_MEMORY {
            let mut cut = g.0.len() - LOG_MEMORY / 2;
            while !g.0.is_char_boundary(cut) {
                cut += 1;
            }
            g.0.drain(..cut);
        }
    }

    pub fn text(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }
}

/// `tracing` writer into a [`LogSink`].
#[derive(Clone)]
pub struct LogWriter(pub Arc<LogSink>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.append(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Route all tracing output (debug level for this crate) into a log file
/// under `<config dir>/logs/` and memory. Nothing goes to the console.
pub fn init_logging(paths: &Paths, stamp: &str) -> Arc<LogSink> {
    let dir = paths.dir.join("logs");
    let file = std::fs::create_dir_all(&dir)
        .ok()
        .and_then(|_| std::fs::File::create(dir.join(format!("install-{stamp}.txt"))).ok());
    let sink = LogSink::new(file);
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "info,fp_installer=debug,fp_updater=debug,russh=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_ansi(false)
        .with_writer(LogWriter(sink.clone()))
        .try_init();
    sink
}

/// Plain console UI (no colours: the classic Windows console would print
/// raw escape codes).
pub struct ConsoleUi {
    /// A dots/progress line is open and needs a newline first.
    mid_line: Mutex<bool>,
    last_pct: Mutex<Option<u64>>,
}

impl ConsoleUi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mid_line: Mutex::new(false),
            last_pct: Mutex::new(None),
        })
    }

    fn end_line(&self) {
        let mut m = self.mid_line.lock().unwrap();
        if *m {
            println!();
            *m = false;
        }
    }

    /// Wait for Enter (end of every guided run, so a double-clicked window
    /// doesn't vanish).
    pub fn wait_for_enter(&self, prompt: &str) {
        self.ask(prompt);
    }
}

impl Ui for ConsoleUi {
    fn line(&self, text: &str) {
        self.end_line();
        println!("{text}");
        tracing::info!(target: "ui", "{text}");
    }

    fn ask(&self, prompt: &str) -> String {
        self.end_line();
        print!("{prompt} ");
        let _ = std::io::stdout().flush();
        let mut s = String::new();
        let _ = std::io::stdin().lock().read_line(&mut s);
        let a = s.trim().to_string();
        tracing::info!(target: "ui", "{prompt} -> {a:?}");
        a
    }

    fn tick(&self) {
        print!(".");
        let _ = std::io::stdout().flush();
        *self.mid_line.lock().unwrap() = true;
    }

    fn progress(&self, done: u64, total: Option<u64>) {
        let Some(total) = total.filter(|t| *t > 0) else {
            return;
        };
        let pct = done * 100 / total;
        let mut last = self.last_pct.lock().unwrap();
        if *last == Some(pct) {
            return;
        }
        *last = Some(pct);
        print!(
            "\r    {pct:3}%   {:.1} of {:.1} MB",
            done as f64 / 1e6,
            total as f64 / 1e6
        );
        let _ = std::io::stdout().flush();
        let done_now = done >= total;
        *self.mid_line.lock().unwrap() = !done_now;
        if done_now {
            println!();
            *last = None;
        }
    }
}

/// Turns installer events into friendly console lines.
pub struct UiReporter(pub Arc<ConsoleUi>);

impl Reporter for UiReporter {
    fn event(&self, e: Event) {
        tracing::debug!("event: {e:?}");
        match e {
            Event::Step(s) | Event::Info(s) => self.0.line(&format!("  {s}")),
            Event::Warn(s) => self.0.line(&format!("  Note: {s}")),
            Event::Done(s) => self.0.line(&format!("  {s}")),
            Event::Progress { done, total } => self.0.progress(done, total),
            Event::ApproveOnHeadset { .. } => {
                self.0.line("");
                self.0
                    .line("  ***********************************************************");
                self.0
                    .line("  *  LOOK IN THE HEADSET NOW.                               *");
                self.0
                    .line("  *  A message asks whether to allow this computer.         *");
                self.0
                    .line("  *  Press  ALLOW.                                          *");
                self.0
                    .line("  ***********************************************************");
                self.0
                    .line("  Waiting for you to press Allow (up to 3 minutes)");
            }
        }
    }
}

/// The real device side of the wizard.
pub struct RealBackend {
    pub installer: Installer,
    pub log: Arc<LogSink>,
    /// `--tarball`: use this file instead of searching/downloading.
    pub tarball: Option<PathBuf>,
    /// Folders searched for a release tarball before downloading.
    pub search_dirs: Vec<PathBuf>,
}

impl RealBackend {
    fn device_names(&self) -> Vec<String> {
        self.installer
            .paths
            .load_state()
            .map(|s| {
                s.devices
                    .iter()
                    .flat_map(|d| [d.name.clone(), d.user.clone()])
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Backend for RealBackend {
    fn saved_device(&self) -> Option<Device> {
        self.installer.paths.load_state().ok()?.find(None).cloned()
    }

    async fn reachable(&self, d: &Device) -> bool {
        let Ok(r) = self.installer.remote(d) else {
            return false;
        };
        tokio::time::timeout(Duration::from_secs(25), r.check())
            .await
            .unwrap_or(false)
    }

    async fn discover(&self, timeout: Duration) -> Result<Vec<Found>> {
        let found = tokio::task::spawn_blocking(move || {
            discovery::browse_until(timeout, Some(Duration::from_millis(1500)))
        })
        .await??;
        Ok(found
            .into_iter()
            .filter_map(|d| {
                let host = d.best_address()?.to_string();
                Some(Found { name: d.name, host })
            })
            .collect())
    }

    async fn pair(&self, host: &str, name: Option<&str>) -> Result<Device> {
        self.installer
            .pair(&PairOptions {
                host: Some(host.to_string()),
                name: name.filter(|n| !n.is_empty()).map(str::to_string),
                ..Default::default()
            })
            .await
    }

    async fn tarball(&self) -> Result<Tarball> {
        if let Some(p) = &self.tarball {
            return Ok(Tarball {
                path: p.clone(),
                origin: format!("Using {}", p.display()),
            });
        }
        if let Some(p) = crate::github::local_tarball(&self.search_dirs, "aarch64") {
            return Ok(Tarball {
                origin: format!(
                    "Using {} found next to this program (no download needed).",
                    p.file_name().unwrap_or_default().to_string_lossy()
                ),
                path: p,
            });
        }
        let p = self.installer.fetch_github_latest().await?;
        Ok(Tarball {
            path: p,
            origin: "Downloaded from GitHub.".into(),
        })
    }

    fn tarball_version(&self, path: &Path) -> Result<String> {
        Ok(inspect_tarball(path)?.version)
    }

    async fn installed_release(&self, d: &Device) -> Option<String> {
        let s = self.installer.status(d).await.ok()?;
        s.installed.then_some(s.release).flatten()
    }

    async fn install(&self, d: &Device, tarball: &Path) -> Result<String> {
        self.installer
            .install(
                d,
                &InstallSource::Tarball(tarball.to_path_buf()),
                &InstallOptions::default(),
            )
            .await
    }

    async fn run_probe(&self, d: &Device, sink: LineSink) -> Result<i32> {
        self.installer.run_probe_headless(d, sink).await
    }

    async fn probe_state(&self, d: &Device) -> Result<ProbeState> {
        self.installer.probe_state(d).await
    }

    async fn fetch_reports(&self, d: &Device) -> Result<FetchedReports> {
        self.installer.fetch_reports(d).await
    }

    async fn launch_probe(&self, d: &Device) -> Result<()> {
        self.installer.launch_probe(d).await
    }

    fn extra_redactions(&self) -> Vec<(String, &'static str)> {
        self.device_names()
            .into_iter()
            .map(|n| (n, "<headset>"))
            .collect()
    }

    fn log_text(&self) -> String {
        self.log.text()
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<()> {
        crate::share::copy_to_clipboard(text)
    }

    fn open_text_file(&self, path: &Path) -> Result<()> {
        crate::share::open_text_file(path)
    }

    fn open_url(&self, url: &str) -> Result<()> {
        crate::share::open_url(url)
    }
}

/// What to run.
#[derive(Debug, Clone, Default)]
pub struct WizardOptions {
    pub host: Option<String>,
    pub tarball: Option<PathBuf>,
    /// None: ask.
    pub interactive: Option<bool>,
    pub transport: Option<TransportKind>,
    /// `Some(post)`: only the self-test (`frameplayer-install probe`).
    pub probe_only: Option<bool>,
    /// Wait for Enter before returning (guided mode).
    pub pause_at_end: bool,
}

/// Run the guided installer (or the probe-only flow). Returns the process
/// exit code.
pub async fn run(opts: WizardOptions) -> i32 {
    let ui = ConsoleUi::new();
    let now = chrono::Local::now();
    let stamp = crate::report::stamp(now);
    let paths = match Paths::platform_default() {
        Ok(p) => p,
        Err(e) => {
            ui.line(&format!("Can't find a settings folder on this PC: {e:#}"));
            if opts.pause_at_end {
                ui.wait_for_enter("Press Enter to close this window.");
            }
            return 1;
        }
    };
    let log = init_logging(&paths, &stamp);
    if opts.pause_at_end {
        install_panic_hook();
    }
    tracing::info!(
        "frameplayer-install {} on {} ({}), options {opts:?}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let mut installer = Installer::new(paths, Arc::new(UiReporter(ui.clone())));
    if let Some(t) = opts.transport {
        installer = installer.with_transport(t);
    }
    tracing::info!("ssh transport: {}", installer.transport().as_str());
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let search_dirs: Vec<PathBuf> = exe_dir
        .into_iter()
        .chain(std::env::current_dir().ok())
        .collect();
    let backend = RealBackend {
        installer,
        log,
        tarball: opts.tarball.clone(),
        search_dirs,
    };
    let out_dir = crate::report::output_dir();
    tracing::info!("output folder: {}", out_dir.display());
    let mut cfg = WizardConfig::new(out_dir, now);
    cfg.interactive = opts.interactive;
    cfg.host = opts.host.clone();
    let wizard = Wizard::new(ui.clone(), backend, cfg);
    let outcome = match opts.probe_only {
        Some(post) => wizard.run_probe_only(post).await,
        None => wizard.run().await,
    };
    tracing::info!("outcome: {outcome:?}");
    if opts.pause_at_end {
        ui.line("");
        ui.wait_for_enter("Press Enter to close this window.");
    }
    match outcome {
        Outcome::Done(_) => 0,
        Outcome::Quit => 2,
        Outcome::Failed(_) => 1,
    }
}

/// Keep the window open on a crash too.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        tracing::error!("panic: {info}");
        eprintln!();
        eprintln!("The installer crashed (sorry!). Please copy the text above into the chat.");
        eprint!("Press Enter to close this window. ");
        let _ = std::io::stderr().flush();
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }));
}
