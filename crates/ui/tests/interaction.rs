//! Widget interaction state machine tests driven through simulated frames.

use fp_ui::input::{FrameInput, NavDir, NavInput, PointerInput, PointerSource, TextEvent};
use fp_ui::layout::Dir;
use fp_ui::painter::validate;
use fp_ui::widgets::keyboard::{key_rects, KeyboardState};
use fp_ui::widgets::slider::SliderOpts;
use fp_ui::{Hand, Rect, Ui, UiOutput, Vec2};

const DT: f32 = 1.0 / 72.0;

fn laser(pos: Vec2, pressed: bool) -> PointerInput {
    PointerInput::new(PointerSource::Laser(Hand::Right), Some(pos), pressed)
}

fn input(ptr: Option<PointerInput>) -> FrameInput {
    FrameInput {
        dt: DT,
        pointers: ptr.into_iter().collect(),
        ..Default::default()
    }
}

fn run<R>(ui: &mut Ui, inp: FrameInput, f: impl FnOnce(&mut Ui) -> R) -> (R, UiOutput) {
    ui.begin_frame(inp);
    let r = f(ui);
    let out = ui.end_frame();
    validate(&out.draw_list).unwrap();
    (r, out)
}

fn new_ui() -> Ui {
    let mut ui = Ui::new(Vec2::new(1200.0, 800.0), 1000.0);
    ui.set_record_widgets(true);
    ui
}

const BTN: Rect = Rect::new(100.0, 100.0, 200.0, 60.0);

fn button(ui: &mut Ui) -> bool {
    let id = ui.make_id("btn");
    ui.button_at(BTN, id, "Press me", Default::default())
        .clicked
}

#[test]
fn click_requires_press_and_release_over_widget() {
    let mut ui = new_ui();
    let inside = Vec2::new(150.0, 120.0);
    let (c, out) = run(&mut ui, input(Some(laser(inside, false))), button);
    assert!(!c);
    assert!(out.wants_pointer);
    assert!(
        out.feedback
            .iter()
            .any(|f| matches!(f, fp_ui::Feedback::HoverTick(_))),
        "hover tick for laser"
    );
    let (c, _) = run(&mut ui, input(Some(laser(inside, true))), button);
    assert!(!c, "no click on press");
    let (c, out) = run(&mut ui, input(Some(laser(inside, false))), button);
    assert!(c, "click on release");
    assert!(out
        .feedback
        .iter()
        .any(|f| matches!(f, fp_ui::Feedback::ClickTick(_))));
    // Next frame: no repeat click.
    let (c, _) = run(&mut ui, input(Some(laser(inside, false))), button);
    assert!(!c);
}

#[test]
fn press_outside_or_release_outside_does_not_click() {
    let mut ui = new_ui();
    let inside = Vec2::new(150.0, 120.0);
    let outside = Vec2::new(600.0, 600.0);
    // Press outside, slide in, release inside.
    run(&mut ui, input(Some(laser(outside, true))), button);
    run(&mut ui, input(Some(laser(inside, true))), button);
    let (c, _) = run(&mut ui, input(Some(laser(inside, false))), button);
    assert!(!c);
    // Press inside, slide out, release outside.
    run(&mut ui, input(Some(laser(inside, true))), button);
    run(&mut ui, input(Some(laser(outside, true))), button);
    let (c, _) = run(&mut ui, input(Some(laser(outside, false))), button);
    assert!(!c);
    // Pointer lost mid-press releases capture without clicking.
    run(&mut ui, input(Some(laser(inside, true))), button);
    let (c, _) = run(&mut ui, input(None), button);
    assert!(!c);
    let (c, _) = run(&mut ui, input(Some(laser(inside, false))), button);
    assert!(!c);
}

#[test]
fn capture_blocks_other_widgets() {
    let mut ui = new_ui();
    let a = Rect::new(0.0, 0.0, 100.0, 50.0);
    let b = Rect::new(200.0, 0.0, 100.0, 50.0);
    let two = |ui: &mut Ui| {
        let ia = ui.make_id("a");
        let ib = ui.make_id("b");
        let ra = ui.button_at(a, ia, "A", Default::default());
        let rb = ui.button_at(b, ib, "B", Default::default());
        (ra.clicked, rb.clicked, rb.hovered)
    };
    run(
        &mut ui,
        input(Some(laser(Vec2::new(50.0, 25.0), true))),
        two,
    );
    let ((_, _, b_hover), _) = run(
        &mut ui,
        input(Some(laser(Vec2::new(250.0, 25.0), true))),
        two,
    );
    assert!(!b_hover, "B can't hover while A holds capture");
    let ((ca, cb, _), _) = run(
        &mut ui,
        input(Some(laser(Vec2::new(250.0, 25.0), false))),
        two,
    );
    assert!(!ca && !cb);
}

#[test]
fn gaze_pinch_and_hand_poke_click() {
    let mut ui = new_ui();
    let inside = Vec2::new(150.0, 120.0);
    for src in [
        PointerSource::Gaze,
        PointerSource::HandPoke(Hand::Left),
        PointerSource::HandPinch(Hand::Right),
    ] {
        let p = |pressed| FrameInput {
            dt: DT,
            pointers: vec![PointerInput::new(src, Some(inside), pressed)],
            ..Default::default()
        };
        run(&mut ui, p(false), button);
        run(&mut ui, p(true), button);
        let (c, out) = run(&mut ui, p(false), button);
        assert!(c, "{src:?}");
        assert!(
            !out.feedback
                .iter()
                .any(|f| matches!(f, fp_ui::Feedback::HoverTick(_))),
            "no haptics for {src:?}"
        );
    }
}

#[test]
fn touch_hint_arms_widget() {
    let mut ui = new_ui();
    let mut p = laser(Vec2::new(150.0, 120.0), false);
    p.touch_hint = true;
    let (r, _) = run(&mut ui, input(Some(p)), |ui| {
        let id = ui.make_id("btn");
        ui.button_at(BTN, id, "x", Default::default())
    });
    assert!(r.hovered && r.armed && !r.pressed);
}

fn slider_frame(ui: &mut Ui, v: &mut f32) -> fp_ui::Response {
    let row = Rect::new(0.0, 0.0, 1000.0, 400.0);
    ui.region(row, Dir::Vertical, |ui| {
        ui.slider("Zoom", v, 0.0..=10.0, &SliderOpts::default())
    })
    .0
}

#[test]
fn slider_press_drag_release_changes_value() {
    let mut ui = new_ui();
    let mut v = 5.0;
    run(&mut ui, input(None), |ui| slider_frame(ui, &mut v));
    let w = ui.find_widget("Zoom").expect("slider recorded").rect;
    let y = w.center().y;
    // Click outside: no change.
    run(
        &mut ui,
        input(Some(laser(Vec2::new(500.0, 600.0), true))),
        |ui| slider_frame(ui, &mut v),
    );
    run(
        &mut ui,
        input(Some(laser(Vec2::new(500.0, 600.0), false))),
        |ui| slider_frame(ui, &mut v),
    );
    assert_eq!(v, 5.0);
    // Press near the right end jumps there.
    let (r, _) = run(
        &mut ui,
        input(Some(laser(Vec2::new(w.right() - 1.0, y), true))),
        |ui| slider_frame(ui, &mut v),
    );
    assert!(r.changed && v > 9.5, "{v}");
    // Drag left, beyond the widget: keeps tracking and clamps.
    run(
        &mut ui,
        input(Some(laser(Vec2::new(w.x + w.w * 0.25, y + 300.0), true))),
        |ui| slider_frame(ui, &mut v),
    );
    assert!((v - 2.5).abs() < 0.6, "{v}");
    run(
        &mut ui,
        input(Some(laser(Vec2::new(-500.0, y), true))),
        |ui| slider_frame(ui, &mut v),
    );
    assert_eq!(v, 0.0);
    // Release, then moving the pointer no longer changes it.
    run(
        &mut ui,
        input(Some(laser(Vec2::new(-500.0, y), false))),
        |ui| slider_frame(ui, &mut v),
    );
    run(
        &mut ui,
        input(Some(laser(Vec2::new(w.center().x, y), false))),
        |ui| slider_frame(ui, &mut v),
    );
    assert_eq!(v, 0.0);
}

#[test]
fn focus_navigation_and_activation() {
    let mut ui = new_ui();
    let mut v = 5.0;
    let frame = |ui: &mut Ui, v: &mut f32| {
        let mut clicked = Vec::new();
        ui.region(Rect::new(0.0, 0.0, 600.0, 800.0), Dir::Vertical, |ui| {
            for name in ["One", "Two"] {
                if ui.button(name).clicked {
                    clicked.push(name);
                }
            }
            ui.slider("Level", v, 0.0..=10.0, &SliderOpts::default().step(1.0));
            if ui.button("Three").clicked {
                clicked.push("Three");
            }
        });
        clicked
    };
    let nav = |dir: Option<NavDir>, activate: bool| FrameInput {
        dt: DT,
        nav: NavInput {
            dir,
            activate,
            back: false,
        },
        ..Default::default()
    };
    run(&mut ui, nav(None, false), |ui| frame(ui, &mut v));
    run(&mut ui, nav(Some(NavDir::Down), false), |ui| {
        frame(ui, &mut v)
    }); // focus "One"
    run(&mut ui, nav(Some(NavDir::Down), false), |ui| {
        frame(ui, &mut v)
    }); // "Two"
    let (c, _) = run(&mut ui, nav(None, true), |ui| frame(ui, &mut v));
    assert_eq!(c, vec!["Two"]);
    // Move onto the slider; Left/Right adjust it instead of moving focus.
    run(&mut ui, nav(Some(NavDir::Down), false), |ui| {
        frame(ui, &mut v)
    });
    run(&mut ui, nav(Some(NavDir::Right), false), |ui| {
        frame(ui, &mut v)
    });
    run(&mut ui, nav(Some(NavDir::Right), false), |ui| {
        frame(ui, &mut v)
    });
    assert_eq!(v, 7.0);
    run(&mut ui, nav(Some(NavDir::Left), false), |ui| {
        frame(ui, &mut v)
    });
    assert_eq!(v, 6.0);
    run(&mut ui, nav(Some(NavDir::Down), false), |ui| {
        frame(ui, &mut v)
    });
    let (c, _) = run(&mut ui, nav(None, true), |ui| frame(ui, &mut v));
    assert_eq!(c, vec!["Three"]);
    // Up from the top stays put.
    for _ in 0..5 {
        run(&mut ui, nav(Some(NavDir::Up), false), |ui| {
            frame(ui, &mut v)
        });
    }
    let (c, _) = run(&mut ui, nav(None, true), |ui| frame(ui, &mut v));
    assert_eq!(c, vec!["One"]);
}

#[test]
fn nav_target_prefers_aligned_neighbours() {
    use fp_ui::ui::nav_target;
    use fp_ui::Id;
    let from = Rect::new(100.0, 100.0, 100.0, 50.0);
    let below = (Id(1), Rect::new(100.0, 200.0, 100.0, 50.0));
    let diag = (Id(2), Rect::new(400.0, 170.0, 100.0, 50.0));
    let right = (Id(3), Rect::new(260.0, 100.0, 100.0, 50.0));
    let c = [below, diag, right];
    assert_eq!(nav_target(from, NavDir::Down, &c), Some(Id(1)));
    assert_eq!(nav_target(from, NavDir::Right, &c), Some(Id(3)));
    assert_eq!(nav_target(from, NavDir::Up, &c), None);
    assert_eq!(nav_target(from, NavDir::Left, &c), None);
}

#[test]
fn text_input_receives_events_and_virtual_keyboard_types() {
    let mut ui = new_ui();
    let mut text = String::new();
    let mut kb = KeyboardState::default();
    let kb_area = Rect::new(0.0, 300.0, 1200.0, 400.0);
    let frame = |ui: &mut Ui, text: &mut String, kb: &mut KeyboardState| {
        let field = Rect::new(0.0, 0.0, 600.0, 60.0);
        let id = ui.make_id("field");
        let r = ui.text_input_at(field, id, text, "Type here", None);
        let k = ui
            .region(kb_area, Dir::Vertical, |ui| {
                ui.virtual_keyboard("kb", kb, kb_area.h)
            })
            .0;
        (r, k)
    };
    // Events without focus are dropped.
    let mut inp = input(None);
    inp.text.push(TextEvent::Text("zz".into()));
    run(&mut ui, inp, |ui| frame(ui, &mut text, &mut kb));
    assert!(text.is_empty());
    // Click the field to focus it.
    let p = Vec2::new(100.0, 30.0);
    run(&mut ui, input(Some(laser(p, true))), |ui| {
        frame(ui, &mut text, &mut kb)
    });
    let ((r, _), out) = run(&mut ui, input(Some(laser(p, false))), |ui| {
        frame(ui, &mut text, &mut kb)
    });
    assert!(r.gained_focus && r.has_focus && out.wants_text);
    // Hardware/remote keyboard events.
    let mut inp = input(None);
    inp.text = vec![
        TextEvent::Text("ab".into()),
        TextEvent::Backspace,
        TextEvent::Text("c".into()),
    ];
    let ((r, _), _) = run(&mut ui, inp, |ui| frame(ui, &mut text, &mut kb));
    assert!(r.changed);
    assert_eq!(text, "ac");
    // Tap keys on the virtual keyboard: shift, h, i.
    let gap = ui.theme.spacing * 0.5;
    let keys = key_rects(kb_area, gap, false);
    let find = |label: fp_ui::widgets::keyboard::KeyAction| {
        keys.iter()
            .find(|k| k.3.action == label)
            .unwrap()
            .2
            .center()
    };
    use fp_ui::widgets::keyboard::KeyAction;
    for action in [KeyAction::Shift, KeyAction::Char('h'), KeyAction::Char('i')] {
        let at = find(action);
        run(&mut ui, input(Some(laser(at, true))), |ui| {
            frame(ui, &mut text, &mut kb)
        });
        run(&mut ui, input(Some(laser(at, false))), |ui| {
            frame(ui, &mut text, &mut kb)
        });
    }
    // Keyboard events are applied on the frame after the key (field drawn first).
    run(&mut ui, input(None), |ui| frame(ui, &mut text, &mut kb));
    assert_eq!(text, "acHi");
    // The recorded widget labels match the keys.
    assert!(ui.find_widget("q").is_some());
    // Enter submits.
    let at = find(KeyAction::Enter);
    run(&mut ui, input(Some(laser(at, true))), |ui| {
        frame(ui, &mut text, &mut kb)
    });
    let ((_, k), _) = run(&mut ui, input(Some(laser(at, false))), |ui| {
        frame(ui, &mut text, &mut kb)
    });
    assert_eq!(k.events, vec![TextEvent::Enter]);
    let ((r, _), _) = run(&mut ui, input(None), |ui| frame(ui, &mut text, &mut kb));
    assert!(r.submitted);
}

#[test]
fn backspace_auto_repeats_when_held() {
    let mut ui = new_ui();
    let mut kb = KeyboardState::default();
    let area = Rect::new(0.0, 0.0, 1200.0, 400.0);
    let keys = key_rects(area, ui.theme.spacing * 0.5, false);
    let bs = keys
        .iter()
        .find(|k| k.3.action == fp_ui::widgets::keyboard::KeyAction::Backspace)
        .unwrap()
        .2
        .center();
    let mut events = 0;
    for i in 0..72 {
        let (k, _) = run(&mut ui, input(Some(laser(bs, i < 71))), |ui| {
            ui.region(area, Dir::Vertical, |ui| {
                ui.virtual_keyboard("kb", &mut kb, area.h)
            })
            .0
        });
        events += k.events.len();
    }
    // ~1 s hold: (1.0 - 0.5) / 0.07 ≈ 7 repeats, no extra on release.
    assert!((6..=9).contains(&events), "{events}");
}

fn list(ui: &mut Ui, built: &mut Vec<usize>, clicked: &mut Option<usize>) {
    ui.region(Rect::new(0.0, 0.0, 600.0, 400.0), Dir::Vertical, |ui| {
        ui.virtual_list("list", 1000, 50.0, 400.0, |ui, i, rect| {
            built.push(i);
            let id = ui.make_id("row");
            if ui
                .button_at(rect, id, &format!("Row {i}"), Default::default())
                .clicked
            {
                *clicked = Some(i);
            }
        });
    });
}

#[test]
fn virtual_list_only_builds_visible_rows_and_scrolls() {
    let mut ui = new_ui();
    let (mut built, mut clicked) = (Vec::new(), None);
    run(&mut ui, input(None), |ui| {
        list(ui, &mut built, &mut clicked)
    });
    let spacing = ui.theme.spacing;
    let visible = (400.0 / (50.0 + spacing)).ceil() as usize + 1;
    assert!(built.len() <= visible + 1, "built {} rows", built.len());
    assert_eq!(built[0], 0);

    // Thumbstick down while hovering scrolls.
    let mut p = laser(Vec2::new(300.0, 200.0), false);
    p.scroll = Vec2::new(0.0, -1.0);
    for _ in 0..72 {
        built.clear();
        run(&mut ui, input(Some(p)), |ui| {
            list(ui, &mut built, &mut clicked)
        });
    }
    assert!(built[0] > 5, "first built row {}", built[0]);
    assert!(built.len() <= visible + 2);

    // Scrolling far clamps at the end.
    p.scroll = Vec2::new(0.0, -1.0);
    for _ in 0..2000 {
        run(
            &mut ui,
            FrameInput {
                dt: 0.1,
                pointers: vec![p],
                ..Default::default()
            },
            |ui| list(ui, &mut built, &mut clicked),
        );
    }
    built.clear();
    run(&mut ui, input(None), |ui| {
        list(ui, &mut built, &mut clicked)
    });
    assert_eq!(*built.last().unwrap(), 999);
}

#[test]
fn drag_scroll_cancels_click_and_has_momentum() {
    let mut ui = new_ui();
    let (mut built, mut clicked) = (Vec::new(), None);
    run(&mut ui, input(None), |ui| {
        list(ui, &mut built, &mut clicked)
    });
    // Press on a row and drag upwards past the threshold.
    let mut y = 300.0;
    run(
        &mut ui,
        input(Some(laser(Vec2::new(300.0, y), true))),
        |ui| list(ui, &mut built, &mut clicked),
    );
    for _ in 0..10 {
        y -= 20.0;
        built.clear();
        run(
            &mut ui,
            input(Some(laser(Vec2::new(300.0, y), true))),
            |ui| list(ui, &mut built, &mut clicked),
        );
    }
    let first_after_drag = built[0];
    assert!(first_after_drag >= 1, "content moved with the pointer");
    run(
        &mut ui,
        input(Some(laser(Vec2::new(300.0, y), false))),
        |ui| list(ui, &mut built, &mut clicked),
    );
    assert_eq!(clicked, None, "drag-scroll must not click the row");
    // Momentum carries on after release.
    for _ in 0..30 {
        built.clear();
        run(
            &mut ui,
            input(Some(laser(Vec2::new(300.0, y), false))),
            |ui| list(ui, &mut built, &mut clicked),
        );
    }
    assert!(
        built[0] > first_after_drag,
        "momentum: {} > {}",
        built[0],
        first_after_drag
    );
    // A plain click still works.
    let row_y = 25.0;
    run(
        &mut ui,
        input(Some(laser(Vec2::new(300.0, row_y), true))),
        |ui| list(ui, &mut built, &mut clicked),
    );
    run(
        &mut ui,
        input(Some(laser(Vec2::new(300.0, row_y), false))),
        |ui| list(ui, &mut built, &mut clicked),
    );
    assert!(clicked.is_some());
}

#[test]
fn modal_blocks_input_below() {
    let mut ui = new_ui();
    let frame = |ui: &mut Ui, open: bool| {
        let id = ui.make_id("btn");
        let base = ui.button_at(BTN, id, "Base", Default::default()).clicked;
        let mut ok = false;
        let mut dismissed = false;
        if open {
            let m = ui.modal("dlg", "Delete?", 500.0, |ui| ui.button("OK").clicked);
            ok = m.inner;
            dismissed = m.dismissed;
        }
        (base, ok, dismissed)
    };
    let inside = Vec2::new(150.0, 120.0);
    run(&mut ui, input(None), |ui| frame(ui, true));
    let ((_, _, dismissed), _) = run(&mut ui, input(Some(laser(inside, true))), |ui| {
        frame(ui, true)
    });
    assert!(dismissed, "press on backdrop dismisses");
    let ((base, _, _), _) = run(&mut ui, input(Some(laser(inside, false))), |ui| {
        frame(ui, true)
    });
    assert!(!base, "base button blocked by modal");
    // OK inside the modal works.
    let ok_rect = ui.find_widget("OK").unwrap().rect;
    run(&mut ui, input(Some(laser(ok_rect.center(), true))), |ui| {
        frame(ui, true)
    });
    let ((_, ok, _), _) = run(&mut ui, input(Some(laser(ok_rect.center(), false))), |ui| {
        frame(ui, true)
    });
    assert!(ok);
    // B dismisses.
    let inp = FrameInput {
        dt: DT,
        nav: NavInput {
            back: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let ((_, _, dismissed), _) = run(&mut ui, inp, |ui| frame(ui, true));
    assert!(dismissed);
    // Once closed, the base button works again (after one frame).
    run(&mut ui, input(None), |ui| frame(ui, false));
    run(&mut ui, input(Some(laser(inside, true))), |ui| {
        frame(ui, false)
    });
    let ((base, _, _), _) = run(&mut ui, input(Some(laser(inside, false))), |ui| {
        frame(ui, false)
    });
    assert!(base);
}

#[test]
fn dropdown_open_select_close() {
    let mut ui = new_ui();
    let mut sel = 0usize;
    let opts = ["Small", "Medium", "Large"];
    let frame = |ui: &mut Ui, sel: &mut usize| {
        ui.region(Rect::new(0.0, 0.0, 600.0, 800.0), Dir::Vertical, |ui| {
            if let Some(i) = ui.dropdown("size", "Size", *sel, &opts) {
                *sel = i;
            }
        });
    };
    run(&mut ui, input(None), |ui| frame(ui, &mut sel));
    let header = ui.find_widget("Size").unwrap().rect.center();
    run(&mut ui, input(Some(laser(header, true))), |ui| {
        frame(ui, &mut sel)
    });
    run(&mut ui, input(Some(laser(header, false))), |ui| {
        frame(ui, &mut sel)
    });
    run(&mut ui, input(Some(laser(header, false))), |ui| {
        frame(ui, &mut sel)
    });
    let large = ui
        .find_widget("Large")
        .expect("options visible")
        .rect
        .center();
    run(&mut ui, input(Some(laser(large, true))), |ui| {
        frame(ui, &mut sel)
    });
    run(&mut ui, input(Some(laser(large, false))), |ui| {
        frame(ui, &mut sel)
    });
    assert_eq!(sel, 2);
    run(&mut ui, input(None), |ui| frame(ui, &mut sel));
    assert!(ui.find_widget("Medium").is_none(), "closed after selection");
}

#[test]
fn gaze_dimming_reaches_output_opacity() {
    let mut ui = new_ui();
    let mut last = 1.0;
    for _ in 0..(72 * 6) {
        let inp = FrameInput {
            dt: DT,
            gaze_on_panel: Some(false),
            ..Default::default()
        };
        let (_, out) = run(&mut ui, inp, button);
        last = out.opacity;
    }
    assert!(last < 0.3, "{last}");
    let inp = FrameInput {
        dt: DT,
        gaze_on_panel: Some(false),
        ..Default::default()
    };
    ui.begin_frame(inp);
    button(&mut ui);
    let out = ui.end_frame();
    let max_alpha = out
        .draw_list
        .vertices
        .iter()
        .map(|v| v.color[3])
        .fold(0.0, f32::max);
    assert!(max_alpha <= last + 0.01, "opacity baked into vertices");
    // Looking back restores it.
    for _ in 0..30 {
        let inp = FrameInput {
            dt: DT,
            gaze_on_panel: Some(true),
            ..Default::default()
        };
        last = run(&mut ui, inp, button).1.opacity;
    }
    assert!(last > 0.95);
}

#[test]
fn toasts_render_and_expire() {
    let mut ui = new_ui();
    ui.toast("Saved", fp_ui::ToastKind::Success, 1.0);
    let (_, out) = run(&mut ui, input(None), |_| ());
    let (_, out2) = run(&mut ui, input(None), |_| ());
    assert!(out2.draw_list.indices.len() >= out.draw_list.indices.len());
    assert!(!out2.draw_list.indices.is_empty());
    for _ in 0..100 {
        run(&mut ui, input(None), |_| ());
    }
    let (_, done) = run(&mut ui, input(None), |_| ());
    assert!(done.draw_list.indices.is_empty());
}
