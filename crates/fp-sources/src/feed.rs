//! Library managers that publish VR scenes as JSON: the DeoVR feed
//! (`/deovr` in XBVR, Stash plugins, SLR-style sites) and the HereSphere
//! JSON API (`/heresphere` in XBVR and Stash).
//!
//! Both map to the same shape: the root lists scene groups as folders, a
//! group lists its scenes as videos, and [`Source::details`] fetches one
//! scene's full description (declared projection and stereo layout,
//! scripts, subtitles, markers). [`Source::open`] resolves a scene to its
//! best media URL (the tallest encoding not above
//! [`FeedConfig::max_height`]) and reads it over HTTP.
//!
//! Parsing is tolerant: numbers may arrive as strings, unknown fields are
//! ignored and missing ones fall back to sensible defaults.
//!
//! Locations: group folders are `<feed url>#group=<encoded name>`, scenes
//! are their scene/API URL, scripts and subtitles their direct URL.

use crate::config::{Credentials, FeedConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions, HttpReply};
use crate::httpdir::clean_url_and_credentials;
use crate::sidecar::Sidecars;
use crate::urlutil::{
    encode_form_value, encode_segment, is_http_url, last_segment, percent_decode, redact, resolve,
};
use crate::{Source, dir_entry};
use fp_core::format::{Projection, StereoLayout, VideoFormat, detect_from_name};
use fp_core::source::{
    ByteSource, Entry, EntryKind, is_script_name, is_subtitle_name, is_video_name,
};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Largest JSON document accepted from a feed.
const JSON_LIMIT: u64 = 64 << 20;

/// Parallel requests when a HereSphere group is listed.
const DETAIL_WORKERS: usize = 6;

/// One encoding of a scene.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaSource {
    /// Absolute media URL.
    pub url: String,
    /// Frame height in pixels, when known.
    pub height: Option<u32>,
    /// Frame width in pixels, when known.
    pub width: Option<u32>,
    /// Encoding name (`h265`, `h264`, ...).
    pub label: String,
    /// File size, when the feed says.
    pub size: Option<u64>,
}

/// Everything a feed says about one scene.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SceneInfo {
    /// Scene title.
    pub title: String,
    /// Length in seconds.
    pub duration: Option<f64>,
    /// Poster image URL.
    pub thumbnail_url: Option<String>,
    /// Projection and stereo layout the feed declares.
    pub format: Option<VideoFormat>,
    /// Available encodings.
    pub sources: Vec<MediaSource>,
    /// Haptic script URLs.
    pub scripts: Vec<String>,
    /// Subtitle URLs.
    pub subtitles: Vec<String>,
    /// Named timestamps in seconds.
    pub markers: Vec<(f64, String)>,
}

impl SceneInfo {
    /// The tallest source not above `max_height`; when every source is
    /// taller, the smallest one. Sources of unknown height rank lowest.
    pub fn best_source(&self, max_height: Option<u32>) -> Option<&MediaSource> {
        let h = |s: &MediaSource| s.height.unwrap_or(0);
        let fits = |s: &&MediaSource| max_height.is_none_or(|m| h(s) <= m);
        // `max_by_key` keeps the last of equal keys; reverse to prefer the
        // first listed encoding among equals.
        self.sources
            .iter()
            .rev()
            .filter(fits)
            .max_by_key(|s| h(s))
            .or_else(|| self.sources.iter().min_by_key(|s| h(s)))
    }

    /// Copies the scene's details into a listing entry (keeping its
    /// location).
    pub fn apply_to(&self, entry: &mut Entry) {
        if !self.title.is_empty() {
            entry.name = self.title.clone();
        }
        entry.kind = EntryKind::Video;
        entry.duration = self.duration.or(entry.duration);
        if self.thumbnail_url.is_some() {
            entry.thumbnail_url = self.thumbnail_url.clone();
        }
        entry.format = self
            .format
            .or_else(|| detect_from_name(&entry.name))
            .or(entry.format);
        if entry.size.is_none() {
            entry.size = self.best_source(None).and_then(|s| s.size);
        }
        entry.scripts = self.scripts.clone();
        entry.subtitles = self.subtitles.clone();
        entry.markers = self.markers.clone();
    }
}

// ---- tolerant JSON accessors ----

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

fn num(v: &Value, key: &str) -> Option<f64> {
    get(v, key).and_then(as_f64)
}

fn uint(v: &Value, key: &str) -> Option<u32> {
    num(v, key)
        .filter(|x| *x > 0.0 && *x < 1e7)
        .map(|x| x as u32)
}

fn text(v: &Value, key: &str) -> Option<String> {
    match get(v, key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn boolean(v: &Value, key: &str) -> Option<bool> {
    match get(v, key)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|x| x != 0.0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    get(v, key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn url_field(v: &Value, key: &str, base: &str) -> Option<String> {
    text(v, key).and_then(|u| resolve(base, &u).ok())
}

fn media_source(v: &Value, label: &str, base: &str) -> Option<MediaSource> {
    let url = url_field(v, "url", base)?;
    let height = uint(v, "height").or_else(|| uint(v, "resolution"));
    Some(MediaSource {
        url,
        height,
        width: uint(v, "width"),
        label: label.to_string(),
        size: num(v, "size").filter(|s| *s > 0.0).map(|s| s as u64),
    })
}

// ---- DeoVR ----

/// Projection for a DeoVR `screenType`.
fn deovr_projection(screen_type: &str) -> Option<Projection> {
    Some(match screen_type.trim().to_ascii_lowercase().as_str() {
        "dome" | "180" => Projection::EQUIRECT_180,
        "sphere" | "360" => Projection::EQUIRECT_360,
        "flat" => Projection::Flat,
        "fisheye" => Projection::fisheye(180.0),
        "mkx200" => Projection::fisheye(200.0),
        "mkx220" | "vrca220" => Projection::fisheye(220.0),
        "rf52" => Projection::fisheye(190.0),
        _ => return None,
    })
}

fn stereo_from(mode: &str) -> Option<StereoLayout> {
    Some(match mode.trim().to_ascii_lowercase().as_str() {
        "sbs" | "lr" | "sidebyside" => StereoLayout::SideBySide,
        "tb" | "ou" | "topbottom" => StereoLayout::TopBottom,
        "off" | "mono" | "none" | "" => StereoLayout::Mono,
        _ => return None,
    })
}

/// Declared format of a DeoVR scene, `None` when it says nothing.
fn deovr_format(v: &Value) -> Option<VideoFormat> {
    let is3d = boolean(v, "is3d");
    let screen = text(v, "screenType").and_then(|s| deovr_projection(&s));
    let stereo = text(v, "stereoMode").and_then(|s| stereo_from(&s));
    if is3d.is_none() && screen.is_none() && stereo.is_none() {
        return None;
    }
    let (projection, stereo) = match is3d {
        Some(false) if screen.is_none() => (Projection::Flat, stereo.unwrap_or_default()),
        _ => (
            screen.unwrap_or(Projection::EQUIRECT_180),
            stereo.unwrap_or(if is3d == Some(false) {
                StereoLayout::Mono
            } else {
                StereoLayout::SideBySide
            }),
        ),
    };
    Some(VideoFormat::new(projection, stereo))
}

/// Parses a DeoVR scene document fetched from `base`.
pub fn parse_deovr_scene(v: &Value, base: &str) -> SceneInfo {
    let mut sources = Vec::new();
    for enc in array(v, "encodings") {
        let label = text(enc, "name").unwrap_or_default();
        for s in array(enc, "videoSources") {
            if let Some(m) = media_source(s, &label, base) {
                sources.push(m);
            }
        }
    }
    // Some feeds put a single URL on the scene itself.
    if sources.is_empty() {
        if let Some(url) =
            url_field(v, "videoUrl", base).or_else(|| url_field(v, "video_url", base))
        {
            if is_video_name(&last_segment(&url)) {
                sources.push(MediaSource {
                    url,
                    ..MediaSource::default()
                });
            }
        }
    }
    let mut markers: Vec<(f64, String)> = array(v, "timeStamps")
        .iter()
        .filter_map(|t| Some((num(t, "ts")?, text(t, "name").unwrap_or_default())))
        .collect();
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    SceneInfo {
        title: text(v, "title").unwrap_or_default(),
        duration: num(v, "videoLength").filter(|d| *d > 0.0),
        thumbnail_url: url_field(v, "thumbnailUrl", base),
        format: deovr_format(v),
        sources,
        scripts: array(v, "fleshlight")
            .iter()
            .filter_map(|s| url_field(s, "url", base))
            .collect(),
        subtitles: array(v, "subtitles")
            .iter()
            .filter_map(|s| url_field(s, "url", base))
            .collect(),
        markers,
    }
}

/// A group of scenes in a feed's root.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneGroup {
    /// Group name ("Recent", "Favourites", ...).
    pub name: String,
    /// Scenes, as video entries located at their scene URL.
    pub scenes: Vec<Entry>,
}

/// Parses a DeoVR feed root (`{"scenes":[{"name","list":[...]}]}`).
pub fn parse_deovr_root(v: &Value, base: &str) -> Vec<SceneGroup> {
    array(v, "scenes")
        .iter()
        .enumerate()
        .map(|(i, g)| SceneGroup {
            name: text(g, "name").unwrap_or_else(|| format!("Group {}", i + 1)),
            scenes: array(g, "list")
                .iter()
                .filter_map(|s| {
                    let url = url_field(s, "video_url", base)
                        .or_else(|| url_field(s, "videoUrl", base))?;
                    let title = text(s, "title").unwrap_or_else(|| last_segment(&url));
                    let mut e = Entry::new(title, url, EntryKind::Video);
                    e.duration = num(s, "videoLength").filter(|d| *d > 0.0);
                    e.thumbnail_url = url_field(s, "thumbnailUrl", base);
                    e.format = detect_from_name(&e.name);
                    Some(e)
                })
                .collect(),
        })
        .collect()
}

// ---- HereSphere ----

/// Declared format of a HereSphere video.
fn heresphere_format(v: &Value) -> Option<VideoFormat> {
    let projection = text(v, "projection")?;
    let fov = num(v, "fov")
        .filter(|f| *f > 0.0 && *f <= 360.0)
        .map(|f| f as f32);
    let lens = text(v, "lens").unwrap_or_default().to_ascii_uppercase();
    let projection = match projection.to_ascii_lowercase().as_str() {
        "equirectangular" => Projection::Equirect {
            h_fov: fov.unwrap_or(180.0),
            v_fov: 180.0,
        },
        "equirectangular360" => Projection::EQUIRECT_360,
        "fisheye" => match lens.as_str() {
            "MKX220" | "VRCA220" => Projection::fisheye(220.0),
            "MKX200" => Projection::fisheye(200.0),
            _ => Projection::fisheye(fov.unwrap_or(180.0)),
        },
        "perspective" => Projection::Flat,
        // fp-core has no plain cubemap; EAC is the closest renderer.
        "cubemap" | "equiangularcubemap" => Projection::Eac { h_fov: 360.0 },
        _ => return None,
    };
    let stereo = text(v, "stereo")
        .and_then(|s| stereo_from(&s))
        .unwrap_or_default();
    Some(VideoFormat::new(projection, stereo))
}

/// Parses a HereSphere video document fetched from `base`.
pub fn parse_heresphere_video(v: &Value, base: &str) -> SceneInfo {
    let mut sources = Vec::new();
    for m in array(v, "media") {
        let label = text(m, "name").unwrap_or_default();
        for s in array(m, "sources") {
            if let Some(src) = media_source(s, &label, base) {
                sources.push(src);
            }
        }
    }
    let mut markers: Vec<(f64, String)> = array(v, "tags")
        .iter()
        .filter_map(|t| {
            let start = num(t, "start")?;
            let name = text(t, "name")?;
            Some((start / 1000.0, name))
        })
        .collect();
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));
    SceneInfo {
        title: text(v, "title").unwrap_or_default(),
        duration: num(v, "duration")
            .filter(|d| *d > 0.0)
            .map(|ms| ms / 1000.0),
        thumbnail_url: url_field(v, "thumbnailImage", base),
        format: heresphere_format(v),
        sources,
        scripts: array(v, "scripts")
            .iter()
            .filter_map(|s| url_field(s, "url", base))
            .collect(),
        subtitles: array(v, "subtitles")
            .iter()
            .filter_map(|s| url_field(s, "url", base))
            .collect(),
        markers,
    }
}

/// Parses a HereSphere library (`{"library":[{"name","list":[urls]}]}`).
/// Scenes carry only their URL (named after its last segment) until their
/// details are fetched.
pub fn parse_heresphere_library(v: &Value, base: &str) -> Result<Vec<SceneGroup>> {
    if num(v, "access").is_some_and(|a| a < 0.0) {
        return Err(Error::Auth(redact(base)));
    }
    Ok(array(v, "library")
        .iter()
        .enumerate()
        .map(|(i, g)| SceneGroup {
            name: text(g, "name").unwrap_or_else(|| format!("Group {}", i + 1)),
            scenes: array(g, "list")
                .iter()
                .filter_map(|u| u.as_str())
                .filter_map(|u| resolve(base, u).ok())
                .map(|url| {
                    let name = last_segment(&url);
                    Entry::new(name, url, EntryKind::Video)
                })
                .collect(),
        })
        .collect())
}

// ---- shared plumbing ----

/// Location of a group folder.
fn group_location(feed_url: &str, name: &str) -> String {
    format!("{feed_url}#group={}", encode_segment(name))
}

/// Splits a group location into feed URL and group name.
fn split_group(location: &str) -> Option<(&str, String)> {
    let (feed, name) = location.split_once("#group=")?;
    Some((feed, percent_decode(name)))
}

/// True when a URL obviously points at a file rather than a scene document.
fn is_direct_file(url: &str) -> bool {
    let name = last_segment(url);
    is_video_name(&name) || is_script_name(&name) || is_subtitle_name(&name)
}

/// True when a response looks like JSON rather than media.
fn is_json_reply(r: &HttpReply) -> bool {
    match r.header("content-type") {
        None => true,
        Some(ct) => {
            let ct = ct.to_ascii_lowercase();
            ct.contains("json") || ct.starts_with("text/") || ct.contains("javascript")
        }
    }
}

struct FeedCore {
    id: String,
    name: String,
    url: String,
    creds: Option<Credentials>,
    client: HttpClient,
    max_height: Option<u32>,
}

impl std::fmt::Debug for FeedCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedCore")
            .field("id", &self.id)
            .field("url", &redact(&self.url))
            .field("credentials", &self.creds)
            .field("max_height", &self.max_height)
            .finish_non_exhaustive()
    }
}

impl FeedCore {
    fn new(config: &FeedConfig) -> Result<FeedCore> {
        if !is_http_url(&config.url) {
            return Err(Error::invalid(
                &config.url,
                "expected an http:// or https:// feed URL",
            ));
        }
        let (url, creds) = clean_url_and_credentials(&config.url, config.credentials.as_ref())?;
        let opts = HttpOptions {
            insecure_tls: config.insecure_tls,
            ..HttpOptions::default()
        };
        Ok(FeedCore {
            id: config.id.clone(),
            name: config.name.clone(),
            client: HttpClient::new(opts, creds.as_ref()),
            url,
            creds,
            max_height: config.max_height,
        })
    }

    /// Sends a request and parses the JSON answer. `Ok(None)` when the
    /// server answered with something that is not JSON (a media stream),
    /// without downloading it.
    fn json(
        &self,
        method: &str,
        url: &str,
        content_type: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Option<Value>> {
        let mut headers = vec![("Accept", "application/json")];
        if let Some(ct) = content_type {
            headers.push(("Content-Type", ct));
        }
        let (reply, read) =
            self.client
                .request_if(method, url, &headers, body, JSON_LIMIT, is_json_reply)?;
        let reply = reply.error_for_status(url)?;
        if !read {
            return Ok(None);
        }
        serde_json::from_slice(&reply.body)
            .map(Some)
            .map_err(|e| Error::parse(format!("JSON from {}", redact(url)), e))
    }

    fn open_url(&self, url: &str) -> Result<Arc<dyn ByteSource>> {
        Ok(Arc::new(self.client.open(url)?))
    }

    fn group_entries(&self, groups: &[SceneGroup]) -> Vec<Entry> {
        groups
            .iter()
            .map(|g| {
                let mut e = dir_entry(g.name.clone(), group_location(&self.url, &g.name), None);
                e.thumbnail_url = g.scenes.iter().find_map(|s| s.thumbnail_url.clone());
                e
            })
            .collect()
    }

    fn pick(&self, scene: &SceneInfo, location: &str) -> Result<String> {
        scene
            .best_source(self.max_height)
            .map(|s| s.url.clone())
            .ok_or_else(|| Error::NotFound(format!("no playable source in {}", redact(location))))
    }
}

fn find_group(groups: Vec<SceneGroup>, name: &str, location: &str) -> Result<Vec<Entry>> {
    groups
        .into_iter()
        .find(|g| g.name == name)
        .map(|g| g.scenes)
        .ok_or_else(|| Error::NotFound(redact(location)))
}

/// A DeoVR JSON feed (XBVR `/deovr`, Stash and others).
///
/// With credentials, documents are fetched with a `POST` of the form
/// fields `login` and `password` (the DeoVR app's login); otherwise with
/// `GET`.
#[derive(Debug)]
pub struct DeoVrSource {
    core: FeedCore,
}

impl DeoVrSource {
    /// Creates the source.
    pub fn new(config: &FeedConfig) -> Result<DeoVrSource> {
        Ok(DeoVrSource {
            core: FeedCore::new(config)?,
        })
    }

    fn fetch(&self, url: &str) -> Result<Option<Value>> {
        match &self.core.creds {
            Some(c) => {
                let form = format!(
                    "login={}&password={}",
                    encode_form_value(&c.username),
                    encode_form_value(&c.password)
                );
                self.core.json(
                    "POST",
                    url,
                    Some("application/x-www-form-urlencoded"),
                    Some(form.as_bytes()),
                )
            }
            None => self.core.json("GET", url, None, None),
        }
    }

    fn fetch_required(&self, url: &str) -> Result<Value> {
        self.fetch(url)?
            .ok_or_else(|| Error::parse("DeoVR feed", format!("{} is not JSON", redact(url))))
    }

    fn groups(&self, feed_url: &str) -> Result<Vec<SceneGroup>> {
        let v = self.fetch_required(feed_url)?;
        Ok(parse_deovr_root(&v, feed_url))
    }

    /// Fetches and parses one scene.
    pub fn scene(&self, scene_url: &str) -> Result<SceneInfo> {
        let v = self.fetch_required(scene_url)?;
        Ok(parse_deovr_scene(&v, scene_url))
    }
}

impl Source for DeoVrSource {
    fn id(&self) -> &str {
        &self.core.id
    }

    fn name(&self) -> &str {
        &self.core.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::DeoVr
    }

    fn describe(&self) -> String {
        format!("DeoVR feed {}", redact(&self.core.url))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        match location {
            None => {
                let groups = self.groups(&self.core.url)?;
                Ok(self.core.group_entries(&groups))
            }
            Some(loc) => match split_group(loc) {
                Some((feed, name)) => find_group(self.groups(feed)?, &name, loc),
                None => {
                    let groups = self.groups(loc)?;
                    Ok(self.core.group_entries(&groups))
                }
            },
        }
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        if is_direct_file(location) {
            return self.core.open_url(location);
        }
        match self.fetch(location)? {
            Some(v) => {
                let scene = parse_deovr_scene(&v, location);
                self.core.open_url(&self.core.pick(&scene, location)?)
            }
            None => self.core.open_url(location),
        }
    }

    fn details(&self, entry: &Entry) -> Result<Entry> {
        if entry.kind != EntryKind::Video || is_direct_file(&entry.location) {
            return Ok(entry.clone());
        }
        let mut e = entry.clone();
        self.scene(&entry.location)?.apply_to(&mut e);
        Ok(e)
    }

    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        let full = if video.scripts.is_empty() && video.subtitles.is_empty() {
            self.details(video)?
        } else {
            video.clone()
        };
        Ok(Sidecars::from_entry(&full))
    }
}

/// A HereSphere JSON API (XBVR and Stash `/heresphere`).
///
/// Every request is a `POST` with a JSON body carrying `username` and
/// `password` when credentials are configured.
#[derive(Debug)]
pub struct HereSphereSource {
    core: FeedCore,
}

impl HereSphereSource {
    /// Creates the source.
    pub fn new(config: &FeedConfig) -> Result<HereSphereSource> {
        Ok(HereSphereSource {
            core: FeedCore::new(config)?,
        })
    }

    fn post(&self, url: &str, needs_media: Option<bool>) -> Result<Option<Value>> {
        let mut body = serde_json::Map::new();
        if let Some(c) = &self.core.creds {
            body.insert("username".into(), Value::String(c.username.clone()));
            body.insert("password".into(), Value::String(c.password.clone()));
        }
        if let Some(n) = needs_media {
            body.insert("needsMediaSource".into(), Value::Bool(n));
        }
        let bytes = serde_json::to_vec(&Value::Object(body))?;
        self.core
            .json("POST", url, Some("application/json"), Some(&bytes))
    }

    fn post_required(&self, url: &str, needs_media: Option<bool>) -> Result<Value> {
        self.post(url, needs_media)?
            .ok_or_else(|| Error::parse("HereSphere API", format!("{} is not JSON", redact(url))))
    }

    fn groups(&self, feed_url: &str) -> Result<Vec<SceneGroup>> {
        let v = self.post_required(feed_url, None)?;
        parse_heresphere_library(&v, feed_url)
    }

    /// Fetches and parses one video. `with_media` asks for media sources
    /// (slower on some servers).
    pub fn video(&self, url: &str, with_media: bool) -> Result<SceneInfo> {
        let v = self.post_required(url, Some(with_media))?;
        Ok(parse_heresphere_video(&v, url))
    }

    /// Fills in titles and details for a group's scenes, several requests
    /// at a time. Scenes whose details fail keep their URL-derived name.
    fn fill(&self, scenes: Vec<Entry>) -> Vec<Entry> {
        let next = AtomicUsize::new(0);
        let slots: Vec<std::sync::Mutex<Entry>> =
            scenes.into_iter().map(std::sync::Mutex::new).collect();
        std::thread::scope(|scope| {
            for _ in 0..DETAIL_WORKERS.min(slots.len()) {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(slot) = slots.get(i) else { break };
                        let mut e = crate::cache::lock(slot);
                        match self.video(&e.location, false) {
                            Ok(info) => info.apply_to(&mut e),
                            Err(err) => log::debug!(
                                "HereSphere details for {} failed: {err}",
                                redact(&e.location)
                            ),
                        }
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|m| {
                m.into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect()
    }
}

impl Source for HereSphereSource {
    fn id(&self) -> &str {
        &self.core.id
    }

    fn name(&self) -> &str {
        &self.core.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::HereSphere
    }

    fn describe(&self) -> String {
        format!("HereSphere API {}", redact(&self.core.url))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        match location {
            None => {
                let groups = self.groups(&self.core.url)?;
                Ok(self.core.group_entries(&groups))
            }
            Some(loc) => match split_group(loc) {
                Some((feed, name)) => Ok(self.fill(find_group(self.groups(feed)?, &name, loc)?)),
                None => {
                    let groups = self.groups(loc)?;
                    Ok(self.core.group_entries(&groups))
                }
            },
        }
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        if is_direct_file(location) {
            return self.core.open_url(location);
        }
        match self.post(location, Some(true))? {
            Some(v) => {
                let scene = parse_heresphere_video(&v, location);
                self.core.open_url(&self.core.pick(&scene, location)?)
            }
            None => self.core.open_url(location),
        }
    }

    fn details(&self, entry: &Entry) -> Result<Entry> {
        if entry.kind != EntryKind::Video || is_direct_file(&entry.location) {
            return Ok(entry.clone());
        }
        let mut e = entry.clone();
        self.video(&entry.location, true)?.apply_to(&mut e);
        Ok(e)
    }

    fn sidecars(&self, video: &Entry) -> Result<Sidecars> {
        let full = if video.scripts.is_empty() && video.subtitles.is_empty() {
            self.details(video)?
        } else {
            video.clone()
        };
        Ok(Sidecars::from_entry(&full))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deovr_root() {
        let v = json!({
            "scenes": [
                {"name": "Recent", "list": [
                    {"title": "Beach_180_LR", "videoLength": 1800, "thumbnailUrl": "/img/1.jpg",
                     "video_url": "http://xbvr:9999/deovr/1"},
                    {"title": "No URL"},
                    {"title": "Rel", "videoLength": "60", "video_url": "/deovr/2"}
                ]},
                {"list": []}
            ]
        });
        let g = parse_deovr_root(&v, "http://xbvr:9999/deovr");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "Recent");
        assert_eq!(g[1].name, "Group 2");
        assert_eq!(g[0].scenes.len(), 2);
        let s = &g[0].scenes[0];
        assert_eq!(s.location, "http://xbvr:9999/deovr/1");
        assert_eq!(s.duration, Some(1800.0));
        assert_eq!(
            s.thumbnail_url.as_deref(),
            Some("http://xbvr:9999/img/1.jpg")
        );
        assert_eq!(s.format.unwrap().stereo, StereoLayout::SideBySide);
        assert_eq!(g[0].scenes[1].duration, Some(60.0));
        assert_eq!(g[0].scenes[1].location, "http://xbvr:9999/deovr/2");
        assert!(parse_deovr_root(&json!({"x": 1}), "http://h/").is_empty());
    }

    #[test]
    fn deovr_scene_and_best_source() {
        let v = json!({
            "title": "Beach",
            "videoLength": 1800,
            "is3d": true,
            "screenType": "mkx200",
            "stereoMode": "sbs",
            "thumbnailUrl": "http://xbvr/img/1.jpg",
            "encodings": [
                {"name": "h265", "videoSources": [
                    {"resolution": 4096, "height": 4096, "width": 8192, "url": "http://xbvr/f/8k.mp4"},
                    {"resolution": 2880, "url": "http://xbvr/f/6k.mp4"}
                ]},
                {"name": "h264", "videoSources": [
                    {"resolution": "1920", "height": 1920, "width": 3840, "url": "/f/4k.mp4"},
                    {"height": 1920}
                ]}
            ],
            "timeStamps": [{"ts": 300, "name": "Two"}, {"ts": 10, "name": "One"}],
            "fleshlight": [{"title": "Beach.funscript", "url": "http://xbvr/s/1.funscript"}]
        });
        let s = parse_deovr_scene(&v, "http://xbvr/deovr/1");
        assert_eq!(s.title, "Beach");
        assert_eq!(s.duration, Some(1800.0));
        assert_eq!(
            s.format,
            Some(VideoFormat::new(
                Projection::fisheye(200.0),
                StereoLayout::SideBySide
            ))
        );
        assert_eq!(s.sources.len(), 3);
        assert_eq!(s.best_source(None).unwrap().url, "http://xbvr/f/8k.mp4");
        assert_eq!(
            s.best_source(Some(2880)).unwrap().url,
            "http://xbvr/f/6k.mp4"
        );
        assert_eq!(
            s.best_source(Some(2000)).unwrap().url,
            "http://xbvr/f/4k.mp4"
        );
        assert_eq!(
            s.best_source(Some(100)).unwrap().url,
            "http://xbvr/f/4k.mp4"
        );
        assert_eq!(
            s.markers,
            [(10.0, "One".to_string()), (300.0, "Two".to_string())]
        );
        assert_eq!(s.scripts, ["http://xbvr/s/1.funscript"]);

        let mut e = Entry::new("x", "http://xbvr/deovr/1", EntryKind::Video);
        s.apply_to(&mut e);
        assert_eq!(e.name, "Beach");
        assert_eq!(e.location, "http://xbvr/deovr/1");
        assert_eq!(e.scripts.len(), 1);
        assert_eq!(e.markers.len(), 2);
        assert_eq!(e.format, s.format);
    }

    #[test]
    fn deovr_screen_types() {
        let f = |v: Value| deovr_format(&v);
        let p = |v: Value| f(v).map(|x| x.projection);
        assert_eq!(
            p(json!({"screenType": "dome"})),
            Some(Projection::EQUIRECT_180)
        );
        assert_eq!(
            p(json!({"screenType": "sphere"})),
            Some(Projection::EQUIRECT_360)
        );
        assert_eq!(p(json!({"screenType": "flat"})), Some(Projection::Flat));
        assert_eq!(
            p(json!({"screenType": "fisheye"})),
            Some(Projection::fisheye(180.0))
        );
        assert_eq!(
            p(json!({"screenType": "mkx220"})),
            Some(Projection::fisheye(220.0))
        );
        assert_eq!(
            p(json!({"screenType": "VRCA220"})),
            Some(Projection::fisheye(220.0))
        );
        assert_eq!(
            p(json!({"screenType": "rf52"})),
            Some(Projection::fisheye(190.0))
        );
        assert_eq!(
            f(json!({"is3d": true, "stereoMode": "tb"})),
            Some(VideoFormat::new(
                Projection::EQUIRECT_180,
                StereoLayout::TopBottom
            ))
        );
        assert_eq!(
            f(json!({"is3d": false})),
            Some(VideoFormat::new(Projection::Flat, StereoLayout::Mono))
        );
        assert_eq!(
            f(json!({"is3d": "true", "screenType": "sphere", "stereoMode": "off"})),
            Some(VideoFormat::new(
                Projection::EQUIRECT_360,
                StereoLayout::Mono
            ))
        );
        assert_eq!(f(json!({"title": "x"})), None);
    }

    #[test]
    fn heresphere_library_and_video() {
        let lib = json!({"access": 1, "library": [
            {"name": "All", "list": ["http://xbvr:9999/heresphere/1", "/heresphere/2"]}
        ]});
        let g = parse_heresphere_library(&lib, "http://xbvr:9999/heresphere").unwrap();
        assert_eq!(g[0].scenes.len(), 2);
        assert_eq!(g[0].scenes[1].location, "http://xbvr:9999/heresphere/2");
        assert_eq!(g[0].scenes[1].name, "2");
        assert!(matches!(
            parse_heresphere_library(&json!({"access": -1}), "http://h/"),
            Err(Error::Auth(_))
        ));

        let v = json!({
            "title": "Beach", "duration": 1_800_000.0, "thumbnailImage": "http://x/t.jpg",
            "projection": "fisheye", "stereo": "sbs", "lens": "MKX220", "fov": 180.0,
            "media": [{"name": "h265", "sources": [
                {"resolution": 2880, "height": 2880, "width": 5760, "size": 123, "url": "http://x/6k.mp4"},
                {"resolution": 1440, "height": 1440, "width": 2880, "url": "http://x/3k.mp4"}
            ]}],
            "scripts": [{"name": "Beach.funscript", "url": "http://x/Beach.funscript"}],
            "subtitles": [{"name": "English", "language": "en", "url": "http://x/Beach.en.srt"}],
            "tags": [{"name": "Talk", "start": 1500.0, "end": 9000.0}, {"name": "Genre:VR"}]
        });
        let s = parse_heresphere_video(&v, "http://x/heresphere/1");
        assert_eq!(s.duration, Some(1800.0));
        assert_eq!(
            s.format,
            Some(VideoFormat::new(
                Projection::fisheye(220.0),
                StereoLayout::SideBySide
            ))
        );
        assert_eq!(s.best_source(Some(2000)).unwrap().url, "http://x/3k.mp4");
        assert_eq!(s.best_source(None).unwrap().size, Some(123));
        assert_eq!(s.subtitles, ["http://x/Beach.en.srt"]);
        assert_eq!(s.markers, [(1.5, "Talk".to_string())]);
    }

    #[test]
    fn heresphere_projections() {
        let p = |v: Value| heresphere_format(&v).map(|f| (f.projection, f.stereo));
        assert_eq!(
            p(json!({"projection": "equirectangular", "stereo": "tb"})),
            Some((Projection::EQUIRECT_180, StereoLayout::TopBottom))
        );
        assert_eq!(
            p(json!({"projection": "equirectangular360", "stereo": "mono"})),
            Some((Projection::EQUIRECT_360, StereoLayout::Mono))
        );
        assert_eq!(
            p(json!({"projection": "fisheye", "lens": "Linear", "fov": 190})),
            Some((Projection::fisheye(190.0), StereoLayout::Mono))
        );
        assert_eq!(
            p(json!({"projection": "fisheye", "lens": "MKX200"})),
            Some((Projection::fisheye(200.0), StereoLayout::Mono))
        );
        assert_eq!(
            p(json!({"projection": "perspective"})).map(|x| x.0),
            Some(Projection::Flat)
        );
        assert_eq!(
            p(json!({"projection": "equiangularCubemap"})).map(|x| x.0),
            Some(Projection::Eac { h_fov: 360.0 })
        );
        assert_eq!(p(json!({"projection": "weird"})), None);
        assert_eq!(p(json!({})), None);
    }

    #[test]
    fn describe_and_debug_hide_secrets() {
        let config = FeedConfig {
            id: "f".into(),
            name: "Stash".into(),
            url: "http://u:pwx9@stash:9999/deovr?apikey=SECRETKEY".into(),
            credentials: Some(Credentials::new("bob", "hunter2")),
            insecure_tls: false,
            max_height: None,
        };
        let d = DeoVrSource::new(&config).unwrap();
        let h = HereSphereSource::new(&config).unwrap();
        for t in [
            d.describe(),
            format!("{d:?}"),
            h.describe(),
            format!("{h:?}"),
        ] {
            assert!(
                !t.contains("SECRETKEY") && !t.contains("hunter2") && !t.contains("pwx9"),
                "{t}"
            );
        }
        assert!(
            DeoVrSource::new(&FeedConfig {
                url: "ftp://x/".into(),
                ..config
            })
            .is_err()
        );
    }

    #[test]
    fn group_locations() {
        let l = group_location("http://h/deovr?apikey=1", "Recent & New");
        assert_eq!(l, "http://h/deovr?apikey=1#group=Recent%20%26%20New");
        assert_eq!(
            split_group(&l),
            Some(("http://h/deovr?apikey=1", "Recent & New".to_string()))
        );
        assert!(is_direct_file("http://h/a/b.mp4?x=1"));
        assert!(!is_direct_file("http://h/deovr/12"));
    }
}
