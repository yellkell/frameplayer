//! Settings: general/comfort, playback, remote API (with pairing QR),
//! haptics devices and updates.

use super::{panel_background, UiAction};
use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::layout::{Dir, FILL};
use crate::text::{Align, TextParams};
use crate::theme::Color;
use crate::ui::Ui;
use crate::widgets::basic::ButtonStyle;
use crate::widgets::slider::SliderOpts;

#[derive(Debug, Clone, PartialEq)]
pub struct GeneralSettings {
    /// 0 = runtime default.
    pub refresh_rate_hz: f32,
    /// Rates the runtime offers (from the XR layer).
    pub available_refresh_rates: Vec<f32>,
    pub gaze_dimming: bool,
    pub passthrough_background: bool,
    pub head_locked_screen: bool,
    pub lying_down: bool,
    pub environment_dim: f32,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        GeneralSettings {
            refresh_rate_hz: 0.0,
            available_refresh_rates: Vec::new(),
            gaze_dimming: true,
            passthrough_background: false,
            head_locked_screen: false,
            lying_down: false,
            environment_dim: 0.2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackSettings {
    pub default_speed: f32,
    pub resume_playback: bool,
    pub seek_step_s: f32,
    pub subtitle_depth_m: f32,
    pub av_offset_ms: i32,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        PlaybackSettings {
            default_speed: 1.0,
            resume_playback: true,
            seek_step_s: 10.0,
            subtitle_depth_m: 2.0,
            av_offset_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RemoteSettings {
    pub api_enabled: bool,
    pub api_port: u16,
    pub deovr_enabled: bool,
    pub deovr_port: u16,
    /// LAN pairing URL (with token) shown as a QR code when the API is on.
    pub pairing_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HapticsDevice {
    pub id: String,
    pub name: String,
    /// "handy", "buttplug", "tcode".
    pub backend: String,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HapticsSettings {
    pub enabled: bool,
    pub offset_ms: i32,
    pub devices: Vec<HapticsDevice>,
    pub scanning: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateSettings {
    pub current_version: String,
    pub beta_channel: bool,
    pub check_on_start: bool,
    /// Newer version available.
    pub available: Option<String>,
    /// Download/install progress 0..1.
    pub progress: Option<f32>,
    pub status: Option<String>,
}

/// Settings view-model; mirrors the app config plus runtime status.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsModel {
    pub general: GeneralSettings,
    pub playback: PlaybackSettings,
    pub remote: RemoteSettings,
    pub haptics: HapticsSettings,
    pub updates: UpdateSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    General,
    Playback,
    Remote,
    Haptics,
    Updates,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::General,
        SettingsTab::Playback,
        SettingsTab::Remote,
        SettingsTab::Haptics,
        SettingsTab::Updates,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Playback => "Playback",
            SettingsTab::Remote => "Remote",
            SettingsTab::Haptics => "Haptics",
            SettingsTab::Updates => "Updates",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsScreen {
    pub tab: SettingsTab,
}

impl SettingsScreen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn show(&mut self, ui: &mut Ui, model: &SettingsModel) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let screen = panel_background(ui);
        let t = ui.theme.clone();
        let mut m = model.clone();
        ui.region(screen.shrink(t.padding * 1.5), Dir::Vertical, |ui| {
            ui.row(t.widget_height, |ui| {
                if ui.icon_button("back", Icon::Back).clicked {
                    actions.push(UiAction::Back);
                }
                ui.add_space(t.spacing);
                ui.label_styled("Settings", t.title_text_size, t.text);
            });
            let mut tab = SettingsTab::ALL
                .iter()
                .position(|x| *x == self.tab)
                .unwrap_or(0);
            let labels: Vec<&str> = SettingsTab::ALL.iter().map(|x| x.label()).collect();
            if ui.tabs("tabs", &mut tab, &labels) {
                self.tab = SettingsTab::ALL[tab];
            }
            let tab = self.tab;
            ui.scroll_area(("settings", tab as u8), FILL, |ui| match tab {
                SettingsTab::General => general(ui, &mut m.general),
                SettingsTab::Playback => playback(ui, &mut m.playback),
                SettingsTab::Remote => remote(ui, &mut m.remote, &mut actions),
                SettingsTab::Haptics => haptics(ui, &mut m.haptics, &mut actions),
                SettingsTab::Updates => updates(ui, &mut m.updates, &mut actions),
            });
        });
        if m != *model {
            actions.push(UiAction::SettingsChanged(m));
        }
        if ui.take_back() {
            actions.push(UiAction::Back);
        }
        actions
    }
}

fn general(ui: &mut Ui, g: &mut GeneralSettings) {
    if !g.available_refresh_rates.is_empty() {
        let mut opts = vec!["Default".to_string()];
        opts.extend(
            g.available_refresh_rates
                .iter()
                .map(|r| format!("{} Hz", r.round() as i32)),
        );
        let cur = g
            .available_refresh_rates
            .iter()
            .position(|r| (r - g.refresh_rate_hz).abs() < 0.5)
            .map(|i| i + 1)
            .unwrap_or(0);
        if let Some(i) = ui.dropdown("refresh", "Refresh rate", cur, &opts) {
            g.refresh_rate_hz = if i == 0 {
                0.0
            } else {
                g.available_refresh_rates[i - 1]
            };
        }
    }
    ui.toggle("Dim UI when not looking at it", &mut g.gaze_dimming);
    ui.toggle("Passthrough background", &mut g.passthrough_background);
    ui.toggle("Head-locked screen", &mut g.head_locked_screen);
    ui.toggle("Lying-down mode", &mut g.lying_down);
    ui.slider(
        "Environment",
        &mut g.environment_dim,
        0.0..=1.0,
        &SliderOpts::default().step(0.05),
    );
}

fn playback(ui: &mut Ui, p: &mut PlaybackSettings) {
    ui.toggle("Resume where I left off", &mut p.resume_playback);
    ui.slider(
        "Default speed",
        &mut p.default_speed,
        0.25..=4.0,
        &SliderOpts::default().step(0.25).suffix("×"),
    );
    ui.slider(
        "Seek step",
        &mut p.seek_step_s,
        1.0..=60.0,
        &SliderOpts::default().step(1.0).decimals(0).suffix(" s"),
    );
    ui.slider(
        "Subtitle depth",
        &mut p.subtitle_depth_m,
        0.5..=10.0,
        &SliderOpts::default().step(0.1).decimals(1).suffix(" m"),
    );
    let mut off = p.av_offset_ms as f32;
    if ui
        .slider(
            "A/V offset",
            &mut off,
            -500.0..=500.0,
            &SliderOpts::default().step(10.0).decimals(0).suffix(" ms"),
        )
        .changed
    {
        p.av_offset_ms = off.round() as i32;
    }
}

fn remote(ui: &mut Ui, r: &mut RemoteSettings, actions: &mut Vec<UiAction>) {
    let t = ui.theme.clone();
    ui.toggle("Web remote & API (LAN only)", &mut r.api_enabled);
    ui.toggle("DeoVR-compatible remote", &mut r.deovr_enabled);
    if r.api_enabled || r.deovr_enabled {
        let ports = match (r.api_enabled, r.deovr_enabled) {
            (true, true) => format!("API port {} · DeoVR port {}", r.api_port, r.deovr_port),
            (true, false) => format!("API port {}", r.api_port),
            _ => format!("DeoVR port {}", r.deovr_port),
        };
        ui.label_dim(&ports);
    }
    if r.api_enabled {
        let size = (t.widget_height * 5.0).min(ui.available().w * 0.5);
        ui.horizontal(|ui| {
            match &r.pairing_url {
                Some(url) => {
                    ui.qr_code(url, size);
                }
                None => {
                    // Placeholder while the server starts.
                    let rect = ui.allocate(Vec2::splat(size));
                    ui.painter().rect_rounded(rect, t.corner_radius, t.surface);
                    ui.draw_spinner(Rect::from_center(rect.center(), Vec2::splat(size * 0.25)));
                }
            }
            ui.vertical(|ui| {
                ui.label("Scan with your phone to pair the web remote.");
                if let Some(url) = &r.pairing_url {
                    ui.label_dim(url);
                }
                if ui
                    .button_ex(
                        "New token",
                        ButtonStyle {
                            icon: Some(Icon::Refresh),
                            ..Default::default()
                        },
                    )
                    .clicked
                {
                    actions.push(UiAction::RegenerateApiToken);
                }
            });
        });
    }
}

fn haptics(ui: &mut Ui, h: &mut HapticsSettings, actions: &mut Vec<UiAction>) {
    let t = ui.theme.clone();
    ui.toggle("Enable haptics", &mut h.enabled);
    let mut off = h.offset_ms as f32;
    if ui
        .slider(
            "Script offset",
            &mut off,
            -500.0..=500.0,
            &SliderOpts::default().step(10.0).decimals(0).suffix(" ms"),
        )
        .changed
    {
        h.offset_ms = off.round() as i32;
    }
    ui.horizontal(|ui| {
        ui.label_styled("Devices", t.title_text_size * 0.8, t.text);
        ui.add_space(t.spacing);
        let label = if h.scanning { "Scanning…" } else { "Scan" };
        if ui
            .button_ex(
                label,
                ButtonStyle {
                    icon: Some(Icon::Refresh),
                    ..Default::default()
                },
            )
            .clicked
            && !h.scanning
        {
            actions.push(UiAction::ScanHapticsDevices);
        }
        if h.scanning {
            ui.spinner(t.widget_height * 0.8);
        }
    });
    if h.devices.is_empty() {
        ui.label_dim(
            "No devices found. Start Intiface Central, connect a Handy, or plug in an OSR/SR6.",
        );
    }
    for d in &h.devices {
        ui.scope(("dev", &d.id), |ui| {
            let row = ui.allocate_row(t.widget_height);
            ui.painter()
                .rect_rounded(row, t.corner_radius, t.surface.alpha(0.6));
            let is = t.icon_size * 0.8;
            let color = if d.connected { t.success } else { t.text_dim };
            draw_icon(
                ui.painter(),
                Icon::Haptics,
                Rect::new(row.x + t.padding, row.center().y - is * 0.5, is, is),
                color,
            );
            let btn_w = ui.fonts.measure("Disconnect", t.text_size) + t.padding * 2.0;
            let label = format!("{}  ·  {}", d.name, d.backend);
            let text_x = row.x + t.padding * 2.0 + is;
            ui.draw_text_in(
                Rect::new(
                    text_x,
                    row.y,
                    row.right() - text_x - btn_w - t.padding,
                    row.h,
                ),
                &label,
                t.text_size,
                t.text,
                Align::Left,
            );
            let br = Rect::new(
                row.right() - btn_w - t.spacing * 0.5,
                row.y + 4.0,
                btn_w,
                row.h - 8.0,
            );
            let id = ui.make_id("connect");
            let (txt, style) = if d.connected {
                ("Disconnect", ButtonStyle::default())
            } else {
                (
                    "Connect",
                    ButtonStyle {
                        primary: true,
                        ..Default::default()
                    },
                )
            };
            if ui.button_at(br, id, txt, style).clicked {
                actions.push(if d.connected {
                    UiAction::DisconnectHapticsDevice(d.id.clone())
                } else {
                    UiAction::ConnectHapticsDevice(d.id.clone())
                });
            }
        });
    }
}

fn updates(ui: &mut Ui, u: &mut UpdateSettings, actions: &mut Vec<UiAction>) {
    let t = ui.theme.clone();
    ui.label(&format!("FramePlayer {}", u.current_version));
    ui.toggle("Check for updates on start", &mut u.check_on_start);
    ui.toggle("Beta channel", &mut u.beta_channel);
    if let Some(p) = u.progress {
        ui.label_dim("Downloading update…");
        ui.progress_bar(p);
    } else if let Some(v) = &u.available {
        let v = v.clone();
        let rect = ui.allocate_row(t.widget_height * 1.4);
        ui.painter()
            .rect_rounded(rect, t.corner_radius, t.accent.alpha(0.2));
        let layout = ui.fonts.layout(
            &format!("Version {v} is available"),
            TextParams::new(t.text_size),
        );
        ui.draw_text_at(
            Vec2::new(rect.x + t.padding, rect.center().y - layout.size.y * 0.5),
            &layout,
            t.text,
        );
        let bw = ui.fonts.measure("Install", t.text_size) + t.padding * 2.0 + t.icon_size;
        let br = Rect::new(
            rect.right() - bw - t.spacing,
            rect.y + t.spacing * 0.5,
            bw,
            rect.h - t.spacing,
        );
        let id = ui.make_id("install");
        if ui
            .button_at(
                br,
                id,
                "Install",
                ButtonStyle {
                    primary: true,
                    icon: Some(Icon::Download),
                    ..Default::default()
                },
            )
            .clicked
        {
            actions.push(UiAction::InstallUpdate);
        }
    } else if ui
        .button_ex(
            "Check now",
            ButtonStyle {
                icon: Some(Icon::Refresh),
                ..Default::default()
            },
        )
        .clicked
    {
        actions.push(UiAction::CheckForUpdates);
    }
    if let Some(s) = &u.status {
        let color: Color = t.text_dim;
        ui.label_styled(s, t.small_text_size, color);
    }
}
