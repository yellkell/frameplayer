//! Headless preview: renders the app offscreen with a simulated head and
//! right controller, driven by a small script, and saves screenshots.
//!
//! Script commands (one per line, `#` comments):
//!
//! | command | effect |
//! |---|---|
//! | `frames N` | run N frames |
//! | `wait SECONDS` | run frames in real time |
//! | `wait-playing [TIMEOUT]` | run until a video is playing |
//! | `look YAW PITCH` | head direction in degrees |
//! | `aim YAW PITCH` | controller ray direction in degrees |
//! | `point PANEL X Y` | aim at a panel (`main`, `bar`, `adjust`, `keyboard`) at fractional X, Y |
//! | `click` | pull and release the trigger |
//! | `button a/b/x/y/menu/stick/shoulder` | press and release a right-hand button |
//! | `stick X Y FRAMES` | hold the thumbstick |
//! | `squeeze` | squeeze and release the grip |
//! | `grip V` / `trigger V` | hold the grip / trigger at V (0..1) until changed |
//! | `status` | print what is playing and the view settings |
//! | `open LOCATION` | open a file or URL |
//! | `shot NAME` | save the left eye as OUT_DIR/NAME.png |
//! | `sbs NAME` | save both eyes side by side |
//! | `screen NAME [TAB]` | show a browser screen (`home`, `library`, `sources`, `settings` and its tab) |
//! | `adjust [TAB]` | open the adjustments panel (at tab TAB) |
//! | `chroma` | turn chroma key on for the open video |
//! | `panel PANEL NAME` | save a panel's image flat, pixel for pixel, as OUT_DIR/NAME.png |

use crate::app::{App, FrameInput};
use crate::{Args, Error};
use fp_render::capture::OffscreenEyes;
use fp_render::math::{Fov, projection, view};
use fp_render::{EyeView, Gpu, Renderer};
use fp_xr::Hand;
use glam::{EulerRot, Quat, Vec2, Vec3};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_SCRIPT: &str = "wait 1.5\nshot home\n";
const EYE_HEIGHT: f32 = 1.6;
const IPD: f32 = 0.064;

struct Sim {
    app: App,
    renderer: Renderer,
    eyes: OffscreenEyes,
    size: u32,
    head: Quat,
    hand: Hand,
    start: Instant,
    last: Instant,
    frames: u64,
    out: std::path::PathBuf,
}

fn dir_quat(yaw: f32, pitch: f32) -> Quat {
    Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), pitch.to_radians(), 0.0)
}

impl Sim {
    fn head_pos(&self) -> Vec3 {
        Vec3::new(0.0, EYE_HEIGHT, 0.0)
    }

    fn hand_pos(&self) -> Vec3 {
        self.head_pos() + Vec3::new(0.18, -0.3, -0.25)
    }

    fn step(&mut self) -> Result<bool, Error> {
        let dt = self.last.elapsed().as_secs_f32().clamp(0.001, 0.1);
        self.last = Instant::now();
        self.renderer.begin_frame()?;
        let hands = [Hand::default(), self.hand];
        let head = Some((self.head_pos(), self.head));
        let out = self.app.frame(
            &mut self.renderer,
            FrameInput {
                time: self.start.elapsed().as_secs_f64(),
                dt,
                head,
                hands,
                passthrough_available: true,
                layers: false,
            },
        );
        self.renderer.set_video(out.frame.as_ref())?;
        let proj = projection(Fov::symmetric(100.0), 0.05, 200.0);
        let right = self.head * Vec3::X * (IPD / 2.0);
        let ev = |p: Vec3| EyeView {
            view: view(p, self.head),
            proj,
        };
        let eyes = [ev(self.head_pos() - right), ev(self.head_pos() + right)];
        self.renderer
            .draw(&self.eyes.targets(), &eyes, &out.video, &out.quads)?;
        self.renderer.end_frame()?;
        self.frames += 1;
        Ok(out.quit)
    }

    fn run_frames(&mut self, n: u32) -> Result<(), Error> {
        for _ in 0..n {
            self.step()?;
            std::thread::sleep(Duration::from_millis(11));
        }
        Ok(())
    }

    fn save(&mut self, name: &str, both: bool) -> Result<(), Error> {
        self.renderer.wait_idle();
        let l = self.eyes.read(&self.renderer, 0)?;
        let (w, h) = (self.size, self.size);
        let img = if both {
            let r = self.eyes.read(&self.renderer, 1)?;
            let mut out = image::RgbaImage::new(w * 2, h);
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    out.put_pixel(x, y, image::Rgba([l[i], l[i + 1], l[i + 2], 255]));
                    out.put_pixel(x + w, y, image::Rgba([r[i], r[i + 1], r[i + 2], 255]));
                }
            }
            out
        } else {
            let mut px = l;
            for a in px.chunks_mut(4) {
                a[3] = 255;
            }
            image::RgbaImage::from_raw(w, h, px).ok_or("bad image size")?
        };
        let path = self.out.join(format!("{name}.png"));
        img.save(&path)?;
        println!("saved {}", path.display());
        Ok(())
    }

    fn save_panel(&mut self, panel: &str, name: &str) -> Result<(), Error> {
        let id = self
            .app
            .panel_id(panel)
            .ok_or(format!("panel {panel} is not visible"))?;
        let (w, h, mut px) = self.renderer.read_panel(id)?;
        // egui paints premultiplied alpha; PNG wants it straight.
        for p in px.chunks_mut(4) {
            let a = p[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut p[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let img = image::RgbaImage::from_raw(w, h, px).ok_or("bad panel size")?;
        let path = self.out.join(format!("{name}.png"));
        img.save(&path)?;
        println!("saved {}", path.display());
        Ok(())
    }

    fn command(&mut self, line: &str) -> Result<bool, Error> {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let num = |i: usize| -> Result<f32, Error> {
            Ok(parts
                .get(i)
                .ok_or(format!("missing argument in {line:?}"))?
                .parse::<f32>()?)
        };
        match parts.first().copied() {
            None => {}
            Some("frames") => self.run_frames(num(1)? as u32)?,
            Some("wait") => {
                let until = Instant::now() + Duration::from_secs_f32(num(1)?);
                while Instant::now() < until {
                    if self.step()? {
                        return Ok(true);
                    }
                    std::thread::sleep(Duration::from_millis(11));
                }
            }
            Some("wait-playing") => {
                let timeout = num(1).unwrap_or(20.0);
                let until = Instant::now() + Duration::from_secs_f32(timeout);
                loop {
                    self.step()?;
                    let playing = self
                        .app
                        .playback
                        .as_ref()
                        .is_some_and(|p| p.player.current_frame().is_some());
                    if playing {
                        break;
                    }
                    if Instant::now() > until {
                        return Err(format!(
                            "not playing after {timeout}s: {:?}",
                            self.app.ui.open_error
                        )
                        .into());
                    }
                    std::thread::sleep(Duration::from_millis(11));
                }
            }
            Some("screen") => {
                use crate::ui::{Screen, SettingsTab};
                let screen = match parts.get(1).copied().unwrap_or("home") {
                    "home" => Screen::Home,
                    "library" => Screen::Library,
                    "sources" => Screen::Sources,
                    "settings" => Screen::Settings,
                    other => return Err(format!("no screen {other:?}").into()),
                };
                let ui = &mut self.app.ui;
                ui.screen = screen;
                ui.details = None;
                ui.remap_open = None;
                if let Some(tab) = parts.get(2) {
                    ui.settings_tab = match *tab {
                        "playback" => SettingsTab::Playback,
                        "passthrough" => SettingsTab::Passthrough,
                        "controller" => SettingsTab::Controller,
                        // The Controller tab with the right A button's choices open.
                        "controller-open" => {
                            ui.remap_open = Some(crate::ui::RemapSlot::Button(
                                crate::bindings::RIGHT,
                                crate::bindings::Button::South,
                            ));
                            SettingsTab::Controller
                        }
                        // ...and with the left thumbstick's choices open.
                        "controller-stick" => {
                            ui.remap_open = Some(crate::ui::RemapSlot::Axis(
                                crate::bindings::LEFT,
                                crate::bindings::Axis::StickX,
                            ));
                            SettingsTab::Controller
                        }
                        "library" => SettingsTab::Library,
                        "haptics" => SettingsTab::Haptics,
                        "remote" => SettingsTab::Remote,
                        "updates" => SettingsTab::Updates,
                        "about" => SettingsTab::About,
                        other => return Err(format!("no settings tab {other:?}").into()),
                    };
                }
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            Some("panel") => {
                let name = parts.get(1).copied().unwrap_or("main");
                let file = parts.get(2).copied().unwrap_or(name);
                // A missing panel shouldn't lose the rest of the shots.
                if let Err(e) = self.save_panel(name, file) {
                    log::warn!("{line}: {e}");
                }
            }
            Some("adjust") => {
                self.app.ui.adjust_open = true;
                // `adjust N` opens tab N.
                if let Some(tab) = parts.get(1).and_then(|t| t.parse().ok()) {
                    self.app.ui.adjust_tab = tab;
                }
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            // Passthrough videos as if bought (crate::unlock).
            Some("unlocked") => {
                self.app.unlocked = true;
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            // The open video uses its own mask, as an `_alpha` video does.
            Some("mask") => {
                if let Some(p) = &mut self.app.playback {
                    p.format.alpha_packed = true;
                }
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            // Turns the global chroma key on (Settings > Passthrough).
            Some("chroma-global") => {
                self.app.settings.default_view.chroma_key = true;
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            // Turns chroma key on for the open video (the Passthrough tab).
            Some("chroma") => {
                if let Some(p) = &mut self.app.playback {
                    p.settings.chroma_key = true;
                    p.settings.key_own = Some(true);
                }
                self.app.repaint_all();
                self.run_frames(3)?;
            }
            Some("look") => self.head = dir_quat(num(1)?, num(2)?),
            Some("aim") => {
                self.hand.aim = Some((self.hand_pos(), dir_quat(num(1)?, num(2)?)));
            }
            Some("point") => {
                let name = parts.get(1).copied().unwrap_or("main");
                let target = self
                    .app
                    .panel_point(name, num(2)?, num(3)?)
                    .ok_or(format!("panel {name} is not visible"))?;
                let dir = (target - self.hand_pos()).normalize();
                self.hand.aim = Some((self.hand_pos(), Quat::from_rotation_arc(Vec3::NEG_Z, dir)));
                self.run_frames(2)?;
            }
            Some("click") => {
                self.hand.trigger = 1.0;
                self.run_frames(2)?;
                self.hand.trigger = 0.0;
                self.run_frames(3)?;
            }
            Some("button") => {
                let b = parts.get(1).copied().unwrap_or("primary");
                // The simulated hand is the right one: A/south plays, B/east
                // goes back ("primary"/"secondary" are the old names).
                let set = |h: &mut Hand, v: bool| match b {
                    "east" | "b" | "secondary" => h.east = v,
                    "north" | "y" => h.north = v,
                    "west" | "x" => h.west = v,
                    "stick" => h.stick_click = v,
                    "menu" => h.menu = v,
                    "shoulder" => h.shoulder = v,
                    _ => h.south = v,
                };
                set(&mut self.hand, true);
                self.run_frames(2)?;
                set(&mut self.hand, false);
                self.run_frames(2)?;
            }
            Some("stick") => {
                self.hand.stick = Vec2::new(num(1)?, num(2)?);
                self.run_frames(num(3).unwrap_or(10.0) as u32)?;
                self.hand.stick = Vec2::ZERO;
                self.run_frames(2)?;
            }
            Some("grip") => self.hand.squeeze = num(1)?,
            Some("trigger") => self.hand.trigger = num(1)?,
            Some("status") => println!("status: {}", self.app.status_line()),
            Some("squeeze") => {
                self.hand.squeeze = 1.0;
                self.run_frames(2)?;
                self.hand.squeeze = 0.0;
                self.run_frames(2)?;
            }
            Some("open") => {
                // The rest of the line, so paths may contain spaces.
                let arg = line.trim().strip_prefix("open").unwrap_or("").trim();
                if arg.is_empty() {
                    return Err("open needs a location".into());
                }
                let loc = crate::location(arg);
                self.app.open(crate::playback::OpenRequest {
                    location: loc,
                    ..Default::default()
                });
            }
            Some("shot") => self.save(parts.get(1).copied().unwrap_or("shot"), false)?,
            Some("sbs") => self.save(parts.get(1).copied().unwrap_or("sbs"), true)?,
            Some(c) => return Err(format!("unknown script command {c:?}").into()),
        }
        Ok(false)
    }
}

pub fn run(args: &Args, out: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(out)?;
    let gpu = Arc::new(Gpu::headless()?);
    let renderer = Renderer::new(gpu.clone(), ash::vk::Format::R8G8B8A8_SRGB)?;
    let size = args.size.unwrap_or(1024);
    let eyes = OffscreenEyes::new(&renderer, size, size)?;
    let mut app = crate::make_app()?;
    app.set_about(vec![
        ("Mode".into(), "Preview (no headset)".into()),
        (
            "GPU".into(),
            format!("{} ({})", gpu.device_name, gpu.driver),
        ),
    ]);
    if let Some(o) = &args.open {
        app.open(crate::playback::OpenRequest {
            location: crate::location(o),
            ..Default::default()
        });
    }
    let script = match &args.script {
        Some(p) => std::fs::read_to_string(p)?,
        None => DEFAULT_SCRIPT.to_string(),
    };
    let mut sim = Sim {
        app,
        renderer,
        eyes,
        size,
        head: Quat::IDENTITY,
        hand: Hand {
            active: true,
            aim: None,
            ..Default::default()
        },
        start: Instant::now(),
        last: Instant::now(),
        frames: 0,
        out: out.to_path_buf(),
    };
    sim.hand.aim = Some((sim.hand_pos(), Quat::IDENTITY));
    for (n, line) in script.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        log::info!("script {}: {line}", n + 1);
        match sim.command(line) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                sim.renderer.wait_idle();
                return Err(format!("script line {}: {line}: {e}", n + 1).into());
            }
        }
    }
    println!("{} frames rendered", sim.frames);
    let Sim {
        app,
        renderer,
        mut eyes,
        ..
    } = sim;
    renderer.wait_idle();
    eyes.destroy(&renderer);
    app.shutdown();
    renderer.wait_idle();
    drop(renderer);
    Ok(())
}
