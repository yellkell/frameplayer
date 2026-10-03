//! Guided mode: what a double-click on `frameplayer-install.exe` runs.
//!
//! A numbered, plain-language walk through: find the headset (mDNS, or a
//! typed IP), pair (press Allow in the headset), get FramePlayer (a tarball
//! next to the program, else the newest GitHub release, pre-releases
//! included), install, run the self-test headless over SSH, optionally the
//! interactive part through Steam, then save ONE paste-ready, redacted text
//! report on the Desktop, copy it to the clipboard and open it in Notepad.
//! On failure the (redacted) install log gets the same treatment.
//!
//! The flow is written against two small traits so it can be tested without
//! a headset or a terminal: [`Ui`] (print / ask) and [`Backend`] (device
//! operations). [`crate::console`] has the real implementations.

use crate::config::Device;
use crate::installer::FetchedReports;
use crate::redact::{redact, RedactContext};
use crate::remote::{ProbeState, NOT_INSTALLED_EXIT, PROBE_MISSING_EXIT};
use crate::report::{self, ReportText};
use crate::transport::LineSink;
use anyhow::Result;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Console interaction.
pub trait Ui: Send + Sync + 'static {
    /// Print one line.
    fn line(&self, text: &str);
    /// Read one line of input (trimmed; empty on end of input).
    fn ask(&self, prompt: &str) -> String;
    /// A "still working" dot.
    fn tick(&self) {}
    /// Byte progress of a download/upload.
    fn progress(&self, _done: u64, _total: Option<u64>) {}
}

/// A headset found on the network.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub name: String,
    pub host: String,
}

/// Where the build came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Tarball {
    pub path: PathBuf,
    /// Plain-language origin ("downloaded from GitHub", …).
    pub origin: String,
}

/// Everything the wizard needs from the outside world.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// The headset paired last time, if any.
    fn saved_device(&self) -> Option<Device>;
    async fn reachable(&self, d: &Device) -> bool;
    async fn discover(&self, timeout: Duration) -> Result<Vec<Found>>;
    async fn pair(&self, host: &str, name: Option<&str>) -> Result<Device>;
    async fn tarball(&self) -> Result<Tarball>;
    fn tarball_version(&self, path: &Path) -> Result<String>;
    async fn installed_release(&self, d: &Device) -> Option<String>;
    async fn install(&self, d: &Device, tarball: &Path) -> Result<String>;
    async fn run_probe(&self, d: &Device, sink: LineSink) -> Result<i32>;
    async fn probe_state(&self, d: &Device) -> Result<ProbeState>;
    async fn fetch_reports(&self, d: &Device) -> Result<FetchedReports>;
    async fn launch_probe(&self, d: &Device) -> Result<()>;
    /// Extra names to scrub (headset names, login).
    fn extra_redactions(&self) -> Vec<(String, &'static str)> {
        Vec::new()
    }
    /// Full debug log so far (unredacted).
    fn log_text(&self) -> String;
    fn copy_to_clipboard(&self, text: &str) -> Result<()>;
    fn open_text_file(&self, path: &Path) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
}

/// Tunables (tests shrink the timeouts).
#[derive(Debug, Clone)]
pub struct WizardConfig {
    /// Where report/log files go (normally the Desktop).
    pub out_dir: PathBuf,
    /// File-name date stamp, see [`report::stamp`].
    pub stamp: String,
    /// Human date for headers.
    pub when: String,
    pub discover_timeout: Duration,
    pub tick: Duration,
    pub poll: Duration,
    pub interactive_timeout: Duration,
    /// None: ask. Some(b): don't ask.
    pub interactive: Option<bool>,
    /// Skip discovery and pair with this host.
    pub host: Option<String>,
    pub redact: RedactContext,
}

impl WizardConfig {
    pub fn new(out_dir: PathBuf, now: chrono::DateTime<chrono::Local>) -> Self {
        Self {
            out_dir,
            stamp: report::stamp(now),
            when: now.format("%Y-%m-%d %H:%M").to_string(),
            discover_timeout: Duration::from_secs(20),
            tick: Duration::from_secs(1),
            poll: Duration::from_secs(3),
            interactive_timeout: Duration::from_secs(300),
            interactive: None,
            host: None,
            redact: RedactContext::for_this_pc(),
        }
    }
}

/// Why the wizard stopped early.
#[derive(Debug)]
pub enum Stop {
    /// The user chose to quit.
    Quit,
    /// Something failed: a plain explanation plus the technical error.
    Failed { plain: String, error: anyhow::Error },
}

trait Plain<T> {
    fn plain(self, msg: &str) -> std::result::Result<T, Stop>;
}

impl<T> Plain<T> for Result<T> {
    fn plain(self, msg: &str) -> std::result::Result<T, Stop> {
        self.map_err(|error| Stop::Failed {
            plain: msg.to_string(),
            error,
        })
    }
}

fn failed(plain: &str, error: &str) -> Stop {
    Stop::Failed {
        plain: plain.to_string(),
        error: anyhow::anyhow!(error.to_string()),
    }
}

/// Files the wizard produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    /// The paste-ready text file (report, or install log on failure).
    pub text: Option<PathBuf>,
    pub json: Option<PathBuf>,
    pub copied: bool,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Report saved.
    Done(Saved),
    /// Failed; the log was saved.
    Failed(Saved),
    Quit,
}

const TOTAL_STEPS: u32 = 6;
const RULE: &str = "==============================================================";

/// Accept an IPv4/IPv6 address or a host name typed by the user.
pub fn parse_host_input(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches("http://").trim_end_matches('/');
    let s = s.split_once(":32000").map(|(h, _)| h).unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    if s.parse::<std::net::IpAddr>().is_ok() {
        return Some(s.to_string());
    }
    let looks_like_ipv4 = s.chars().all(|c| c.is_ascii_digit() || c == '.');
    let hostname = s.len() <= 253
        && s.split('.')
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    (hostname && !looks_like_ipv4).then(|| s.to_string())
}

/// A plain explanation for a pairing failure.
pub fn explain_pair_error(e: &anyhow::Error) -> &'static str {
    let s = format!("{e:#}").to_ascii_lowercase();
    if s.contains("declined") || s.contains("denied") {
        "The headset said no to the connection. If you didn't mean to press Deny, try again and press Allow."
    } else if s.contains("timed out waiting for approval")
        || s.contains("operation timed out") && s.contains("register")
    {
        "I didn't get an answer from the headset in time. Put the headset on, look for the message asking to allow this computer, and press Allow."
    } else if s.contains("devkit service")
        || s.contains("error sending request")
        || s.contains("connect")
    {
        "I couldn't talk to the headset. Check that Developer Mode is still on, the headset is awake, and it is on the same Wi-Fi as this PC."
    } else if s.contains("ssh") || s.contains("key") {
        "The headset accepted the request, but logging in to it didn't work. Try again; if it keeps failing, turn Developer Mode off and on again on the headset."
    } else {
        "Connecting to the headset didn't work."
    }
}

/// The guided flow.
pub struct Wizard<U: Ui, B: Backend> {
    pub ui: Arc<U>,
    pub backend: B,
    pub cfg: WizardConfig,
}

impl<U: Ui, B: Backend> Wizard<U, B> {
    pub fn new(ui: Arc<U>, backend: B, cfg: WizardConfig) -> Self {
        Self { ui, backend, cfg }
    }

    fn say(&self, s: &str) {
        self.ui.line(s);
    }

    fn redact_ctx(&self) -> RedactContext {
        let mut c = self.cfg.redact.clone();
        for (n, ph) in self.backend.extra_redactions() {
            c.add(&n, ph);
        }
        c
    }

    fn step(&self, n: u32, title: &str) {
        self.say("");
        self.say(RULE);
        self.say(&format!("  Step {n} of {TOTAL_STEPS}: {title}"));
        self.say(RULE);
    }

    /// Ask until a number in `1..=options.len()` is typed; returns 0-based.
    fn choose(&self, question: &str, options: &[&str]) -> usize {
        self.say("");
        self.say(question);
        for (i, o) in options.iter().enumerate() {
            self.say(&format!("   {}) {o}", i + 1));
        }
        for _ in 0..20 {
            let a = self.ui.ask(&format!(
                "Type a number (1-{}) and press Enter:",
                options.len()
            ));
            if let Ok(n) = a.parse::<usize>() {
                if (1..=options.len()).contains(&n) {
                    return n - 1;
                }
            }
            if a.is_empty() && options.len() == 1 {
                return 0;
            }
            self.say("Sorry, I didn't get that.");
        }
        options.len() - 1 // treat endless bad input (closed stdin) as the last option (Quit)
    }

    fn yes_no(&self, question: &str, default: bool) -> bool {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        for _ in 0..20 {
            let a = self
                .ui
                .ask(&format!("{question} {hint}"))
                .to_ascii_lowercase();
            match a.as_str() {
                "" => return default,
                "y" | "yes" | "j" | "ja" => return true,
                "n" | "no" | "nein" => return false,
                _ => self.say("Please type y (yes) or n (no)."),
            }
        }
        default
    }

    async fn with_dots<F: Future>(&self, fut: F) -> F::Output {
        tokio::pin!(fut);
        let mut iv = tokio::time::interval(self.cfg.tick);
        iv.tick().await;
        loop {
            tokio::select! {
                r = &mut fut => return r,
                _ = iv.tick() => self.ui.tick(),
            }
        }
    }

    /// The whole guided install. Never panics on user input; the caller
    /// waits for Enter afterwards.
    pub async fn run(&self) -> Outcome {
        self.intro();
        match self.run_steps().await {
            Ok(saved) => Outcome::Done(saved),
            Err(Stop::Quit) => {
                self.say("");
                self.say(
                    "OK, stopping here. Nothing else was changed. You can run me again any time.",
                );
                Outcome::Quit
            }
            Err(Stop::Failed { plain, error }) => Outcome::Failed(self.fail(&plain, &error)),
        }
    }

    fn intro(&self) {
        self.say(RULE);
        self.say("  FramePlayer installer");
        self.say(RULE);
        self.say("");
        self.say("This will:");
        self.say("  1. find your Steam Frame on your Wi-Fi,");
        self.say("  2. install FramePlayer and its self-test on it,");
        self.say("  3. run the self-test and save a report you can paste into the chat.");
        self.say("");
        self.say("Before you start, on the headset:");
        self.say("  - connect it to the SAME Wi-Fi as this PC");
        self.say("  - turn on Developer Mode:  Settings > System > Developer Mode");
        self.say("  - keep it awake (don't let it go to sleep while this runs)");
        self.say("");
        self.ui.ask("Press Enter when you're ready...");
    }

    async fn run_steps(&self) -> std::result::Result<Saved, Stop> {
        let device = self.connect().await?;

        self.step(3, "Getting FramePlayer");
        let tb = self.backend.tarball().await.plain(
            "I couldn't get FramePlayer from GitHub. Check that this PC is connected to the internet, then run me again.",
        )?;
        self.say(&format!("  {}", tb.origin));
        let version = self.backend.tarball_version(&tb.path).plain(
            "The FramePlayer download looks damaged. Run me again to download it once more.",
        )?;

        self.step(4, "Installing FramePlayer on the headset");
        if self.backend.installed_release(&device).await.as_deref() == Some(version.as_str()) {
            self.say(&format!(
                "  FramePlayer {version} is already on the headset. Skipping this step."
            ));
        } else {
            self.say("  Copying it over Wi-Fi. This can take a minute...");
            self.backend.install(&device, &tb.path).await.plain(
                "Installing on the headset didn't work. Make sure the headset stays awake and on Wi-Fi, then run me again.",
            )?;
        }

        let mut saved = self.self_test(&device, 5).await?;
        self.interactive(&device, &mut saved).await?;
        self.finish(&mut saved);
        Ok(saved)
    }

    /// Steps 1 and 2: a reachable paired device, or discover + pair.
    async fn connect(&self) -> std::result::Result<Device, Stop> {
        if self.cfg.host.is_none() {
            if let Some(d) = self.backend.saved_device() {
                self.say("");
                self.say(&format!(
                    "Checking the headset you used last time ({})",
                    d.name
                ));
                if self.with_dots(self.backend.reachable(&d)).await {
                    self.say(&format!(
                        "Connected to {}. Steps 1 and 2 are already done.",
                        d.name
                    ));
                    return Ok(d);
                }
                self.say("It didn't answer, so let's find it again.");
            }
        }
        let mut typed = self.cfg.host.clone();
        loop {
            let (host, name) = match typed.take() {
                Some(h) => (h, None),
                None => match self.find().await? {
                    Some(f) => (f.host, Some(f.name)),
                    None => continue,
                },
            };
            self.step(2, "Connecting to the headset");
            self.say(&format!("  Talking to the headset at {host}"));
            self.say(
                "  If a message appears in the headset asking to allow this computer, press Allow.",
            );
            match self
                .with_dots(self.backend.pair(&host, name.as_deref()))
                .await
            {
                Ok(d) => {
                    self.say("");
                    self.say(&format!("  Connected to {}.", d.name));
                    return Ok(d);
                }
                Err(e) => {
                    tracing::warn!("pairing failed: {e:#}");
                    self.say("");
                    self.say(&format!("  {}", explain_pair_error(&e)));
                    self.say(&format!("  (Details: {e:#})"));
                    match self.choose(
                        "What do you want to do?",
                        &["Try again", "Type the headset's IP address", "Quit"],
                    ) {
                        0 => typed = Some(host),
                        1 => typed = Some(self.ask_ip()?),
                        _ => return Err(Stop::Quit),
                    }
                }
            }
        }
    }

    /// Step 1. `Ok(None)` means "look again".
    async fn find(&self) -> std::result::Result<Option<Found>, Stop> {
        self.step(1, "Finding your headset");
        self.say("  Looking for a Steam Frame on your network (up to 20 seconds)");
        let found = match self
            .with_dots(self.backend.discover(self.cfg.discover_timeout))
            .await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("discovery failed: {e:#}");
                Vec::new()
            }
        };
        self.say("");
        match found.len() {
            0 => {
                self.say("  I couldn't find your headset. Common reasons:");
                self.say("   - The PC and the headset are on different Wi-Fi networks");
                self.say("     (for example a guest network, or a 5 GHz/2.4 GHz split with separate names).");
                self.say("   - Developer Mode is off:  Settings > System > Developer Mode");
                self.say("   - The headset is asleep. Put it on to wake it up.");
                self.say("   - Windows asked whether to allow this program on the network and it was blocked.");
                match self.choose(
                    "What do you want to do?",
                    &["Try again", "Type the headset's IP address", "Quit"],
                ) {
                    0 => Ok(None),
                    1 => Ok(Some(Found {
                        name: String::new(),
                        host: self.ask_ip()?,
                    })),
                    _ => Err(Stop::Quit),
                }
            }
            1 => {
                let f = found.into_iter().next().expect("one");
                self.say(&format!("  Found {} ({}).", f.name, f.host));
                Ok(Some(f))
            }
            _ => {
                let labels: Vec<String> = found
                    .iter()
                    .map(|f| format!("{} ({})", f.name, f.host))
                    .collect();
                let mut opts: Vec<&str> = labels.iter().map(String::as_str).collect();
                opts.push("None of these: look again");
                let i = self.choose("I found more than one headset. Which one is yours?", &opts);
                Ok(found.into_iter().nth(i))
            }
        }
    }

    fn ask_ip(&self) -> std::result::Result<String, Stop> {
        self.say("");
        self.say("  To find the headset's IP address:");
        // [verify] Menu path to the IP address on the Frame.
        self.say("   on the headset open Settings > Network (or Wi-Fi), select your network,");
        self.say("   and look for \"IP address\". It looks like 192.168.1.23");
        for _ in 0..5 {
            let a = self
                .ui
                .ask("Type the IP address and press Enter (or just Enter to quit):");
            if a.is_empty() {
                return Err(Stop::Quit);
            }
            if let Some(h) = parse_host_input(&a) {
                return Ok(h);
            }
            self.say("  That doesn't look like an IP address. It should be four numbers with dots, like 192.168.1.23");
        }
        Err(Stop::Quit)
    }

    /// Headless self-test and copy-back. `n` is the step number.
    async fn self_test(&self, device: &Device, n: u32) -> std::result::Result<Saved, Stop> {
        self.step(n, "Running the self-test on the headset");
        self.say("  This takes about a minute. You don't need to wear the headset for this part.");
        self.say("");
        let ui = self.ui.clone();
        let sink: LineSink = Box::new(move |l| ui.line(&format!("   | {l}")));
        let code = self.backend.run_probe(device, sink).await.plain(
            "The self-test couldn't be started on the headset. Make sure it is awake and on Wi-Fi, then run me again.",
        )?;
        tracing::info!("probe exited with {code}");
        match code {
            PROBE_MISSING_EXIT => {
                return Err(failed(
                    "This FramePlayer build doesn't include the self-test yet. Please tell me in the chat; there's nothing you can fix on your side.",
                    "frameplayer-probe not found in the installed release (exit 127)",
                ))
            }
            NOT_INSTALLED_EXIT => {
                return Err(failed(
                    "FramePlayer isn't installed on the headset. Run me again without options to install it.",
                    "install directory missing (exit 3)",
                ))
            }
            _ => {}
        }
        let reports = self
            .backend
            .fetch_reports(device)
            .await
            .plain("The self-test ran, but I couldn't copy its report from the headset.")?;
        if reports.json.is_none() && reports.txt.is_none() {
            return Err(failed(
                "The self-test ran, but it didn't leave a report on the headset.",
                &format!("no report files after probe exit code {code}"),
            ));
        }
        let saved = self
            .save_report(&reports)
            .plain("I couldn't save the report on this PC.")?;
        self.say("");
        if let Some(p) = &saved.text {
            self.say(&format!("  Saved the report: {}", p.display()));
        }
        Ok(saved)
    }

    /// Write `.json` (redacted) and the paste-ready `.txt`.
    fn save_report(&self, r: &FetchedReports) -> Result<Saved> {
        std::fs::create_dir_all(&self.cfg.out_dir)?;
        let (jname, tname) = report::report_names(&self.cfg.stamp);
        let mut saved = Saved::default();
        let json = r.json.clone().unwrap_or_else(|| "{}".into());
        if r.json.is_some() {
            let p = self.cfg.out_dir.join(jname);
            std::fs::write(&p, redact(&json, &self.redact_ctx()))?;
            saved.json = Some(p);
        }
        let probe = report::probe_version(&json).unwrap_or_else(|| "?".into());
        let header = format!(
            "FramePlayer self-test report | {} | probe {probe} | installer {}",
            self.cfg.when,
            env!("CARGO_PKG_VERSION")
        );
        let text = report::paste_ready(
            &ReportText {
                summary: r.txt.as_deref().unwrap_or("(no summary file)"),
                json: &json,
                header: &header,
            },
            &self.redact_ctx(),
            report::PASTE_LIMIT,
        );
        let p = self.cfg.out_dir.join(tname);
        std::fs::write(&p, &text)?;
        saved.text = Some(p);
        Ok(saved)
    }

    async fn interactive(
        &self,
        device: &Device,
        saved: &mut Saved,
    ) -> std::result::Result<(), Stop> {
        self.step(TOTAL_STEPS, "Second part: in the headset (optional)");
        self.say("  The second part checks the controllers and the picture in the headset.");
        self.say("  You'll wear the headset and press some buttons when it asks. About 3 minutes.");
        let go = match self.cfg.interactive {
            Some(b) => b,
            None => self.yes_no("  Do you want to do it now?", true),
        };
        if !go {
            self.say("  Skipped. The report from the first part is enough to get started.");
            return Ok(());
        }
        let before = self.backend.probe_state(device).await.unwrap_or_default();
        if let Err(e) = self.backend.launch_probe(device).await {
            tracing::warn!("launch_probe failed: {e:#}");
            self.say("");
            self.say("  I couldn't start the second part on the headset, so I'll keep the report");
            self.say(
                "  from the first part. (You can also start \"FramePlayer Self-Test\" from the",
            );
            self.say("  Steam library on the headset later.)");
            self.say(&format!("  (Details: {e:#})"));
            return Ok(());
        }
        self.say("");
        self.say("  ***********************************************************");
        self.say("  *  PUT THE HEADSET ON NOW.                                *");
        self.say("  *  The self-test will tell you what to press.             *");
        self.say("  *  When it says it's finished, take the headset off and   *");
        self.say("  *  come back here. I'm waiting (up to 5 minutes).         *");
        self.say("  ***********************************************************");
        let finished = self.with_dots(self.wait_for_report(device, before)).await;
        self.say("");
        if !finished {
            self.say("  The test in the headset didn't finish in time. I'll save what we have.");
        } else {
            self.say("  The test in the headset is finished.");
        }
        let reports = self
            .backend
            .fetch_reports(device)
            .await
            .plain("I couldn't copy the updated report from the headset.")?;
        if reports.json.is_some() || reports.txt.is_some() {
            *saved = self
                .save_report(&reports)
                .plain("I couldn't save the report on this PC.")?;
        }
        Ok(())
    }

    /// Poll until the report is newer than `before` (and the probe has
    /// finished writing it). False on timeout.
    async fn wait_for_report(&self, device: &Device, before: ProbeState) -> bool {
        let deadline = Instant::now() + self.cfg.interactive_timeout;
        while Instant::now() < deadline {
            tokio::time::sleep(self.cfg.poll).await;
            match self.backend.probe_state(device).await {
                Ok(s)
                    if s.json_mtime > before.json_mtime
                        && (!s.running || s.txt_mtime >= s.json_mtime) =>
                {
                    return true
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("probe state: {e:#}"),
            }
        }
        false
    }

    /// Clipboard + Notepad + the final instructions.
    fn finish(&self, saved: &mut Saved) {
        let Some(path) = saved.text.clone() else {
            return;
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        saved.copied = self.backend.copy_to_clipboard(&text).is_ok();
        let opened = self.backend.open_text_file(&path).is_ok();
        self.say("");
        self.say(RULE);
        self.say("  DONE!");
        self.say(RULE);
        self.say("");
        if saved.copied {
            self.say("  Your report is ready and already copied.");
            self.say("  Go to the chat with Claude and press Ctrl+V to paste it, then send.");
            self.say("");
            if opened {
                self.say("  (If that doesn't work: the report is open in Notepad. Press Ctrl+A,");
                self.say("   then Ctrl+C, then paste it into the chat.)");
            }
        } else if opened {
            self.say("  Your report is open in Notepad. Press Ctrl+A, then Ctrl+C,");
            self.say("  then go to the chat with Claude and press Ctrl+V to paste it.");
        } else {
            self.say("  Your report is ready. Open the file below, copy everything in it");
            self.say("  and paste it into the chat with Claude.");
        }
        self.say(&format!(
            "  The report is also saved here: {}",
            path.display()
        ));
    }

    /// Failure screen: explanation, redacted log on the Desktop, clipboard.
    fn fail(&self, plain: &str, error: &anyhow::Error) -> Saved {
        tracing::error!("wizard failed: {error:#}");
        self.say("");
        self.say(RULE);
        self.say("  Something went wrong");
        self.say(RULE);
        self.say("");
        self.say(&format!("  {plain}"));
        self.say("");
        self.say(&format!("  Technical details: {error:#}"));
        let mut saved = Saved::default();
        let log = format!(
            "FramePlayer install log | {} | installer {}\nProblem: {plain}\nError: {error:#}\n\n{}",
            self.cfg.when,
            env!("CARGO_PKG_VERSION"),
            self.backend.log_text()
        );
        let log = redact(&log, &self.redact_ctx());
        let log = if log.len() > report::PASTE_LIMIT {
            // Keep the start (context) and the end (where it failed).
            let head: String = log.chars().take(8_000).collect();
            let tail_start = log.len() - (report::PASTE_LIMIT - 10_000);
            let mut i = tail_start;
            while !log.is_char_boundary(i) {
                i += 1;
            }
            format!("{head}\n…(middle of the log left out)…\n{}", &log[i..])
        } else {
            log
        };
        let path = self.cfg.out_dir.join(report::log_name(&self.cfg.stamp));
        if std::fs::create_dir_all(&self.cfg.out_dir).is_ok() && std::fs::write(&path, &log).is_ok()
        {
            saved.copied = self.backend.copy_to_clipboard(&log).is_ok();
            let opened = self.backend.open_text_file(&path).is_ok();
            self.say("");
            if saved.copied {
                self.say("  I copied a log of what happened. Go to the chat with Claude and");
                self.say("  press Ctrl+V to paste it, then send. That tells me what to fix.");
                if opened {
                    self.say("  (If that doesn't work: the log is open in Notepad. Press Ctrl+A,");
                    self.say("   then Ctrl+C, then paste it into the chat.)");
                }
            } else {
                self.say("  Please send me the log file in the chat:");
            }
            self.say(&format!("  The log is saved here: {}", path.display()));
            saved.text = Some(path);
        } else {
            self.say(
                "  (I couldn't even save a log file. Please copy the text above into the chat.)",
            );
        }
        saved
    }

    /// `frameplayer-install probe`: self-test on an already-installed
    /// headset, optionally the interactive part, optionally the GitHub
    /// issue hand-off.
    pub async fn run_probe_only(&self, post: bool) -> Outcome {
        let r: std::result::Result<Saved, Stop> = async {
            let device = match self.backend.saved_device() {
                Some(d) => d,
                None => {
                    return Err(failed(
                        "No headset is set up yet. Run frameplayer-install without options first.",
                        "no paired device",
                    ))
                }
            };
            if !self.with_dots(self.backend.reachable(&device)).await {
                return Err(failed(
                    "I couldn't reach the headset. Make sure it is awake, on the same Wi-Fi, and in Developer Mode.",
                    &format!("ssh to {} failed", device.host),
                ));
            }
            let mut saved = self.self_test(&device, 5).await?;
            if self.cfg.interactive == Some(true) {
                self.interactive(&device, &mut saved).await?;
            }
            if post {
                self.post_issue(&saved, "Self-test report", "probe-report");
            } else {
                self.finish(&mut saved);
            }
            Ok(saved)
        }
        .await;
        match r {
            Ok(s) => Outcome::Done(s),
            Err(Stop::Quit) => Outcome::Quit,
            Err(Stop::Failed { plain, error }) => {
                let saved = self.fail(&plain, &error);
                if post {
                    self.post_issue(&saved, "Install log", "install-log");
                }
                Outcome::Failed(saved)
            }
        }
    }

    /// Opt-in: put an issue body on the clipboard and open GitHub's
    /// new-issue page for the user to paste and submit. Nothing is uploaded
    /// by this program.
    fn post_issue(&self, saved: &Saved, kind: &str, label: &str) {
        let Some(path) = &saved.text else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let (body, version) = match &saved.json {
            Some(j) => {
                let json = std::fs::read_to_string(j).unwrap_or_default();
                let summary = text
                    .split("=== Summary ===")
                    .nth(1)
                    .and_then(|s| s.split("=== Full report").next())
                    .unwrap_or("")
                    .to_string();
                let version = report::probe_version(&json).unwrap_or_default();
                let header = format!("{kind} {} (probe {version})", self.cfg.when);
                (
                    report::issue_body(
                        &ReportText {
                            summary: &summary,
                            json: &json,
                            header: &header,
                        },
                        &self.redact_ctx(),
                    ),
                    version,
                )
            }
            None => {
                let mut t = format!("```text\n{text}\n```\n");
                if t.len() > report::ISSUE_BODY_LIMIT {
                    let mut end = report::ISSUE_BODY_LIMIT - 100;
                    while !t.is_char_boundary(end) {
                        end -= 1;
                    }
                    t.truncate(end);
                    t.push_str("\n```\n_truncated; full file on Desktop_\n");
                }
                (t, String::new())
            }
        };
        let title = format!("{kind} {} {version}", self.cfg.when)
            .trim()
            .to_string();
        let url = report::issue_url(&title, label);
        let copied = self.backend.copy_to_clipboard(&body).is_ok();
        let opened = self.backend.open_url(&url).is_ok();
        self.say("");
        self.say(RULE);
        if opened {
            self.say("  Your browser just opened GitHub.");
        } else {
            self.say("  Open this address in your browser:");
            self.say(&format!("  {url}"));
        }
        if copied {
            self.say("  Click in the big text box, press Ctrl+V, then click the green");
            self.say("  \"Submit new issue\" button.");
        } else {
            self.say(&format!(
                "  Open {} in Notepad, copy everything (Ctrl+A, Ctrl+C), paste it into the big text box",
                path.display()
            ));
            self.say("  and click the green \"Submit new issue\" button.");
        }
        self.say(RULE);
        self.ui.ask("Press Enter when you're done...");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeUi {
        answers: Mutex<VecDeque<String>>,
        out: Mutex<Vec<String>>,
    }

    impl FakeUi {
        fn with(answers: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().map(|s| s.to_string()).collect()),
                out: Default::default(),
            })
        }
        fn text(&self) -> String {
            self.out.lock().unwrap().join("\n")
        }
    }

    impl Ui for FakeUi {
        fn line(&self, text: &str) {
            self.out.lock().unwrap().push(text.to_string());
        }
        fn ask(&self, prompt: &str) -> String {
            self.out.lock().unwrap().push(format!("? {prompt}"));
            self.answers.lock().unwrap().pop_front().unwrap_or_default()
        }
    }

    fn device(host: &str) -> Device {
        Device {
            name: "frame".into(),
            host: host.into(),
            service_port: 32000,
            ssh_port: 22,
            user: "steamos".into(),
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        saved: Option<Device>,
        saved_reachable: bool,
        /// Successive discovery results.
        discoveries: Mutex<VecDeque<Vec<Found>>>,
        /// Hosts for which pairing succeeds.
        pair_ok: Vec<String>,
        installed: Option<String>,
        probe_exit: i32,
        no_report: bool,
        launch_fails: bool,
        /// After this many probe_state polls the report looks updated.
        finish_after_polls: usize,
        calls: Mutex<Vec<String>>,
        polls: Mutex<usize>,
        clipboard: Mutex<Option<String>>,
        opened_url: Mutex<Option<String>>,
        tarball_dir: PathBuf,
    }

    impl FakeBackend {
        fn log(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
        fn called(&self, s: &str) -> bool {
            self.calls.lock().unwrap().iter().any(|c| c.starts_with(s))
        }
    }

    impl Backend for FakeBackend {
        fn saved_device(&self) -> Option<Device> {
            self.saved.clone()
        }
        async fn reachable(&self, _: &Device) -> bool {
            self.saved_reachable
        }
        async fn discover(&self, _: Duration) -> Result<Vec<Found>> {
            self.log("discover");
            Ok(self
                .discoveries
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }
        async fn pair(&self, host: &str, _: Option<&str>) -> Result<Device> {
            self.log(format!("pair {host}"));
            if self.pair_ok.iter().any(|h| h == host) {
                Ok(device(host))
            } else {
                anyhow::bail!("timed out waiting for approval on the headset")
            }
        }
        async fn tarball(&self) -> Result<Tarball> {
            Ok(Tarball {
                path: self.tarball_dir.join("frameplayer-0.2.0-aarch64.tar.gz"),
                origin: "Downloaded FramePlayer 0.2.0 from GitHub.".into(),
            })
        }
        fn tarball_version(&self, _: &Path) -> Result<String> {
            Ok("0.2.0".into())
        }
        async fn installed_release(&self, _: &Device) -> Option<String> {
            self.installed.clone()
        }
        async fn install(&self, d: &Device, _: &Path) -> Result<String> {
            self.log(format!("install {}", d.host));
            Ok("0.2.0".into())
        }
        async fn run_probe(&self, _: &Device, mut sink: LineSink) -> Result<i32> {
            self.log("probe");
            sink("PASS vulkan  Turnip");
            sink("headset ip 192.168.1.77");
            Ok(self.probe_exit)
        }
        async fn probe_state(&self, _: &Device) -> Result<ProbeState> {
            let mut p = self.polls.lock().unwrap();
            *p += 1;
            let done = self.finish_after_polls > 0 && *p > self.finish_after_polls;
            Ok(ProbeState {
                json_mtime: if done { 200 } else { 100 },
                txt_mtime: if done { 200 } else { 100 },
                running: !done,
            })
        }
        async fn fetch_reports(&self, _: &Device) -> Result<FetchedReports> {
            self.log("fetch");
            if self.no_report {
                return Ok(FetchedReports::default());
            }
            let interactive = *self.polls.lock().unwrap() > 1;
            Ok(FetchedReports {
                json: Some(format!(
                    "{{\"schema\":\"frameplayer-probe-report\",\"probe_version\":\"0.1.0\",\"mode\":\"{}\",\"summary\":{{}},\"net\":{{\"ip\":\"10.1.2.3\"}}}}",
                    if interactive { "interactive" } else { "headless" }
                )),
                txt: Some("PASS vulkan\nuser dir /home/jane".into()),
            })
        }
        async fn launch_probe(&self, _: &Device) -> Result<()> {
            self.log("launch_probe");
            if self.launch_fails {
                anyhow::bail!("Steam not reachable")
            }
            Ok(())
        }
        fn log_text(&self) -> String {
            "DEBUG connecting to 192.168.1.77 as steamos, key SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG".into()
        }
        fn copy_to_clipboard(&self, text: &str) -> Result<()> {
            *self.clipboard.lock().unwrap() = Some(text.to_string());
            Ok(())
        }
        fn open_text_file(&self, p: &Path) -> Result<()> {
            self.log(format!("open {}", p.display()));
            Ok(())
        }
        fn open_url(&self, url: &str) -> Result<()> {
            *self.opened_url.lock().unwrap() = Some(url.to_string());
            Ok(())
        }
    }

    fn cfg(dir: &Path) -> WizardConfig {
        let now =
            chrono::TimeZone::with_ymd_and_hms(&chrono::Local, 2026, 10, 3, 14, 5, 0).unwrap();
        let mut c = WizardConfig::new(dir.to_path_buf(), now);
        c.discover_timeout = Duration::from_millis(1);
        c.tick = Duration::from_millis(5);
        c.poll = Duration::from_millis(1);
        c.interactive_timeout = Duration::from_millis(200);
        c.redact = RedactContext::default();
        c
    }

    fn found(host: &str) -> Found {
        Found {
            name: "steamframe".into(),
            host: host.into(),
        }
    }

    #[tokio::test]
    async fn happy_path_discovers_pairs_installs_and_reports() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&["", "n"]); // Enter to start, skip interactive
        let b = FakeBackend {
            discoveries: Mutex::new(VecDeque::from([vec![found("192.168.1.77")]])),
            pair_ok: vec!["192.168.1.77".into()],
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        let out = w.run().await;
        let Outcome::Done(saved) = out else {
            panic!("{out:?}\n{}", ui.text())
        };
        assert!(w.backend.called("install 192.168.1.77"));
        assert!(!w.backend.called("launch_probe"));
        let txt = saved.text.unwrap();
        assert!(txt.ends_with("FramePlayer-report-2026-10-03_14-05.txt"));
        let body = std::fs::read_to_string(&txt).unwrap();
        assert!(body.starts_with("FramePlayer self-test report | 2026-10-03 14:05 | probe 0.1.0"));
        assert!(body.contains("=== Summary ===\nPASS vulkan\nuser dir ~"));
        assert!(!body.contains("10.1.2.3"), "redacted");
        let json = std::fs::read_to_string(saved.json.unwrap()).unwrap();
        assert!(json.contains("<ip>"));
        assert!(saved.copied);
        assert_eq!(
            w.backend.clipboard.lock().unwrap().as_deref(),
            Some(body.as_str())
        );
        assert!(w.backend.called("open "));
        let screen = ui.text();
        assert!(screen.contains("Step 1 of 6: Finding your headset"));
        assert!(screen.contains("Step 2 of 6: Connecting to the headset"));
        assert!(screen.contains("   | PASS vulkan  Turnip"));
        assert!(screen.contains("press Ctrl+V to paste it"));
    }

    #[tokio::test]
    async fn not_found_then_typed_ip() {
        let d = tempfile::tempdir().unwrap();
        // Enter, "2" (type IP), a bad IP, a good IP, interactive: no.
        let ui = FakeUi::with(&["", "2", "192.168.1", "10.0.0.9", "n"]);
        let b = FakeBackend {
            pair_ok: vec!["10.0.0.9".into()],
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Done(_)), "{}", ui.text());
        let s = ui.text();
        assert!(s.contains("I couldn't find your headset"));
        assert!(s.contains("Developer Mode is off"));
        assert!(s.contains("doesn't look like an IP address"));
        assert!(w.backend.called("pair 10.0.0.9"));
    }

    #[tokio::test]
    async fn retry_discovery_then_quit() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&["", "1", "3"]);
        let w = Wizard::new(ui.clone(), FakeBackend::default(), cfg(d.path()));
        assert_eq!(w.run().await, Outcome::Quit);
        assert_eq!(
            w.backend
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| *c == "discover")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn pair_timeout_explained_and_retried() {
        let d = tempfile::tempdir().unwrap();
        // Pair fails on .5; user types another IP which works.
        let ui = FakeUi::with(&["", "2", "192.168.1.6", "n"]);
        let b = FakeBackend {
            discoveries: Mutex::new(VecDeque::from([vec![found("192.168.1.5")]])),
            pair_ok: vec!["192.168.1.6".into()],
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Done(_)));
        assert!(ui
            .text()
            .contains("I didn't get an answer from the headset in time"));
    }

    #[tokio::test]
    async fn saved_device_skips_pairing_and_same_version_skips_install() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&["", "n"]);
        let b = FakeBackend {
            saved: Some(device("192.168.1.8")),
            saved_reachable: true,
            installed: Some("0.2.0".into()),
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Done(_)));
        assert!(!w.backend.called("discover"));
        assert!(!w.backend.called("pair"));
        assert!(!w.backend.called("install"));
        assert!(ui.text().contains("already on the headset"));
    }

    #[tokio::test]
    async fn interactive_part_waits_for_new_report() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&["", ""]); // default yes
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            finish_after_polls: 3,
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        let Outcome::Done(saved) = w.run().await else {
            panic!("{}", ui.text())
        };
        assert!(w.backend.called("launch_probe"));
        assert!(ui.text().contains("PUT THE HEADSET ON NOW"));
        assert!(ui.text().contains("The test in the headset is finished."));
        let body = std::fs::read_to_string(saved.text.unwrap()).unwrap();
        assert!(
            body.contains("\"interactive\""),
            "updated report saved: {body}"
        );
    }

    #[tokio::test]
    async fn interactive_timeout_and_launch_failure_are_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&["", "y"]);
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Done(_)));
        assert!(ui.text().contains("didn't finish in time"));

        let ui = FakeUi::with(&["", "y"]);
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            launch_fails: true,
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Done(_)));
        assert!(ui.text().contains("couldn't start the second part"));
    }

    #[tokio::test]
    async fn missing_probe_fails_with_redacted_log() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&[""]);
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            probe_exit: PROBE_MISSING_EXIT,
            ..Default::default()
        };
        let w = Wizard::new(ui.clone(), b, cfg(d.path()));
        let Outcome::Failed(saved) = w.run().await else {
            panic!()
        };
        let p = saved.text.unwrap();
        assert!(p.ends_with("FramePlayer-install-log-2026-10-03_14-05.txt"));
        let log = std::fs::read_to_string(&p).unwrap();
        assert!(log.contains("doesn't include the self-test"));
        assert!(!log.contains("192.168.1.77") && log.contains("<ip>"));
        assert!(log.contains("SHA256:<fingerprint>"));
        assert_eq!(
            w.backend.clipboard.lock().unwrap().as_deref(),
            Some(log.as_str())
        );
        assert!(ui.text().contains("Something went wrong"));
    }

    #[tokio::test]
    async fn no_report_is_a_failure() {
        let d = tempfile::tempdir().unwrap();
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            no_report: true,
            ..Default::default()
        };
        let w = Wizard::new(FakeUi::with(&[""]), b, cfg(d.path()));
        assert!(matches!(w.run().await, Outcome::Failed(_)));
    }

    #[tokio::test]
    async fn probe_only_with_post_opens_issue_page() {
        let d = tempfile::tempdir().unwrap();
        let ui = FakeUi::with(&[""]);
        let b = FakeBackend {
            saved: Some(device("h")),
            saved_reachable: true,
            ..Default::default()
        };
        let mut c = cfg(d.path());
        c.interactive = Some(false);
        let w = Wizard::new(ui.clone(), b, c);
        assert!(matches!(w.run_probe_only(true).await, Outcome::Done(_)));
        let url = w.backend.opened_url.lock().unwrap().clone().unwrap();
        assert!(url.contains("labels=probe-report"));
        assert!(url.contains("title=Self-test+report+2026-10-03+14%3A05+0.1.0"));
        let clip = w.backend.clipboard.lock().unwrap().clone().unwrap();
        assert!(clip.contains("```json"));
        assert!(!clip.contains("10.1.2.3"));
        assert!(ui.text().contains("Submit new issue"));

        // No paired headset: friendly failure.
        let w = Wizard::new(FakeUi::with(&[]), FakeBackend::default(), cfg(d.path()));
        assert!(matches!(w.run_probe_only(false).await, Outcome::Failed(_)));
    }

    #[test]
    fn host_input_parsing() {
        assert_eq!(
            parse_host_input(" 192.168.1.23 ").as_deref(),
            Some("192.168.1.23")
        );
        assert_eq!(parse_host_input("fe80::1").as_deref(), Some("fe80::1"));
        assert_eq!(
            parse_host_input("steamframe.local").as_deref(),
            Some("steamframe.local")
        );
        assert_eq!(
            parse_host_input("http://10.0.0.2:32000/").as_deref(),
            Some("10.0.0.2")
        );
        assert_eq!(parse_host_input("192.168.1"), None);
        assert_eq!(parse_host_input("999.1.1.1"), None);
        assert_eq!(parse_host_input("my headset"), None);
        assert_eq!(parse_host_input(""), None);
    }

    #[test]
    fn pair_error_explanations() {
        let e = |s: &str| explain_pair_error(&anyhow::anyhow!(s.to_string()));
        assert!(e("pairing was declined on the headset").contains("said no"));
        assert!(e("timed out waiting for approval on the headset").contains("in time"));
        assert!(e("cannot reach the devkit service at http://x").contains("Developer Mode"));
    }
}
