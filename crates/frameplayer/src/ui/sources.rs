//! Network and local sources: list, browse, add.

use super::theme::{self, Weight};
use super::widgets::{self, Kind};
use super::{Action, SourceForm, View, fmt_size, fmt_time, icons};
use crate::playback::OpenRequest;
use egui::{Align, Align2, Color32, Layout, RichText, Sense, Vec2};
use fp_core::source::EntryKind;
use fp_sources::{Credentials, SourceConfig, SourceKind};

fn kind_icon(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Local => icons::HARD_DRIVE,
        SourceKind::Http => icons::GLOBE,
        SourceKind::WebDav => icons::CLOUD,
        SourceKind::Dlna => icons::BROADCAST,
        SourceKind::DeoVr | SourceKind::HereSphere => icons::DATABASE,
        SourceKind::Smb => icons::SHARE_NETWORK,
    }
}

/// A page title with an optional line under it and actions on the right.
fn page_header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: Option<&str>,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new(title)
                    .font(theme::font(Weight::Bold, 30.0))
                    .color(theme::TEXT),
            );
            if let Some(s) = subtitle {
                ui.label(RichText::new(s).color(theme::TEXT_2));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(14.0);
}

/// A rounded square holding an icon, the leading mark of a list row.
fn icon_tile(ui: &mut egui::Ui, icon: &str, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(12), theme::SURFACE_3);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        theme::icon(size * 0.5),
        color,
    );
}

pub fn sources(ui: &mut egui::Ui, v: &mut View) {
    if v.state.source_form.is_some() {
        add_form(ui, v);
        return;
    }
    if v.state.browse.is_some() {
        browse(ui, v);
        return;
    }
    page_header(
        ui,
        "Sources",
        Some("Browse network servers and drives. Videos you play are remembered in the library."),
        |ui| {
            if widgets::button(ui, Some(icons::PLUS), "Add source", Kind::Primary).clicked() {
                v.state.source_form = Some(SourceForm::default());
            }
        },
    );
    egui::ScrollArea::vertical().show(ui, |ui| {
        let all: Vec<(String, String, String, &'static str, bool)> =
            crate::services::builtin_sources()
                .iter()
                .map(|c| {
                    let icon = if c.id() == crate::services::REMOVABLE_SOURCE {
                        icons::USB
                    } else {
                        icons::HARD_DRIVE
                    };
                    (
                        c.id().to_string(),
                        c.name().to_string(),
                        c.describe(),
                        icon,
                        true,
                    )
                })
                .chain(v.services.source_configs.iter().map(|c| {
                    (
                        c.id().to_string(),
                        c.name().to_string(),
                        format!("{} · {}", c.kind().label(), c.describe()),
                        kind_icon(c.kind()),
                        false,
                    )
                }))
                .collect();
        widgets::section_label(ui, "On this device and your network");
        ui.add_space(4.0);
        widgets::card(ui, |ui| {
            for (i, (id, name, desc, icon, builtin)) in all.into_iter().enumerate() {
                if i > 0 {
                    let x = ui.max_rect().x_range();
                    ui.painter().hline(
                        (x.min + 84.0)..=(x.max - 16.0),
                        ui.cursor().top(),
                        egui::Stroke::new(1.0_f32, theme::STROKE),
                    );
                }
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 16.0;
                            icon_tile(ui, icon, 52.0, theme::ACCENT_HOVER);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(
                                    RichText::new(&name)
                                        .font(theme::font(Weight::SemiBold, 19.0))
                                        .color(theme::TEXT),
                                );
                                ui.label(
                                    RichText::new(&desc)
                                        .font(theme::font(Weight::Regular, 15.0))
                                        .color(theme::TEXT_2),
                                );
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if !builtin
                                    && widgets::icon_button(ui, icons::TRASH, 44.0, false)
                                        .on_hover_text("Remove")
                                        .clicked()
                                {
                                    v.actions.push(Action::RemoveSource(id.clone()));
                                }
                                if widgets::button_sized(
                                    ui,
                                    Some(icons::FOLDER_OPEN),
                                    "Browse",
                                    Kind::Secondary,
                                    44.0,
                                )
                                .clicked()
                                {
                                    v.actions.push(Action::Browse {
                                        source: id.clone(),
                                        location: None,
                                    });
                                }
                            });
                        });
                    });
            }
        });
    });
}

fn browse(ui: &mut egui::Ui, v: &mut View) {
    let Some(b) = v.state.browse.as_ref() else {
        return;
    };
    let source = b.source.clone();
    let location = b.location.clone();
    let name = v
        .services
        .opener
        .source(&source)
        .map(|s| s.name().to_string())
        .unwrap_or_else(|| source.clone());
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if widgets::icon_button(ui, icons::ARROW_LEFT, 48.0, false)
            .on_hover_text("Back")
            .clicked()
        {
            match v.state.browse.as_mut().and_then(|b| b.stack.pop()) {
                Some(parent) => {
                    if let Some(b) = v.state.browse.as_mut() {
                        b.location = parent.clone();
                        b.loading = true;
                    }
                    v.actions.push(Action::Browse {
                        source: source.clone(),
                        location: parent,
                    });
                }
                None => v.state.browse = None,
            }
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(
                RichText::new(&name)
                    .font(theme::font(Weight::Bold, 26.0))
                    .color(theme::TEXT),
            );
            if let Some(l) = &location {
                ui.label(
                    RichText::new(short_location(l))
                        .font(theme::font(Weight::Regular, 15.0))
                        .color(theme::TEXT_3),
                );
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_sized(
                ui,
                Some(icons::PLUS),
                "Add to library",
                Kind::Secondary,
                44.0,
            )
            .on_hover_text("Index every video here so it shows in Library")
            .clicked()
            {
                v.actions.push(Action::ImportFolder {
                    source: source.clone(),
                    location: location.clone(),
                });
            }
            if widgets::icon_button(ui, icons::ARROWS_CLOCKWISE, 44.0, false)
                .on_hover_text("Refresh")
                .clicked()
            {
                v.actions.push(Action::Browse {
                    source: source.clone(),
                    location: location.clone(),
                });
            }
        });
    });
    ui.add_space(12.0);
    let Some(b) = v.state.browse.as_ref() else {
        return;
    };
    if b.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Loading…").color(theme::TEXT_2));
        });
    }
    if let Some(e) = &b.error {
        ui.label(RichText::new(format!("{}  {e}", icons::WARNING_CIRCLE)).color(theme::ERROR));
    }
    let entries = b.entries.clone();
    let mut go: Option<String> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        if entries.is_empty() && !b.loading && b.error.is_none() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No videos or folders here.").color(theme::TEXT_2));
            });
        }
        for e in &entries {
            if e.kind == EntryKind::Other {
                continue;
            }
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 88.0), Sense::click());
            let t = ui
                .ctx()
                .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
            let painter = ui.painter_at(rect);
            painter.rect_filled(
                rect,
                egui::CornerRadius::same(14),
                theme::SURFACE.gamma_multiply(t),
            );
            let thumb =
                egui::Rect::from_min_size(rect.min + Vec2::new(10.0, 8.0), Vec2::new(128.0, 72.0));
            match e.kind {
                EntryKind::Directory => {
                    painter.rect_filled(thumb, egui::CornerRadius::same(10), theme::SURFACE_2);
                    painter.text(
                        thumb.center(),
                        Align2::CENTER_CENTER,
                        icons::FOLDER_SIMPLE,
                        theme::icon_fill(34.0),
                        theme::WARN,
                    );
                }
                _ => match e.thumbnail_url.as_deref().and_then(|u| v.thumbs.get(u)) {
                    Some(tex) => super::thumbs::paint_cover_rounded(&painter, thumb, &tex, 10),
                    None => {
                        painter.rect_filled(thumb, egui::CornerRadius::same(10), theme::SURFACE_2);
                        painter.text(
                            thumb.center(),
                            Align2::CENTER_CENTER,
                            icons::FILM_STRIP,
                            theme::icon(28.0),
                            theme::TEXT_3,
                        );
                    }
                },
            }
            let title = if e.kind == EntryKind::Directory {
                e.name.clone()
            } else {
                widgets::display_title(&e.name)
            };
            painter.text(
                thumb.right_center() + Vec2::new(18.0, -12.0),
                Align2::LEFT_CENTER,
                title,
                theme::font(Weight::SemiBold, 18.0),
                theme::TEXT,
            );
            let mut facts = Vec::new();
            if e.kind == EntryKind::Directory {
                facts.push("Folder".to_string());
            }
            if let Some(f) = e.format {
                facts.push(widgets::format_short(&f));
            }
            if let Some(d) = e.duration {
                facts.push(fmt_time(d));
            }
            if let Some(s) = e.size {
                facts.push(fmt_size(s));
            }
            if !e.scripts.is_empty() {
                facts.push("haptics".into());
            }
            painter.text(
                thumb.right_center() + Vec2::new(18.0, 13.0),
                Align2::LEFT_CENTER,
                facts.join("  ·  "),
                theme::font(Weight::Regular, 15.0),
                theme::TEXT_2,
            );
            painter.text(
                rect.right_center() - Vec2::new(24.0, 0.0),
                Align2::CENTER_CENTER,
                if e.kind == EntryKind::Directory {
                    icons::CARET_RIGHT
                } else {
                    icons::PLAY
                },
                theme::icon(22.0),
                theme::TEXT_3.lerp_to_gamma(Color32::WHITE, t),
            );
            if resp.clicked() {
                match e.kind {
                    EntryKind::Directory => go = Some(e.location.clone()),
                    _ => {
                        // Queue the folder's videos for next/previous.
                        let videos: Vec<&fp_core::source::Entry> = entries
                            .iter()
                            .filter(|x| x.kind == EntryKind::Video)
                            .collect();
                        let pos = videos
                            .iter()
                            .position(|x| x.location == e.location)
                            .unwrap_or(0);
                        let list = videos
                            .iter()
                            .map(|x| OpenRequest {
                                location: x.location.clone(),
                                source_id: Some(source.clone()),
                                entry: Some((*x).clone()),
                                start_at: None,
                            })
                            .collect();
                        v.actions.push(Action::OpenList(list, pos));
                    }
                }
            }
        }
    });
    if let Some(loc) = go {
        if let Some(b) = v.state.browse.as_mut() {
            b.stack.push(b.location.clone());
            b.location = Some(loc.clone());
            b.loading = true;
        }
        v.actions.push(Action::Browse {
            source,
            location: Some(loc),
        });
    }
}

fn short_location(l: &str) -> String {
    let l = l.split('#').next().unwrap_or(l);
    if l.chars().count() > 60 {
        let tail: String = l
            .chars()
            .rev()
            .take(57)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        l.to_string()
    }
}

const KINDS: [(SourceKind, &str); 7] = [
    (
        SourceKind::DeoVr,
        "XBVR / Stash / any DeoVR feed URL, e.g. http://192.168.1.10:9999/deovr",
    ),
    (
        SourceKind::HereSphere,
        "XBVR / Stash HereSphere API, e.g. http://192.168.1.10:9999/heresphere",
    ),
    (
        SourceKind::WebDav,
        "WebDAV share URL (Nextcloud, NAS), e.g. https://nas.local/dav/videos/",
    ),
    (
        SourceKind::Http,
        "Web server folder listing, e.g. http://192.168.1.10:8080/videos/",
    ),
    (
        SourceKind::Smb,
        "Windows / Samba share, e.g. smb://nas.local/videos",
    ),
    (
        SourceKind::Dlna,
        "DLNA media server on your network (Plex, Jellyfin, Emby, Serviio)",
    ),
    (
        SourceKind::Local,
        "A folder on this device, a microSD card or a USB drive",
    ),
];

fn add_form(ui: &mut egui::Ui, v: &mut View) {
    let mut cancel = false;
    let mut save: Option<SourceConfig> = None;
    let dlna_found = v.state.dlna_found.clone();
    let searching = v.state.dlna_searching;
    let Some(f) = v.state.source_form.as_mut() else {
        return;
    };
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        if widgets::icon_button(ui, icons::ARROW_LEFT, 48.0, false)
            .on_hover_text("Cancel")
            .clicked()
        {
            cancel = true;
        }
        ui.label(
            RichText::new("Add a source")
                .font(theme::font(Weight::Bold, 30.0))
                .color(theme::TEXT),
        );
    });
    ui.add_space(12.0);
    widgets::section_label(ui, "Type");
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        for (k, _) in KINDS {
            if widgets::chip_icon(ui, Some(kind_icon(k)), k.label(), f.kind == k).clicked() {
                f.kind = k;
            }
        }
    });
    let help = KINDS
        .iter()
        .find(|(k, _)| *k == f.kind)
        .map(|(_, h)| *h)
        .unwrap_or("");
    ui.add_space(4.0);
    ui.label(RichText::new(help).color(theme::TEXT_2));
    ui.add_space(12.0);
    let id = format!(
        "{}-{}",
        format!("{:?}", f.kind).to_lowercase(),
        fp_core::playback::now_ms()
    );
    if f.kind == SourceKind::Dlna {
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(!searching, |ui| {
                    widgets::button(
                        ui,
                        Some(icons::MAGNIFYING_GLASS),
                        "Search the network",
                        Kind::Primary,
                    )
                })
                .inner
                .clicked()
            {
                v.actions.push(Action::DiscoverDlna);
            }
            if searching {
                ui.spinner();
            }
        });
        ui.add_space(8.0);
        if dlna_found.is_empty() {
            if !searching {
                ui.label(RichText::new("No servers found yet.").color(theme::TEXT_3));
            }
        } else {
            widgets::rows(ui, |r| {
                for d in &dlna_found {
                    r.row(&d.friendly_name, Some(d.manufacturer.as_str()), |ui| {
                        if widgets::button_sized(
                            ui,
                            Some(icons::PLUS),
                            "Add",
                            Kind::Secondary,
                            44.0,
                        )
                        .clicked()
                        {
                            save = Some(SourceConfig::Dlna(d.to_config(id.clone())));
                        }
                    });
                }
            });
        }
    } else {
        let local = f.kind == SourceKind::Local;
        let smb = f.kind == SourceKind::Smb;
        let field = |ui: &mut egui::Ui, value: &mut String, hint: &str, password: bool| {
            ui.add(
                egui::TextEdit::singleline(value)
                    .margin(egui::Margin::symmetric(12, 9))
                    .password(password)
                    .hint_text(hint)
                    .font(theme::font(Weight::Regular, 17.0))
                    .desired_width(460.0),
            );
        };
        widgets::rows(ui, |r| {
            r.row(
                "Name",
                Some("Optional; the server's name otherwise."),
                |ui| field(ui, &mut f.name, "", false),
            );
            r.row(if local { "Folder" } else { "Address" }, None, |ui| {
                field(
                    ui,
                    &mut f.address,
                    if local {
                        "/run/media/deck/SDCARD/Videos"
                    } else {
                        "http://192.168.1.10:9999/deovr"
                    },
                    false,
                )
            });
            if !local {
                r.row("User name", Some("If the server asks for one."), |ui| {
                    field(ui, &mut f.username, "", false)
                });
                r.row("Password", None, |ui| field(ui, &mut f.password, "", true));
                if !smb {
                    r.switch(
                        "Accept self-signed certificates",
                        Some("For HTTPS servers on your own network."),
                        &mut f.insecure_tls,
                    );
                }
            }
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| match build_config(f, id) {
            Ok(cfg) => {
                if widgets::button(ui, Some(icons::CHECK), "Save", Kind::Primary).clicked() {
                    save = Some(cfg);
                }
            }
            Err(e) => {
                ui.add_enabled_ui(false, |ui| {
                    widgets::button(ui, Some(icons::CHECK), "Save", Kind::Primary)
                });
                if !f.address.is_empty() {
                    ui.label(RichText::new(e).color(theme::WARN));
                }
            }
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("Passwords are stored on this device only, in a file only you can read.")
                .font(theme::font(Weight::Regular, 15.0))
                .color(theme::TEXT_3),
        );
    }
    if let Some(cfg) = save {
        v.actions.push(Action::AddSource(cfg));
        v.state.source_form = None;
    } else if cancel {
        v.state.source_form = None;
    }
}

/// Builds a source configuration from the form, validating the address.
pub fn build_config(f: &SourceForm, id: String) -> Result<SourceConfig, String> {
    let address = f.address.trim().to_string();
    if address.is_empty() {
        return Err("Enter an address".into());
    }
    let credentials = (!f.username.trim().is_empty())
        .then(|| Credentials::new(f.username.trim(), f.password.clone()));
    let default_name = |a: &str| {
        let host = a.split("://").nth(1).unwrap_or(a);
        host.split(['/', ':']).next().unwrap_or(host).to_string()
    };
    let name = if f.name.trim().is_empty() {
        default_name(&address)
    } else {
        f.name.trim().to_string()
    };
    let http_url = |a: &str| -> Result<String, String> {
        if a.starts_with("http://") || a.starts_with("https://") {
            Ok(a.to_string())
        } else if a.contains("://") {
            Err("Address must start with http:// or https://".into())
        } else {
            Ok(format!("http://{a}"))
        }
    };
    Ok(match f.kind {
        SourceKind::Local => {
            let p = std::path::PathBuf::from(&address);
            if !p.is_absolute() {
                return Err("Enter an absolute folder path".into());
            }
            SourceConfig::Local(fp_sources::LocalConfig { id, name, root: p })
        }
        SourceKind::Http | SourceKind::WebDav => {
            let c = fp_sources::HttpConfig {
                id,
                name,
                url: http_url(&address)?,
                credentials,
                insecure_tls: f.insecure_tls,
            };
            if f.kind == SourceKind::Http {
                SourceConfig::Http(c)
            } else {
                SourceConfig::WebDav(c)
            }
        }
        SourceKind::DeoVr | SourceKind::HereSphere => {
            let c = fp_sources::FeedConfig {
                id,
                name,
                url: http_url(&address)?,
                credentials,
                insecure_tls: f.insecure_tls,
                max_height: None,
            };
            if f.kind == SourceKind::DeoVr {
                SourceConfig::DeoVr(c)
            } else {
                SourceConfig::HereSphere(c)
            }
        }
        SourceKind::Smb => {
            let rest = address
                .strip_prefix("smb://")
                .unwrap_or(&address)
                .trim_start_matches(['/', '\\'])
                .replace('\\', "/");
            let mut parts = rest.splitn(3, '/');
            let hostport = parts.next().unwrap_or_default().to_string();
            let share = parts.next().unwrap_or_default().to_string();
            let path = parts
                .next()
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string();
            if hostport.is_empty() || share.is_empty() {
                return Err("Use smb://server/share[/folder]".into());
            }
            let (host, port) = match hostport.rsplit_once(':') {
                Some((h, p)) => (
                    h.to_string(),
                    Some(p.parse::<u16>().map_err(|_| "Bad port".to_string())?),
                ),
                None => (hostport, None),
            };
            SourceConfig::Smb(fp_sources::SmbConfig {
                id,
                name,
                host,
                port,
                share,
                path,
                credentials,
            })
        }
        SourceKind::Dlna => return Err("Pick a server found on the network".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_validation() {
        let mut f = SourceForm {
            kind: SourceKind::Smb,
            address: "smb://nas:1445/videos/vr".into(),
            ..Default::default()
        };
        match build_config(&f, "x".into()).unwrap() {
            SourceConfig::Smb(c) => {
                assert_eq!(
                    (c.host.as_str(), c.port, c.share.as_str(), c.path.as_str()),
                    ("nas", Some(1445), "videos", "vr")
                );
                assert_eq!(c.name, "nas");
            }
            other => panic!("{other:?}"),
        }
        f.address = "\\\\nas".into();
        assert!(build_config(&f, "x".into()).is_err());
        f.kind = SourceKind::DeoVr;
        f.address = "192.168.1.5:9999/deovr".into();
        f.username = "me".into();
        match build_config(&f, "x".into()).unwrap() {
            SourceConfig::DeoVr(c) => {
                assert_eq!(c.url, "http://192.168.1.5:9999/deovr");
                assert!(c.credentials.is_some());
            }
            other => panic!("{other:?}"),
        }
        f.kind = SourceKind::Local;
        f.address = "relative/path".into();
        assert!(build_config(&f, "x".into()).is_err());
    }
}
