//! DLNA / UPnP AV media servers: SSDP discovery, device description,
//! ContentDirectory `Browse` (BrowseDirectChildren) and DIDL-Lite parsing.
//!
//! Directory URIs are `dlna://<udn>/<object id>` (object id
//! percent-encoded, root is `0`); file entries carry the server's `res`
//! HTTP URL directly, so opening them is a plain ranged HTTP read.

pub mod ssdp;

use crate::config::SourceKind;
use crate::error::{Result, SourceError};
use crate::http::{HttpClient, HttpFile};
use crate::source::{Entry, RandomAccess, Source};
use crate::xml::{self, Element};
use async_trait::async_trait;
use bytes::Bytes;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;
use std::time::Duration;
use url::Url;

pub const CONTENT_DIRECTORY: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";

/// What we need from a UPnP device description.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceDescription {
    pub friendly_name: String,
    /// `uuid:...`
    pub udn: String,
    pub device_type: String,
    pub manufacturer: Option<String>,
    pub model_name: Option<String>,
    /// Absolute ContentDirectory control URL.
    pub content_directory_url: Option<String>,
    /// Largest icon, absolute URL.
    pub icon_url: Option<String>,
}

fn find_service<'a>(
    device: &'a Element,
    service_type_prefix: &str,
) -> Option<(&'a Element, &'a Element)> {
    if let Some(list) = device.child("serviceList") {
        for s in list.children_named("service") {
            if s.child_text("serviceType")
                .is_some_and(|t| t.starts_with(service_type_prefix))
            {
                return Some((device, s));
            }
        }
    }
    device
        .child("deviceList")?
        .children_named("device")
        .find_map(|d| find_service(d, service_type_prefix))
}

/// Parse a device description fetched from `location`.
pub fn parse_device_description(body: &str, location: &str) -> Result<DeviceDescription> {
    let root = xml::parse(body)?;
    let device = root
        .child("device")
        .ok_or_else(|| SourceError::parse("device description has no <device>"))?;
    let base = match root.child_text("URLBase").filter(|s| !s.is_empty()) {
        Some(b) => Url::parse(b)?,
        None => Url::parse(location)?,
    };
    let abs = |u: &str| base.join(u).map(|u| u.to_string()).ok();
    // The ContentDirectory may live on an embedded device.
    let found = find_service(device, "urn:schemas-upnp-org:service:ContentDirectory:");
    let (dev, cd) = match found {
        Some((d, s)) => (d, Some(s)),
        None => (device, None),
    };
    let icon_url = dev
        .child("iconList")
        .into_iter()
        .flat_map(|l| l.children_named("icon"))
        .max_by_key(|i| {
            i.child_text("width")
                .and_then(|w| w.parse::<u32>().ok())
                .unwrap_or(0)
        })
        .and_then(|i| i.child_text("url"))
        .and_then(abs);
    Ok(DeviceDescription {
        friendly_name: dev
            .child_text("friendlyName")
            .unwrap_or("Media Server")
            .to_string(),
        udn: dev.child_text("UDN").unwrap_or("").to_string(),
        device_type: dev.child_text("deviceType").unwrap_or("").to_string(),
        manufacturer: dev.child_text("manufacturer").map(str::to_string),
        model_name: dev.child_text("modelName").map(str::to_string),
        content_directory_url: cd.and_then(|s| s.child_text("controlURL")).and_then(abs),
        icon_url,
    })
}

/// SOAP body for ContentDirectory:1 Browse.
pub fn browse_request_body(object_id: &str, start: u32, count: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Browse xmlns:u="{CONTENT_DIRECTORY}"><ObjectID>{}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"#,
        xml::escape(object_id)
    )
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowseResult {
    pub objects: Vec<DidlObject>,
    pub number_returned: u32,
    pub total_matches: u32,
}

/// Parse a Browse SOAP response (the DIDL-Lite document is escaped inside
/// `<Result>`).
pub fn parse_browse_response(body: &str) -> Result<BrowseResult> {
    let root = xml::parse(body)?;
    if let Some(fault) = root.find("Fault") {
        let desc = fault
            .find("errorDescription")
            .map(|e| e.text.trim().to_string())
            .unwrap_or_else(|| "SOAP fault".into());
        return Err(SourceError::Protocol(format!("Browse failed: {desc}")));
    }
    let resp = root
        .find("BrowseResponse")
        .ok_or_else(|| SourceError::parse("no BrowseResponse"))?;
    let didl = resp.child_text("Result").unwrap_or("");
    let objects = if didl.is_empty() {
        Vec::new()
    } else {
        parse_didl(didl)?
    };
    let n = |k: &str| resp.child_text(k).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok(BrowseResult {
        number_returned: n("NumberReturned"),
        total_matches: n("TotalMatches"),
        objects,
    })
}

/// One DIDL-Lite `<res>` element.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DidlResource {
    pub url: String,
    pub protocol_info: String,
    pub size: Option<u64>,
    pub duration_secs: Option<f64>,
    pub resolution: Option<(u32, u32)>,
}

impl DidlResource {
    /// MIME type from protocolInfo (`http-get:*:video/mp4:...`).
    pub fn mime(&self) -> &str {
        self.protocol_info.split(':').nth(2).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DidlObject {
    pub id: String,
    pub parent_id: String,
    pub is_container: bool,
    pub title: String,
    /// upnp:class, e.g. `object.item.videoItem`.
    pub class: String,
    pub child_count: Option<u32>,
    pub resources: Vec<DidlResource>,
    pub album_art: Option<String>,
}

impl DidlObject {
    /// Best video resource: video MIME with the most pixels, else the first.
    pub fn best_resource(&self) -> Option<&DidlResource> {
        self.resources
            .iter()
            .filter(|r| r.mime().starts_with("video/") && r.protocol_info.starts_with("http-get"))
            .max_by_key(|r| r.resolution.map_or(0, |(w, h)| w as u64 * h as u64))
            .or_else(|| self.resources.iter().find(|r| r.url.starts_with("http")))
    }

    pub fn is_video(&self) -> bool {
        self.class.starts_with("object.item.videoItem")
            || self
                .resources
                .iter()
                .any(|r| r.mime().starts_with("video/"))
    }
}

/// `H+:MM:SS[.F+]` or `H+:MM:SS.F0/F1`.
pub fn parse_didl_duration(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec = parts.next()?;
    let secs = match sec.split_once('/') {
        Some((a, b)) => {
            // "SS.F0/F1": fraction F0/F1.
            let (whole, f0) = a.split_once('.').unwrap_or((a, "0"));
            let f1: f64 = b.parse().ok()?;
            whole.parse::<f64>().ok()?
                + if f1 > 0.0 {
                    f0.parse::<f64>().ok()? / f1
                } else {
                    0.0
                }
        }
        None => sec.parse().ok()?,
    };
    Some(h * 3600.0 + m * 60.0 + secs)
}

/// Parse a DIDL-Lite document.
pub fn parse_didl(didl: &str) -> Result<Vec<DidlObject>> {
    let root = xml::parse(didl)?;
    let mut out = Vec::new();
    for el in &root.children {
        let is_container = match el.name.as_str() {
            "container" => true,
            "item" => false,
            _ => continue,
        };
        let resources = el
            .children_named("res")
            .map(|r| DidlResource {
                url: r.text.trim().to_string(),
                protocol_info: r.attr("protocolInfo").unwrap_or("").to_string(),
                size: r.attr("size").and_then(|v| v.parse().ok()),
                duration_secs: r.attr("duration").and_then(parse_didl_duration),
                resolution: r.attr("resolution").and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                }),
            })
            .filter(|r| !r.url.is_empty())
            .collect();
        out.push(DidlObject {
            id: el.attr("id").unwrap_or("").to_string(),
            parent_id: el.attr("parentID").unwrap_or("").to_string(),
            is_container,
            title: el.child_text("title").unwrap_or("").to_string(),
            class: el.child_text("class").unwrap_or("").to_string(),
            child_count: el.attr("childCount").and_then(|v| v.parse().ok()),
            resources,
            album_art: el
                .child_text("albumArtURI")
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    Ok(out)
}

/// A discovered media server.
#[derive(Debug, Clone, PartialEq)]
pub struct DlnaServer {
    pub location: String,
    pub description: DeviceDescription,
}

/// SSDP-discover MediaServer:1 devices and fetch their descriptions.
pub async fn discover_media_servers(
    client: &HttpClient,
    timeout: Duration,
) -> Result<Vec<DlnaServer>> {
    let replies = ssdp::search(ssdp::MEDIA_SERVER_ST, timeout).await?;
    let mut out = Vec::new();
    for r in replies {
        match fetch_description(client, &r.location).await {
            Ok(d) if d.content_directory_url.is_some() => {
                if !out
                    .iter()
                    .any(|s: &DlnaServer| s.description.udn == d.udn && !d.udn.is_empty())
                {
                    out.push(DlnaServer {
                        location: r.location,
                        description: d,
                    });
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("ignoring SSDP reply from {}: {e}", r.location),
        }
    }
    Ok(out)
}

pub async fn fetch_description(client: &HttpClient, location: &str) -> Result<DeviceDescription> {
    let body = client.get_text(location).await?;
    parse_device_description(&body, location)
}

/// Browse one container of a media server.
pub struct DlnaSource {
    client: HttpClient,
    description: DeviceDescription,
    control_url: String,
}

impl DlnaSource {
    /// Fetch the device description at `location` and set up a source.
    pub async fn connect(client: HttpClient, location: &str) -> Result<Self> {
        let description = fetch_description(&client, location).await?;
        Self::from_description(client, description)
    }

    pub fn from_description(client: HttpClient, description: DeviceDescription) -> Result<Self> {
        let control_url = description.content_directory_url.clone().ok_or_else(|| {
            SourceError::Unsupported(format!(
                "{} has no ContentDirectory service",
                description.friendly_name
            ))
        })?;
        Ok(DlnaSource {
            client,
            description,
            control_url,
        })
    }

    pub fn description(&self) -> &DeviceDescription {
        &self.description
    }

    fn udn_host(&self) -> String {
        let u = self.description.udn.trim_start_matches("uuid:");
        if u.is_empty() {
            "server".into()
        } else {
            u.to_string()
        }
    }

    fn dir_uri(&self, object_id: &str) -> String {
        format!(
            "dlna://{}/{}",
            self.udn_host(),
            utf8_percent_encode(object_id, NON_ALPHANUMERIC)
        )
    }

    fn object_id_from_uri(dir: &str) -> String {
        if dir.is_empty() {
            return "0".into();
        }
        let rest = dir.strip_prefix("dlna://").unwrap_or(dir);
        let id = rest.split_once('/').map(|(_, id)| id).unwrap_or("");
        if id.is_empty() {
            "0".into()
        } else {
            percent_decode_str(id).decode_utf8_lossy().into_owned()
        }
    }

    /// Browse all direct children of `object_id`, paging through results.
    pub async fn browse(&self, object_id: &str) -> Result<Vec<DidlObject>> {
        const PAGE: u32 = 200;
        let mut out = Vec::new();
        let mut start = 0u32;
        loop {
            let mut h = HeaderMap::new();
            h.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/xml; charset=\"utf-8\""),
            );
            h.insert(
                "SOAPACTION",
                HeaderValue::from_str(&format!("\"{CONTENT_DIRECTORY}#Browse\"")).expect("ascii"),
            );
            let body = browse_request_body(object_id, start, PAGE);
            let resp = self
                .client
                .send(Method::POST, &self.control_url, h, Some(Bytes::from(body)))
                .await?;
            // SOAP faults come back as HTTP 500 with a body worth reading.
            let status = resp.status();
            let text = resp.text().await?;
            let res = match parse_browse_response(&text) {
                Ok(r) => r,
                Err(e) if status.is_success() => return Err(e),
                Err(SourceError::Protocol(p)) => return Err(SourceError::Protocol(p)),
                Err(_) => {
                    return Err(SourceError::Status {
                        status: status.as_u16(),
                        url: self.control_url.clone(),
                    })
                }
            };
            let got = res.number_returned.max(res.objects.len() as u32);
            out.extend(res.objects);
            start += got;
            if got == 0 || start >= res.total_matches {
                break;
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Source for DlnaSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Dlna
    }

    fn root_uri(&self) -> String {
        self.dir_uri("0")
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let objects = self.browse(&Self::object_id_from_uri(dir)).await?;
        Ok(objects
            .into_iter()
            .filter_map(|o| {
                if o.is_container {
                    return Some(Entry {
                        name: o.title.clone(),
                        uri: self.dir_uri(&o.id),
                        is_dir: true,
                        ..Default::default()
                    });
                }
                if !o.is_video() {
                    return None;
                }
                let res = o.best_resource()?;
                Some(Entry {
                    name: o.title.clone(),
                    uri: res.url.clone(),
                    is_dir: false,
                    size: res.size,
                    mtime: None,
                    thumbnail: o.album_art.clone(),
                    duration_secs: res.duration_secs,
                    is_media: true,
                })
            })
            .collect())
    }

    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
        if !uri.starts_with("http://") && !uri.starts_with("https://") {
            return Err(SourceError::InvalidUri(format!(
                "DLNA items are HTTP resources, got {uri}"
            )));
        }
        Ok(Box::new(HttpFile::open(self.client.clone(), uri).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_XML: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
<device><deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType><friendlyName>NAS: minidlna</friendlyName>
<manufacturer>Justin Maggard</manufacturer><modelName>Windows Media Connect compatible (MiniDLNA)</modelName>
<UDN>uuid:4d696e69-444c-164e-9d41-001c42f1b5e6</UDN>
<iconList><icon><mimetype>image/png</mimetype><width>48</width><height>48</height><url>/icons/sm.png</url></icon>
<icon><mimetype>image/png</mimetype><width>120</width><height>120</height><url>/icons/lrg.png</url></icon></iconList>
<serviceList>
<service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/ctl/ConnectionMgr</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId><controlURL>/ctl/ContentDir</controlURL><eventSubURL>/evt/ContentDir</eventSubURL><SCPDURL>/ContentDir.xml</SCPDURL></service>
</serviceList></device></root>"#;

    const BROWSE_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>&lt;DIDL-Lite xmlns:dc=&quot;http://purl.org/dc/elements/1.1/&quot; xmlns:upnp=&quot;urn:schemas-upnp-org:metadata-1-0/upnp/&quot; xmlns=&quot;urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/&quot;&gt;
&lt;container id=&quot;64$0&quot; parentID=&quot;64&quot; restricted=&quot;1&quot; childCount=&quot;12&quot;&gt;&lt;dc:title&gt;VR &amp;amp; 3D&lt;/dc:title&gt;&lt;upnp:class&gt;object.container.storageFolder&lt;/upnp:class&gt;&lt;/container&gt;
&lt;item id=&quot;64$1&quot; parentID=&quot;64&quot; restricted=&quot;1&quot;&gt;&lt;dc:title&gt;Scene_180_LR&lt;/dc:title&gt;&lt;upnp:class&gt;object.item.videoItem&lt;/upnp:class&gt;
&lt;res size=&quot;2000&quot; duration=&quot;0:30:01.500&quot; resolution=&quot;1920x960&quot; protocolInfo=&quot;http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC&quot;&gt;http://192.168.1.10:8200/MediaItems/21.mp4&lt;/res&gt;
&lt;res size=&quot;9000&quot; duration=&quot;0:30:01.500&quot; resolution=&quot;5760x2880&quot; protocolInfo=&quot;http-get:*:video/mp4:*&quot;&gt;http://192.168.1.10:8200/MediaItems/22.mp4&lt;/res&gt;
&lt;upnp:albumArtURI&gt;http://192.168.1.10:8200/AlbumArt/22-1.jpg&lt;/upnp:albumArtURI&gt;&lt;/item&gt;
&lt;item id=&quot;64$2&quot; parentID=&quot;64&quot;&gt;&lt;dc:title&gt;song&lt;/dc:title&gt;&lt;upnp:class&gt;object.item.audioItem.musicTrack&lt;/upnp:class&gt;&lt;res protocolInfo=&quot;http-get:*:audio/mpeg:*&quot;&gt;http://x/1.mp3&lt;/res&gt;&lt;/item&gt;
&lt;/DIDL-Lite&gt;</Result><NumberReturned>3</NumberReturned><TotalMatches>3</TotalMatches><UpdateID>7</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#;

    #[test]
    fn device_description() {
        let d =
            parse_device_description(DEVICE_XML, "http://192.168.1.10:8200/rootDesc.xml").unwrap();
        assert_eq!(d.friendly_name, "NAS: minidlna");
        assert_eq!(d.udn, "uuid:4d696e69-444c-164e-9d41-001c42f1b5e6");
        assert_eq!(
            d.content_directory_url.as_deref(),
            Some("http://192.168.1.10:8200/ctl/ContentDir")
        );
        assert_eq!(
            d.icon_url.as_deref(),
            Some("http://192.168.1.10:8200/icons/lrg.png")
        );
    }

    #[test]
    fn embedded_device_and_urlbase() {
        let xml = r#"<root><URLBase>http://10.0.0.2:9000/base/</URLBase><device><deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType><friendlyName>Hub</friendlyName>
            <deviceList><device><friendlyName>Inner</friendlyName><UDN>uuid:inner</UDN><serviceList><service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:2</serviceType><controlURL>cd/control</controlURL></service></serviceList></device></deviceList></device></root>"#;
        let d = parse_device_description(xml, "http://ignored/").unwrap();
        assert_eq!(d.friendly_name, "Inner");
        assert_eq!(
            d.content_directory_url.as_deref(),
            Some("http://10.0.0.2:9000/base/cd/control")
        );
    }

    #[test]
    fn browse_and_didl() {
        let r = parse_browse_response(BROWSE_XML).unwrap();
        assert_eq!((r.number_returned, r.total_matches), (3, 3));
        assert_eq!(r.objects.len(), 3);
        let c = &r.objects[0];
        assert!(c.is_container);
        assert_eq!(c.title, "VR & 3D");
        assert_eq!(c.id, "64$0");
        assert_eq!(c.child_count, Some(12));
        let v = &r.objects[1];
        assert!(v.is_video());
        let best = v.best_resource().unwrap();
        assert_eq!(best.url, "http://192.168.1.10:8200/MediaItems/22.mp4");
        assert_eq!(best.size, Some(9000));
        assert_eq!(best.duration_secs, Some(1801.5));
        assert_eq!(best.mime(), "video/mp4");
        assert_eq!(
            v.album_art.as_deref(),
            Some("http://192.168.1.10:8200/AlbumArt/22-1.jpg")
        );
        assert!(!r.objects[2].is_video());
    }

    #[test]
    fn soap_fault_and_request() {
        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert!(
            matches!(parse_browse_response(fault), Err(SourceError::Protocol(m)) if m.contains("No such object"))
        );
        let body = browse_request_body("64$0&x", 0, 200);
        assert!(body.contains("<ObjectID>64$0&amp;x</ObjectID>"));
        assert!(body.contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>"));
        let parsed = xml::parse(&body).unwrap();
        assert_eq!(parsed.find("ObjectID").unwrap().text, "64$0&x");
    }

    #[test]
    fn durations_and_uris() {
        assert_eq!(parse_didl_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_didl_duration("0:00:01.5/2"), Some(1.0 + 5.0 / 2.0));
        assert_eq!(parse_didl_duration("x"), None);
        assert_eq!(DlnaSource::object_id_from_uri(""), "0");
        assert_eq!(
            DlnaSource::object_id_from_uri("dlna://uuid-1/64%240"),
            "64$0"
        );
        let d =
            parse_device_description(DEVICE_XML, "http://192.168.1.10:8200/rootDesc.xml").unwrap();
        let s = DlnaSource::from_description(HttpClient::new(None).unwrap(), d).unwrap();
        assert_eq!(
            s.root_uri(),
            "dlna://4d696e69-444c-164e-9d41-001c42f1b5e6/0"
        );
        assert_eq!(DlnaSource::object_id_from_uri(&s.dir_uri("64$0")), "64$0");
    }
}
