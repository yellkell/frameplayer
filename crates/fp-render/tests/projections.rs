//! Renders reference images through every projection and checks that each
//! view matches the same view rendered from the equirectangular original.
//! The fisheye and EAC references were produced from that original with
//! FFmpeg's v360 filter, so an exact projection implementation matches it.
//!
//! Needs a Vulkan 1.3 device (lavapipe is enough); skipped otherwise.

use fp_core::format::{Projection, StereoLayout, VideoFormat};
use fp_render::capture::OffscreenEyes;
use fp_render::math::{Fov, projection, view};
use fp_render::{EyeView, Gpu, Renderer, VideoParams};
use glam::{EulerRot, Quat, Vec3};
use std::path::PathBuf;
use std::sync::Arc;

const SIZE: u32 = 192;

struct Rig {
    r: Renderer,
    eyes: OffscreenEyes,
}

fn rig() -> Option<Rig> {
    let gpu = match Gpu::headless() {
        Ok(g) => Arc::new(g),
        Err(e) => {
            eprintln!("skipping: no Vulkan device ({e})");
            return None;
        }
    };
    let r = Renderer::new(gpu, ash::vk::Format::R8G8B8A8_SRGB).expect("renderer");
    let eyes = OffscreenEyes::new(&r, SIZE, SIZE).expect("eyes");
    Some(Rig { r, eyes })
}

fn data(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/render-tests");
    std::fs::create_dir_all(&d).ok();
    d
}

fn load(name: &str) -> Arc<fp_media::VideoFrame> {
    let decoder = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(data(name)).expect("fixture"),
    ));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("png data");
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        other => panic!("unexpected PNG colour type {other:?}"),
    };
    Arc::new(fp_media::VideoFrame::from_rgba(info.width, info.height, &rgba).expect("frame"))
}

fn save(name: &str, rgba: &[u8]) {
    let f = std::fs::File::create(out_dir().join(name)).expect("png");
    let mut e = png::Encoder::new(f, SIZE, SIZE);
    e.set_color(png::ColorType::Rgba);
    e.write_header().unwrap().write_image_data(rgba).unwrap();
}

/// Renders eye `eye` looking at (yaw, pitch) degrees with a 90° field of view.
fn render(
    rig: &mut Rig,
    frame: &Arc<fp_media::VideoFrame>,
    params: &VideoParams,
    yaw: f32,
    pitch: f32,
    eye: usize,
) -> Vec<u8> {
    let q = Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), pitch.to_radians(), 0.0);
    let ev = |x: f32| EyeView {
        view: view(Vec3::new(x, 1.6, 0.0), q),
        proj: projection(Fov::symmetric(90.0), 0.05, 100.0),
    };
    let eyes = [ev(-0.032), ev(0.032)];
    rig.r.begin_frame().unwrap();
    rig.r.set_video(Some(frame)).unwrap();
    rig.r.draw(&rig.eyes.targets(), &eyes, params, &[]).unwrap();
    rig.r.end_frame().unwrap();
    rig.eyes.read(&rig.r, eye).unwrap()
}

/// 4x4 box average, so the comparison measures geometry rather than the
/// resampling detail FFmpeg's v360 filter adds when producing references.
fn coarse(img: &[u8]) -> Vec<f64> {
    let s = SIZE as usize;
    let mut out = Vec::new();
    for by in (0..s).step_by(4) {
        for bx in (0..s).step_by(4) {
            for c in 0..3 {
                let mut sum = 0u32;
                for y in by..by + 4 {
                    for x in bx..bx + 4 {
                        sum += img[(y * s + x) * 4 + c] as u32;
                    }
                }
                out.push(sum as f64 / 16.0);
            }
        }
    }
    out
}

fn mean_diff(a: &[u8], b: &[u8]) -> f64 {
    let (a, b) = (coarse(a), coarse(b));
    a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f64>() / a.len() as f64
}

fn params(projection: Projection, stereo: StereoLayout) -> VideoParams {
    VideoParams {
        format: VideoFormat::new(projection, stereo),
        ..Default::default()
    }
}

/// (name, frame, params, which views the image covers, tolerance)
type Case = (
    &'static str,
    Arc<fp_media::VideoFrame>,
    VideoParams,
    Box<dyn Fn(f32, f32) -> bool>,
    f64,
);

const VIEWS: &[(f32, f32)] = &[
    (0.0, 0.0),
    (20.0, 0.0),
    (-20.0, 15.0),
    (90.0, 0.0),
    (-90.0, 0.0),
    (180.0, 0.0),
    (0.0, 60.0),
    (0.0, -60.0),
    (45.0, 30.0),
    (-135.0, -20.0),
];

#[test]
fn projections_agree_with_equirect() {
    let Some(mut rig) = rig() else { return };
    let eq = load("equirect360.png");
    let eq_p = params(Projection::EQUIRECT_360, StereoLayout::Mono);
    let cases: Vec<Case> = vec![
        (
            "eac360",
            load("eac360.png"),
            params(Projection::Eac { h_fov: 360.0 }, StereoLayout::Mono),
            Box::new(|_, _| true),
            5.5,
        ),
        // The fisheye image covers 200°: views within ~55° of forward are fully inside.
        (
            "fisheye200",
            load("fisheye200.png"),
            params(Projection::fisheye(200.0), StereoLayout::Mono),
            Box::new(|y: f32, p: f32| y.abs() <= 45.0 && p.abs() <= 30.0),
            3.0,
        ),
        // The 180° crop covers yaw ±90°: views up to 45° off-centre are
        // inside, as long as they do not look over a pole (where longitude
        // jumps to the hemisphere the crop removed).
        (
            "equirect180",
            load("equirect180.png"),
            params(Projection::EQUIRECT_180, StereoLayout::Mono),
            Box::new(|y: f32, p: f32| y.abs() <= 25.0 && p.abs() <= 25.0),
            2.0,
        ),
    ];
    let mut failures = Vec::new();
    for &(yaw, pitch) in VIEWS {
        let reference = render(&mut rig, &eq, &eq_p, yaw, pitch, 0);
        save(&format!("equirect360_{yaw}_{pitch}.png"), &reference);
        for (name, frame, p, in_range, tol) in &cases {
            if !in_range(yaw, pitch) {
                continue;
            }
            let img = render(&mut rig, frame, p, yaw, pitch, 0);
            save(&format!("{name}_{yaw}_{pitch}.png"), &img);
            let d = mean_diff(&reference, &img);
            eprintln!("{name:12} yaw {yaw:6} pitch {pitch:5}: mean diff {d:.2}");
            if d > *tol {
                failures.push(format!(
                    "{name} at yaw {yaw} pitch {pitch}: mean diff {d:.2} > {tol}"
                ));
            }
        }
    }
    rig.eyes.destroy(&rig.r);
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn looking_around_finds_the_coloured_markers() {
    let Some(mut rig) = rig() else { return };
    let eq = load("equirect360.png");
    let p = params(Projection::EQUIRECT_360, StereoLayout::Mono);
    let centre = |img: &[u8]| {
        let i = ((SIZE / 2 * SIZE + SIZE / 2) * 4) as usize;
        [img[i], img[i + 1], img[i + 2]]
    };
    // Forward green, left (turning +90° about up) blue, right yellow, up magenta-ish region.
    let fwd = centre(&render(&mut rig, &eq, &p, 0.0, 0.0, 0));
    let left = centre(&render(&mut rig, &eq, &p, 90.0, 0.0, 0));
    let right = centre(&render(&mut rig, &eq, &p, -90.0, 0.0, 0));
    rig.eyes.destroy(&rig.r);
    assert!(
        fwd[1] > 200 && fwd[0] < 120 && fwd[2] < 120,
        "forward should be green: {fwd:?}"
    );
    assert!(
        left[2] > 200 && left[0] < 120,
        "left should be blue: {left:?}"
    );
    assert!(
        right[0] > 200 && right[1] > 200 && right[2] < 120,
        "right should be yellow: {right:?}"
    );
}
