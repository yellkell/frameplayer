//! Settings screen. Edits [`crate::settings::Settings`] in place; the app
//! notices changes, saves them and applies them to the subsystems.

use super::{Action, SettingsTab, View, big_button, slider_row, theme};
use crate::settings::HapticDeviceConfig;
use egui::{Color32, RichText, Sense, Vec2};

pub fn settings(ui: &mut egui::Ui, v: &mut View) {
    ui.horizontal(|ui| {
        for (t, label) in [
            (SettingsTab::Playback, "Playback"),
            (SettingsTab::Library, "Library"),
            (SettingsTab::Haptics, "Haptics"),
            (SettingsTab::Remote, "Remote control"),
            (SettingsTab::Updates, "Updates"),
            (SettingsTab::About, "About"),
        ] {
            if ui
                .selectable_label(v.state.settings_tab == t, RichText::new(label).size(20.0))
                .clicked()
            {
                v.state.settings_tab = t;
            }
        }
    });
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| match v.state.settings_tab {
        SettingsTab::Playback => playback(ui, v),
        SettingsTab::Library => library(ui, v),
        SettingsTab::Haptics => haptics(ui, v),
        SettingsTab::Remote => remote(ui, v),
        SettingsTab::Updates => updates(ui, v),
        SettingsTab::About => about(ui, v),
    });
}

fn playback(ui: &mut egui::Ui, v: &mut View) {
    let s = &mut *v.settings;
    ui.checkbox(&mut s.resume, "Resume videos where I left off");
    ui.checkbox(
        &mut s.hardware_decoding,
        "Hardware video decoding (recommended)",
    );
    let mut step = s.seek_step as f32;
    if slider_row(ui, "Thumbstick seek", &mut step, 5.0..=60.0, 10.0, " s") {
        s.seek_step = step.round() as f64;
    }
    slider_row(
        ui,
        "Hide controls after",
        &mut s.auto_hide_secs,
        2.0..=15.0,
        4.0,
        " s",
    );
    slider_row(ui, "Volume", &mut s.volume, 0.0..=1.5, 1.0, "");
    ui.horizontal(|ui| {
        ui.add_sized([170.0, 36.0], egui::Label::new("Audio device"));
        ui.add(egui::TextEdit::singleline(&mut s.audio_device).desired_width(280.0));
        ui.label(
            RichText::new("\"default\" follows the system output")
                .small()
                .color(theme::MUTED),
        );
    });
    ui.add_space(8.0);
    ui.label(RichText::new("Defaults for new videos").strong());
    let d = &mut s.default_view;
    slider_row(
        ui,
        "Flat screen distance",
        &mut d.screen_distance,
        1.0..=20.0,
        4.0,
        " m",
    );
    slider_row(
        ui,
        "Flat screen width",
        &mut d.screen_width,
        0.5..=30.0,
        6.0,
        " m",
    );
    slider_row(
        ui,
        "Screen curvature",
        &mut d.screen_curvature,
        0.0..=1.0,
        0.0,
        "",
    );
    slider_row(
        ui,
        "Subtitle depth",
        &mut d.subtitle_distance,
        0.8..=10.0,
        2.5,
        " m",
    );
    ui.add_space(8.0);
    ui.label(RichText::new("Comfort").strong());
    slider_row(ui, "Interface size", &mut s.ui_scale, 0.8..=1.5, 1.0, "×");
    if v.passthrough_available {
        ui.checkbox(
            &mut s.passthrough,
            "Show my room around flat videos and menus (passthrough)",
        );
    } else {
        ui.label(
            RichText::new("Passthrough is not offered by this headset runtime.")
                .color(theme::MUTED),
        );
    }
}

fn library(ui: &mut egui::Ui, v: &mut View) {
    ui.label(RichText::new("Folders on this device").strong());
    let mut remove = None;
    for (i, f) in v.settings.library_folders.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(f.display().to_string()).monospace());
            if !f.is_dir() {
                ui.label(RichText::new("not found").color(theme::WARN));
            }
            if ui.small_button("Remove").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        v.actions.push(Action::RemoveFolder(i));
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut v.state.new_folder)
                .hint_text("/run/media/deck/SDCARD/Videos")
                .desired_width(420.0),
        );
        let p = std::path::PathBuf::from(v.state.new_folder.trim());
        if ui
            .add_enabled(
                p.is_absolute() && p.is_dir(),
                egui::Button::new("Add folder"),
            )
            .clicked()
        {
            v.actions.push(Action::AddFolder(p));
            v.state.new_folder.clear();
        }
    });
    ui.checkbox(
        &mut v.settings.index_removable,
        "Add videos on microSD cards and USB drives automatically",
    );
    let mounts = crate::services::removable_mounts();
    if !mounts.is_empty() {
        let names: Vec<String> = mounts.iter().map(|m| m.display().to_string()).collect();
        ui.label(
            RichText::new(format!("Drives: {}", names.join(", ")))
                .small()
                .color(theme::MUTED),
        );
    }
    let suggestions = crate::services::suggested_folders(&v.settings.library_folders);
    if !suggestions.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Found:").color(theme::MUTED));
            for s in suggestions {
                if ui.button(format!("+ {}", s.display())).clicked() {
                    v.actions.push(Action::AddFolder(s));
                }
            }
        });
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if big_button(ui, "Scan now", false).clicked() {
            v.actions.push(Action::Rescan { force: false });
        }
        if ui.button("Rebuild thumbnails").clicked() {
            v.actions.push(Action::Rescan { force: true });
        }
        if ui.button("Retry failed videos").clicked() {
            v.actions.push(Action::RetryFailed);
        }
        if ui.button("Clear watch history").clicked() {
            v.actions.push(Action::ClearHistory);
        }
    });
    if let Some(s) = &v.state.scan_status {
        ui.label(RichText::new(s).color(theme::MUTED));
    }
    ui.add_space(6.0);
    ui.label(
        RichText::new(format!(
            "Haptic scripts are also looked up in {}",
            fp_core::dirs::interactive_dir().display()
        ))
        .small()
        .color(theme::MUTED),
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
    ui.label(RichText::new("Devices").strong());
    let configs = v.settings.haptic_devices.clone();
    for (i, cfg) in configs.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(cfg.label());
            if ui.small_button("Remove").clicked() {
                v.actions.push(Action::RemoveDevice(i));
            }
        });
    }
    for d in v.devices {
        let (c, s) = if d.connected {
            (theme::OK, "connected")
        } else {
            (theme::ERROR, "not connected")
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚡").color(c));
            ui.label(format!("{} — {s} · axes {:?}", d.name, d.axes));
        });
        if let Some(n) = &d.notice {
            ui.label(RichText::new(n).small().color(theme::MUTED));
        }
        if let Some(e) = &d.last_error {
            ui.label(RichText::new(e).small().color(theme::ERROR));
        }
    }
    if !configs.is_empty() && ui.button("Reconnect all").clicked() {
        v.actions.push(Action::ReconnectDevices);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for (i, (k, _)) in DEVICE_KINDS.iter().enumerate() {
            if ui
                .selectable_label(v.state.new_device_kind == i, *k)
                .clicked()
            {
                v.state.new_device_kind = i;
            }
        }
    });
    ui.horizontal(|ui| {
        let hint = DEVICE_KINDS[v.state.new_device_kind.min(2)].1;
        ui.add(
            egui::TextEdit::singleline(&mut v.state.new_device)
                .hint_text(hint)
                .desired_width(420.0),
        );
        let text = v.state.new_device.trim().to_string();
        if ui
            .add_enabled(!text.is_empty(), egui::Button::new("Add device"))
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
    ui.add_space(8.0);
    ui.label(RichText::new("Timing and range").strong());
    let h = &mut v.settings.haptics;
    let mut offset = h.offset_ms as f32;
    if slider_row(ui, "Timing offset", &mut offset, -500.0..=500.0, 0.0, " ms") {
        h.offset_ms = offset.round() as i64;
    }
    slider_row(
        ui,
        "Speed limit",
        &mut h.speed_limit,
        0.0..=1000.0,
        fp_haptics::HapticsSettings::default().speed_limit,
        " u/s",
    );
    ui.checkbox(
        &mut h.stroke_to_vibration,
        "Turn strokes into vibration on vibrate-only toys",
    );
}

fn remote(ui: &mut egui::Ui, v: &mut View) {
    let r = &mut v.settings.remote;
    ui.checkbox(
        &mut r.web_enabled,
        "Phone / browser remote (scan the code below)",
    );
    ui.checkbox(
        &mut r.deovr_enabled,
        "DeoVR remote API (port 23554, for DeoVR-compatible apps)",
    );
    ui.label(
        RichText::new("Both only answer devices on your local network.")
            .small()
            .color(theme::MUTED),
    );
    if let Some(e) = &v.services.remote_error {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
    let Some(hub) = &v.services.remote else {
        return;
    };
    if let Some(a) = hub.deovr_addr() {
        ui.label(format!(
            "DeoVR API on port {} · {} client(s)",
            a.port(),
            hub.deovr_client_count()
        ));
    }
    if hub.web_addr().is_some() {
        let ips = fp_remote::lan_addresses();
        match ips.first() {
            Some(ip) => {
                let url = hub.pairing_url(*ip);
                ui.label(format!(
                    "{} phone(s) connected. Open on your phone:",
                    hub.web_client_count()
                ));
                ui.label(RichText::new(&url).monospace());
                if let Ok(qr) = fp_remote::pairing_qr(&url) {
                    draw_qr(ui, &qr, 260.0);
                }
            }
            None => {
                ui.label(RichText::new("Not connected to a network.").color(theme::WARN));
            }
        }
        if ui
            .button("New pairing code")
            .on_hover_text("Disconnects paired phones")
            .clicked()
        {
            v.actions.push(Action::RegenerateToken);
        }
    }
}

fn draw_qr(ui: &mut egui::Ui, qr: &fp_remote::QrMatrix, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 8.0, Color32::WHITE);
    let quiet = 4.0;
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
    ui.label(format!("FramePlayer {}", env!("CARGO_PKG_VERSION")));
    ui.checkbox(
        &mut v.settings.check_updates,
        "Check for updates when FramePlayer starts",
    );
    ui.horizontal(|ui| {
        ui.label("Channel");
        for c in ["stable", "beta"] {
            if ui
                .selectable_label(v.settings.update_channel == c, c)
                .clicked()
            {
                v.settings.update_channel = c.into();
            }
        }
    });
    if fp_updater::RELEASE_PUBLIC_KEY.is_none() {
        ui.label(RichText::new("This is a development build: in-app updates are off. Install new versions with Frame Control or FrameDrop.").color(theme::WARN));
        return;
    }
    match &v.state.update {
        U::Unknown => {}
        U::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Checking…");
            });
        }
        U::UpToDate => {
            ui.label(RichText::new("You have the latest version.").color(theme::OK));
        }
        U::Available { version, notes } => {
            ui.label(RichText::new(format!("Version {version} is available")).strong());
            ui.label(notes);
            if big_button(ui, "Download and install", false).clicked() {
                v.actions.push(Action::InstallUpdate);
            }
        }
        U::Downloading { done, total } => {
            ui.add(
                egui::ProgressBar::new(if *total > 0 {
                    *done as f32 / *total as f32
                } else {
                    0.0
                })
                .show_percentage(),
            );
        }
        U::Installed { version } => {
            ui.label(
                RichText::new(format!(
                    "Version {version} is installed. Restart FramePlayer to use it."
                ))
                .color(theme::OK),
            );
            if ui.button("Quit now").clicked() {
                v.actions.push(Action::Quit);
            }
        }
        U::Failed(e) => {
            ui.label(RichText::new(e).color(theme::ERROR));
        }
    }
    if !matches!(v.state.update, U::Checking | U::Downloading { .. })
        && ui.button("Check now").clicked()
    {
        v.actions.push(Action::CheckUpdates);
    }
}

fn about(ui: &mut egui::Ui, v: &mut View) {
    ui.label(RichText::new(format!("FramePlayer {}", env!("CARGO_PKG_VERSION"))).heading());
    ui.label("A native VR video player for the Steam Frame.");
    ui.add_space(6.0);
    egui::Grid::new("about")
        .num_columns(2)
        .spacing([20.0, 6.0])
        .show(ui, |ui| {
            for (k, val) in &v.state.about {
                ui.label(RichText::new(k).color(theme::MUTED));
                ui.label(val);
                ui.end_row();
            }
            ui.label(RichText::new("Settings").color(theme::MUTED));
            ui.label(crate::settings::Settings::path().display().to_string());
            ui.end_row();
            ui.label(RichText::new("Log").color(theme::MUTED));
            ui.label(crate::logger::log_path().display().to_string());
            ui.end_row();
        });
    ui.add_space(8.0);
    ui.label(RichText::new("Video decoding uses FFmpeg (LGPL 2.1+) and dav1d (BSD), shipped as separate libraries in lib/. Their licences and source offer are in the licenses/ folder.").small().color(theme::MUTED));
    ui.add_space(8.0);
    ui.label(RichText::new("Controller").strong());
    for (k, d) in [
        (
            "Trigger",
            "Click buttons; click empty space to show or hide controls",
        ),
        ("A / X", "Play or pause"),
        ("B / Y", "Show or hide controls"),
        ("Thumbstick left/right", "Seek (not over a menu)"),
        ("Thumbstick up/down", "Scroll menus; volume during playback"),
        ("Both grips", "Recenter the view"),
        ("Menu", "Library"),
    ] {
        ui.horizontal(|ui| {
            ui.add_sized([230.0, 28.0], egui::Label::new(RichText::new(k).strong()));
            ui.label(d);
        });
    }
}
