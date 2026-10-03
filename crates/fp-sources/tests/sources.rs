//! Every network source against a local server: autoindex HTTP, WebDAV,
//! DLNA (device description + SOAP Browse), DeoVR and HereSphere feeds.

mod common;

use common::{Req, Resp, TestServer, pattern, resp, serve_bytes};
use fp_core::format::{Projection, StereoLayout};
use fp_core::source::EntryKind;
use fp_sources::{
    Credentials, DlnaConfig, Error, FeedConfig, HttpConfig, SourceConfig, SourceKind, build,
};
use std::sync::Arc;

fn names(v: &[fp_core::Entry]) -> Vec<&str> {
    v.iter().map(|e| e.name.as_str()).collect()
}

fn http_config(url: String, credentials: Option<Credentials>) -> HttpConfig {
    HttpConfig {
        id: "x".into(),
        name: "X".into(),
        url,
        credentials,
        insecure_tls: false,
    }
}

// ---------------------------------------------------------------- autoindex

#[test]
fn autoindex_listing_open_and_sidecars() {
    let video = Arc::new(pattern(300_000));
    let v = video.clone();
    let server = TestServer::start(move |req| match req.path() {
        "/vr/" => resp(
            200,
            r#"<html><body><h1>Index of /vr/</h1><hr><pre><a href="../">../</a>
<a href="Old%20Stuff/">Old Stuff/</a>                                         12-Jan-2024 10:22                   -
<a href="Beach_180_LR.mp4">Beach_180_LR.mp4</a>                                   12-Jan-2024 10:22              300000
<a href="Beach_180_LR.funscript">Beach_180_LR.funscript</a>                             12-Jan-2024 10:22                  20
<a href="Beach_180_LR.roll.funscript">Beach_180_LR.roll.funscript</a>                        12-Jan-2024 10:22                  20
<a href="Beach_180_LR.en.srt">Beach_180_LR.en.srt</a>                                12-Jan-2024 10:22                  10
</pre><hr></body></html>"#,
            &[("Content-Type", "text/html")],
        ),
        "/vr/Old%20Stuff/" => resp(200, "<pre><a href=\"../\">../</a>\n</pre>", &[]),
        "/vr/Beach_180_LR.mp4" => serve_bytes(req, &v, true),
        _ => resp(404, "", &[]),
    });
    let src = build(&SourceConfig::Http(http_config(server.url("/vr"), None))).unwrap();
    assert_eq!(src.kind(), SourceKind::Http);
    let list = src.list(None).unwrap();
    assert_eq!(
        names(&list),
        [
            "Old Stuff",
            "Beach_180_LR.en.srt",
            "Beach_180_LR.funscript",
            "Beach_180_LR.mp4",
            "Beach_180_LR.roll.funscript"
        ]
    );
    assert!(src.list(Some(&list[0].location)).unwrap().is_empty());
    let video_entry = list.iter().find(|e| e.kind == EntryKind::Video).unwrap();
    assert_eq!(video_entry.size, Some(300_000));
    assert_eq!(
        video_entry.format.unwrap().projection,
        Projection::EQUIRECT_180
    );

    let f = src.open(&video_entry.location).unwrap();
    assert_eq!(f.size(), Some(300_000));
    let mut buf = [0u8; 1000];
    assert_eq!(f.read_at(299_500, &mut buf).unwrap(), 500);
    assert_eq!(buf[..500], video[299_500..]);

    let sc = src.sidecars(video_entry).unwrap();
    assert_eq!(sc.scripts.len(), 2);
    assert_eq!(sc.scripts[1].axis.as_deref(), Some("roll"));
    assert_eq!(sc.subtitles[0].language.as_deref(), Some("en"));
    assert_eq!(
        sc.subtitles[0].location,
        server.url("/vr/Beach_180_LR.en.srt")
    );
}

// ---------------------------------------------------------------- WebDAV

const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/dav/vr/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/dav/vr/Season%201/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getlastmodified>Fri, 12 Jan 2024 10:22:33 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/dav/vr/Trip%20%231_360_TB.mkv</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>4096</D:getcontentlength><D:getlastmodified>Fri, 12 Jan 2024 10:22:33 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>/dav/vr/Trip%20%231_360_TB.funscript</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>12</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
</D:multistatus>"#;

#[test]
fn webdav_propfind_and_read() {
    let data = Arc::new(pattern(4096));
    let d = data.clone();
    let server = TestServer::start(move |req: &Req| {
        // "nas:pa ss" in Basic auth.
        if req.header("authorization") != Some("Basic bmFzOnBhIHNz") {
            return resp(401, "", &[]);
        }
        match (req.method.as_str(), req.path()) {
            ("PROPFIND", "/dav/vr/") => {
                assert_eq!(req.header("depth"), Some("1"));
                assert!(req.body.contains("getcontentlength"));
                resp(
                    207,
                    MULTISTATUS,
                    &[("Content-Type", "application/xml; charset=utf-8")],
                )
            }
            ("GET", "/dav/vr/Trip%20%231_360_TB.mkv") => serve_bytes(req, &d, true),
            _ => resp(404, "", &[]),
        }
    });
    // Credentials embedded in a webdav:// URL.
    let url = server.base.replace("http://", "webdav://nas:pa%20ss@") + "/dav/vr";
    let src = build(&SourceConfig::WebDav(http_config(url, None))).unwrap();
    assert!(!src.describe().contains("pa ss") && !src.describe().contains("pa%20ss"));
    let list = src.list(None).unwrap();
    assert_eq!(
        names(&list),
        ["Season 1", "Trip #1_360_TB.funscript", "Trip #1_360_TB.mkv"]
    );
    for e in &list {
        assert!(e.location.starts_with(&server.base), "{}", e.location);
    }
    let mkv = &list[2];
    assert_eq!(mkv.size, Some(4096));
    assert_eq!(mkv.modified, Some(1_705_054_953));
    let fmt = mkv.format.unwrap();
    assert_eq!(
        (fmt.projection, fmt.stereo),
        (Projection::EQUIRECT_360, StereoLayout::TopBottom)
    );
    assert_eq!(list[0].kind, EntryKind::Directory);
    assert!(list[0].location.ends_with("/dav/vr/Season%201/"));

    let f = src.open(&mkv.location).unwrap();
    let mut buf = vec![0u8; 5000];
    assert_eq!(f.read_at(0, &mut buf).unwrap(), 4096);
    assert_eq!(buf[..4096], data[..]);

    let sc = src.sidecars(mkv).unwrap();
    assert_eq!(sc.scripts.len(), 1);
    assert_eq!(sc.scripts[0].name, "Trip #1_360_TB.funscript");

    // Wrong password.
    let bad = server.base.replace("http://", "dav://nas:nope@") + "/dav/vr/";
    let src = build(&SourceConfig::WebDav(http_config(bad, None))).unwrap();
    assert!(matches!(src.list(None), Err(Error::Auth(_))));
}

// ---------------------------------------------------------------- DLNA

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion>
<device><deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType><friendlyName>Test NAS</friendlyName>
<UDN>uuid:1234</UDN><serviceList>
<service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/ctl/cm</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><controlURL>/ctl/cd</controlURL></service>
</serviceList></device></root>"#;

fn didl_item(id: u32, title: &str, path: &str, mime: &str) -> String {
    format!(
        r#"<item id="{id}" parentID="1" restricted="1"><dc:title>{title}</dc:title><upnp:class>object.item.videoItem</upnp:class><res size="1000" duration="0:01:00.000" protocolInfo="http-get:*:{mime}:*">{path}</res></item>"#
    )
}

fn browse_reply(base: &str, req: &Req) -> Resp {
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = req.body.find(&open).unwrap() + open.len();
        let end = req.body[start..].find('<').unwrap() + start;
        req.body[start..end].to_string()
    };
    let object = tag("ObjectID");
    let start: usize = tag("StartingIndex").parse().unwrap();
    let all: Vec<String> = match object.as_str() {
        "0" => vec![r#"<container id="1" parentID="0" restricted="1"><dc:title>Videos</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>"#.to_string()],
        "1" => vec![
            didl_item(10, "Beach_180_LR", &format!("{base}/media/10.mp4"), "video/mp4"),
            didl_item(11, "Beach_180_LR.en.srt", &format!("{base}/media/11.srt"), "text/srt"),
            didl_item(12, "Dome Tour", "/media/12.mkv", "video/x-matroska"),
        ],
        _ => {
            return resp(
                500,
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#,
                &[("Content-Type", "text/xml")],
            );
        }
    };
    // Two per page whatever was asked, to exercise paging.
    let page: Vec<&String> = all.iter().skip(start).take(2).collect();
    let didl = format!(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">{}</DIDL-Lite>"#,
        page.iter().map(|s| s.as_str()).collect::<String>()
    );
    let escaped = didl
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    resp(
        200,
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>{escaped}</Result><NumberReturned>{}</NumberReturned><TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#,
            page.len(),
            all.len()
        ),
        &[("Content-Type", "text/xml; charset=\"utf-8\"")],
    )
}

#[test]
fn dlna_description_browse_paging_and_read() {
    let media = Arc::new(pattern(1000));
    let m = media.clone();
    let base: Arc<std::sync::OnceLock<String>> = Arc::default();
    let b = base.clone();
    let server = TestServer::start(move |req| match (req.method.as_str(), req.path()) {
        ("GET", "/desc.xml") => resp(200, DESCRIPTION, &[("Content-Type", "text/xml")]),
        ("POST", "/ctl/cd") => {
            assert_eq!(
                req.header("soapaction"),
                Some("\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"")
            );
            assert!(
                req.body
                    .contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>")
            );
            browse_reply(b.get().unwrap(), req)
        }
        ("GET", "/media/10.mp4") => serve_bytes(req, &m, true),
        _ => resp(404, "", &[]),
    });
    base.set(server.base.clone()).unwrap();

    let src = build(&SourceConfig::Dlna(DlnaConfig {
        id: "d".into(),
        name: "Test NAS".into(),
        location: server.url("/desc.xml"),
        control_url: None,
        service_type: None,
    }))
    .unwrap();
    let root = src.list(None).unwrap();
    assert_eq!(names(&root), ["Videos"]);
    assert_eq!(root[0].kind, EntryKind::Directory);

    let items = src.list(Some(&root[0].location)).unwrap();
    assert_eq!(
        names(&items),
        ["Beach_180_LR.en.srt", "Beach_180_LR.mp4", "Dome Tour.mkv"]
    );
    // Folder "1" took two Browse calls (2 + 1 items), root one; description once.
    let browses: Vec<Req> = server
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/ctl/cd")
        .collect();
    assert_eq!(browses.len(), 3);
    assert!(browses[2].body.contains("<StartingIndex>2</StartingIndex>"));
    assert_eq!(server.count(|r| r.path() == "/desc.xml"), 1);

    let beach = &items[1];
    assert_eq!(beach.kind, EntryKind::Video);
    assert_eq!(beach.duration, Some(60.0));
    assert_eq!(beach.size, Some(1000));
    assert_eq!(beach.format.unwrap().stereo, StereoLayout::SideBySide);
    assert_eq!(items[2].location, server.url("/media/12.mkv"));

    let f = src.open(&beach.location).unwrap();
    let mut buf = [0u8; 10];
    assert_eq!(f.read_at(990, &mut buf).unwrap(), 10);
    assert_eq!(buf[..], media[990..]);

    let sc = src.sidecars(beach).unwrap();
    assert_eq!(sc.subtitles.len(), 1);
    assert_eq!(sc.subtitles[0].language.as_deref(), Some("en"));

    assert!(matches!(
        src.list(Some("dlna:nope")),
        Err(Error::NotFound(_))
    ));
    assert!(src.open(&root[0].location).is_err());
}

// ---------------------------------------------------------------- DeoVR

fn deovr_server(require_login: bool) -> TestServer {
    TestServer::start(move |req| {
        if require_login {
            assert_eq!(req.method, "POST");
            assert_eq!(
                req.header("content-type"),
                Some("application/x-www-form-urlencoded")
            );
            if req.body != "login=bob&password=p%40ss" {
                return resp(401, "", &[]);
            }
        }
        let json = |s: String| resp(200, s, &[("Content-Type", "application/json")]);
        match req.path() {
            "/deovr" => json(
                r#"{"authorized":"1","scenes":[{"name":"Recent","list":[
                    {"title":"Beach Day","videoLength":1800,"thumbnailUrl":"/img/1.jpg","video_url":"/deovr/1"},
                    {"title":"Other_360_TB","videoLength":60,"video_url":"/deovr/2"}]},
                  {"name":"Favourites","list":[]}]}"#
                    .into(),
            ),
            "/deovr/1" => json(
                r#"{"id":1,"title":"Beach Day","videoLength":1800,"is3d":true,"screenType":"mkx220","stereoMode":"sbs",
                    "thumbnailUrl":"/img/1.jpg",
                    "encodings":[{"name":"h265","videoSources":[
                        {"resolution":4096,"height":4096,"width":8192,"url":"/files/8k.mp4"},
                        {"resolution":2880,"height":2880,"width":5760,"url":"/files/6k.mp4"}]},
                      {"name":"h264","videoSources":[{"resolution":1920,"height":1920,"width":3840,"url":"/files/4k.mp4"}]}],
                    "timeStamps":[{"ts":95,"name":"Swim"}],
                    "fleshlight":[{"title":"Beach Day.funscript","url":"/files/1.funscript"}],
                    "unknownField":{"nested":[1,2,3]}}"#
                    .into(),
            ),
            "/files/6k.mp4" => serve_bytes(req, b"six k video", true),
            "/files/1.funscript" => serve_bytes(req, br#"{"actions":[]}"#, true),
            "/stream/9" => serve_bytes(req, b"raw stream", true),
            _ => resp(404, "", &[]),
        }
    })
}

fn feed(url: String, credentials: Option<Credentials>, max_height: Option<u32>) -> FeedConfig {
    FeedConfig {
        id: "f".into(),
        name: "XBVR".into(),
        url,
        credentials,
        insecure_tls: false,
        max_height,
    }
}

#[test]
fn deovr_feed_groups_scenes_details_and_open() {
    let server = deovr_server(false);
    let src = build(&SourceConfig::DeoVr(feed(
        server.url("/deovr"),
        None,
        Some(2880),
    )))
    .unwrap();
    let groups = src.list(None).unwrap();
    assert_eq!(names(&groups), ["Recent", "Favourites"]);
    assert!(groups.iter().all(|g| g.kind == EntryKind::Directory));
    assert_eq!(
        groups[0].thumbnail_url.as_deref(),
        Some(server.url("/img/1.jpg").as_str())
    );

    let scenes = src.list(Some(&groups[0].location)).unwrap();
    assert_eq!(names(&scenes), ["Beach Day", "Other_360_TB"]);
    assert_eq!(scenes[0].location, server.url("/deovr/1"));
    assert_eq!(scenes[0].duration, Some(1800.0));
    assert!(scenes[0].format.is_none());
    assert_eq!(
        scenes[1].format.unwrap().projection,
        Projection::EQUIRECT_360
    );
    assert!(src.list(Some(&groups[1].location)).unwrap().is_empty());

    let full = src.details(&scenes[0]).unwrap();
    let fmt = full.format.unwrap();
    assert_eq!(
        (fmt.projection, fmt.stereo),
        (Projection::fisheye(220.0), StereoLayout::SideBySide)
    );
    assert_eq!(full.markers, [(95.0, "Swim".to_string())]);
    assert_eq!(full.scripts, [server.url("/files/1.funscript")]);
    assert_eq!(full.location, scenes[0].location);

    // Opening the scene plays the tallest encoding within 2880 lines.
    let f = src.open(&scenes[0].location).unwrap();
    let mut buf = [0u8; 32];
    let n = f.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"six k video");
    assert_eq!(server.count(|r| r.path() == "/files/8k.mp4"), 0);

    let sc = src.sidecars(&scenes[0]).unwrap();
    assert_eq!(sc.scripts.len(), 1);
    let s = src.open(&sc.scripts[0].location).unwrap();
    let n = s.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], br#"{"actions":[]}"#);

    // A media URL without an extension is read directly, not parsed.
    let raw = src.open(&server.url("/stream/9")).unwrap();
    let n = raw.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"raw stream");
}

#[test]
fn deovr_feed_login() {
    let server = deovr_server(true);
    let good = Credentials::new("bob", "p@ss");
    let src = build(&SourceConfig::DeoVr(feed(
        server.url("/deovr"),
        Some(good),
        None,
    )))
    .unwrap();
    assert_eq!(src.list(None).unwrap().len(), 2);
    let bad = Credentials::new("bob", "nope");
    let src = build(&SourceConfig::DeoVr(feed(
        server.url("/deovr"),
        Some(bad),
        None,
    )))
    .unwrap();
    assert!(matches!(src.list(None), Err(Error::Auth(_))));
}

// ---------------------------------------------------------------- HereSphere

#[test]
fn heresphere_library_details_and_open() {
    let base: Arc<std::sync::OnceLock<String>> = Arc::default();
    let b = base.clone();
    let server = TestServer::start(move |req| {
        let json = |s: String| resp(200, s, &[("Content-Type", "application/json")]);
        if req.method == "POST" {
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            assert_eq!(req.header("content-type"), Some("application/json"));
            if body["username"] != "bob" || body["password"] != "pw" {
                return json(r#"{"access":-1,"library":[]}"#.into());
            }
        }
        let base = b.get().unwrap();
        match (req.method.as_str(), req.path()) {
            ("POST", "/heresphere") => json(format!(
                r#"{{"access":1,"library":[{{"name":"All","list":["{base}/heresphere/1","/heresphere/2"]}}]}}"#
            )),
            ("POST", "/heresphere/1") => json(
                r#"{"title":"Beach Day","duration":1800000.0,"thumbnailImage":"/img/1.jpg",
                    "projection":"equirectangular","stereo":"tb","lens":"Linear","fov":180.0,
                    "media":[{"name":"h265","sources":[
                        {"resolution":2880,"height":2880,"width":5760,"size":11,"url":"/files/6k.mp4"},
                        {"resolution":1440,"height":1440,"width":2880,"size":11,"url":"/files/3k.mp4"}]}],
                    "scripts":[{"name":"Beach Day.funscript","url":"/files/1.funscript"}],
                    "subtitles":[{"name":"English","language":"en","url":"/files/1.en.srt"}],
                    "tags":[{"name":"Swim","start":95000.0,"end":120000.0},{"name":"Studio:X"}]}"#
                    .into(),
            ),
            ("GET", "/files/3k.mp4") => serve_bytes(req, b"three k vid", true),
            _ => resp(404, "", &[]),
        }
    });
    base.set(server.base.clone()).unwrap();
    let creds = Credentials::new("bob", "pw");
    let src = build(&SourceConfig::HereSphere(feed(
        server.url("/heresphere"),
        Some(creds),
        Some(2000),
    )))
    .unwrap();
    assert_eq!(src.kind(), SourceKind::HereSphere);
    let groups = src.list(None).unwrap();
    assert_eq!(names(&groups), ["All"]);
    let scenes = src.list(Some(&groups[0].location)).unwrap();
    // The second video's details fail (404): it keeps its URL-derived name.
    assert_eq!(names(&scenes), ["Beach Day", "2"]);
    let beach = &scenes[0];
    assert_eq!(beach.duration, Some(1800.0));
    assert_eq!(
        beach.thumbnail_url.as_deref(),
        Some(server.url("/img/1.jpg").as_str())
    );
    let fmt = beach.format.unwrap();
    assert_eq!(
        (fmt.projection, fmt.stereo),
        (Projection::EQUIRECT_180, StereoLayout::TopBottom)
    );
    assert_eq!(beach.markers, [(95.0, "Swim".to_string())]);

    let sc = src.sidecars(beach).unwrap();
    assert_eq!(sc.scripts[0].location, server.url("/files/1.funscript"));
    assert_eq!(sc.subtitles[0].location, server.url("/files/1.en.srt"));

    let f = src.open(&beach.location).unwrap();
    let mut buf = [0u8; 32];
    let n = f.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"three k vid");
    // The details request asked for media sources only when opening.
    let posts: Vec<serde_json::Value> = server
        .requests()
        .iter()
        .filter(|r| r.path() == "/heresphere/1")
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert!(posts.iter().any(|p| p["needsMediaSource"] == false));
    assert!(posts.iter().any(|p| p["needsMediaSource"] == true));

    let wrong = build(&SourceConfig::HereSphere(feed(
        server.url("/heresphere"),
        Some(Credentials::new("bob", "bad")),
        None,
    )))
    .unwrap();
    assert!(matches!(wrong.list(None), Err(Error::Auth(_))));
}
