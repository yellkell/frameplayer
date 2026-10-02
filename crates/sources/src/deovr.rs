//! DeoVR JSON feed client (the `/deovr` API served by XBVR, Stash plugins,
//! SLR-style servers and friends).
//!
//! Two documents matter: the library (`GET <server>/deovr`) with
//! `scenes[].list[]` scene references, and a per-video document reached
//! through each reference's `video_url` with encodings, projection,
//! scripts, chapters and metadata. Real servers are loose with types
//! (numbers as strings, booleans as 0/1), so parsing is lenient.
//!
//! As a [`Source`], scene lists are directories and scenes are files whose
//! URIs are `deovr+<video_url>`; opening one picks the best encoding and
//! returns a ranged HTTP reader.

use crate::config::SourceKind;
use crate::error::{Result, SourceError};
use crate::http::{check_status, HttpClient, HttpFile};
use crate::source::{Entry, RandomAccess, Source};
use async_trait::async_trait;
use bytes::Bytes;
use fp_core::{FisheyeLens, Projection, StereoMode, ViewSettings};
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;
use serde::de::Deserializer;
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}

fn de_f64<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<f64>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?
        .as_ref()
        .and_then(value_f64))
}

fn de_u64<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<u64>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?
        .as_ref()
        .and_then(value_f64)
        .filter(|f| *f >= 0.0)
        .map(|f| f as u64))
}

fn de_bool<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<bool>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Bool(b)) => Some(b),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" | "" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn de_string<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    })
}

fn de_str<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(de_string(d)?.unwrap_or_default())
}

fn de_vec<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// `GET /deovr` response.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct DeoVrLibrary {
    #[serde(deserialize_with = "de_vec")]
    pub scenes: Vec<SceneList>,
    #[serde(deserialize_with = "de_bool")]
    pub authorized: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SceneList {
    #[serde(deserialize_with = "de_str")]
    pub name: String,
    #[serde(deserialize_with = "de_vec")]
    pub list: Vec<SceneRef>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SceneRef {
    #[serde(deserialize_with = "de_str")]
    pub title: String,
    #[serde(rename = "videoLength", deserialize_with = "de_f64")]
    pub video_length: Option<f64>,
    #[serde(rename = "thumbnailUrl")]
    pub thumbnail_url: Option<String>,
    #[serde(rename = "video_url", alias = "videoUrl", deserialize_with = "de_str")]
    pub video_url: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct VideoSource {
    #[serde(deserialize_with = "de_u64")]
    pub resolution: Option<u64>,
    #[serde(deserialize_with = "de_u64")]
    pub height: Option<u64>,
    #[serde(deserialize_with = "de_u64")]
    pub width: Option<u64>,
    #[serde(deserialize_with = "de_u64")]
    pub size: Option<u64>,
    #[serde(deserialize_with = "de_str")]
    pub url: String,
}

impl VideoSource {
    /// Vertical resolution, from `height` or the `resolution` label.
    pub fn lines(&self) -> u64 {
        self.height.or(self.resolution).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Encoding {
    pub name: String,
    #[serde(rename = "videoSources", deserialize_with = "de_vec")]
    pub video_sources: Vec<VideoSource>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct TimeStamp {
    #[serde(deserialize_with = "de_f64")]
    pub ts: Option<f64>,
    #[serde(deserialize_with = "de_str")]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct ScriptRef {
    #[serde(deserialize_with = "de_str")]
    pub title: String,
    #[serde(deserialize_with = "de_str")]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Named {
    #[serde(deserialize_with = "de_string")]
    pub id: Option<String>,
    #[serde(deserialize_with = "de_str")]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct Category {
    pub tag: Named,
}

/// Per-video document.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct DeoVrVideo {
    #[serde(deserialize_with = "de_string")]
    pub id: Option<String>,
    #[serde(deserialize_with = "de_str")]
    pub title: String,
    pub description: Option<String>,
    #[serde(deserialize_with = "de_bool")]
    pub authorized: Option<bool>,
    /// Release date, Unix seconds.
    #[serde(deserialize_with = "de_f64")]
    pub date: Option<f64>,
    #[serde(deserialize_with = "de_vec")]
    pub encodings: Vec<Encoding>,
    #[serde(deserialize_with = "de_bool")]
    pub is3d: Option<bool>,
    #[serde(rename = "screenType")]
    pub screen_type: Option<String>,
    #[serde(rename = "stereoMode")]
    pub stereo_mode: Option<String>,
    #[serde(rename = "videoLength", deserialize_with = "de_f64")]
    pub video_length: Option<f64>,
    #[serde(rename = "thumbnailUrl")]
    pub thumbnail_url: Option<String>,
    #[serde(rename = "videoThumbnail")]
    pub video_thumbnail: Option<String>,
    #[serde(rename = "videoPreview")]
    pub video_preview: Option<String>,
    #[serde(rename = "timeStamps", deserialize_with = "de_vec")]
    pub time_stamps: Vec<TimeStamp>,
    #[serde(deserialize_with = "de_vec")]
    pub fleshlight: Vec<ScriptRef>,
    #[serde(rename = "isScripted", deserialize_with = "de_bool")]
    pub is_scripted: Option<bool>,
    #[serde(rename = "fullVideoReady", deserialize_with = "de_bool")]
    pub full_video_ready: Option<bool>,
    #[serde(rename = "fullAccess", deserialize_with = "de_bool")]
    pub full_access: Option<bool>,
    #[serde(deserialize_with = "de_vec")]
    pub categories: Vec<Category>,
    #[serde(deserialize_with = "de_vec")]
    pub actors: Vec<Named>,
    pub paysite: Option<Named>,
}

/// Map DeoVR `screenType` / `stereoMode` / `is3d` to our projection and
/// stereo layout.
pub fn map_projection(
    screen_type: Option<&str>,
    stereo_mode: Option<&str>,
    is3d: Option<bool>,
) -> (Projection, StereoMode) {
    let st = screen_type.unwrap_or("").trim().to_ascii_lowercase();
    let projection = match st.as_str() {
        "flat" | "screen" => Projection::FLAT_DEFAULT,
        "dome" | "180" | "equirect180" => Projection::EQUIRECT_180,
        "sphere" | "360" | "equirect360" => Projection::EQUIRECT_360,
        "fisheye" | "fisheye180" => Projection::fisheye_fov(180.0),
        "fisheye190" => Projection::fisheye_fov(190.0),
        "fisheye200" => Projection::fisheye_fov(200.0),
        "mkx200" => Projection::fisheye(FisheyeLens::Mkx200),
        "mkx220" => Projection::fisheye(FisheyeLens::Mkx220),
        "rf52" => Projection::fisheye(FisheyeLens::CanonRf52),
        "vrca220" => Projection::fisheye_fov(220.0),
        "eac" | "cubemap" => Projection::Eac,
        // DeoVR treats an unknown/missing type on a 3D video as 180° dome.
        _ if is3d == Some(true) => Projection::EQUIRECT_180,
        _ => Projection::FLAT_DEFAULT,
    };
    let stereo = match stereo_mode
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("sbs") | Some("lr") => StereoMode::Sbs,
        Some("tb") | Some("ou") => StereoMode::Ou,
        Some("off") | Some("mono") | Some("none") => StereoMode::Mono,
        _ if is3d == Some(false) => StereoMode::Mono,
        _ if is3d == Some(true) => StereoMode::Sbs,
        _ if projection.is_immersive() => StereoMode::Sbs,
        _ => StereoMode::Mono,
    };
    (projection, stereo)
}

impl DeoVrVideo {
    pub fn projection(&self) -> (Projection, StereoMode) {
        map_projection(
            self.screen_type.as_deref(),
            self.stereo_mode.as_deref(),
            self.is3d,
        )
    }

    pub fn view_settings(&self) -> ViewSettings {
        let (projection, stereo) = self.projection();
        ViewSettings {
            projection,
            stereo,
            ..Default::default()
        }
    }

    /// Best source with height ≤ `max_height` (highest otherwise), preferring
    /// the encoding order the server lists (usually best codec first) only
    /// as a tie-break.
    pub fn best_source(&self, max_height: Option<u64>) -> Option<&VideoSource> {
        let all = || {
            self.encodings
                .iter()
                .enumerate()
                .flat_map(|(ei, e)| e.video_sources.iter().map(move |s| (ei, s)))
                .filter(|(_, s)| !s.url.is_empty())
        };
        let key = |(ei, s): &(usize, &VideoSource)| (s.lines(), std::cmp::Reverse(*ei));
        all()
            .filter(|(_, s)| max_height.is_none_or(|m| s.lines() <= m))
            .max_by_key(key)
            .or_else(|| all().min_by_key(|(_, s)| s.lines()))
            .map(|(_, s)| s)
    }

    pub fn tags(&self) -> Vec<String> {
        self.categories
            .iter()
            .map(|c| c.tag.name.clone())
            .filter(|n| !n.is_empty())
            .collect()
    }

    pub fn actors(&self) -> Vec<String> {
        self.actors
            .iter()
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .collect()
    }

    /// Funscript URLs (`fleshlight[]`) when the video is scripted.
    pub fn scripts(&self) -> Vec<&ScriptRef> {
        self.fleshlight
            .iter()
            .filter(|s| !s.url.is_empty())
            .collect()
    }

    /// Chapter markers in seconds.
    pub fn chapters(&self) -> Vec<(f64, String)> {
        let mut v: Vec<_> = self
            .time_stamps
            .iter()
            .filter_map(|t| Some((t.ts?, t.name.clone())))
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v
    }

    /// The scene is only a trailer/preview if the server says so.
    pub fn is_full(&self) -> bool {
        self.full_video_ready.unwrap_or(true) && self.full_access.unwrap_or(true)
    }

    pub fn thumbnail(&self) -> Option<&str> {
        self.thumbnail_url
            .as_deref()
            .or(self.video_thumbnail.as_deref())
    }
}

/// HTTP client for one DeoVR feed server.
#[derive(Clone)]
pub struct DeoVrClient {
    http: HttpClient,
    feed_url: String,
    /// DeoVR "login" form credentials (XBVR/Stash accept these as POST form
    /// fields `login` / `password` on every feed request).
    // [verify] Confirm against current XBVR and Stash releases that the
    // form-POST login is still what their /deovr endpoints expect.
    login: Option<(String, Zeroizing<String>)>,
}

impl DeoVrClient {
    /// `server` may be `http://host:9999`, `.../deovr`, or `deovr+http://...`.
    pub fn new(http: HttpClient, server: &str) -> Result<Self> {
        let s = server.trim_start_matches("deovr+").trim_end_matches('/');
        let feed_url = if s.ends_with("/deovr") {
            s.to_string()
        } else {
            format!("{s}/deovr")
        };
        url::Url::parse(&feed_url)?;
        Ok(DeoVrClient {
            http,
            feed_url,
            login: None,
        })
    }

    pub fn with_login(mut self, login: &str, password: &str) -> Self {
        self.login = Some((login.to_string(), Zeroizing::new(password.to_string())));
        self
    }

    pub fn feed_url(&self) -> &str {
        &self.feed_url
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let resp = match &self.login {
            Some((l, p)) => {
                let mut h = HeaderMap::new();
                h.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
                let enc = |s: &str| {
                    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC)
                        .to_string()
                };
                let body = format!("login={}&password={}", enc(l), enc(p));
                check_status(
                    self.http
                        .send(Method::POST, url, h, Some(Bytes::from(body)))
                        .await?,
                )?
            }
            None => self.http.get(url).await?,
        };
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn library(&self) -> Result<DeoVrLibrary> {
        let lib: DeoVrLibrary = self.fetch_json(&self.feed_url).await?;
        if lib.authorized == Some(false) && lib.scenes.is_empty() {
            return Err(SourceError::Auth);
        }
        Ok(lib)
    }

    /// Fetch a per-video document by its `video_url` (or `deovr+` URI).
    pub async fn video(&self, video_url: &str) -> Result<DeoVrVideo> {
        self.fetch_json(video_url.trim_start_matches("deovr+"))
            .await
    }
}

/// A DeoVR feed as a browsable [`Source`].
pub struct DeoVrSource {
    client: DeoVrClient,
    cache: Mutex<Option<(Instant, DeoVrLibrary)>>,
    /// Maximum video height to stream (None = best available).
    pub max_height: Option<u64>,
}

const CACHE_TTL: Duration = Duration::from_secs(60);

impl DeoVrSource {
    pub fn new(client: DeoVrClient) -> Self {
        DeoVrSource {
            client,
            cache: Mutex::new(None),
            max_height: None,
        }
    }

    pub fn client(&self) -> &DeoVrClient {
        &self.client
    }

    async fn library(&self) -> Result<DeoVrLibrary> {
        if let Some((t, lib)) = self.cache.lock().as_ref() {
            if t.elapsed() < CACHE_TTL {
                return Ok(lib.clone());
            }
        }
        let lib = self.client.library().await?;
        *self.cache.lock() = Some((Instant::now(), lib.clone()));
        Ok(lib)
    }

    fn list_uri(&self, idx: usize) -> String {
        format!("deovr+{}#list={idx}", self.client.feed_url)
    }
}

/// Build entries for one scene list.
pub fn scene_entries(list: &SceneList) -> Vec<Entry> {
    list.list
        .iter()
        .filter(|s| !s.video_url.is_empty())
        .map(|s| Entry {
            name: s.title.clone(),
            uri: format!("deovr+{}", s.video_url),
            is_dir: false,
            size: None,
            mtime: None,
            thumbnail: s.thumbnail_url.clone(),
            duration_secs: s.video_length,
            is_media: true,
        })
        .collect()
}

#[async_trait]
impl Source for DeoVrSource {
    fn kind(&self) -> SourceKind {
        SourceKind::DeoVr
    }

    fn root_uri(&self) -> String {
        format!("deovr+{}", self.client.feed_url)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let lib = self.library().await?;
        if let Some(idx) = dir
            .rsplit_once("#list=")
            .and_then(|(_, i)| i.parse::<usize>().ok())
        {
            let list = lib
                .scenes
                .get(idx)
                .ok_or_else(|| SourceError::NotFound(dir.into()))?;
            return Ok(scene_entries(list));
        }
        Ok(lib
            .scenes
            .iter()
            .enumerate()
            .map(|(i, l)| Entry {
                name: if l.name.is_empty() {
                    format!("List {}", i + 1)
                } else {
                    l.name.clone()
                },
                uri: self.list_uri(i),
                is_dir: true,
                ..Default::default()
            })
            .collect())
    }

    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
        let video = self.client.video(uri).await?;
        let src = video.best_source(self.max_height).ok_or_else(|| {
            SourceError::NotFound(format!("no playable encodings for {}", video.title))
        })?;
        Ok(Box::new(
            HttpFile::open(self.client.http.clone(), &src.url).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARY: &str = r#"{"scenes":[{"name":"Recent","list":[
        {"title":"Scene A","videoLength":1834,"thumbnailUrl":"http://xbvr:9999/img/700x/a.jpg","video_url":"http://xbvr:9999/deovr/123"},
        {"title":"Scene B","videoLength":"60.5","thumbnailUrl":null,"video_url":"http://xbvr:9999/deovr/124"}]},
        {"name":"Favourites","list":null}],"authorized":"1"}"#;

    // Shape follows XBVR's DeoVR output (stashapp's is a subset).
    const VIDEO: &str = r#"{"id":123,"title":"Scene A","authorized":1,"description":"desc","date":1700000000,
        "paysite":{"id":5,"name":"Studio"},"is3d":true,"screenType":"dome","stereoMode":"sbs","videoLength":1834,
        "thumbnailUrl":"http://xbvr:9999/img/a.jpg","videoPreview":"http://xbvr:9999/api/dms/preview/123",
        "encodings":[{"name":"h265","videoSources":[{"resolution":2880,"height":2880,"width":5760,"size":3000000000,"url":"http://xbvr:9999/api/dms/file/1?dnt=true"},
                                                       {"resolution":1920,"url":"http://xbvr:9999/api/dms/file/2"}]},
                     {"name":"h264","videoSources":[{"resolution":2880,"url":"http://xbvr:9999/api/dms/file/3"}]}],
        "timeStamps":[{"ts":300,"name":"Middle"},{"ts":10,"name":"Intro"}],
        "isScripted":true,"fleshlight":[{"title":"Scene A.funscript","url":"http://xbvr:9999/api/dms/file/9"}],
        "fullVideoReady":true,"fullAccess":true,
        "categories":[{"tag":{"id":1,"name":"Outdoor"}},{"tag":{"id":2,"name":""}}],
        "actors":[{"id":"7","name":"Jane"}],"corrections":{}}"#;

    #[test]
    fn parses_library() {
        let lib: DeoVrLibrary = serde_json::from_str(LIBRARY).unwrap();
        assert_eq!(lib.authorized, Some(true));
        assert_eq!(lib.scenes.len(), 2);
        assert!(lib.scenes[1].list.is_empty());
        let e = scene_entries(&lib.scenes[0]);
        assert_eq!(e[0].uri, "deovr+http://xbvr:9999/deovr/123");
        assert_eq!(e[0].duration_secs, Some(1834.0));
        assert_eq!(e[1].duration_secs, Some(60.5));
        assert!(e[0].is_video());
    }

    #[test]
    fn parses_video() {
        let v: DeoVrVideo = serde_json::from_str(VIDEO).unwrap();
        assert_eq!(v.id.as_deref(), Some("123"));
        assert_eq!(v.projection(), (Projection::EQUIRECT_180, StereoMode::Sbs));
        assert_eq!(
            v.best_source(None).unwrap().url,
            "http://xbvr:9999/api/dms/file/1?dnt=true"
        );
        assert_eq!(
            v.best_source(Some(2000)).unwrap().url,
            "http://xbvr:9999/api/dms/file/2"
        );
        assert_eq!(
            v.best_source(Some(100)).unwrap().url,
            "http://xbvr:9999/api/dms/file/2"
        );
        assert_eq!(v.scripts().len(), 1);
        assert_eq!(
            v.chapters(),
            vec![(10.0, "Intro".to_string()), (300.0, "Middle".to_string())]
        );
        assert_eq!(v.tags(), vec!["Outdoor"]);
        assert_eq!(v.actors(), vec!["Jane"]);
        assert_eq!(v.paysite.as_ref().unwrap().name, "Studio");
        assert!(v.is_full());
        assert_eq!(v.is_scripted, Some(true));
        assert_eq!(v.thumbnail(), Some("http://xbvr:9999/img/a.jpg"));
    }

    #[test]
    fn projection_mapping() {
        assert_eq!(
            map_projection(Some("sphere"), Some("tb"), Some(true)),
            (Projection::EQUIRECT_360, StereoMode::Ou)
        );
        assert_eq!(
            map_projection(Some("flat"), Some("off"), Some(false)),
            (Projection::FLAT_DEFAULT, StereoMode::Mono)
        );
        assert_eq!(
            map_projection(Some("mkx200"), Some("sbs"), None).0,
            Projection::fisheye(FisheyeLens::Mkx200)
        );
        assert_eq!(
            map_projection(Some("rf52"), None, Some(true)),
            (Projection::fisheye(FisheyeLens::CanonRf52), StereoMode::Sbs)
        );
        assert_eq!(
            map_projection(Some("fisheye"), None, None),
            (Projection::fisheye_fov(180.0), StereoMode::Sbs)
        );
        assert_eq!(
            map_projection(None, None, Some(true)),
            (Projection::EQUIRECT_180, StereoMode::Sbs)
        );
        assert_eq!(
            map_projection(None, None, None),
            (Projection::FLAT_DEFAULT, StereoMode::Mono)
        );
        assert_eq!(
            map_projection(Some("vrca220"), Some("sbs"), None).0,
            Projection::fisheye_fov(220.0)
        );
    }

    #[test]
    fn minimal_and_odd_documents() {
        let v: DeoVrVideo = serde_json::from_str(
            r#"{"title":null,"encodings":null,"is3d":"false","fullVideoReady":0}"#,
        )
        .unwrap();
        assert!(v.best_source(None).is_none());
        assert_eq!(v.projection(), (Projection::FLAT_DEFAULT, StereoMode::Mono));
        assert!(!v.is_full());
    }

    #[test]
    fn feed_urls() {
        let h = HttpClient::new(None).unwrap();
        assert_eq!(
            DeoVrClient::new(h.clone(), "http://xbvr:9999")
                .unwrap()
                .feed_url(),
            "http://xbvr:9999/deovr"
        );
        assert_eq!(
            DeoVrClient::new(h.clone(), "deovr+http://xbvr:9999/deovr/")
                .unwrap()
                .feed_url(),
            "http://xbvr:9999/deovr"
        );
        let s = DeoVrSource::new(DeoVrClient::new(h, "http://s:1").unwrap());
        assert_eq!(s.list_uri(2), "deovr+http://s:1/deovr#list=2");
    }
}
