//! Network and local sources: list, browse, add.

use super::{Action, SourceForm, View, big_button, fmt_size, fmt_time, theme};
use crate::playback::OpenRequest;
use egui::{RichText, Sense, Vec2};
use fp_core::source::EntryKind;
use fp_sources::{Credentials, SourceConfig, SourceKind};

pub fn sources(ui: &mut egui::Ui, v: &mut View) {
    if v.state.source_form.is_some() {
        add_form(ui, v);
        return;
    }
    if v.state.browse.is_some() {
        browse(ui, v);
        return;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Sources").heading());
        if big_button(ui, "+ Add source", false).clicked() {
            v.state.source_form = Some(SourceForm::default());
        }
    });
    ui.label(
        RichText::new(
            "Browse network servers and drives. Videos you play are remembered in the library.",
        )
        .color(theme::MUTED),
    );
    ui.add_space(6.0);
    egui::ScrollArea::vertical().show(ui, |ui| {
        let all: Vec<(String, String, String, bool)> = crate::services::builtin_sources()
            .iter()
            .map(|c| (c.id().to_string(), c.name().to_string(), c.describe(), true))
            .chain(v.services.source_configs.iter().map(|c| {
                (
                    c.id().to_string(),
                    c.name().to_string(),
                    format!("{} · {}", c.kind().label(), c.describe()),
                    false,
                )
            }))
            .collect();
        for (id, name, desc, builtin) in all {
            egui::Frame::new()
                .fill(theme::CARD_BG)
                .corner_radius(12.0)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&name).size(22.0).strong());
                            ui.label(RichText::new(&desc).color(theme::MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !builtin
                                && ui
                                    .button(RichText::new("Remove").color(theme::ERROR))
                                    .clicked()
                            {
                                v.actions.push(Action::RemoveSource(id.clone()));
                            }
                            if big_button(ui, "Browse", false).clicked() {
                                v.actions.push(Action::Browse {
                                    source: id.clone(),
                                    location: None,
                                });
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
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
    ui.horizontal(|ui| {
        if big_button(ui, "◀ Back", false).clicked() {
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
        ui.label(RichText::new(&name).heading());
        if let Some(l) = &location {
            ui.label(RichText::new(short_location(l)).color(theme::MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("Add folder to library")
                .on_hover_text("Index every video here so it shows in Library")
                .clicked()
            {
                v.actions.push(Action::ImportFolder {
                    source: source.clone(),
                    location: location.clone(),
                });
            }
            if ui.button("⟳").clicked() {
                v.actions.push(Action::Browse {
                    source: source.clone(),
                    location: location.clone(),
                });
            }
        });
    });
    let Some(b) = v.state.browse.as_ref() else {
        return;
    };
    if b.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading…");
        });
    }
    if let Some(e) = &b.error {
        ui.label(RichText::new(e).color(theme::ERROR));
    }
    let entries = b.entries.clone();
    let mut go: Option<String> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        if entries.is_empty() && !b.loading && b.error.is_none() {
            ui.label(RichText::new("No videos or folders here.").color(theme::MUTED));
        }
        for e in &entries {
            if e.kind == EntryKind::Other {
                continue;
            }
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 74.0), Sense::click());
            let painter = ui.painter_at(rect);
            if resp.hovered() {
                painter.rect_filled(rect, 10.0, theme::CARD_BG);
            }
            let thumb =
                egui::Rect::from_min_size(rect.min + Vec2::new(6.0, 5.0), Vec2::new(114.0, 64.0));
            match e.kind {
                EntryKind::Directory => {
                    painter.text(
                        thumb.center(),
                        egui::Align2::CENTER_CENTER,
                        "📁",
                        egui::FontId::proportional(36.0),
                        theme::WARN,
                    );
                }
                _ => {
                    painter.rect_filled(thumb, 6.0, egui::Color32::from_rgb(20, 22, 30));
                    if let Some(tex) = e.thumbnail_url.as_deref().and_then(|u| v.thumbs.get(u)) {
                        super::thumbs::paint_cover(&painter, thumb, &tex);
                    } else {
                        painter.text(
                            thumb.center(),
                            egui::Align2::CENTER_CENTER,
                            "▶",
                            egui::FontId::proportional(26.0),
                            theme::MUTED,
                        );
                    }
                }
            }
            painter.text(
                thumb.right_top() + Vec2::new(14.0, 6.0),
                egui::Align2::LEFT_TOP,
                &e.name,
                egui::FontId::proportional(20.0),
                egui::Color32::WHITE,
            );
            let mut facts = Vec::new();
            if let Some(f) = e.format {
                facts.push(f.label());
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
                thumb.right_bottom() + Vec2::new(14.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                facts.join(" · "),
                egui::FontId::proportional(16.0),
                theme::MUTED,
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
    ui.horizontal(|ui| {
        if big_button(ui, "◀ Cancel", false).clicked() {
            cancel = true;
        }
        ui.label(RichText::new("Add a source").heading());
    });
    ui.horizontal_wrapped(|ui| {
        for (k, _) in KINDS {
            if ui.selectable_label(f.kind == k, k.label()).clicked() {
                f.kind = k;
            }
        }
    });
    let help = KINDS
        .iter()
        .find(|(k, _)| *k == f.kind)
        .map(|(_, h)| *h)
        .unwrap_or("");
    ui.label(RichText::new(help).color(theme::MUTED));
    ui.add_space(6.0);
    let field = |ui: &mut egui::Ui, label: &str, value: &mut String, password: bool| {
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 36.0], egui::Label::new(label));
            ui.add(
                egui::TextEdit::singleline(value)
                    .password(password)
                    .desired_width(520.0),
            );
        });
    };
    let id = format!(
        "{}-{}",
        format!("{:?}", f.kind).to_lowercase(),
        fp_core::playback::now_ms()
    );
    if f.kind == SourceKind::Dlna {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!searching, egui::Button::new("Search the network"))
                .clicked()
            {
                v.actions.push(Action::DiscoverDlna);
            }
            if searching {
                ui.spinner();
            }
        });
        for d in &dlna_found {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.friendly_name).strong());
                ui.label(RichText::new(&d.manufacturer).color(theme::MUTED));
                if ui.button("Add").clicked() {
                    save = Some(SourceConfig::Dlna(d.to_config(id.clone())));
                }
            });
        }
        if dlna_found.is_empty() && !searching {
            ui.label(RichText::new("No servers found yet.").color(theme::MUTED));
        }
    } else {
        field(ui, "Name", &mut f.name, false);
        let addr_label = if f.kind == SourceKind::Local {
            "Folder"
        } else {
            "Address"
        };
        field(ui, addr_label, &mut f.address, false);
        if f.kind != SourceKind::Local {
            field(ui, "User name", &mut f.username, false);
            field(ui, "Password", &mut f.password, true);
            if f.kind != SourceKind::Smb {
                ui.checkbox(&mut f.insecure_tls, "Accept self-signed certificates");
            }
        }
        ui.add_space(8.0);
        match build_config(f, id) {
            Ok(cfg) => {
                if big_button(ui, "Save", false).clicked() {
                    save = Some(cfg);
                }
            }
            Err(e) => {
                ui.add_enabled(false, egui::Button::new("Save"));
                if !f.address.is_empty() {
                    ui.label(RichText::new(e).color(theme::WARN));
                }
            }
        }
        ui.label(
            RichText::new("Passwords are stored on this device only, in a file only you can read.")
                .small()
                .color(theme::MUTED),
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
