//! DLNA/UPnP media servers (MiniDLNA/ReadyMedia, Plex, Jellyfin, Emby,
//! Serviio, Universal Media Server, Windows Media Player sharing, NAS
//! firmware).
//!
//! - [`discover`] sends an SSDP `M-SEARCH` for the ContentDirectory service
//!   to `239.255.255.250:1900`, then fetches each answering device's
//!   description to find the ContentDirectory control URL.
//! - [`DlnaSource`] browses with the SOAP `Browse` action
//!   (`BrowseDirectChildren`, paged until `TotalMatches`) and maps DIDL-Lite
//!   containers to folders and items to files.
//!
//! Folder locations are `dlna:<percent-encoded object id>`; file locations
//! are the item's `res` URL, read with the HTTP
//! [`crate::http::HttpFile`].

use crate::config::{DlnaConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions};
use crate::sidecar::{Sidecars, match_sidecars};
use crate::timeutil::parse_duration;
use crate::urlutil::{encode_segment, last_segment, percent_decode, redact, resolve};
use crate::xml::{self, Element};
use crate::{Source, dir_entry, sort_entries};
use fp_core::source::{
    ByteSource, Entry, EntryKind, is_script_name, is_subtitle_name, is_video_name,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SSDP multicast group and port.
pub const SSDP_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

/// ContentDirectory:1, the service every DLNA media server offers.
pub const CONTENT_DIRECTORY_1: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";

/// Object id of the root container.
const ROOT_ID: &str = "0";

/// Items requested per `Browse` call.
const PAGE_SIZE: u32 = 200;

/// Upper bound on pages for one folder, against servers that never stop.
const MAX_PAGES: u32 = 500;

/// A media server found by [`discover`] (or described by
/// [`parse_device_description`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DlnaDevice {
    /// `friendlyName` from the description (falls back to the host).
    pub friendly_name: String,
    /// Device description URL (SSDP `LOCATION`).
    pub location: String,
    /// SSDP `USN` (`uuid:...::urn:...`).
    #[serde(default)]
    pub usn: String,
    /// SSDP `SERVER` header (OS and server software).
    #[serde(default)]
    pub server: String,
    /// Unique device name (`uuid:...`).
    #[serde(default)]
    pub udn: String,
    /// Manufacturer from the description.
    #[serde(default)]
    pub manufacturer: String,
    /// Model name from the description.
    #[serde(default)]
    pub model_name: String,
    /// Absolute ContentDirectory control URL.
    #[serde(default)]
    pub control_url: Option<String>,
    /// ContentDirectory service type (version may be 1 to 4).
    #[serde(default)]
    pub service_type: Option<String>,
    /// Absolute URL of the largest device icon.
    #[serde(default)]
    pub icon_url: Option<String>,
}

impl DlnaDevice {
    /// A source configuration for this device.
    pub fn to_config(&self, id: impl Into<String>) -> DlnaConfig {
        DlnaConfig {
            id: id.into(),
            name: self.friendly_name.clone(),
            location: self.location.clone(),
            control_url: self.control_url.clone(),
            service_type: self.service_type.clone(),
        }
    }
}

/// Headers of one SSDP answer (`HTTP/1.1 200 OK`) or announcement
/// (`NOTIFY * HTTP/1.1`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SsdpResponse {
    /// `LOCATION`: device description URL.
    pub location: String,
    /// `USN`.
    pub usn: String,
    /// `SERVER`.
    pub server: String,
    /// `ST` (answers) or `NT` (announcements).
    pub search_target: String,
}

/// The `M-SEARCH` request for `search_target`, waiting up to `mx` seconds.
pub fn m_search_request(search_target: &str, mx: u32) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\
         ST: {search_target}\r\n\
         USER-AGENT: Linux/1.0 UPnP/1.1 FramePlayer/{}\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Parses an SSDP datagram. Returns `None` for searches from other control
/// points, `byebye` announcements, and answers without a `LOCATION`.
pub fn parse_ssdp_response(text: &str) -> Option<SsdpResponse> {
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    let is_answer = first.starts_with("HTTP/") && first.split_whitespace().nth(1) == Some("200");
    let is_notify = first.to_ascii_uppercase().starts_with("NOTIFY");
    if !is_answer && !is_notify {
        return None;
    }
    let mut r = SsdpResponse::default();
    let mut nts = String::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim().to_ascii_uppercase().as_str() {
            "LOCATION" => r.location = v,
            "USN" => r.usn = v,
            "SERVER" => r.server = v,
            "ST" | "NT" => r.search_target = v,
            "NTS" => nts = v,
            _ => {}
        }
    }
    if is_notify && !nts.eq_ignore_ascii_case("ssdp:alive") {
        return None;
    }
    (!r.location.is_empty()).then_some(r)
}

/// Parses a UPnP device description fetched from `location`. Relative URLs
/// resolve against `URLBase` when present, else against `location`. Nested
/// devices are searched for the ContentDirectory service.
pub fn parse_device_description(xml_text: &str, location: &str) -> Result<DlnaDevice> {
    let doc = xml::parse(xml_text)?;
    let root = doc
        .find("root")
        .ok_or_else(|| Error::parse("UPnP device description", "no root element"))?;
    let base = root
        .child_text("URLBase")
        .map(str::to_string)
        .unwrap_or_else(|| location.to_string());
    let device = root
        .child("device")
        .ok_or_else(|| Error::parse("UPnP device description", "no device element"))?;
    let mut out = DlnaDevice {
        friendly_name: device.child_text("friendlyName").unwrap_or("").to_string(),
        location: location.to_string(),
        udn: device.child_text("UDN").unwrap_or("").to_string(),
        manufacturer: device.child_text("manufacturer").unwrap_or("").to_string(),
        model_name: device.child_text("modelName").unwrap_or("").to_string(),
        ..DlnaDevice::default()
    };
    let mut services = Vec::new();
    device.find_all("service", &mut services);
    for s in services {
        let Some(st) = s.child_text("serviceType") else {
            continue;
        };
        if !st.contains(":service:ContentDirectory:") {
            continue;
        }
        if let Some(ctl) = s.child_text("controlURL") {
            out.control_url = Some(resolve(&base, ctl)?);
            out.service_type = Some(st.to_string());
            break;
        }
    }
    let icon = device
        .child("iconList")
        .into_iter()
        .flat_map(|l| l.children_named("icon"))
        .filter_map(|i| {
            let w: u32 = i
                .child_text("width")
                .and_then(|w| w.parse().ok())
                .unwrap_or(0);
            Some((w, i.child_text("url")?))
        })
        .max_by_key(|(w, _)| *w);
    if let Some((_, url)) = icon {
        out.icon_url = resolve(&base, url).ok();
    }
    if out.friendly_name.is_empty() {
        out.friendly_name = url::Url::parse(location)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "Media server".into());
    }
    Ok(out)
}

fn discovery_client() -> HttpClient {
    HttpClient::new(
        HttpOptions {
            connect_timeout: Duration::from_secs(3),
            read_timeout: Duration::from_secs(5),
            max_retries: 1,
            ..HttpOptions::default()
        },
        None,
    )
}

/// Finds DLNA media servers on the local network.
///
/// Sends `M-SEARCH` for ContentDirectory:1 and MediaServer:1, collects
/// answers for `timeout`, then fetches every device description (in
/// parallel) to find its ContentDirectory. Devices without one, or whose
/// description cannot be read, are left out. Sorted by name.
pub fn discover(timeout: Duration) -> Result<Vec<DlnaDevice>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_multicast_ttl_v4(4)?;
    let mx = timeout.as_secs().clamp(1, 5) as u32;
    for st in [
        CONTENT_DIRECTORY_1,
        "urn:schemas-upnp-org:device:MediaServer:1",
    ] {
        socket.send_to(m_search_request(st, mx).as_bytes(), SSDP_ADDR)?;
    }
    let deadline = Instant::now() + timeout;
    let mut found: HashMap<String, SsdpResponse> = HashMap::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        socket.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(r) = parse_ssdp_response(&text) {
                    let st = r.search_target.to_ascii_lowercase();
                    if st.contains("contentdirectory") || st.contains("mediaserver") {
                        log::debug!("SSDP answer from {from}: {}", redact(&r.location));
                        found.entry(r.location.clone()).or_insert(r);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let client = discovery_client();
    let answers: Vec<SsdpResponse> = found.into_values().collect();
    let mut devices: Vec<DlnaDevice> = std::thread::scope(|scope| {
        let handles: Vec<_> = answers
            .iter()
            .map(|a| {
                let client = &client;
                scope.spawn(move || describe_device(client, a))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    });
    devices.sort_by(|a, b| crate::urlutil::natural_cmp(&a.friendly_name, &b.friendly_name));
    Ok(devices)
}

fn describe_device(client: &HttpClient, answer: &SsdpResponse) -> Option<DlnaDevice> {
    let fetched = client
        .get_text(&answer.location)
        .and_then(|xml| parse_device_description(&xml, &answer.location));
    match fetched {
        Ok(mut d) if d.control_url.is_some() => {
            d.usn = answer.usn.clone();
            d.server = answer.server.clone();
            Some(d)
        }
        Ok(_) => None,
        Err(e) => {
            log::debug!("ignoring DLNA device at {}: {e}", redact(&answer.location));
            None
        }
    }
}

/// One page of a `Browse` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowsePage {
    /// Folders and files, in server order.
    pub entries: Vec<Entry>,
    /// `NumberReturned`.
    pub returned: u32,
    /// `TotalMatches` (0 when the server does not know).
    pub total: u32,
    /// Object id of the parent container for each file location.
    pub parents: Vec<(String, String)>,
}

/// SOAP body of a `Browse` request.
pub fn browse_request(service_type: &str, object_id: &str, start: u32, count: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Browse xmlns:u="{}"><ObjectID>{}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#,
        xml::escape(service_type),
        xml::escape(object_id)
    )
}

/// Location of a DLNA container.
pub fn container_location(object_id: &str) -> String {
    format!("dlna:{}", encode_segment(object_id))
}

fn object_id_of(location: &str) -> Result<String> {
    location
        .strip_prefix("dlna:")
        .map(percent_decode)
        .ok_or_else(|| Error::invalid(location, "not a DLNA folder location"))
}

/// File extension for a MIME type, for items whose title has none.
fn ext_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime.to_ascii_lowercase().as_str() {
        "video/mp4" | "video/mpeg4" => "mp4",
        "video/x-matroska" | "video/matroska" | "video/x-mkv" => "mkv",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-msvideo" | "video/avi" => "avi",
        "video/mpeg" => "mpg",
        "video/mp2t" | "video/vnd.dlna.mpeg-tts" => "ts",
        "video/x-flv" => "flv",
        "video/x-ms-wmv" => "wmv",
        "video/x-m4v" => "m4v",
        "video/ogg" => "ogv",
        "text/srt" | "application/x-subrip" | "text/x-srt" => "srt",
        "text/vtt" => "vtt",
        _ => return None,
    })
}

/// MIME type from a `protocolInfo` (`http-get:*:video/mp4:DLNA.ORG_PN=...`).
fn protocol_mime(protocol_info: &str) -> &str {
    protocol_info.split(':').nth(2).unwrap_or("")
}

/// Converts one DIDL-Lite `item` to an entry. Audio and image items are
/// skipped.
fn item_entry(item: &Element, base: &str) -> Option<(Entry, Option<String>)> {
    let title = item.child_text("title").unwrap_or("").to_string();
    let class = item.child_text("class").unwrap_or("").to_ascii_lowercase();
    let resources: Vec<&Element> = item.children_named("res").collect();
    let http: Vec<&Element> = resources
        .iter()
        .copied()
        .filter(|r| {
            let pi = r.attr("protocolInfo").unwrap_or("");
            pi.is_empty() || pi.to_ascii_lowercase().starts_with("http-get")
        })
        .collect();
    // Prefer the original video resource (servers list transcodes later).
    let res = http
        .iter()
        .find(|r| {
            protocol_mime(r.attr("protocolInfo").unwrap_or(""))
                .to_ascii_lowercase()
                .starts_with("video/")
        })
        .or_else(|| http.first())?;
    let url = resolve(base, res.text.trim()).ok()?;
    let mime = protocol_mime(res.attr("protocolInfo").unwrap_or("")).to_string();

    let mut name = title.clone();
    let has_known_ext = is_video_name(&name) || is_subtitle_name(&name) || is_script_name(&name);
    if !has_known_ext {
        let url_name = last_segment(&url);
        let ext = ext_for_mime(&mime).map(str::to_string).or_else(|| {
            (is_video_name(&url_name) || is_subtitle_name(&url_name))
                .then(|| {
                    url_name
                        .rsplit_once('.')
                        .map(|(_, e)| e.to_ascii_lowercase())
                })
                .flatten()
        });
        match ext {
            Some(ext) if !name.is_empty() => name = format!("{name}.{ext}"),
            _ if name.is_empty() => name = url_name,
            _ => {}
        }
    }
    let is_video = class.contains("videoitem")
        || mime.to_ascii_lowercase().starts_with("video/")
        || is_video_name(&name);
    // A side file stays a side file even when the server calls it video.
    let kind = if is_subtitle_name(&name) || is_script_name(&name) {
        EntryKind::Other
    } else if is_video {
        EntryKind::Video
    } else {
        return None;
    };
    let mut e = Entry::new(name, url, kind);
    e.size = res.attr("size").and_then(|s| s.trim().parse().ok());
    e.duration = res.attr("duration").and_then(parse_duration);
    e.thumbnail_url = item
        .child_text("albumArtURI")
        .and_then(|u| resolve(base, u).ok());
    if kind == EntryKind::Video {
        e.format = fp_core::format::detect_from_name(&e.name)
            .or_else(|| fp_core::format::detect_from_name(&title));
        // Subtitles offered as extra resources or Samsung/MiniDLNA captions.
        for r in &resources {
            let m = protocol_mime(r.attr("protocolInfo").unwrap_or("")).to_ascii_lowercase();
            if (m.starts_with("text/") || m.contains("subrip"))
                && let Ok(u) = resolve(base, r.text.trim())
            {
                e.subtitles.push(u);
            }
        }
        if let Some(cap) = item
            .child_text("CaptionInfoEx")
            .or(item.child_text("CaptionInfo"))
            && let Ok(u) = resolve(base, cap)
            && !e.subtitles.contains(&u)
        {
            e.subtitles.push(u);
        }
    }
    Some((e, item.attr("parentID").map(str::to_string)))
}

/// Folder and file locations of one container, as listed by DIDL-Lite.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Didl {
    /// Folders and files in document order.
    pub entries: Vec<Entry>,
    /// `(file location, parent container id)` for items that name one.
    pub parents: Vec<(String, String)>,
}

/// Parses a DIDL-Lite document. Relative URLs resolve against `base` (the
/// control URL).
pub fn parse_didl(didl: &str, base: &str) -> Result<Didl> {
    let doc = xml::parse(didl)?;
    let root = doc
        .find("DIDL-Lite")
        .ok_or_else(|| Error::parse("DIDL-Lite", "no DIDL-Lite element"))?;
    let mut entries = Vec::new();
    let mut parents = Vec::new();
    for el in &root.children {
        if el.name.eq_ignore_ascii_case("container") {
            let Some(id) = el.attr("id") else { continue };
            let title = el.child_text("title").unwrap_or(id).to_string();
            let mut e = dir_entry(title, container_location(id), None);
            e.thumbnail_url = el
                .child_text("albumArtURI")
                .and_then(|u| resolve(base, u).ok());
            entries.push(e);
        } else if el.name.eq_ignore_ascii_case("item")
            && let Some((e, parent)) = item_entry(el, base)
        {
            if let Some(p) = parent {
                parents.push((e.location.clone(), p));
            }
            entries.push(e);
        }
    }
    Ok(Didl { entries, parents })
}

/// Parses a SOAP `BrowseResponse`, or turns a SOAP fault into an error.
pub fn parse_browse_response(xml_text: &str, base: &str) -> Result<BrowsePage> {
    let doc = xml::parse(xml_text)?;
    if let Some(fault) = doc.find("Fault") {
        let code = fault.find("errorCode").map(|c| c.text.trim().to_string());
        let desc = fault
            .find("errorDescription")
            .or_else(|| fault.find("faultstring"))
            .map(|d| d.text.trim().to_string())
            .unwrap_or_default();
        return Err(match code.as_deref() {
            Some("701") => Error::NotFound(format!("DLNA object ({desc})")),
            Some(c) => Error::Unsupported(format!("DLNA Browse failed: UPnP error {c} {desc}")),
            None => Error::Unsupported(format!("DLNA Browse failed: {desc}")),
        });
    }
    let resp = doc
        .find("BrowseResponse")
        .ok_or_else(|| Error::parse("DLNA Browse response", "no BrowseResponse element"))?;
    let num = |n: &str| {
        resp.child_text(n)
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    };
    let Didl { entries, parents } = match resp.child_text("Result") {
        Some(didl) => parse_didl(didl, base)?,
        None => Didl::default(),
    };
    let returned = match num("NumberReturned") {
        0 => entries.len() as u32,
        n => n,
    };
    Ok(BrowsePage {
        entries,
        returned,
        total: num("TotalMatches"),
        parents,
    })
}

#[derive(Clone, Debug)]
struct Control {
    url: String,
    service_type: String,
}

/// A DLNA/UPnP media server.
pub struct DlnaSource {
    id: String,
    name: String,
    location: String,
    client: HttpClient,
    control: Mutex<Option<Control>>,
    /// Parent container of each file listed so far, for sidecars.
    parents: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for DlnaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DlnaSource")
            .field("id", &self.id)
            .field("location", &redact(&self.location))
            .finish_non_exhaustive()
    }
}

impl DlnaSource {
    /// Creates the source. The device description is fetched on first use
    /// when the configuration has no control URL.
    pub fn new(config: &DlnaConfig) -> Result<DlnaSource> {
        if !crate::urlutil::is_http_url(&config.location) {
            return Err(Error::invalid(
                &config.location,
                "expected an http:// device description URL",
            ));
        }
        let control = config.control_url.as_ref().map(|u| Control {
            url: u.clone(),
            service_type: config
                .service_type
                .clone()
                .unwrap_or_else(|| CONTENT_DIRECTORY_1.to_string()),
        });
        Ok(DlnaSource {
            id: config.id.clone(),
            name: config.name.clone(),
            location: config.location.clone(),
            client: HttpClient::new(HttpOptions::default(), None),
            control: Mutex::new(control),
            parents: Mutex::new(HashMap::new()),
        })
    }

    fn control(&self) -> Result<Control> {
        let mut guard = crate::cache::lock(&self.control);
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let xml_text = self.client.get_text(&self.location)?;
        let dev = parse_device_description(&xml_text, &self.location)?;
        let c = Control {
            url: dev.control_url.ok_or_else(|| {
                Error::Unsupported(format!(
                    "{} has no ContentDirectory service",
                    redact(&self.location)
                ))
            })?,
            service_type: dev
                .service_type
                .unwrap_or_else(|| CONTENT_DIRECTORY_1.to_string()),
        };
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Fetches one page of a container.
    pub fn browse(&self, object_id: &str, start: u32, count: u32) -> Result<BrowsePage> {
        let control = self.control()?;
        let body = browse_request(&control.service_type, object_id, start, count);
        let action = format!("\"{}#Browse\"", control.service_type);
        let reply = self.client.request_no_status_retry(
            "POST",
            &control.url,
            &[
                ("Content-Type", "text/xml; charset=\"utf-8\""),
                ("SOAPAction", action.as_str()),
            ],
            Some(body.as_bytes()),
            64 << 20,
        )?;
        // Faults come back as 500 with a SOAP body; parse those too.
        if reply.status == 500 || (200..300).contains(&reply.status) {
            match parse_browse_response(&reply.text(), &control.url) {
                Err(Error::Parse { .. }) if reply.status == 500 => {}
                other => return other,
            }
        }
        Err(reply
            .error_for_status(&control.url)
            .err()
            .unwrap_or(Error::HttpStatus {
                status: 500,
                url: redact(&control.url),
            }))
    }

    /// Lists every child of a container, paging until `TotalMatches`.
    pub fn browse_all(&self, object_id: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut start = 0u32;
        for _ in 0..MAX_PAGES {
            let page = self.browse(object_id, start, PAGE_SIZE)?;
            {
                let mut parents = crate::cache::lock(&self.parents);
                for (loc, parent) in page.parents {
                    parents.insert(loc, parent);
                }
                // Items without parentID still belong to this container.
                for e in page
                    .entries
                    .iter()
                    .filter(|e| e.kind != EntryKind::Directory)
                {
                    parents
                        .entry(e.location.clone())
                        .or_insert_with(|| object_id.to_string());
                }
            }
            out.extend(page.entries);
            start = start.saturating_add(page.returned);
            let done = page.returned == 0
                || (page.total > 0 && start >= page.total)
                || (page.total == 0 && page.returned < PAGE_SIZE);
            if done {
                break;
            }
        }
        Ok(out)
    }
}

impl Source for DlnaSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Dlna
    }

    fn describe(&self) -> String {
        format!("DLNA server {}", redact(&self.location))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let id = match location {
            None => ROOT_ID.to_string(),
            Some(l) => object_id_of(l)?,
        };
        let mut entries = self.browse_all(&id)?;
        sort_entries(&mut entries);
        Ok(entries)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        if location.starts_with("dlna:") {
            return Err(Error::invalid(location, "a DLNA folder cannot be opened"));
        }
        Ok(Arc::new(self.client.open(location)?))
    }

    fn parent(&self, location: &str) -> Option<String> {
        crate::cache::lock(&self.parents)
            .get(location)
            .map(|id| container_location(id))
    }

    /// Captions the item carries, plus files in the same container whose
    /// title matches the video's (titles stand in for file names: DLNA
    /// URLs are opaque).
    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        let mut out = Sidecars::from_entry(video);
        if let Some(parent) = self.parent(&video.location) {
            let siblings = self.list(Some(&parent))?;
            out.merge(match_sidecars(&video.name, &siblings));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::format::{Projection, StereoLayout};

    #[test]
    fn ssdp_answers_and_notifies() {
        let answer = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: Fri, 12 Jan 2024 10:00:00 GMT\r\nEXT:\r\nLOCATION: http://192.168.1.10:8200/rootDesc.xml\r\nSERVER: Linux/5.10 DLNADOC/1.50 UPnP/1.0 MiniDLNA/1.3.3\r\nST: urn:schemas-upnp-org:service:ContentDirectory:1\r\nUSN: uuid:4d696e69-444c-164e-9d41-001132e5ddb0::urn:schemas-upnp-org:service:ContentDirectory:1\r\nContent-Length: 0\r\n\r\n";
        let r = parse_ssdp_response(answer).unwrap();
        assert_eq!(r.location, "http://192.168.1.10:8200/rootDesc.xml");
        assert!(r.server.contains("MiniDLNA"));
        assert_eq!(r.search_target, CONTENT_DIRECTORY_1);
        assert!(r.usn.starts_with("uuid:4d69"));

        let notify = "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: urn:schemas-upnp-org:device:MediaServer:1\r\nNTS: ssdp:alive\r\nlocation: http://10.0.0.2:32469/DeviceDescription.xml\r\nusn: uuid:x::urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n";
        let r = parse_ssdp_response(notify).unwrap();
        assert_eq!(r.location, "http://10.0.0.2:32469/DeviceDescription.xml");
        assert_eq!(r.search_target, "urn:schemas-upnp-org:device:MediaServer:1");

        assert!(parse_ssdp_response(&notify.replace("ssdp:alive", "ssdp:byebye")).is_none());
        assert!(parse_ssdp_response(&m_search_request(CONTENT_DIRECTORY_1, 2)).is_none());
        assert!(parse_ssdp_response("HTTP/1.1 200 OK\r\nST: x\r\n\r\n").is_none());
        assert!(parse_ssdp_response("HTTP/1.1 404 Not Found\r\nLOCATION: http://x/\r\n").is_none());
        assert!(parse_ssdp_response("").is_none());
    }

    #[test]
    fn m_search_format() {
        let m = m_search_request(CONTENT_DIRECTORY_1, 3);
        assert!(m.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(m.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(m.contains("MX: 3\r\n"));
        assert!(m.contains(&format!("ST: {CONTENT_DIRECTORY_1}\r\n")));
        assert!(m.ends_with("\r\n\r\n"));
    }

    #[test]
    fn device_description_with_url_base_and_embedded_device() {
        let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion>
 <URLBase>http://192.168.1.10:8200/</URLBase>
 <device>
  <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
  <friendlyName>NAS: minidlna</friendlyName>
  <manufacturer>Justin Maggard</manufacturer><modelName>Windows Media Connect compatible (MiniDLNA)</modelName>
  <UDN>uuid:4d696e69</UDN>
  <iconList><icon><width>48</width><url>/icons/sm.png</url></icon><icon><width>120</width><url>/icons/lrg.png</url></icon></iconList>
  <serviceList>
   <service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/ctl/ConnectionMgr</controlURL></service>
  </serviceList>
  <deviceList><device><friendlyName>inner</friendlyName><serviceList>
   <service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:2</serviceType>
    <controlURL>ctl/ContentDir</controlURL></service>
  </serviceList></device></deviceList>
 </device>
</root>"#;
        let d = parse_device_description(xml, "http://192.168.1.10:8200/rootDesc.xml").unwrap();
        assert_eq!(d.friendly_name, "NAS: minidlna");
        assert_eq!(
            d.control_url.as_deref(),
            Some("http://192.168.1.10:8200/ctl/ContentDir")
        );
        assert_eq!(
            d.service_type.as_deref(),
            Some("urn:schemas-upnp-org:service:ContentDirectory:2")
        );
        assert_eq!(
            d.icon_url.as_deref(),
            Some("http://192.168.1.10:8200/icons/lrg.png")
        );
        assert_eq!(d.udn, "uuid:4d696e69");
        let cfg = d.to_config("x");
        assert_eq!(cfg.control_url, d.control_url);
    }

    #[test]
    fn device_description_relative_to_location() {
        let xml = r#"<root><device><friendlyName></friendlyName><serviceList><service>
<serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
<controlURL>cds/control</controlURL></service></serviceList></device></root>"#;
        let d = parse_device_description(xml, "http://10.0.0.5:9000/dev/desc.xml").unwrap();
        assert_eq!(
            d.control_url.as_deref(),
            Some("http://10.0.0.5:9000/dev/cds/control")
        );
        assert_eq!(d.friendly_name, "10.0.0.5");
        let none = parse_device_description(
            "<root><device><friendlyName>x</friendlyName></device></root>",
            "http://h/",
        )
        .unwrap();
        assert!(none.control_url.is_none());
    }

    const DIDL: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:dlna="urn:schemas-dlna-org:metadata-1-0/" xmlns:sec="http://www.sec.co.kr/">
<container id="64$1" parentID="64" restricted="1" childCount="3"><dc:title>VR &amp; 3D</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>
<item id="64$2" parentID="64" restricted="1"><dc:title>Scene_180_LR</dc:title><upnp:class>object.item.videoItem</upnp:class>
 <upnp:albumArtURI dlna:profileID="JPEG_TN">/AlbumArt/22-1.jpg</upnp:albumArtURI>
 <sec:CaptionInfoEx sec:type="srt">http://192.168.1.10:8200/Captions/22.srt</sec:CaptionInfoEx>
 <res size="1073741824" duration="1:02:03.500" resolution="5760x2880" protocolInfo="http-get:*:video/mp4:DLNA.ORG_OP=01;DLNA.ORG_CI=0">http://192.168.1.10:8200/MediaItems/22.mp4</res>
 <res protocolInfo="rtsp-rtp-udp:*:video/mp4:*">rtsp://x/y</res>
</item>
<item id="64$3" parentID="64"><dc:title>Holiday.jpg</dc:title><upnp:class>object.item.imageItem.photo</upnp:class><res protocolInfo="http-get:*:image/jpeg:*">http://192.168.1.10:8200/MediaItems/23.jpg</res></item>
<item id="64$4" parentID="64"><dc:title>Clip.mkv</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/x-matroska:*" size="99">/MediaItems/24.mkv</res></item>
</DIDL-Lite>"#;

    #[test]
    fn didl_containers_and_items() {
        let Didl {
            entries: v,
            parents,
        } = parse_didl(DIDL, "http://192.168.1.10:8200/ctl/ContentDir").unwrap();
        assert_eq!(v.len(), 3, "{v:?}");
        assert_eq!(v[0].kind, EntryKind::Directory);
        assert_eq!(v[0].name, "VR & 3D");
        assert_eq!(v[0].location, "dlna:64%241");
        assert_eq!(object_id_of(&v[0].location).unwrap(), "64$1");

        let s = &v[1];
        assert_eq!(s.name, "Scene_180_LR.mp4");
        assert_eq!(s.kind, EntryKind::Video);
        assert_eq!(s.location, "http://192.168.1.10:8200/MediaItems/22.mp4");
        assert_eq!(s.size, Some(1_073_741_824));
        assert_eq!(s.duration, Some(3723.5));
        assert_eq!(
            s.thumbnail_url.as_deref(),
            Some("http://192.168.1.10:8200/AlbumArt/22-1.jpg")
        );
        assert_eq!(s.subtitles, ["http://192.168.1.10:8200/Captions/22.srt"]);
        let f = s.format.unwrap();
        assert_eq!(
            (f.projection, f.stereo),
            (Projection::EQUIRECT_180, StereoLayout::SideBySide)
        );

        assert_eq!(v[2].name, "Clip.mkv");
        assert_eq!(v[2].location, "http://192.168.1.10:8200/MediaItems/24.mkv");
        assert_eq!(v[2].size, Some(99));
        assert_eq!(parents.len(), 2);
        assert_eq!(parents[0].1, "64");
    }

    #[test]
    fn browse_response_and_faults() {
        let escaped = xml::escape(DIDL);
        let soap = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>{escaped}</Result><NumberReturned>4</NumberReturned><TotalMatches>9</TotalMatches><UpdateID>5</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#
        );
        let p = parse_browse_response(&soap, "http://192.168.1.10:8200/ctl/ContentDir").unwrap();
        assert_eq!((p.returned, p.total, p.entries.len()), (4, 9, 3));

        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert!(matches!(
            parse_browse_response(fault, "http://h/"),
            Err(Error::NotFound(_))
        ));
        assert!(parse_browse_response("<x/>", "http://h/").is_err());
    }

    #[test]
    fn browse_request_escapes() {
        let b = browse_request(CONTENT_DIRECTORY_1, "a<&>", 200, 50);
        assert!(b.contains("<ObjectID>a&lt;&amp;&gt;</ObjectID>"));
        assert!(
            b.contains("<StartingIndex>200</StartingIndex><RequestedCount>50</RequestedCount>")
        );
        assert!(b.contains("BrowseDirectChildren"));
    }
}
