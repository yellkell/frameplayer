//! Runs the real OpenXR frame loop for a few seconds and reports what
//! happened. Use with any runtime, e.g. Monado's simulated headset:
//!   XRT_COMPOSITOR_NULL=1 SIMULATED_ENABLE=1 monado-service
//!   cargo run -p fp-xr --example xr_smoke

use fp_render::{Gpu, Renderer, VideoParams};
use fp_xr::{SessionEvent, XrContext, XrSession};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = Arc::new(XrContext::new("frameplayer-xr-smoke")?);
    println!(
        "runtime: {} | system: {} | blend modes {:?}",
        ctx.runtime, ctx.system_name, ctx.blend_modes
    );
    let gpu = Arc::new(Gpu::new(&ctx.creator(), "frameplayer")?);
    println!("gpu: {} ({})", gpu.device_name, gpu.driver);
    let mut session = XrSession::new(ctx.clone(), gpu.clone())?;
    println!(
        "eye: {:?} {:?}; bindings {:?}",
        session.extent,
        session.format,
        session.bindings()
    );
    let mut renderer = Renderer::new(gpu.clone(), session.format)?;
    let start = Instant::now();
    let (mut frames, mut rendered) = (0, 0);
    while start.elapsed() < Duration::from_secs(4) {
        match session.poll()? {
            SessionEvent::Exit => break,
            SessionEvent::None => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            SessionEvent::Running => {}
        }
        let frame = session.begin_frame()?;
        if let (Some(targets), Some(eyes)) = (frame.targets, frame.eyes) {
            renderer.begin_frame()?;
            renderer.set_video(None)?;
            renderer.draw(&targets, &eyes, &VideoParams::default(), &[])?;
            renderer.end_frame()?;
            rendered += 1;
        }
        let input = session.input(frame.state.predicted_display_time);
        if frames == 10 {
            println!(
                "input: {:?}",
                input.hands.map(|h| (h.active, h.aim.is_some()))
            );
        }
        session.end_frame(frame, &[])?;
        frames += 1;
    }
    renderer.wait_idle();
    println!(
        "frames {frames}, rendered {rendered}, state {:?}",
        session.state()
    );
    drop(renderer);
    drop(session);
    Ok(())
}
