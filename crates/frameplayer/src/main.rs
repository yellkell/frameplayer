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
mod webxr;
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
    exit_after: Option<f64>,
    info: Option<String>,
    verbose: bool,
    open: Option<String>,
}

const USAGE: &str = "usage: frameplayer [--verbose] [--exit-after SECONDS] [FILE|URL]
       frameplayer --preview OUT_DIR [--script FILE] [--size PIXELS] [FILE|URL]
       frameplayer --info FILE      (print what FramePlayer detects about a video)
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
            "--exit-after" => {
                a.exit_after = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .ok_or("--exit-after needs seconds")?,
                )
            }
            "--info" => a.info = Some(it.next().ok_or("--info needs a file")?),
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
    fp_media::init_logging();
    log::info!("FramePlayer {} starting", env!("CARGO_PKG_VERSION"));
    if let Some(f) = &args.info {
        std::process::exit(match info(f) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        });
    }
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

/// Prints container, streams and the detected VR format, and decodes the
/// first frame, without needing a headset or GPU.
fn info(file: &str) -> Result<(), Error> {
    let path = std::path::Path::new(file);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let src = || -> Result<Arc<dyn fp_core::ByteSource>, Error> {
        Ok(Arc::new(fp_core::source::FileSource::open(path)?))
    };
    let i = fp_media::thumb::probe(src()?, &name)?;
    println!("{name}: {} · {:.1} s", i.container, i.duration);
    for s in &i.streams {
        let lang = s
            .language
            .as_deref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        match s.kind {
            fp_media::info::StreamKind::Video => {
                println!(
                    "  #{} video {} {}x{} {:.3} fps {}-bit {:?}{lang}",
                    s.index, s.codec, s.width, s.height, s.fps, s.bit_depth, s.transfer
                )
            }
            fp_media::info::StreamKind::Audio => println!(
                "  #{} audio {} {} Hz {} ch{}{lang}",
                s.index,
                s.codec,
                s.sample_rate,
                s.channels,
                if s.ambisonic { " ambisonic" } else { "" }
            ),
            _ => println!("  #{} {:?} {}{lang}", s.index, s.kind, s.codec),
        }
    }
    let (w, h) = i
        .video_stream()
        .map(|v| (v.width, v.height))
        .unwrap_or((0, 0));
    let d = fp_core::format::resolve(None, i.hints, &name, w, h);
    println!("  format: {} ({})", d.format.label(), d.evidence.label());
    let f = fp_media::thumb::decode_first_frame(src()?, &name)?;
    println!(
        "  first frame: {}x{} {:?} decoded",
        f.width, f.height, f.layout
    );
    Ok(())
}

/// Gives the headset to a WebXR browser: through `frameplayer.sh` when it
/// started us (it runs the browser, then restarts FramePlayer), else
/// directly.
fn hand_off(argv: &[String]) -> Result<(), Error> {
    if std::env::var_os(webxr::LAUNCHER_ENV).is_some() {
        webxr::write_handoff(&webxr::handoff_path(), argv)?;
        log::info!("handing off to the launcher: {argv:?}");
        std::process::exit(webxr::HANDOFF_EXIT_CODE);
    }
    log::info!("starting {argv:?}");
    webxr::spawn_detached(argv)?;
    Ok(())
}

fn run_xr(args: &Args) -> Result<(), Error> {
    // Coming back from a WebXR browser, SteamVR may still be closing its
    // session: keep trying for a while instead of quitting.
    let attempts = if std::env::var_os("FRAMEPLAYER_RESUMED").is_some() {
        20
    } else {
        1
    };
    let mut tries = 0;
    let ctx = loop {
        tries += 1;
        match XrContext::new("FramePlayer") {
            Ok(c) => break Arc::new(c),
            Err(e) if tries < attempts => {
                log::info!("OpenXR not ready yet ({e}); retrying");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => {
                return Err(format!("{e}\nIs SteamVR running? FramePlayer needs an OpenXR runtime (the Steam Frame provides one).").into());
            }
        }
    };
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
    let (mut frames, mut video_frames) = (0u64, 0u64);
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
        frames += 1;
        video_frames += out.frame.is_some() as u64;
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
        let timed_out = args
            .exit_after
            .is_some_and(|t| start.elapsed().as_secs_f64() > t);
        if (out.quit || timed_out) && !quitting {
            quitting = true;
            session.request_exit();
        }
    }
    log::info!("{frames} frames, {} with video", video_frames);
    renderer.wait_idle();
    let handoff = app.handoff.take();
    app.shutdown();
    drop(renderer);
    drop(session);
    drop(gpu);
    drop(ctx);
    if let Some(argv) = handoff {
        hand_off(&argv)?;
    }
    log::info!("bye");
    Ok(())
}
