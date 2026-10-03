//! `frameplayer-install`: pair with a Steam Frame and install FramePlayer.
//!
//! ```text
//! frameplayer-install                      # guided mode (what a double-click runs)
//! frameplayer-install probe [--interactive] [--post]   # self-test + report
//! frameplayer-install pair                 # find the headset, approve on it
//! frameplayer-install install --latest     # download, upload, add to library
//! frameplayer-install launch | logs -f | status | uninstall
//! ```
//! Release maintainers also use `gen-key`, `make-manifest`, `make-delta`,
//! `sign-manifest`, `verify-manifest` and `site-manifest` (see tools/release.sh).

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fp_installer::config::{Paths, DEVKIT_SERVICE_PORT};
use fp_installer::console::{self, WizardOptions};
use fp_installer::installer::{
    Event, InstallOptions, InstallSource, Installer, PairOptions, Reporter,
};
use fp_installer::release;
use fp_installer::transport::{TransportChoice, TransportKind};
use fp_updater::manifest::Channel;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[derive(Parser)]
#[command(
    name = "frameplayer-install",
    version,
    about = "Install FramePlayer on a Steam Frame over Wi-Fi"
)]
struct Cli {
    /// Paired headset to use (name or IP); defaults to the last one paired.
    #[arg(long, short = 'd', global = true)]
    device: Option<String>,
    /// More detailed output.
    #[arg(long, short = 'v', global = true)]
    verbose: bool,
    /// SSH implementation: auto (built-in on Windows, system OpenSSH
    /// elsewhere), system or native. Also FRAMEPLAYER_SSH.
    #[arg(long, global = true)]
    ssh: Option<TransportChoice>,
    /// Without a command: the guided installer.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided install + self-test (same as running without a command).
    Wizard {
        /// Headset IP address (skips discovery).
        #[arg(long)]
        host: Option<String>,
        /// Release tarball to install instead of downloading.
        #[arg(long)]
        tarball: Option<PathBuf>,
        /// Run the in-headset part without asking.
        #[arg(long, conflicts_with = "no_interactive")]
        interactive: bool,
        /// Skip the in-headset part without asking.
        #[arg(long)]
        no_interactive: bool,
        /// Don't wait for Enter at the end.
        #[arg(long)]
        no_pause: bool,
    },
    /// Run the self-test on the paired headset and save the report on the Desktop.
    Probe {
        /// Also run the in-headset part (put the headset on).
        #[arg(long)]
        interactive: bool,
        /// Open a GitHub issue page with the report on the clipboard (you paste and submit).
        #[arg(long)]
        post: bool,
    },
    /// List headsets in Developer Mode on the local network.
    Discover {
        #[arg(long, default_value_t = 4)]
        timeout: u64,
    },
    /// Pair with a headset (approve the request on the headset).
    Pair {
        /// Headset IP or hostname; discovered automatically if omitted.
        #[arg(long)]
        host: Option<String>,
        /// Name to remember the headset by.
        #[arg(long)]
        name: Option<String>,
        /// Login user (normally reported by the headset).
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = DEVKIT_SERVICE_PORT)]
        service_port: u16,
        #[arg(long, default_value_t = 22)]
        ssh_port: u16,
    },
    /// Forget a paired headset.
    Unpair { name: String },
    /// Show paired headsets.
    Devices,
    /// Install or update FramePlayer on the headset.
    Install {
        /// Local release tarball (frameplayer-<ver>-aarch64.tar.gz).
        #[arg(long, conflicts_with = "latest")]
        tarball: Option<PathBuf>,
        /// Download the newest signed release (default).
        #[arg(long)]
        latest: bool,
        #[arg(long, default_value = "stable")]
        channel: Channel,
        #[arg(long, default_value = fp_updater::updater::DEFAULT_UPDATE_BASE)]
        update_base: Url,
        /// Also add FramePlayer to Favorites.
        #[arg(long)]
        pin: bool,
        /// Only copy files; don't touch the Steam library.
        #[arg(long)]
        no_steam: bool,
    },
    /// Remove FramePlayer from the headset.
    Uninstall {
        /// Also delete settings, library database and thumbnails.
        #[arg(long)]
        purge: bool,
    },
    /// Start FramePlayer on the headset.
    Launch,
    /// Show FramePlayer's logs from the headset.
    Logs {
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: u32,
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Show what is installed on the headset.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Open a shell on the headset, or run one command.
    Shell {
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Print an OpenSSH config block for the paired headset (used by tools/frame.sh).
    SshConfig {
        #[arg(long, default_value = "frame")]
        alias: String,
    },
    /// [release] Generate an ed25519 manifest signing key.
    GenKey {
        /// Write the secret key here (mode 0600) instead of printing it.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// [release] Write a release manifest for a tarball.
    MakeManifest {
        #[arg(long)]
        version: semver::Version,
        #[arg(long, default_value = "stable")]
        channel: Channel,
        #[arg(long, default_value = "aarch64")]
        arch: String,
        #[arg(long)]
        tarball: PathBuf,
        /// Public download URL of the tarball.
        #[arg(long)]
        url: Url,
        #[arg(long)]
        min_steamos: Option<String>,
        #[arg(long)]
        notes_file: Option<PathBuf>,
        #[arg(long)]
        notes_url: Option<Url>,
        /// Delta patch to list: FROM=PATH=URL (repeatable).
        #[arg(long = "delta")]
        deltas: Vec<release::DeltaInput>,
        #[arg(long)]
        out: PathBuf,
    },
    /// [release] Create a delta patch between two release tarballs.
    MakeDelta {
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// [release] Sign a manifest, writing <manifest>.sig.
    SignManifest {
        manifest: PathBuf,
        /// Environment variable holding the hex secret key.
        #[arg(long, default_value = "FP_SIGNING_KEY")]
        key_env: String,
        /// File holding the hex secret key (overrides --key-env).
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Fail unless the key is one this build's updater trusts.
        #[arg(long)]
        require_trusted: bool,
    },
    /// [release] Verify a manifest signature.
    VerifyManifest {
        manifest: PathBuf,
        #[arg(long)]
        sig: Option<PathBuf>,
        /// Hex public key(s), comma separated; default: compiled-in keys.
        #[arg(long)]
        pubkey: Option<String>,
    },
    /// [release] Update the website install manifest for a new tarball.
    SiteManifest {
        #[arg(long, default_value = "dist/frameplayer.json")]
        template: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        tarball: PathBuf,
        #[arg(long)]
        url: Url,
        /// Output path (defaults to overwriting the template).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// Prints events for a terminal.
struct Console {
    last_pct: std::sync::Mutex<Option<u64>>,
}

impl Reporter for Console {
    fn event(&self, e: Event) {
        match e {
            Event::Step(s) => eprintln!("==> {s}"),
            Event::Info(s) => eprintln!("    {s}"),
            Event::Warn(s) => eprintln!("warning: {s}"),
            Event::Done(s) => {
                eprintln!();
                eprintln!("{s}");
            }
            Event::ApproveOnHeadset { fingerprint } => {
                eprintln!();
                eprintln!("    Put on the headset and approve the pairing request.");
                eprintln!("    Key fingerprint: {fingerprint}");
                eprintln!();
            }
            Event::Progress { done, total } => {
                let Some(total) = total.filter(|t| *t > 0) else {
                    return;
                };
                let pct = done * 100 / total;
                let mut last = self.last_pct.lock().unwrap();
                if *last != Some(pct) {
                    *last = Some(pct);
                    eprint!(
                        "\r    {pct:3}%  {:.1} / {:.1} MB",
                        done as f64 / 1e6,
                        total as f64 / 1e6
                    );
                    if done >= total {
                        eprintln!();
                    }
                    let _ = std::io::stderr().flush();
                }
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let cli = Cli::parse();
    let transport = cli
        .ssh
        .map(TransportChoice::resolve)
        .unwrap_or_else(TransportKind::from_env_or_default);
    // Guided modes keep their own log and talk to the user directly.
    let guided = match &cli.cmd {
        None => Some(WizardOptions {
            pause_at_end: true,
            ..Default::default()
        }),
        Some(Cmd::Wizard {
            host,
            tarball,
            interactive,
            no_interactive,
            no_pause,
        }) => Some(WizardOptions {
            host: host.clone(),
            tarball: tarball.clone(),
            interactive: if *interactive {
                Some(true)
            } else if *no_interactive {
                Some(false)
            } else {
                None
            },
            pause_at_end: !no_pause,
            ..Default::default()
        }),
        Some(Cmd::Probe { interactive, post }) => Some(WizardOptions {
            interactive: Some(*interactive),
            probe_only: Some(*post),
            ..Default::default()
        }),
        _ => None,
    };
    if let Some(mut opts) = guided {
        opts.transport = Some(transport);
        std::process::exit(console::run(opts).await);
    }
    let filter = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let reporter = Arc::new(Console {
        last_pct: Default::default(),
    });
    let transport = cli
        .ssh
        .map(TransportChoice::resolve)
        .unwrap_or_else(TransportKind::from_env_or_default);
    let inst = || -> Result<Installer> {
        Ok(Installer::new(Paths::platform_default()?, reporter.clone()).with_transport(transport))
    };
    let dev = cli.device.as_deref();
    let Some(cmd) = cli.cmd else {
        return Ok(0);
    };
    match cmd {
        Cmd::Wizard { .. } | Cmd::Probe { .. } => unreachable!("handled in main"),
        Cmd::Discover { timeout } => {
            let found = inst()?.discover(Duration::from_secs(timeout)).await?;
            if found.is_empty() {
                eprintln!(
                    "No headsets found. Is Developer Mode on and the headset on this network?"
                );
                return Ok(1);
            }
            for d in found {
                let addrs: Vec<String> = d.addresses.iter().map(|a| a.to_string()).collect();
                println!(
                    "{}\t{}\t{}\tlogin={}",
                    d.name,
                    addrs.join(","),
                    d.port,
                    d.properties.login().unwrap_or_default()
                );
            }
        }
        Cmd::Pair {
            host,
            name,
            user,
            service_port,
            ssh_port,
        } => {
            let opts = PairOptions {
                host,
                name,
                user,
                service_port,
                ssh_port,
                ..Default::default()
            };
            inst()?.pair(&opts).await?;
        }
        Cmd::Unpair { name } => {
            let paths = Paths::platform_default()?;
            let mut s = paths.load_state()?;
            if !s.remove(&name) {
                bail!("no paired headset named {name:?}");
            }
            paths.save_state(&s)?;
            eprintln!("Forgot {name}. (The key stays authorised on the headset until you remove it there.)");
        }
        Cmd::Devices => {
            let s = Paths::platform_default()?.load_state()?;
            for d in &s.devices {
                let mark = if s.default_device.as_deref() == Some(d.name.as_str()) {
                    "*"
                } else {
                    " "
                };
                println!("{mark} {}\t{}@{}:{}", d.name, d.user, d.host, d.ssh_port);
            }
        }
        Cmd::Install {
            tarball,
            latest: _,
            channel,
            update_base,
            pin,
            no_steam,
        } => {
            let i = inst()?;
            let device = i.device(dev)?;
            let source = match tarball {
                Some(p) => InstallSource::Tarball(p),
                None => InstallSource::Latest {
                    channel,
                    base_url: update_base,
                },
            };
            i.install(
                &device,
                &source,
                &InstallOptions {
                    pin,
                    skip_steam: no_steam,
                },
            )
            .await?;
        }
        Cmd::Uninstall { purge } => {
            let i = inst()?;
            let device = i.device(dev)?;
            i.uninstall(&device, purge).await?;
        }
        Cmd::Launch => {
            let i = inst()?;
            let device = i.device(dev)?;
            i.launch(&device).await?;
        }
        Cmd::Logs { lines, follow } => {
            let i = inst()?;
            let device = i.device(dev)?;
            i.logs(&device, lines, follow).await?;
        }
        Cmd::Status { json } => {
            let i = inst()?;
            let device = i.device(dev)?;
            let s = i.status(&device).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "installed": s.installed, "release": s.release, "current": s.current,
                        "previous": s.previous, "trial": s.trial, "blocked": s.blocked,
                        "running_pid": s.running_pid, "os_version": s.os_version,
                        "os_build": s.os_build, "devkit_utils": s.devkit_utils, "free_kb": s.free_kb,
                    }))?
                );
            } else if !s.installed {
                println!("FramePlayer is not installed.");
            } else {
                println!(
                    "Installed release : {}",
                    s.release.as_deref().unwrap_or("?")
                );
                println!(
                    "Active version    : {}",
                    s.current.as_deref().unwrap_or("(not launched yet)")
                );
                if let Some(p) = &s.previous {
                    println!("Rollback version  : {p}");
                }
                if let Some(t) = &s.trial {
                    println!("On trial          : {t} (version attempts max)");
                }
                if !s.blocked.is_empty() {
                    println!("Rolled back from  : {}", s.blocked.join(", "));
                }
                println!(
                    "Running           : {}",
                    s.running_pid
                        .map(|p| format!("yes (pid {p})"))
                        .unwrap_or_else(|| "no".into())
                );
                println!(
                    "SteamOS           : {} {}",
                    s.os_version.unwrap_or_default(),
                    s.os_build.unwrap_or_default()
                );
                if let Some(kb) = s.free_kb {
                    println!("Free space        : {:.1} GB", kb as f64 / 1e6);
                }
            }
        }
        Cmd::Shell { command } => {
            let i = inst()?;
            let device = i.device(dev)?;
            let cmd = (!command.is_empty()).then(|| command.join(" "));
            return tokio::task::spawn_blocking(move || i.shell(&device, cmd.as_deref())).await?;
        }
        Cmd::SshConfig { alias } => {
            let i = inst()?;
            let device = i.device(dev)?;
            print!("{}", i.target(&device)?.to_ssh_config(&alias));
        }
        Cmd::GenKey { out } => {
            let (secret, public) = fp_updater::signing::generate_keypair();
            match out {
                Some(p) => {
                    write_secret(&p, &secret)?;
                    eprintln!("Secret key written to {}", p.display());
                }
                None => println!("secret: {secret}"),
            }
            println!("public: {public}");
            eprintln!("Put the public key in crates/updater/release-public-keys.txt and the secret in the FP_SIGNING_KEY CI secret.");
        }
        Cmd::MakeManifest {
            version,
            channel,
            arch,
            tarball,
            url,
            min_steamos,
            notes_file,
            notes_url,
            deltas,
            out,
        } => {
            let notes = match notes_file {
                Some(p) => std::fs::read_to_string(&p).with_context(|| p.display().to_string())?,
                None => String::new(),
            };
            let input = release::ManifestInput {
                version,
                channel,
                arch,
                tarball,
                url,
                min_steamos,
                notes,
                notes_url,
                deltas,
            };
            let m = release::build_manifest(&input)?;
            std::fs::write(&out, m.to_json_pretty()?)?;
            eprintln!("Wrote {}", out.display());
        }
        Cmd::MakeDelta { from, to, out } => {
            let size = release::make_delta(&from, &to, &out)?;
            eprintln!("Wrote {} ({size} bytes)", out.display());
        }
        Cmd::SignManifest {
            manifest,
            key_env,
            key_file,
            require_trusted,
        } => {
            let secret = match key_file {
                Some(p) => release::load_secret(&p.to_string_lossy())?,
                None => release::load_secret(
                    &std::env::var(&key_env).with_context(|| format!("${key_env} is not set"))?,
                )?,
            };
            let (sig, public, trusted) = release::sign_manifest(&manifest, &secret)?;
            eprintln!("Wrote {} (signed by {public})", sig.display());
            if !trusted {
                let msg = "this key is NOT in crates/updater/release-public-keys.txt; installed apps will reject the update";
                if require_trusted {
                    bail!(msg);
                }
                eprintln!("warning: {msg}");
            }
        }
        Cmd::VerifyManifest {
            manifest,
            sig,
            pubkey,
        } => {
            let m = release::verify_manifest(&manifest, sig.as_deref(), pubkey.as_deref())?;
            println!(
                "OK: {} {} ({}), {} artifact(s)",
                m.name,
                m.version,
                m.channel,
                m.artifacts.len()
            );
        }
        Cmd::SiteManifest {
            template,
            version,
            tarball,
            url,
            out,
        } => {
            let text = std::fs::read_to_string(&template)
                .with_context(|| template.display().to_string())?;
            let m = release::update_site_manifest(&text, &version, &tarball, url)?;
            let out = out.unwrap_or(template);
            std::fs::write(&out, m.to_json_pretty()?)?;
            eprintln!("Wrote {}", out.display());
        }
    }
    Ok(0)
}

fn write_secret(path: &std::path::Path, secret: &str) -> Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writeln!(f, "{secret}")?;
    Ok(())
}
