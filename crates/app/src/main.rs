//! FramePlayer — native VR video player for the Valve Steam Frame.
//!
//! Startup order:
//! 1. updater boot check (roll back a build that never became healthy and
//!    re-exec the launcher) — before anything else can crash;
//! 2. CLI, paths, config, logging;
//! 3. the tokio I/O runtime with every service (library, sources, remote
//!    APIs, haptics, updater) — `runtime::services`;
//! 4. the playback engine (`fp_video::Player`, its own threads);
//! 5. the render thread (this thread): the OpenXR + Vulkan frame loop, or
//!    the headless loop with `--headless`.

mod app;
mod config;
mod controller;
mod frame_convert;
mod frame_loop;
mod haptics_bridge;
mod headless;
mod input_map;
mod logging;
mod media_input;
mod perf;
mod remote_bridge;
mod runtime;
mod state;
mod subtitles;
mod thumbnailer;
mod view_models;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, Paths};
use fp_core::MediaTime;
use fp_updater::{BootOutcome, HealthPolicy, InstallLayout};
use runtime::services::{ServiceOptions, Services};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Parser)]
#[command(
    name = "frameplayer",
    version,
    about = "Native VR video player for the Steam Frame"
)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/frameplayer/config.toml).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Log level: error, warn, info, debug, trace (RUST_LOG overrides).
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,
    /// Run the player pipeline without OpenXR/Vulkan (CI, debugging).
    #[arg(long, alias = "no-xr")]
    headless: bool,
    /// Open this file or URI at start.
    #[arg(long, value_name = "URI")]
    open: Option<String>,
    /// Start position for --open, in seconds.
    #[arg(long, value_name = "SECONDS", requires = "open")]
    start: Option<f64>,
    /// Headless: stop after this many seconds.
    #[arg(long, value_name = "SECONDS")]
    exit_after: Option<f64>,
    /// Don't scan library sources at start.
    #[arg(long)]
    no_scan: bool,
    /// Keep config, data and cache under this directory instead of the XDG
    /// locations (portable installs, tests).
    #[arg(long, value_name = "DIR")]
    data_root: Option<PathBuf>,
    /// Export the library (database, overrides, config) to a zip and exit.
    #[arg(long, value_name = "ZIP", conflicts_with = "import")]
    export: Option<PathBuf>,
    /// Restore a library export (replaces the library and config) and exit.
    #[arg(long, value_name = "ZIP")]
    import: Option<PathBuf>,
}

/// `--export` / `--import`: backup and migration (§3.5), no XR needed.
fn maintenance(
    cli: &Cli,
    paths: &Paths,
    config_path: &std::path::Path,
) -> Option<Result<ExitCode>> {
    if let Some(zip) = &cli.export {
        return Some((|| {
            let lib = fp_library::Library::open(paths.library_db())?;
            let m = fp_library::export::export_library(&lib, zip, &[("config.toml", config_path)])?;
            tracing::info!("exported library to {} ({m:?})", zip.display());
            Ok(ExitCode::SUCCESS)
        })());
    }
    if let Some(zip) = &cli.import {
        return Some((|| {
            let dir = config_path.parent().unwrap_or(&paths.config_dir);
            let r = fp_library::export::import_library(zip, &paths.library_db(), dir)?;
            tracing::info!(
                "imported library from {} into {} ({} config files)",
                zip.display(),
                r.db_path.display(),
                r.config_files.len()
            );
            Ok(ExitCode::SUCCESS)
        })());
    }
    None
}

/// Step 1. Returns the install layout (if managed) and the boot outcome.
fn boot_check() -> (Option<InstallLayout>, Option<Result<BootOutcome, String>>) {
    let Some(layout) = InstallLayout::from_env() else {
        return (None, None);
    };
    let outcome = layout.boot_check(&HealthPolicy::default());
    if let Ok(BootOutcome::RolledBack { from, to }) = &outcome {
        eprintln!("FramePlayer {from} never became healthy; rolled back to {to}, restarting");
        let err = layout.reexec_launcher();
        eprintln!("FramePlayer: re-exec of the launcher failed: {err}");
        std::process::exit(1);
    }
    let outcome = outcome.map_err(|e| e.to_string());
    (Some(layout), Some(outcome))
}

fn main() -> ExitCode {
    let (layout, boot) = boot_check();
    let cli = Cli::parse();
    let paths = match &cli.data_root {
        Some(root) => Paths::under(root),
        None => Paths::from_env(),
    };
    let config_path = cli.config.clone().unwrap_or_else(|| paths.config_file());
    let dirs_ok = paths.create_all();
    let config = Config::load(&config_path).unwrap_or_default();
    if !config_path.exists() {
        // Give first-time users a complete file to edit (library roots, …).
        let _ = config.save(&config_path);
    }
    let level = cli
        .log_level
        .clone()
        .unwrap_or_else(|| config.general.log_level.clone());
    let log_dir = std::env::var_os("FP_LOG_DIR").map(PathBuf::from);
    let log_file = logging::init(&level, log_dir.as_deref());
    tracing::info!(
        "FramePlayer {} starting ({}{})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        if cli.headless { ", headless" } else { "" }
    );
    if let Some(f) = log_file {
        tracing::info!("logging to {}", f.display());
    }
    if let Err(e) = dirs_ok {
        tracing::warn!("{e:#}");
    }
    match &boot {
        Some(Ok(BootOutcome::Trial { version, attempt })) => {
            tracing::info!("version {version} on trial (launch {attempt})")
        }
        Some(Ok(o)) => tracing::info!("install: {o:?}"),
        Some(Err(e)) => tracing::warn!("install health check failed: {e}"),
        None => tracing::info!("not running from a managed install"),
    }
    let result = match maintenance(&cli, &paths, &config_path) {
        Some(r) => r,
        None => run(cli, paths, config, config_path, layout),
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("FramePlayer: {e:#}");
            ExitCode::FAILURE
        }
    };
    logging::flush();
    code
}

fn run(
    cli: Cli,
    paths: Paths,
    config: Config,
    config_path: PathBuf,
    layout: Option<InstallLayout>,
) -> Result<ExitCode> {
    let rt = runtime::build_runtime().context("starting the I/O runtime")?;
    let library = Arc::new(
        fp_library::Library::open(paths.library_db())
            .with_context(|| format!("opening {}", paths.library_db().display()))?,
    );
    let services = Services::start(
        rt.handle(),
        config.clone(),
        paths.clone(),
        library,
        ServiceOptions {
            config_path,
            scan_on_start: !cli.no_scan,
            watch_mounts: !cli.headless,
            default_sources: !cli.headless,
            thumbnails: true,
        },
    );
    let player = fp_video::Player::spawn(
        Box::new(fp_video::DefaultBackend::default()),
        fp_video::PlayerConfig::default(),
    );
    let mut app = app::App::new(config, services, player);

    let open = cli
        .open
        .clone()
        .or_else(|| perf::take_open_request(&paths.perf_dir()));
    if let Some(uri) = &open {
        app.open_uri(uri, cli.start.map(MediaTime::from_secs_f64));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    rt.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            s2.store(true, Ordering::Relaxed);
        }
    });
    let mut guard = layout.map(|l| l.launch_guard(&HealthPolicy::default()));

    let code = if cli.headless {
        let report = headless::run(
            &mut app,
            &headless::HeadlessOptions {
                max_seconds: cli.exit_after,
                exit_when_done: open.is_some(),
                stop,
            },
        );
        tracing::info!(
            "headless run finished (ended: {}): states {:?}, {} frames, reached {}, decoder {}",
            report.reached(fp_video::PlaybackState::Ended),
            report.states,
            report.frames,
            report.max_position,
            report.decoder.as_deref().unwrap_or("none")
        );
        if let Some(g) = guard.as_mut() {
            g.poll();
        }
        match report.error {
            Some(e) => {
                tracing::error!("playback failed: {e}");
                ExitCode::FAILURE
            }
            None => ExitCode::SUCCESS,
        }
    } else {
        frame_loop::run(&mut app, &paths, guard.as_mut(), &stop)?;
        ExitCode::SUCCESS
    };
    app.shutdown();
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(code)
}
