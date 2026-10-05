//! Settings screen: categories on the left, each a page of grouped rows.
//! Edits [`crate::settings::Settings`] in place; the app notices changes,
//! saves them and applies them to the subsystems.

use super::theme::{self, Weight};
use super::widgets::{self, Kind, Tip};
use super::{Action, SettingsTab, View, icons};
use crate::settings::HapticDeviceConfig;
use egui::{Align2, Color32, RichText, Sense, Vec2};

const TABS: [(SettingsTab, &str, &str); 8] = [
    (SettingsTab::Playback, icons::PLAY_CIRCLE, "Playback"),
    (SettingsTab::Passthrough, icons::EYEGLASSES, "Passthrough"),
    (
        SettingsTab::Controller,
        icons::GAME_CONTROLLER,
        "Controller",
    ),
    (SettingsTab::Library, icons::SQUARES_FOUR, "Library"),
    (SettingsTab::Haptics, icons::VIBRATE, "Haptics"),
    (SettingsTab::Remote, icons::DEVICE_MOBILE, "Remote control"),
    (SettingsTab::Updates, icons::CLOUD_ARROW_DOWN, "Updates"),
    (SettingsTab::About, icons::INFO, "About"),
];

pub fn settings(ui: &mut egui::Ui, v: &mut View) {
    ui.add_space(12.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 28.0;
        ui.vertical(|ui| {
            ui.set_width(240.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            for (t, icon, label) in TABS {
                if side_tab(ui, icon, label, v.state.settings_tab == t).clicked() {
                    v.state.settings_tab = t;
                }
            }
        });
        ui.vertical(|ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                ui.spacing_mut().item_spacing.y = 10.0;
                let title = TABS
                    .iter()
                    .find(|t| t.0 == v.state.settings_tab)
                    .map(|t| t.2)
                    .unwrap_or("");
                ui.label(
                    RichText::new(title)
                        .font(theme::font(Weight::Bold, 30.0))
                        .color(theme::TEXT),
                );
                match v.state.settings_tab {
                    SettingsTab::Playback => playback(ui, v),
                    SettingsTab::Passthrough => passthrough(ui, v),
                    SettingsTab::Controller => controller(ui, v),
                    SettingsTab::Library => library(ui, v),
                    SettingsTab::Haptics => haptics(ui, v),
                    SettingsTab::Remote => remote(ui, v),
                    SettingsTab::Updates => updates(ui, v),
                    SettingsTab::About => about(ui, v),
                }
                ui.add_space(24.0);
            });
        });
    });
}

/// A category in the sidebar: icon and label, filled when selected.
fn side_tab(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let s = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("sel"), selected, 0.12);
        let p = ui.painter();
        let bg = Color32::from_white_alpha((t * 10.0) as u8).lerp_to_gamma(theme::SURFACE_2, s);
        p.rect_filled(rect, egui::CornerRadius::same(12), bg);
        if s > 0.0 {
            let bar = egui::Rect::from_min_size(
                rect.left_center() - Vec2::new(0.0, 11.0 * s),
                Vec2::new(3.0, 22.0 * s),
            );
            p.rect_filled(bar, egui::CornerRadius::same(2), theme::ACCENT);
        }
        let fg = theme::TEXT_2
            .lerp_to_gamma(theme::TEXT, t)
            .lerp_to_gamma(Color32::WHITE, s);
        p.text(
            rect.left_center() + Vec2::new(30.0, 0.0),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(22.0),
            fg.lerp_to_gamma(theme::ACCENT_HOVER, s),
        );
        p.text(
            rect.left_center() + Vec2::new(54.0, 0.0),
            Align2::LEFT_CENTER,
            label,
            theme::font(Weight::SemiBold, 17.0),
            fg,
        );
    }
    resp
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(12.0);
    widgets::section_label(ui, text);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::font(Weight::Regular, 15.0))
            .color(theme::TEXT_3),
    );
}

fn controller(ui: &mut egui::Ui, v: &mut View) {
    use crate::bindings::Bindings;
    super::controller_map::controller_map(ui, v);
    heading(ui, "Always");
    widgets::rows(ui, |r| {
        for (k, d) in [
            (
                "Trigger",
                "Click; on empty space, show or hide the controls",
            ),
            ("Hold grip + trigger", "Drag the picture to move it"),
            ("Both grips", "Recenter"),
            (
                "Pointing at a menu",
                "Thumbstick left / right pages it, up / down scrolls it",
            ),
        ] {
            r.row(k, None, |ui| {
                ui.label(RichText::new(d).color(theme::TEXT_2));
            });
        }
    });
    ui.add_space(6.0);
    let defaults = Bindings::default();
    ui.add_enabled_ui(v.settings.controls != defaults, |ui| {
        if widgets::button(
            ui,
            Some(icons::ARROW_COUNTER_CLOCKWISE),
            "Reset to defaults",
            Kind::Secondary,
        )
        .clicked()
        {
            v.settings.controls = defaults;
            v.state.remap_open = None;
        }
    });
}

fn playback(ui: &mut egui::Ui, v: &mut View) {
    let s = &mut *v.settings;
    heading(ui, "Playing");
    widgets::rows(ui, |r| {
        r.switch(
            "Resume where I left off",
            Some("Videos start again from the last position."),
            &mut s.resume,
        );
        r.switch(
            "Hardware video decoding",
            Some("Recommended: smooth 8K. Turn off only to rule out a decoder problem."),
            &mut s.hardware_decoding,
        );
        let mut step = s.seek_step as f32;
        if r.slider(
            "Thumbstick seek",
            Some("How far one push of the thumbstick jumps."),
            &mut step,
            5.0..=60.0,
            10.0,
            " s",
            0,
        ) {
            s.seek_step = step.round() as f64;
        }
        r.slider(
            "Hide controls after",
            Some("While playing, without moving the pointer."),
            &mut s.auto_hide_secs,
            2.0..=15.0,
            4.0,
            " s",
            0,
        );
    });
    heading(ui, "Sound");
    widgets::rows(ui, |r| {
        r.slider("Volume", None, &mut s.volume, 0.0..=1.5, 1.0, "", 2);
        r.row(
            "Audio device",
            Some("\"default\" follows the system output."),
            |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.audio_device)
                        .margin(egui::Margin::symmetric(12, 9))
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(260.0),
                );
            },
        );
    });
    heading(ui, "Flat videos and subtitles");
    let d = &mut s.default_view;
    widgets::rows(ui, |r| {
        r.slider(
            "Screen distance",
            None,
            &mut d.screen_distance,
            1.0..=20.0,
            4.0,
            " m",
            1,
        );
        r.slider(
            "Screen width",
            None,
            &mut d.screen_width,
            0.5..=30.0,
            6.0,
            " m",
            1,
        );
        r.slider(
            "Screen curvature",
            None,
            &mut d.screen_curvature,
            0.0..=1.0,
            0.0,
            "",
            2,
        );
        r.slider(
            "Subtitle depth",
            Some("How far away subtitles float."),
            &mut d.subtitle_distance,
            0.8..=10.0,
            2.5,
            " m",
            1,
        );
    });
    heading(ui, "Comfort");
    widgets::rows(ui, |r| {
        r.slider(
            "Interface size",
            Some("Text and buttons on every panel."),
            &mut s.ui_scale,
            0.8..=1.5,
            1.0,
            "×",
            2,
        );
    });
}

fn passthrough(ui: &mut egui::Ui, v: &mut View) {
    let available = v.passthrough_available;
    let s = &mut *v.settings;
    heading(ui, "Your room");
    widgets::rows(ui, |r| {
        if available {
            r.switch(
                "Passthrough",
                Some("Show your room around flat videos and the menus."),
                &mut s.passthrough,
            );
        } else {
            r.row(
                "Passthrough",
                Some("This headset's runtime doesn't offer passthrough to apps."),
                |_| {},
            );
        }
    });
    ui.add_space(14.0);
    if !v.unlocked {
        super::player::unlock_card(ui, v.purchase, v.actions, false);
        return;
    }
    super::player::unlocked_header(
        ui,
        "Every video uses these. To change one video, open the sliders button while it plays, then the Passthrough tab.",
        false,
        v.state,
    );
    super::player::chroma_controls(ui, &mut s.default_view, available);
}

fn library(ui: &mut egui::Ui, v: &mut View) {
    heading(ui, "Folders on this device");
    let mut remove = None;
    widgets::rows(ui, |r| {
        for (i, f) in v.settings.library_folders.iter().enumerate() {
            let missing = !f.is_dir();
            r.row(
                &f.display().to_string(),
                missing.then_some("Not found"),
                |ui| {
                    if widgets::icon_button(ui, icons::TRASH, 40.0, false)
                        .tip("Remove")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                },
            );
        }
        r.content(|ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut v.state.new_folder)
                        .margin(egui::Margin::symmetric(12, 9))
                        .hint_text("/run/media/deck/SDCARD/Videos")
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(ui.available_width() - 170.0),
                );
                let p = std::path::PathBuf::from(v.state.new_folder.trim());
                let ok = p.is_absolute() && p.is_dir();
                if ui
                    .add_enabled_ui(ok, |ui| {
                        widgets::button_sized(
                            ui,
                            Some(icons::PLUS),
                            "Add folder",
                            Kind::Secondary,
                            44.0,
                        )
                    })
                    .inner
                    .clicked()
                {
                    v.actions.push(Action::AddFolder(p));
                    v.state.new_folder.clear();
                }
            });
            let suggestions = crate::services::suggested_folders(&v.settings.library_folders);
            if !suggestions.is_empty() {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    for s in suggestions {
                        if widgets::chip_icon(
                            ui,
                            Some(icons::PLUS),
                            &s.display().to_string(),
                            false,
                        )
                        .clicked()
                        {
                            v.actions.push(Action::AddFolder(s));
                        }
                    }
                });
            }
        });
    });
    if let Some(i) = remove {
        v.actions.push(Action::RemoveFolder(i));
    }
    heading(ui, "Drives");
    widgets::rows(ui, |r| {
        let mounts = crate::services::removable_mounts();
        let names: Vec<String> = mounts.iter().map(|m| m.display().to_string()).collect();
        let desc = if names.is_empty() {
            "None plugged in.".to_string()
        } else {
            names.join(", ")
        };
        r.switch(
            "Add videos on microSD cards and USB drives",
            Some(desc.as_str()),
            &mut v.settings.index_removable,
        );
    });
    heading(ui, "Maintenance");
    widgets::rows(ui, |r| {
        r.row(
            "Scan for new videos",
            v.state.scan_status.as_deref(),
            |ui| {
                if widgets::button_sized(
                    ui,
                    Some(icons::ARROWS_CLOCKWISE),
                    "Scan now",
                    Kind::Primary,
                    44.0,
                )
                .clicked()
                {
                    v.actions.push(Action::Rescan { force: false });
                }
            },
        );
        r.row(
            "Rebuild thumbnails",
            Some("Scans everything again."),
            |ui| {
                if widgets::button_sized(ui, None, "Rebuild", Kind::Secondary, 44.0).clicked() {
                    v.actions.push(Action::Rescan { force: true });
                }
            },
        );
        r.row("Retry videos that failed to read", None, |ui| {
            if widgets::button_sized(ui, None, "Retry", Kind::Secondary, 44.0).clicked() {
                v.actions.push(Action::RetryFailed);
            }
        });
        r.row(
            "Clear watch history",
            Some("Positions and play counts."),
            |ui| {
                if widgets::button_sized(ui, None, "Clear", Kind::Danger, 44.0).clicked() {
                    v.actions.push(Action::ClearHistory);
                }
            },
        );
    });
    note(
        ui,
        &format!(
            "Haptic scripts are also looked up in {}",
            fp_core::dirs::interactive_dir().display()
        ),
    );
}

const DEVICE_KINDS: [(&str, &str); 3] = [
    ("Intiface Central", "ws://192.168.1.20:12345"),
    (
        "TCode (OSR2/SR6)",
        "/dev/ttyACM0 or tcp://192.168.1.30:8000",
    ),
    ("The Handy", "connection key"),
];

fn haptics(ui: &mut egui::Ui, v: &mut View) {
    heading(ui, "Devices");
    let configs = v.settings.haptic_devices.clone();
    widgets::rows(ui, |r| {
        for (i, cfg) in configs.iter().enumerate() {
            let status = v.devices.get(i);
            let desc = match status {
                Some(d) if d.connected => format!("Connected · axes {:?}", d.axes),
                Some(d) => d
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "Not connected".into()),
                None => "Not connected".into(),
            };
            r.row(&cfg.label(), Some(desc.as_str()), |ui| {
                if widgets::icon_button(ui, icons::TRASH, 40.0, false)
                    .tip("Remove")
                    .clicked()
                {
                    v.actions.push(Action::RemoveDevice(i));
                }
                let c = if status.is_some_and(|d| d.connected) {
                    theme::OK
                } else {
                    theme::TEXT_3
                };
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 5.0, c);
            });
        }
        r.content(|ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (i, (k, _)) in DEVICE_KINDS.iter().enumerate() {
                    if widgets::chip(ui, k, v.state.new_device_kind == i).clicked() {
                        v.state.new_device_kind = i;
                    }
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let hint = DEVICE_KINDS[v.state.new_device_kind.min(2)].1;
                ui.add(
                    egui::TextEdit::singleline(&mut v.state.new_device)
                        .margin(egui::Margin::symmetric(12, 9))
                        .hint_text(hint)
                        .font(theme::font(Weight::Regular, 17.0))
                        .desired_width(ui.available_width() - 180.0),
                );
                let text = v.state.new_device.trim().to_string();
                if ui
                    .add_enabled_ui(!text.is_empty(), |ui| {
                        widgets::button_sized(
                            ui,
                            Some(icons::PLUS),
                            "Add device",
                            Kind::Secondary,
                            44.0,
                        )
                    })
                    .inner
                    .clicked()
                {
                    let cfg = match v.state.new_device_kind {
                        0 => HapticDeviceConfig::Buttplug {
                            url: if text.contains("://") {
                                text
                            } else {
                                format!("ws://{text}")
                            },
                        },
                        1 => HapticDeviceConfig::Tcode { endpoint: text },
                        _ => HapticDeviceConfig::Handy { key: text },
                    };
                    v.actions.push(Action::AddDevice(cfg));
                    v.state.new_device.clear();
                }
            });
        });
    });
    if !configs.is_empty()
        && widgets::button(
            ui,
            Some(icons::ARROWS_CLOCKWISE),
            "Reconnect all",
            Kind::Secondary,
        )
        .clicked()
    {
        v.actions.push(Action::ReconnectDevices);
    }
    heading(ui, "Timing and range");
    let h = &mut v.settings.haptics;
    widgets::rows(ui, |r| {
        let mut offset = h.offset_ms as f32;
        if r.slider(
            "Timing offset",
            Some("Move strokes earlier (−) or later (+)."),
            &mut offset,
            -500.0..=500.0,
            0.0,
            " ms",
            0,
        ) {
            h.offset_ms = offset.round() as i64;
        }
        r.slider(
            "Speed limit",
            None,
            &mut h.speed_limit,
            0.0..=1000.0,
            fp_haptics::HapticsSettings::default().speed_limit,
            " u/s",
            0,
        );
        r.switch(
            "Strokes as vibration",
            Some("For vibrate-only toys."),
            &mut h.stroke_to_vibration,
        );
    });
}

fn remote(ui: &mut egui::Ui, v: &mut View) {
    let r = &mut v.settings.remote;
    heading(ui, "Remotes");
    widgets::rows(ui, |rows| {
        rows.switch(
            "Phone and browser remote",
            Some("Control playback from your phone: scan the code below."),
            &mut r.web_enabled,
        );
        rows.switch(
            "DeoVR remote API",
            Some("Port 23554, for DeoVR-compatible apps."),
            &mut r.deovr_enabled,
        );
    });
    note(ui, "Both only answer devices on your local network.");
    if let Some(e) = &v.services.remote_error {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
    let Some(hub) = &v.services.remote else {
        return;
    };
    if let Some(a) = hub.deovr_addr() {
        note(
            ui,
            &format!(
                "DeoVR API on port {} · {} client(s)",
                a.port(),
                hub.deovr_client_count()
            ),
        );
    }
    if hub.web_addr().is_some() {
        heading(ui, "Pair a phone");
        let ips = fp_remote::lan_addresses();
        match ips.first() {
            Some(ip) => {
                let url = hub.pairing_url(*ip);
                widgets::card(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::same(16))
                        .show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = 24.0;
                                if let Ok(qr) = fp_remote::pairing_qr(&url) {
                                    draw_qr(ui, &qr, 220.0);
                                }
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new("Scan with your phone's camera")
                                            .font(theme::font(Weight::SemiBold, 20.0)),
                                    );
                                    ui.label(
                                        RichText::new(format!(
                                            "or open {url}. {} phone(s) connected.",
                                            hub.web_client_count()
                                        ))
                                        .color(theme::TEXT_2),
                                    );
                                    ui.add_space(10.0);
                                    if widgets::button(
                                        ui,
                                        Some(icons::ARROWS_CLOCKWISE),
                                        "New pairing code",
                                        Kind::Secondary,
                                    )
                                    .clicked()
                                    {
                                        v.actions.push(Action::RegenerateToken);
                                    }
                                });
                            });
                        });
                });
            }
            None => {
                ui.label(RichText::new("Not connected to a network.").color(theme::WARN));
            }
        }
    }
}

fn draw_qr(ui: &mut egui::Ui, qr: &fp_remote::QrMatrix, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 12.0, Color32::WHITE);
    let quiet = 3.0;
    let cell = size / (qr.size as f32 + quiet * 2.0);
    for y in 0..qr.size {
        for x in 0..qr.size {
            if qr.is_dark(x, y) {
                let min =
                    rect.min + Vec2::new((x as f32 + quiet) * cell, (y as f32 + quiet) * cell);
                p.rect_filled(
                    egui::Rect::from_min_size(min, Vec2::splat(cell + 0.3)),
                    0.0,
                    Color32::BLACK,
                );
            }
        }
    }
}

fn updates(ui: &mut egui::Ui, v: &mut View) {
    use super::UpdateStatus as U;
    heading(ui, &format!("FramePlayer {}", env!("CARGO_PKG_VERSION")));
    widgets::rows(ui, |r| {
        r.switch(
            "Check for updates at start",
            None,
            &mut v.settings.check_updates,
        );
        r.row("Channel", Some("Beta gets new versions first."), |ui| {
            for c in ["beta", "stable"] {
                let label = if c == "beta" { "Beta" } else { "Stable" };
                if widgets::chip(ui, label, v.settings.update_channel == c).clicked() {
                    v.settings.update_channel = c.into();
                }
            }
        });
    });
    if fp_updater::RELEASE_PUBLIC_KEY.is_none() {
        note(
            ui,
            "This is a development build: in-app updates are off. Install new versions \
             with Frame Control or FrameDrop.",
        );
        return;
    }
    ui.add_space(6.0);
    widgets::rows(ui, |r| match &v.state.update {
        U::Unknown => {
            r.row("Not checked yet", None, |ui| check_button(ui, v.actions));
        }
        U::Checking => {
            r.row("Checking…", None, |ui| {
                ui.spinner();
            });
        }
        U::UpToDate => {
            r.row("You have the latest version", None, |ui| {
                check_button(ui, v.actions);
                ui.label(
                    RichText::new(icons::CHECK_CIRCLE)
                        .font(theme::icon(24.0))
                        .color(theme::OK),
                );
            });
        }
        U::Available { version, notes } => {
            r.row(
                &format!("Version {version} is available"),
                Some(notes.as_str()),
                |ui| {
                    if widgets::button(
                        ui,
                        Some(icons::DOWNLOAD_SIMPLE),
                        "Download and install",
                        Kind::Primary,
                    )
                    .clicked()
                    {
                        v.actions.push(Action::InstallUpdate);
                    }
                },
            );
        }
        U::Downloading { done, total } => {
            r.content(|ui| {
                ui.add(
                    egui::ProgressBar::new(if *total > 0 {
                        *done as f32 / *total as f32
                    } else {
                        0.0
                    })
                    .show_percentage(),
                );
            });
        }
        U::Installed { version } => {
            r.row(
                &format!("Version {version} is installed"),
                Some("Restart FramePlayer to use it."),
                |ui| {
                    if widgets::button(ui, None, "Quit now", Kind::Primary).clicked() {
                        v.actions.push(Action::Quit);
                    }
                },
            );
        }
        U::Failed(e) => {
            r.row("The update failed", Some(e.as_str()), |ui| {
                check_button(ui, v.actions)
            });
        }
    });
}

fn check_button(ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    if widgets::button_sized(
        ui,
        Some(icons::ARROWS_CLOCKWISE),
        "Check now",
        Kind::Secondary,
        44.0,
    )
    .clicked()
    {
        actions.push(Action::CheckUpdates);
    }
}

fn about(ui: &mut egui::Ui, v: &mut View) {
    ui.label(
        RichText::new(format!(
            "FramePlayer {} · a native VR video player for the Steam Frame",
            env!("CARGO_PKG_VERSION")
        ))
        .color(theme::TEXT_2),
    );
    heading(ui, "This headset");
    widgets::rows(ui, |r| {
        for (k, val) in &v.state.about {
            r.row(k, None, |ui| {
                ui.label(RichText::new(val).color(theme::TEXT_2));
            });
        }
        r.row("Settings file", None, |ui| {
            ui.label(
                RichText::new(crate::settings::Settings::path().display().to_string())
                    .color(theme::TEXT_2),
            );
        });
        r.row("Log", None, |ui| {
            ui.label(
                RichText::new(crate::logger::log_path().display().to_string()).color(theme::TEXT_2),
            );
        });
    });
    ui.add_space(6.0);
    note(
        ui,
        "Video decoding uses FFmpeg (LGPL 2.1+) and dav1d (BSD), shipped as separate \
         libraries in lib/. Text is set in Inter (OFL) with Phosphor icons (MIT). \
         Licences and the source offer are in the licenses/ folder.",
    );
}
