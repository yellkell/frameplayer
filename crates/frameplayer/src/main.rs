//! FramePlayer: a native VR video player for the Valve Steam Frame.
//!
//! `frameplayer [FILE|URL]` runs in the headset through OpenXR.
//! `frameplayer --preview OUT_DIR [--script FILE] [FILE|URL]` runs without a
//! headset, simulating the head and a controller, and saves screenshots;
//! used for automated UI tests.

mod app;
mod jobs;
mod logger;
mod playback;
mod preview;
mod prober;
mod services;
mod settings;
mod ui;
mod world;

use app::{App, FrameInput};
use fp_render::{Gpu, Renderer};
use fp_xr::{SessionEvent, XrContext, XrSession};
use settings::Settings;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Error = Box<dyn std::error::Error>;

#[derive(Default)]
struct Args {
    preview: Option<PathBuf>,
    script: Option<PathBuf>,
    size: Option<u32>,
    verbose: bool,
    open: Option<String>,
}

const USAGE: &str = "usage: frameplayer [--verbose] [FILE|URL]
       frameplayer --preview OUT_DIR [--script FILE] [--size PIXELS] [FILE|URL]
       frameplayer --version";

fn parse_args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--preview" => a.preview = Some(it.next().ok_or("--preview needs a folder")?.into()),
            "--script" => a.script = Some(it.next().ok_or("--script needs a file")?.into()),
            "--size" => {
                a.size = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .ok_or("--size needs a number")?,
                )
            }
            "-v" | "--verbose" => a.verbose = true,
            "--version" => {
                println!("FramePlayer {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n{USAGE}")),
            s => a.open = Some(s.to_string()),
        }
    }
    Ok(a)
}

/// Absolute path for local files given on the command line.
fn location(arg: &str) -> String {
    if arg.contains("://") || arg.starts_with('/') {
        return arg.to_string();
    }
    std::env::current_dir()
        .map(|d| d.join(arg).display().to_string())
        .unwrap_or_else(|_| arg.to_string())
}

fn make_app() -> Result<App, Error> {
    let settings = Settings::load(&Settings::path());
    let library = fp_library::Library::open_default()?;
    Ok(App::new(settings, library))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    logger::init(args.verbose);
    log::info!("FramePlayer {} starting", env!("CARGO_PKG_VERSION"));
    let r = match &args.preview {
        Some(dir) => preview::run(&args, dir),
        None => run_xr(&args),
    };
    if let Err(e) = r {
        log::error!("{e}");
        eprintln!("FramePlayer: {e}");
        std::process::exit(1);
    }
}

fn run_xr(args: &Args) -> Result<(), Error> {
    let ctx = Arc::new(XrContext::new("FramePlayer").map_err(|e| {
        format!("{e}\nIs SteamVR running? FramePlayer needs an OpenXR runtime (the Steam Frame provides one).")
    })?);
    log::info!("OpenXR runtime {} on {}", ctx.runtime, ctx.system_name);
    let gpu = Arc::new(Gpu::new(&ctx.creator(), "FramePlayer")?);
    log::info!("GPU {} ({})", gpu.device_name, gpu.driver);
    let mut session = XrSession::new(ctx.clone(), gpu.clone())?;
    log::info!(
        "eye buffers {:?} {:?}; controller bindings {:?}",
        session.extent,
        session.format,
        session.bindings()
    );
    let mut renderer = Renderer::new(gpu.clone(), session.format)?;
    let mut app = make_app()?;
    app.set_about(vec![
        ("OpenXR runtime".into(), ctx.runtime.clone()),
        ("Headset".into(), ctx.system_name.clone()),
        (
            "GPU".into(),
            format!("{} ({})", gpu.device_name, gpu.driver),
        ),
        (
            "Eye resolution".into(),
            format!("{}×{}", session.extent.width, session.extent.height),
        ),
        (
            "Controllers".into(),
            session
                .bindings()
                .iter()
                .map(|b| b.0.clone())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    ]);
    if let Some(o) = &args.open {
        app.open(playback::OpenRequest {
            location: location(o),
            ..Default::default()
        });
    }
    let start = Instant::now();
    let mut last = Instant::now();
    let mut quitting = false;
    loop {
        match session.poll()? {
            SessionEvent::Exit => break,
            SessionEvent::None => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            SessionEvent::Running => {}
        }
        let frame = session.begin_frame()?;
        let input = session.input(frame.state.predicted_display_time);
        let dt = last.elapsed().as_secs_f32().min(0.1);
        last = Instant::now();
        renderer.begin_frame()?;
        let out = app.frame(
            &mut renderer,
            FrameInput {
                time: start.elapsed().as_secs_f64(),
                dt,
                head: frame.head,
                hands: input.hands,
                passthrough_available: session.supports_passthrough_blend(),
            },
        );
        renderer.set_video(out.frame.as_ref())?;
        if let (Some(targets), Some(eyes)) = (frame.targets, frame.eyes) {
            renderer.draw(&targets, &eyes, &out.video, &out.quads)?;
        }
        renderer.end_frame()?;
        session.set_passthrough(out.passthrough);
        for (hand, amp, ms) in out.buzz {
            session.buzz(hand, amp, ms);
        }
        session.end_frame(frame)?;
        if out.quit && !quitting {
            quitting = true;
            session.request_exit();
        }
    }
    renderer.wait_idle();
    app.shutdown();
    drop(renderer);
    drop(session);
    log::info!("bye");
    Ok(())
}
