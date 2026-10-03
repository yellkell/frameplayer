//! HTTP ByteSource against a local server: ranges, block cache,
//! read-ahead, Basic auth, retries and the no-range fallback.

mod common;

use common::{Req, TestServer, pattern, resp, serve_bytes};
use fp_core::ByteSource;
use fp_sources::{CacheOptions, Credentials, Error, HttpClient, HttpOptions};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const AUTH: &str = "Basic Ym9iOnMzY3JldA=="; // bob:s3cret

fn is_get(r: &Req) -> bool {
    r.method == "GET"
}

fn read_all(f: &dyn ByteSource, chunk: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = f.read_at(out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn range_requests_cache_and_read_ahead() {
    let data = Arc::new(pattern(3 * (1 << 20) + 512 * 1024));
    let d = data.clone();
    let server = TestServer::start(move |req| {
        if req.header("authorization") != Some(AUTH) {
            return resp(401, "no", &[("WWW-Authenticate", "Basic realm=\"x\"")]);
        }
        serve_bytes(req, &d, true)
    });
    let client = HttpClient::new(
        HttpOptions::default(),
        Some(&Credentials::new("bob", "s3cret")),
    );
    let url = server.url("/v/My%20Clip.mp4");
    let f = client.open(&url).unwrap();
    assert!(f.supports_ranges());
    assert_eq!(f.size(), Some(data.len() as u64));
    assert_eq!(f.block_size(), 1 << 20);
    assert!(f.describe().contains("My%20Clip.mp4"));

    // Demuxer-style sequential reads of 64 KiB through the first 1.5 MiB.
    let mut buf = vec![0u8; 64 * 1024];
    let mut pos = 0usize;
    while pos < 1536 * 1024 {
        let n = f.read_at(pos as u64, &mut buf).unwrap();
        assert_eq!(buf[..n], data[pos..pos + n]);
        pos += n;
    }
    // The probe filled block 0; blocks 1 and 2 came by read-ahead because
    // access was sequential.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.count(is_get) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.count(is_get), 3);
    let st = f.stats();
    assert_eq!((st.fetches, st.read_aheads), (0, 2), "{st:?}");
    let gets_before = server.count(is_get);
    let n = f.read_at(2 << 20, &mut buf).unwrap();
    assert_eq!(buf[..n], data[2 << 20..(2 << 20) + n]);
    // Wait for any read-ahead of block 3 to land, then check block 2 was not refetched.
    std::thread::sleep(Duration::from_millis(100));
    assert!(server.count(is_get) <= gets_before + 1);

    // Seek to the end (moov atom), then back to the start: served from cache.
    let end = data.len() as u64;
    let mut tail = [0u8; 100];
    assert_eq!(f.read_at(end - 100, &mut tail).unwrap(), 100);
    assert_eq!(tail[..], data[data.len() - 100..]);
    assert_eq!(f.read_at(end, &mut tail).unwrap(), 0);
    let gets = server.count(is_get);
    f.read_at(0, &mut buf).unwrap();
    f.read_at(end - 100, &mut tail).unwrap();
    assert_eq!(
        server.count(is_get),
        gets,
        "cached blocks are not refetched"
    );

    // Every request was a bounded range on one of at most 4 blocks.
    for r in server.requests() {
        let range = r.header("range").expect("all GETs are ranged");
        assert!(range.starts_with("bytes="), "{range}");
    }
    assert!(server.count(is_get) <= 5, "{} GETs", server.count(is_get));

    // Whole file in one pass with odd-sized reads.
    assert_eq!(read_all(&f, 200_000), *data);
}

#[test]
fn wrong_credentials_and_missing_files() {
    let server = TestServer::start(|req| {
        if req.path() == "/missing.mp4" {
            return resp(404, "nope", &[]);
        }
        if req.header("authorization") != Some(AUTH) {
            return resp(401, "no", &[]);
        }
        serve_bytes(req, b"0123456789", true)
    });
    let bad = HttpClient::new(
        HttpOptions::default(),
        Some(&Credentials::new("bob", "wrong")),
    );
    let err = bad.open(&server.url("/a.mp4")).unwrap_err();
    assert!(matches!(err, Error::Auth(_)), "{err:?}");
    let err = bad.open(&server.url("/missing.mp4")).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    // 4xx are not retried.
    assert_eq!(server.requests().len(), 2);

    // Credentials embedded in the URL work and never show up in descriptions.
    let anon = HttpClient::new(HttpOptions::default(), None);
    let url = server
        .url("/a.mp4")
        .replace("http://", "http://bob:s3cret@");
    let f = anon.open(&url).unwrap();
    assert!(!f.describe().contains("s3cret"));
    let mut b = [0u8; 4];
    assert_eq!(f.read_at(3, &mut b).unwrap(), 4);
    assert_eq!(&b, b"3456");
    // Small files are a single short block.
    assert_eq!(f.size(), Some(10));
}

#[test]
fn transient_failures_are_retried() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let server = TestServer::start(move |req| {
        if h.fetch_add(1, Ordering::SeqCst) < 2 {
            return resp(503, "busy", &[]);
        }
        serve_bytes(req, b"hello world", true)
    });
    let opts = HttpOptions {
        retry_delay: Duration::from_millis(5),
        ..HttpOptions::default()
    };
    let client = HttpClient::new(opts.clone(), None);
    let f = client.open(&server.url("/x.mp4")).unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    let mut b = [0u8; 5];
    f.read_at(6, &mut b).unwrap();
    assert_eq!(&b, b"world");

    // Persistent failure gives up after max_retries.
    let always = TestServer::start(|_| resp(503, "down", &[]));
    let client = HttpClient::new(
        HttpOptions {
            max_retries: 2,
            ..opts
        },
        None,
    );
    let err = client.open(&always.url("/x.mp4")).unwrap_err();
    assert!(
        matches!(err, Error::HttpStatus { status: 503, .. }),
        "{err:?}"
    );
    assert_eq!(always.requests().len(), 3);
}

#[test]
fn connection_refused_is_an_error() {
    // Bind and drop a listener to get a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = HttpClient::new(
        HttpOptions {
            max_retries: 1,
            retry_delay: Duration::from_millis(1),
            ..HttpOptions::default()
        },
        None,
    );
    let err = client
        .open(&format!("http://127.0.0.1:{port}/x.mp4"))
        .unwrap_err();
    assert!(matches!(err, Error::Http { .. }), "{err:?}");
}

#[test]
fn servers_without_ranges_stream_sequentially() {
    let data = Arc::new(pattern(1 << 20));
    let d = data.clone();
    let server = TestServer::start(move |req| serve_bytes(req, &d, false));
    let opts = HttpOptions {
        cache: CacheOptions {
            block_size: 64 * 1024,
            max_blocks: 3,
            read_ahead: true,
        },
        max_forward_skip: 128 * 1024,
        ..HttpOptions::default()
    };
    let client = HttpClient::new(opts, None);
    let f = client.open(&server.url("/plain.mp4")).unwrap();
    assert!(!f.supports_ranges());
    assert_eq!(f.size(), Some(data.len() as u64));

    // Sequential reading works through one streamed response.
    assert_eq!(read_all(&f, 48 * 1024), *data);
    assert_eq!(server.count(is_get), 1);

    // A backward seek far from the start, outside the cache, is refused.
    let mut buf = [0u8; 16];
    let err = f.read_at(512 * 1024, &mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported, "{err}");
    assert!(err.to_string().contains("range"), "{err}");

    // Near the start it restarts the stream and skips forward.
    assert_eq!(f.read_at(70_000, &mut buf).unwrap(), 16);
    assert_eq!(buf[..], data[70_000..70_016]);
    assert_eq!(server.count(is_get), 2);

    // The cached tail is still served.
    assert_eq!(f.read_at(data.len() as u64 - 16, &mut buf).unwrap(), 16);
    assert_eq!(buf[..], data[data.len() - 16..]);
}
