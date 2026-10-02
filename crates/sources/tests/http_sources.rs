//! End-to-end tests of the HTTP-based sources against a tiny in-process
//! HTTP/1.1 server (ranges, no-range fallback, auth, redirects, WebDAV,
//! DLNA SOAP, DeoVR feeds, HLS segment streaming).

use fp_sources::http::{HttpAuth, HttpClient, HttpFile, HttpSource};
use fp_sources::{RandomAccess, Source, SourceError};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Resp {
            status,
            headers: vec![],
            body: body.into(),
        }
    }
    fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

type Handler = Arc<dyn Fn(Req) -> Resp + Send + Sync>;

async fn serve(handler: Handler) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let first: Vec<&str> = lines.next().unwrap().split(' ').collect();
                let mut headers = HashMap::new();
                for l in lines {
                    if let Some((k, v)) = l.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                let len: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = buf[header_end..].to_vec();
                while body.len() < len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                let req = Req {
                    method: first[0].into(),
                    path: first[1].into(),
                    headers,
                    body,
                };
                let r = handler(req);
                let mut out = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    r.status,
                    r.body.len()
                );
                for (k, v) in &r.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.write_all(&r.body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 256) as u8).collect()
}

/// Serve `content` honouring `Range` if `ranges` is set.
fn file_response(req: &Req, content: &[u8], ranges: bool) -> Resp {
    if ranges {
        if let Some(r) = req.headers.get("range") {
            let spec = r.trim_start_matches("bytes=");
            let (a, b) = spec.split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            if a >= content.len() {
                return Resp::new(416, "")
                    .header("Content-Range", &format!("bytes */{}", content.len()));
            }
            let b: usize = if b.is_empty() {
                content.len() - 1
            } else {
                b.parse::<usize>().unwrap().min(content.len() - 1)
            };
            return Resp::new(206, content[a..=b].to_vec())
                .header("Content-Range", &format!("bytes {a}-{b}/{}", content.len()));
        }
    }
    Resp::new(200, content.to_vec())
}

fn client() -> HttpClient {
    HttpClient::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), None)
}

fn client_auth(u: &str, p: &str) -> HttpClient {
    HttpClient::with_client(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        Some(HttpAuth::new(u, p)),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ranged_file_and_redirect() {
    let content = Arc::new(data(300_000));
    let hits = Arc::new(AtomicUsize::new(0));
    let (c, h) = (content.clone(), hits.clone());
    let base = serve(Arc::new(move |req| {
        h.fetch_add(1, Ordering::SeqCst);
        match req.path.as_str() {
            "/old.mp4" => Resp::new(302, "").header("Location", "/v.mp4"),
            "/v.mp4" => file_response(&req, &c, true),
            _ => Resp::new(404, ""),
        }
    }))
    .await;
    let f = HttpFile::open(client(), &format!("{base}/old.mp4"))
        .await
        .unwrap();
    assert!(f.supports_ranges());
    assert_eq!(f.size(), Some(300_000));
    assert!(f.url().ends_with("/v.mp4"));
    assert_eq!(
        &f.read_at(1000, 10).await.unwrap()[..],
        &content[1000..1010]
    );
    assert_eq!(
        &f.read_at(299_995, 100).await.unwrap()[..],
        &content[299_995..]
    );
    assert!(f.read_at(300_000, 10).await.unwrap().is_empty());
    assert!(matches!(
        HttpFile::open(client(), &format!("{base}/nope")).await,
        Err(SourceError::NotFound(_))
    ));

    // Blocking adapter over HTTP.
    let handle = tokio::runtime::Handle::current();
    let ra: Arc<dyn RandomAccess> = Arc::new(f);
    let out = tokio::task::spawn_blocking(move || {
        let mut r = fp_sources::BlockingReader::new(
            ra,
            handle,
            fp_sources::ReadAheadConfig {
                chunk_size: 65536,
                prefetch_chunks: 3,
                keep_behind: 1,
            },
        );
        let mut v = Vec::new();
        r.read_to_end(&mut v).unwrap();
        v
    })
    .await
    .unwrap();
    assert_eq!(out, *content);
}

#[tokio::test]
async fn server_without_range_support() {
    let content = Arc::new(data(100_000));
    let gets = Arc::new(AtomicUsize::new(0));
    let (c, g) = (content.clone(), gets.clone());
    let base = serve(Arc::new(move |req| {
        g.fetch_add(1, Ordering::SeqCst);
        file_response(&req, &c, false)
    }))
    .await;
    let f = HttpFile::open(client(), &format!("{base}/v.mp4"))
        .await
        .unwrap();
    assert!(!f.supports_ranges());
    assert_eq!(f.size(), Some(100_000));
    // Forward reads reuse the initial stream.
    assert_eq!(&f.read_at(0, 100).await.unwrap()[..], &content[..100]);
    assert_eq!(
        &f.read_at(50_000, 100).await.unwrap()[..],
        &content[50_000..50_100]
    );
    assert_eq!(gets.load(Ordering::SeqCst), 1);
    // A backwards read restarts the GET.
    assert_eq!(&f.read_at(10, 5).await.unwrap()[..], &content[10..15]);
    assert_eq!(gets.load(Ordering::SeqCst), 2);
    assert_eq!(
        &f.read_at(99_990, 100).await.unwrap()[..],
        &content[99_990..]
    );
}

#[tokio::test]
async fn basic_and_digest_auth() {
    let base_basic = serve(Arc::new(|req: Req| {
        match req.headers.get("authorization") {
            // "deck:pw"
            Some(v) if v == "Basic ZGVjazpwdw==" => Resp::new(200, "ok"),
            _ => Resp::new(401, "").header("WWW-Authenticate", "Basic realm=\"x\""),
        }
    }))
    .await;
    assert_eq!(
        client_auth("deck", "pw")
            .get_text(&format!("{base_basic}/a"))
            .await
            .unwrap(),
        "ok"
    );
    assert!(matches!(
        client_auth("deck", "wrong")
            .get_text(&format!("{base_basic}/a"))
            .await,
        Err(SourceError::Auth)
    ));
    assert!(matches!(
        client().get_text(&format!("{base_basic}/a")).await,
        Err(SourceError::Auth)
    ));

    let base_digest = serve(Arc::new(|req: Req| {
        let ok = req.headers.get("authorization").is_some_and(|v| {
            // Validate against the RFC 2617 algorithm using the client's cnonce/nc.
            let p = |k: &str| {
                v.split(&format!("{k}="))
                    .nth(1)
                    .map(|s| {
                        s.trim_start_matches('"')
                            .split(['"', ','])
                            .next()
                            .unwrap()
                            .to_string()
                    })
                    .unwrap_or_default()
            };
            use md5::{Digest, Md5};
            let h = |s: String| hex::encode(Md5::digest(s.as_bytes()));
            let ha1 = h("deck:r:pw".to_string());
            let ha2 = h(format!("{}:{}", req.method, p("uri")));
            let expect = h(format!("{ha1}:n1:{}:{}:auth:{ha2}", p("nc"), p("cnonce")));
            p("response") == expect && p("uri") == req.path
        });
        if ok {
            Resp::new(200, "digest ok")
        } else {
            Resp::new(401, "").header(
                "WWW-Authenticate",
                "Digest realm=\"r\", qop=\"auth\", nonce=\"n1\", algorithm=MD5",
            )
        }
    }))
    .await;
    let c = client_auth("deck", "pw");
    assert_eq!(
        c.get_text(&format!("{base_digest}/x?y=1")).await.unwrap(),
        "digest ok"
    );
    // Second request reuses the challenge (no extra 401 round trip needed).
    assert_eq!(
        c.get_text(&format!("{base_digest}/z")).await.unwrap(),
        "digest ok"
    );
}

#[tokio::test]
async fn http_index_listing() {
    let base = serve(Arc::new(|req: Req| match req.path.as_str() {
        "/videos/" => Resp::new(200, r#"<html><a href="../">up</a><a href="a_180_LR.mp4">a</a><a href="sub/">sub/</a></html>"#),
        _ => Resp::new(404, ""),
    }))
    .await;
    let src = HttpSource::new(client(), &format!("{base}/videos/")).unwrap();
    let list = src.list("").await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "a_180_LR.mp4");
    assert!(list[1].is_dir);
}

#[tokio::test]
async fn webdav_propfind_and_read() {
    let content = Arc::new(data(5000));
    let c = content.clone();
    let base = serve(Arc::new(move |req: Req| {
        if req.method == "PROPFIND" {
            assert_eq!(req.headers.get("depth").map(String::as_str), Some("1"));
            assert!(String::from_utf8_lossy(&req.body).contains("propfind"));
            if req.headers.get("authorization").map(String::as_str) != Some("Basic dTpw") {
                return Resp::new(401, "").header("WWW-Authenticate", "Basic realm=\"dav\"");
            }
            let body = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">
                <D:response><D:href>/dav/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
                <D:response><D:href>/dav/clip%20one.mp4</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>5000</D:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
                </D:multistatus>"#;
            return Resp::new(207, body).header("Content-Type", "application/xml");
        }
        if req.path == "/dav/clip%20one.mp4" {
            return file_response(&req, &c, true);
        }
        Resp::new(404, "")
    }))
    .await;
    let uri = base.replace("http://", "webdav://") + "/dav";
    let src = fp_sources::webdav::WebDavSource::new(client_auth("u", "p"), &uri).unwrap();
    let list = src.list("").await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "clip one.mp4");
    assert_eq!(list[0].size, Some(5000));
    let f = src.open(&list[0].uri).await.unwrap();
    assert_eq!(&f.read_at(4990, 20).await.unwrap()[..], &content[4990..]);
}

#[tokio::test]
async fn dlna_browse_roundtrip() {
    let base_cell: Arc<parking_lot::Mutex<String>> = Default::default();
    let bc = base_cell.clone();
    let base = serve(Arc::new(move |req: Req| match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/desc.xml") => Resp::new(
            200,
            r#"<root><device><friendlyName>Test</friendlyName><UDN>uuid:abc</UDN><serviceList><service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType><controlURL>/cd</controlURL></service></serviceList></device></root>"#,
        ),
        ("POST", "/cd") => {
            assert!(req.headers.get("soapaction").unwrap().contains("#Browse"));
            let body = String::from_utf8_lossy(&req.body).to_string();
            let base = bc.lock().clone();
            let didl = if body.contains("<ObjectID>0</ObjectID>") {
                r#"<DIDL-Lite><container id="1" parentID="0"><dc:title>Videos</dc:title></container></DIDL-Lite>"#.to_string()
            } else {
                format!(r#"<DIDL-Lite><item id="1$5" parentID="1"><dc:title>clip</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/mp4:*" size="7">{base}/media/5.mp4</res></item></DIDL-Lite>"#)
            };
            let esc = didl.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            Resp::new(200, format!(r#"<s:Envelope><s:Body><u:BrowseResponse><Result>{esc}</Result><NumberReturned>1</NumberReturned><TotalMatches>1</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"#))
        }
        ("GET", "/media/5.mp4") => file_response(&req, b"1234567", true),
        _ => Resp::new(404, ""),
    }))
    .await;
    *base_cell.lock() = base.clone();
    let src = fp_sources::dlna::DlnaSource::connect(client(), &format!("{base}/desc.xml"))
        .await
        .unwrap();
    let root = src.list("").await.unwrap();
    assert_eq!(root.len(), 1);
    assert!(root[0].is_dir);
    let items = src.list(&root[0].uri).await.unwrap();
    assert_eq!(items[0].name, "clip");
    let f = src.open(&items[0].uri).await.unwrap();
    assert_eq!(&f.read_at(2, 3).await.unwrap()[..], b"345");
    let all = fp_sources::walk(&src, "", 4).await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn deovr_source() {
    let base_cell: Arc<parking_lot::Mutex<String>> = Default::default();
    let bc = base_cell.clone();
    let base = serve(Arc::new(move |req: Req| {
        let base = bc.lock().clone();
        match req.path.as_str() {
            "/deovr" => Resp::new(200, format!(r#"{{"scenes":[{{"name":"All","list":[{{"title":"S1","videoLength":60,"video_url":"{base}/deovr/1"}}]}}]}}"#)),
            "/deovr/1" => Resp::new(
                200,
                format!(r#"{{"title":"S1","is3d":true,"screenType":"sphere","stereoMode":"tb","encodings":[{{"name":"h265","videoSources":[{{"resolution":1080,"url":"{base}/f/low"}},{{"resolution":4096,"url":"{base}/f/high"}}]}}]}}"#),
            ),
            "/f/high" => file_response(&req, b"HIGHQUALITY", true),
            "/f/low" => file_response(&req, b"low", true),
            _ => Resp::new(404, ""),
        }
    }))
    .await;
    *base_cell.lock() = base.clone();
    let client = fp_sources::deovr::DeoVrClient::new(client(), &base).unwrap();
    let video = client
        .video(&format!("deovr+{base}/deovr/1"))
        .await
        .unwrap();
    assert_eq!(
        video.projection(),
        (fp_core::Projection::EQUIRECT_360, fp_core::StereoMode::Ou)
    );
    let mut src = fp_sources::deovr::DeoVrSource::new(client);
    let lists = src.list("").await.unwrap();
    assert_eq!(lists[0].name, "All");
    let scenes = src.list(&lists[0].uri).await.unwrap();
    assert_eq!(scenes[0].uri, format!("deovr+{base}/deovr/1"));
    let f = src.open(&scenes[0].uri).await.unwrap();
    assert_eq!(f.size(), Some(11));
    src.max_height = Some(2000);
    let f = src.open(&scenes[0].uri).await.unwrap();
    assert_eq!(&f.read_at(0, 10).await.unwrap()[..], b"low");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hls_segment_stream() {
    let base = serve(Arc::new(|req: Req| match req.path.as_str() {
        "/master.m3u8" => Resp::new(200, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900,RESOLUTION=3840x2160\nhi/index.m3u8\n"),
        "/hi/index.m3u8" => Resp::new(200, "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2,\n#EXT-X-BYTERANGE:3@1\nall.bin\n#EXTINF:2,\nseg2.m4s\n#EXT-X-ENDLIST\n"),
        "/hi/init.mp4" => Resp::new(200, "INIT"),
        "/hi/all.bin" => file_response(&req, b"xABCx", true),
        "/hi/seg2.m4s" => Resp::new(200, "DEF"),
        _ => Resp::new(404, ""),
    }))
    .await;
    let c = client();
    let m = fp_sources::stream::resolve(&c, &format!("{base}/master.m3u8"))
        .await
        .unwrap();
    assert_eq!(m.variants.len(), 2);
    let best = m.best_variant(None).unwrap().clone();
    assert_eq!(best.height, Some(2160));
    let stream = fp_sources::stream::open_variant(&c, &m, &best.id)
        .await
        .unwrap();
    assert_eq!(stream.total_duration(), 4.0);
    let handle = tokio::runtime::Handle::current();
    let out = tokio::task::spawn_blocking(move || {
        let mut r = fp_sources::BlockingSegmentReader::new(stream, &handle, 2);
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        s
    })
    .await
    .unwrap();
    assert_eq!(out, "INITABCDEF");
}
