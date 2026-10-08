//! Stereo eye selection, flat/curved screens, picture controls and egui
//! panels, rendered on a real (software) Vulkan device.

use fp_core::format::{Projection, StereoLayout, VideoFormat};
use fp_core::view::ViewSettings;
use fp_render::capture::OffscreenEyes;
use fp_render::math::{Fov, projection, view};
use fp_render::{EyeView, Gpu, QuadDraw, QuadTexture, Renderer, VideoParams};
use glam::{Mat4, Quat, Vec3};
use std::sync::Arc;

const SIZE: u32 = 128;

fn setup() -> Option<(Renderer, OffscreenEyes)> {
    let gpu = match Gpu::headless() {
        Ok(g) => Arc::new(g),
        Err(e) => {
            eprintln!("skipping: no Vulkan device ({e})");
            return None;
        }
    };
    let r = Renderer::new(gpu, ash::vk::Format::R8G8B8A8_SRGB).expect("renderer");
    let eyes = OffscreenEyes::new(&r, SIZE, SIZE).expect("eyes");
    Some((r, eyes))
}

/// Frame whose pixel colour is a function of normalised (u, v).
fn frame(w: u32, h: u32, f: impl Fn(f32, f32) -> [u8; 3]) -> Arc<fp_media::VideoFrame> {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let c = f((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    Arc::new(fp_media::VideoFrame::from_rgba(w, h, &rgba).expect("frame"))
}

fn eyes_at(height: f32) -> [EyeView; 2] {
    let ev = |x: f32| EyeView {
        view: view(Vec3::new(x, height, 0.0), Quat::IDENTITY),
        proj: projection(Fov::symmetric(90.0), 0.05, 100.0),
    };
    [ev(-0.032), ev(0.032)]
}

fn render(
    r: &mut Renderer,
    e: &mut OffscreenEyes,
    f: Option<&Arc<fp_media::VideoFrame>>,
    p: &VideoParams,
    quads: &[QuadDraw],
) -> [Vec<u8>; 2] {
    r.begin_frame().unwrap();
    r.set_video(f).unwrap();
    r.draw(&e.targets(), &eyes_at(1.6), p, quads).unwrap();
    r.end_frame().unwrap();
    [e.read(r, 0).unwrap(), e.read(r, 1).unwrap()]
}

fn px(img: &[u8], x: u32, y: u32) -> [u8; 3] {
    let i = ((y * SIZE + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

fn alpha(img: &[u8], x: u32, y: u32) -> u8 {
    img[((y * SIZE + x) * 4 + 3) as usize]
}

fn centre(img: &[u8]) -> [u8; 3] {
    px(img, SIZE / 2, SIZE / 2)
}

fn is_red(c: [u8; 3]) -> bool {
    c[0] > 200 && c[1] < 60 && c[2] < 60
}
fn is_blue(c: [u8; 3]) -> bool {
    c[2] > 200 && c[0] < 60 && c[1] < 60
}

fn params(projection: Projection, stereo: StereoLayout, settings: ViewSettings) -> VideoParams {
    VideoParams {
        format: VideoFormat::new(projection, stereo),
        settings,
        ..Default::default()
    }
}

#[test]
fn chroma_key_makes_the_green_screen_see_through() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    // Green screen on the left half of a 180° video, a red subject on the right.
    let f = frame(256, 128, |u, _| {
        if u < 0.5 {
            [20, 220, 40]
        } else {
            [230, 20, 20]
        }
    });
    let keyed = ViewSettings {
        chroma_key: true,
        key_color: [0.0, 1.0, 0.0],
        ..ViewSettings::default()
    };
    let mut p = params(Projection::EQUIRECT_180, StereoLayout::Mono, keyed);
    p.background = [0.0; 4];
    let [l, _] = render(&mut r, &mut e, Some(&f), &p, &[]);
    let (green, red) = ((SIZE / 4, SIZE / 2), (3 * SIZE / 4, SIZE / 2));
    assert!(
        alpha(&l, green.0, green.1) < 10,
        "green screen gone: alpha {}",
        alpha(&l, green.0, green.1)
    );
    assert!(
        alpha(&l, red.0, red.1) > 245,
        "subject stays: alpha {}",
        alpha(&l, red.0, red.1)
    );
    assert!(
        is_red(px(&l, red.0, red.1)),
        "subject colour {:?}",
        px(&l, red.0, red.1)
    );

    // Off, the same frame is opaque everywhere.
    p.settings.chroma_key = false;
    let [l, _] = render(&mut r, &mut e, Some(&f), &p, &[]);
    assert!(alpha(&l, green.0, green.1) > 245);
}

#[test]
fn alpha_packed_mask_cuts_out_each_eye() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    // Alpha-packed side-by-side fisheye: a blue picture in each eye, and in the
    // red channel outside the circles each eye's mask scaled to 0.4, the left
    // half's centred on the top middle, the right half's on the corner. The
    // masks keep the right half of each eye (eye u > 0.5): frame x in
    // (0.5, 0.6] and (0, 0.1], at the top and bottom edges.
    let f = frame(512, 256, |u, v| {
        if v < 0.15 || v > 0.85 {
            if (u > 0.5 && u <= 0.6) || u <= 0.1 {
                [255, 0, 0]
            } else {
                [0, 0, 0]
            }
        } else {
            [20, 20, 230]
        }
    });
    let mut p = params(
        Projection::fisheye(180.0),
        StereoLayout::SideBySide,
        ViewSettings::default(),
    );
    p.format.alpha_packed = true;
    p.background = [0.0; 4];
    let (cut, kept) = ((SIZE / 4, SIZE / 2), (3 * SIZE / 4, SIZE / 2));
    let eyes = render(&mut r, &mut e, Some(&f), &p, &[]);
    for (i, img) in eyes.iter().enumerate() {
        assert!(
            alpha(img, cut.0, cut.1) < 10,
            "eye {i}: masked out, alpha {}",
            alpha(img, cut.0, cut.1)
        );
        assert!(
            alpha(img, kept.0, kept.1) > 245,
            "eye {i}: kept, alpha {}",
            alpha(img, kept.0, kept.1)
        );
        assert!(
            is_blue(px(img, kept.0, kept.1)),
            "eye {i}: picture colour {:?}",
            px(img, kept.0, kept.1)
        );
    }

    // Without the flag the mask is ignored.
    p.format.alpha_packed = false;
    let [l, _] = render(&mut r, &mut e, Some(&f), &p, &[]);
    assert!(alpha(&l, cut.0, cut.1) > 245);
}

#[test]
fn side_by_side_and_top_bottom_pick_each_eye() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    let sbs = frame(
        256,
        128,
        |u, _| if u < 0.5 { [255, 0, 0] } else { [0, 0, 255] },
    );
    let tb = frame(
        128,
        256,
        |_, v| if v < 0.5 { [255, 0, 0] } else { [0, 0, 255] },
    );
    let d = ViewSettings::default();

    let [l, rr] = render(
        &mut r,
        &mut e,
        Some(&sbs),
        &params(Projection::EQUIRECT_180, StereoLayout::SideBySide, d),
        &[],
    );
    assert!(
        is_red(centre(&l)) && is_blue(centre(&rr)),
        "SBS: left {:?} right {:?}",
        centre(&l),
        centre(&rr)
    );

    let swapped = ViewSettings {
        swap_eyes: true,
        ..d
    };
    let [l, rr] = render(
        &mut r,
        &mut e,
        Some(&sbs),
        &params(Projection::EQUIRECT_180, StereoLayout::SideBySide, swapped),
        &[],
    );
    assert!(
        is_blue(centre(&l)) && is_red(centre(&rr)),
        "swapped: left {:?} right {:?}",
        centre(&l),
        centre(&rr)
    );

    let [l, rr] = render(
        &mut r,
        &mut e,
        Some(&tb),
        &params(Projection::EQUIRECT_360, StereoLayout::TopBottom, d),
        &[],
    );
    assert!(
        is_red(centre(&l)) && is_blue(centre(&rr)),
        "TB: left {:?} right {:?}",
        centre(&l),
        centre(&rr)
    );

    let [l, rr] = render(
        &mut r,
        &mut e,
        Some(&sbs),
        &params(Projection::EQUIRECT_360, StereoLayout::Mono, d),
        &[],
    );
    assert_eq!(
        centre(&l),
        centre(&rr),
        "mono shows the same image to both eyes"
    );
    e.destroy(&r);
}

#[test]
fn flat_screen_sits_in_front_and_background_surrounds_it() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    let green = frame(160, 90, |_, _| [0, 255, 0]);
    let bg = [0.0, 0.0, 0.0, 1.0];
    let mut p = params(
        Projection::Flat,
        StereoLayout::Mono,
        ViewSettings::default(),
    );
    p.screen_pose = Mat4::from_translation(Vec3::new(0.0, 1.6, -4.0));
    p.background = bg;
    let [l, _] = render(&mut r, &mut e, Some(&green), &p, &[]);
    assert!(centre(&l)[1] > 200, "screen centre {:?}", centre(&l));
    // A 6 m screen at 4 m spans about ±37°: the 90° view's corners are outside.
    assert_eq!(px(&l, 2, 2), [0, 0, 0]);
    // Its 16:9 height is 3.4 m: ±23°, so the top edge (45°) is background too.
    assert_eq!(px(&l, SIZE / 2, 2), [0, 0, 0]);

    // Curved screens still cover the centre and keep the same width.
    p.settings.screen_curvature = 1.0;
    let [l, _] = render(&mut r, &mut e, Some(&green), &p, &[]);
    assert!(centre(&l)[1] > 200);
    assert_eq!(px(&l, 2, 2), [0, 0, 0]);

    // No video: background everywhere.
    let [l, _] = render(&mut r, &mut e, None, &p, &[]);
    assert_eq!(centre(&l), [0, 0, 0]);
    e.destroy(&r);
}

#[test]
fn brightness_and_saturation_controls() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    let grey = frame(64, 64, |_, _| [100, 120, 140]);
    let base = params(
        Projection::EQUIRECT_360,
        StereoLayout::Mono,
        ViewSettings::default(),
    );
    let [a, _] = render(&mut r, &mut e, Some(&grey), &base, &[]);
    let brighter = params(
        Projection::EQUIRECT_360,
        StereoLayout::Mono,
        ViewSettings {
            brightness: 0.2,
            ..Default::default()
        },
    );
    let [b, _] = render(&mut r, &mut e, Some(&grey), &brighter, &[]);
    let mono = params(
        Projection::EQUIRECT_360,
        StereoLayout::Mono,
        ViewSettings {
            saturation: 0.0,
            ..Default::default()
        },
    );
    let [m, _] = render(&mut r, &mut e, Some(&grey), &mono, &[]);
    let (ca, cb, cm) = (centre(&a), centre(&b), centre(&m));
    // The unadjusted picture reproduces the source colour.
    for (got, want) in ca.iter().zip([100u8, 120, 140]) {
        assert!((*got as i32 - want as i32).abs() <= 3, "neutral {ca:?}");
    }
    assert!(
        cb.iter().zip(ca).all(|(x, y)| *x > y + 30),
        "brighter {cb:?} vs {ca:?}"
    );
    assert!(cm[0].abs_diff(cm[2]) <= 2, "desaturated {cm:?}");
    e.destroy(&r);
}

#[test]
fn egui_panel_and_pointer_render_in_the_scene() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    let panel = r.create_panel(512, 256).unwrap();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(2.0);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(256.0, 128.0),
        )),
        ..Default::default()
    };
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("FramePlayer");
            let _ = ui.button("Play");
        });
    });
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let bg = [0.0, 0.0, 0.0, 1.0];
    let mut p = params(
        Projection::Flat,
        StereoLayout::Mono,
        ViewSettings::default(),
    );
    p.background = bg;
    let quad = QuadDraw::panel(
        QuadTexture::Panel(panel),
        Mat4::from_translation(Vec3::new(0.0, 1.6, -1.0)),
        1.0,
        0.5,
        1.0,
    );
    let ray = QuadDraw::line(
        Vec3::new(0.2, 1.3, -0.2),
        Vec3::new(0.0, 1.6, -1.0),
        0.01,
        Vec3::new(0.0, 1.6, 0.0),
        [1.0, 1.0, 1.0, 1.0],
    );

    r.begin_frame().unwrap();
    r.set_video(None).unwrap();
    r.paint_panel(panel, &prims, &out.textures_delta, out.pixels_per_point)
        .unwrap();
    r.draw(&e.targets(), &eyes_at(1.6), &p, &[quad, ray])
        .unwrap();
    r.end_frame().unwrap();
    let img = e.read(&r, 0).unwrap();

    let out_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/render-tests");
    std::fs::create_dir_all(&out_dir).ok();
    let f = std::fs::File::create(out_dir.join("panel.png")).unwrap();
    let mut enc = png::Encoder::new(f, SIZE, SIZE);
    enc.set_color(png::ColorType::Rgba);
    enc.write_header().unwrap().write_image_data(&img).unwrap();

    // The panel covers the centre with egui's dark-grey panel fill and text.
    let c = centre(&img);
    assert!(c != [0, 0, 0], "panel missing at centre");
    let distinct: std::collections::HashSet<[u8; 3]> = (40..88)
        .flat_map(|y| (16..112).map(move |x| (x, y)))
        .map(|(x, y)| px(&img, x, y))
        .collect();
    assert!(
        distinct.len() > 8,
        "panel looks blank: {} colours",
        distinct.len()
    );
    // Outside the panel: background.
    assert_eq!(px(&img, 2, 2), [0, 0, 0]);
    r.destroy_panel(panel);
    e.destroy(&r);
}

#[test]
fn height_moves_the_viewer_against_the_horizon() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    // Sky red, ground blue, the horizon through the middle of the view.
    let f = frame(256, 128, |_, v| {
        if v < 0.5 {
            [230, 20, 20]
        } else {
            [20, 20, 230]
        }
    });
    let at = |height: f32| {
        params(
            Projection::EQUIRECT_360,
            StereoLayout::Mono,
            ViewSettings {
                height,
                ..Default::default()
            },
        )
    };
    // About 3.6° above and below the middle.
    let (up, down) = ((SIZE / 2, SIZE / 2 - 4), (SIZE / 2, SIZE / 2 + 4));
    let [l, _] = render(&mut r, &mut e, Some(&f), &at(0.0), &[]);
    assert!(is_red(px(&l, up.0, up.1)) && is_blue(px(&l, down.0, down.1)));
    // Lower yourself and the ground rises past eye level; raise yourself and
    // the sky sinks below it.
    let [l, _] = render(&mut r, &mut e, Some(&f), &at(-0.5), &[]);
    assert!(
        is_blue(px(&l, up.0, up.1)),
        "lowered {:?}",
        px(&l, up.0, up.1)
    );
    let [l, _] = render(&mut r, &mut e, Some(&f), &at(0.5), &[]);
    assert!(
        is_red(px(&l, down.0, down.1)),
        "raised {:?}",
        px(&l, down.0, down.1)
    );
    e.destroy(&r);
}

#[test]
fn forward_follows_the_way_the_video_was_centred() {
    let Some((mut r, mut e)) = setup() else {
        return;
    };
    // Red left of the video's front, blue right of it.
    let f = frame(256, 128, |u, _| {
        if u < 0.5 {
            [230, 20, 20]
        } else {
            [20, 20, 230]
        }
    });
    // Recentred while facing 90° to the left, as the app does: the anchor's
    // yaw turns the picture and the viewer's forward.
    let yaw = 90.0f32;
    let p = VideoParams {
        forward_yaw: yaw,
        ..params(
            Projection::EQUIRECT_360,
            StereoLayout::Mono,
            ViewSettings {
                yaw,
                forward: 0.8,
                ..Default::default()
            },
        )
    };
    let facing = Quat::from_rotation_y(yaw.to_radians());
    let ev = |x: f32| EyeView {
        view: view(facing * Vec3::new(x, 0.0, 0.0) + Vec3::Y * 1.6, facing),
        proj: projection(Fov::symmetric(90.0), 0.05, 100.0),
    };
    r.begin_frame().unwrap();
    r.set_video(Some(&f)).unwrap();
    r.draw(&e.targets(), &[ev(-0.032), ev(0.032)], &p, &[])
        .unwrap();
    r.end_frame().unwrap();
    let l = e.read(&r, 0).unwrap();
    // Moving straight at the front keeps it in the middle (sideways, it
    // slid about 15°).
    let (left, right) = (
        px(&l, SIZE / 2 - 4, SIZE / 2),
        px(&l, SIZE / 2 + 4, SIZE / 2),
    );
    assert!(is_red(left) && is_blue(right), "{left:?} {right:?}");
    e.destroy(&r);
}
