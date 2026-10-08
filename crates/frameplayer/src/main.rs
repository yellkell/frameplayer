//! FramePlayer: a native VR video player for the Valve Steam Frame.
//!
//! `frameplayer [FILE|URL]` runs in the headset through OpenXR.
//! `frameplayer --preview OUT_DIR [--script FILE] [FILE|URL]` runs without a
//! headset, simulating the head and a controller, and saves screenshots;
//! used for automated UI tests.
//! `frameplayer --decode-bench FILE` decodes without a headset and prints
//! the decoder and its frame rate.

mod app;
mod bindings;
mod controls;
mod jobs;
mod logger;
mod playback;
mod preview;
mod prober;
mod services;
mod settings;
mod ui;
mod unlock;
mod world;

use app::{App, FrameInput, LayerDraw};
use fp_render::{Gpu, Renderer};
use fp_xr::{QuadSubmit, SessionEvent, XrContext, XrSession};
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
    bench: Option<String>,
    bench_opts: fp_media::bench::BenchOptions,
    verbose: bool,
    open: Option<String>,
}

const USAGE: &str = "usage: frameplayer [--verbose] [--exit-after SECONDS] [FILE|URL]
       frameplayer --preview OUT_DIR [--script FILE] [--size PIXELS] [FILE|URL]
       frameplayer --info FILE      (print what FramePlayer detects about a video)
       frameplayer --decode-bench FILE [--frames N] [--sw] [--seek SECONDS]
                   [--checksum N,N,...]  (decode only; print decoder and fps)
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
            "--decode-bench" => a.bench = Some(it.next().ok_or("--decode-bench needs a file")?),
            "--frames" => {
                a.bench_opts.frames = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--frames needs a number")?
            }
            "--sw" => a.bench_opts.hw = fp_media::HwDecode::Off,
            "--seek" => {
                a.bench_opts.seek = Some(
                    it.next()
                        .and_then(|s| s.parse().ok())
                        .ok_or("--seek needs seconds")?,
                )
            }
            "--checksum" => {
                a.bench_opts.checksum = it
                    .next()
                    .ok_or("--checksum needs frame numbers")?
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect()
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
    fp_media::init_logging();
    log::info!("FramePlayer {} starting", env!("CARGO_PKG_VERSION"));
    if let Some(f) = &args.bench {
        std::process::exit(match bench(f, &args.bench_opts) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        });
    }
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

/// Decodes a file as the player would, without a headset or GPU, and prints
/// the decoder used and its rate.
fn bench(file: &str, opts: &fp_media::bench::BenchOptions) -> Result<(), Error> {
    let path = std::path::Path::new(file);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let src: Arc<dyn fp_core::ByteSource> = Arc::new(fp_core::source::FileSource::open(path)?);
    let r = fp_media::bench::decode_bench(src, &name, opts)?;
    println!(
        "{name}: {}x{} {:?} via {} ({})",
        r.width,
        r.height,
        r.layout,
        r.decoder,
        if r.hardware { "hardware" } else { "software" }
    );
    let line = |label: &str, run: &fp_media::bench::BenchRun| {
        println!(
            "  {label}: {} frames, first after {:.3} s, {:.2} fps (pts {:.3?}..{:.3?})",
            run.frames, run.first_frame_secs, run.fps, run.first_pts, run.last_pts
        )
    };
    line("decode", &r.run);
    if let Some(s) = &r.after_seek {
        line("after seek", s);
    }
    for (i, pts, sum) in &r.checksums {
        println!("  frame {i} pts {pts:.3}: yuv fnv {sum:016x}");
    }
    Ok(())
}

/// Quad layer keys of the two pointer rays (panels use their index).
const RAY_LAYER: u64 = 100;
const RAY_PX: [u32; 2] = [64, 8];

/// The pointer ray's picture: light blue, soft across its width, fainter
/// for the hand that isn't pointing. Premultiplied, as the compositor
/// expects.
fn ray_picture(active: bool) -> Vec<u8> {
    let peak = if active { 0.85 } else { 0.35 };
    let across = [0.12, 0.5, 0.9, 1.0, 1.0, 0.9, 0.5, 0.12];
    let mut px = Vec::with_capacity((RAY_PX[0] * RAY_PX[1] * 4) as usize);
    for a in across {
        let a = a * peak;
        for _ in 0..RAY_PX[0] {
            px.extend([140.0 * a, 190.0 * a, 255.0 * a, 255.0 * a].map(|c: f32| c.round() as u8));
        }
    }
    px
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
            "Passthrough for apps".into(),
            if session.supports_passthrough_blend() {
                "yes (WebXR immersive-ar possible)"
            } else {
                "no (VR only)"
            }
            .into(),
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
    // Render-thread time per frame (after xrWaitFrame), logged every 10 s:
    // over ~9 ms the compositor starts synthesising frames.
    let mut frame_ms: Vec<f32> = Vec::new();
    let mut timing_since = Instant::now();
    let layers = session.supports_quad_layers();
    log::info!(
        "UI as compositor quad layers: {}",
        if layers {
            "yes"
        } else {
            "no (drawn into the eyes)"
        }
    );
    // Panel picture each quad layer holds: (panel, paint version).
    let mut copied: std::collections::HashMap<u64, (fp_render::PanelId, u64)> =
        std::collections::HashMap::new();
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
        let work = Instant::now();
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
                layers,
            },
        );
        frames += 1;
        video_frames += out.frame.is_some() as u64;
        renderer.set_video(out.frame.as_ref())?;
        let mut submits = Vec::new();
        if let (Some(targets), Some(eyes)) = (frame.targets, frame.eyes) {
            renderer.draw(&targets, &eyes, &out.video, &out.quads)?;
            for l in &out.layers {
                match *l {
                    LayerDraw::Panel {
                        key,
                        panel,
                        px,
                        pose,
                        size,
                    } => {
                        let version = renderer.panel_version(panel);
                        if version == 0 {
                            continue;
                        }
                        // Only a changed picture is copied; the compositor
                        // keeps showing the last one.
                        if copied.get(&key) != Some(&(panel, version))
                            || !session.quad_layer_ready(key)
                        {
                            let image = session.quad_layer_image(key, px[0], px[1])?;
                            renderer.copy_panel_to(panel, image)?;
                            copied.insert(key, (panel, version));
                        }
                        let (_, rot, pos) = pose.to_scale_rotation_translation();
                        submits.push(QuadSubmit {
                            key,
                            pose: (pos, rot),
                            size,
                        });
                    }
                    LayerDraw::Ray { active, pose, size } => {
                        let key = RAY_LAYER + active as u64;
                        session.static_quad_layer(key, RAY_PX[0], RAY_PX[1], |image| {
                            renderer
                                .upload_rgba(image, RAY_PX[0], RAY_PX[1], &ray_picture(active))
                                .map_err(|e| e.to_string())
                        })?;
                        submits.push(QuadSubmit { key, pose, size });
                    }
                }
            }
        }
        renderer.end_frame()?;
        session.set_passthrough(out.passthrough);
        for (hand, amp, ms) in out.buzz {
            session.buzz(hand, amp, ms);
        }
        frame_ms.push(work.elapsed().as_secs_f32() * 1000.0);
        session.end_frame(frame, &submits)?;
        if timing_since.elapsed() > Duration::from_secs(10) && !frame_ms.is_empty() {
            frame_ms.sort_by(f32::total_cmp);
            let at = |q: f32| frame_ms[((frame_ms.len() - 1) as f32 * q) as usize];
            let slow = frame_ms.iter().filter(|&&t| t > 9.0).count();
            log::info!(
                "frame CPU ms: median {:.1}, 95% {:.1}, max {:.1}; {slow} of {} over 9 ms",
                at(0.5),
                at(0.95),
                at(1.0),
                frame_ms.len()
            );
            if let Some(pb) = &app.playback {
                let st = pb.player.stats();
                log::info!(
                    "video {} ({}): {} shown, {} dropped, {} stalls, {} queued",
                    st.video_decoder,
                    if st.hardware { "hardware" } else { "software" },
                    st.frames_shown,
                    st.frames_dropped,
                    st.stalls,
                    st.video_queue
                );
            }
            frame_ms.clear();
            timing_since = Instant::now();
        }
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
    app.shutdown();
    drop(renderer);
    drop(session);
    drop(gpu);
    drop(ctx);
    log::info!("bye");
    Ok(())
}
