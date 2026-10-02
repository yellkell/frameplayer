//! Resumable HTTP downloads with SHA-256 verification.
//!
//! Data streams into `<dest>.part`. If a previous attempt left a partial file
//! we ask for the remainder with `Range: bytes=<len>-`; a `206` with a
//! matching `Content-Range` appends, anything else restarts from zero. The
//! finished file is size- and hash-checked, then renamed onto `dest`. A hash
//! mismatch deletes the partial file so a poisoned prefix is never resumed.

use crate::{check_sha256, sha256_file, Result, UpdateError};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use url::Url;

/// Upper bound for small metadata fetches (manifests, signatures).
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;

/// Build the HTTP client used for update traffic.
pub fn http_client(user_agent: &str) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// GET a small resource fully into memory, refusing anything over `max_len`.
pub async fn fetch_bytes(client: &reqwest::Client, url: &Url, max_len: usize) -> Result<Vec<u8>> {
    let mut resp = client.get(url.clone()).send().await?;
    if !resp.status().is_success() {
        return Err(UpdateError::HttpStatus {
            status: resp.status().as_u16(),
            url: url.to_string(),
        });
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > max_len {
            return Err(UpdateError::InvalidManifest(format!(
                "{url} is larger than {max_len} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// What a finished file must look like.
#[derive(Debug, Clone, Default)]
pub struct Expect<'a> {
    pub size: Option<u64>,
    pub sha256: Option<&'a str>,
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Parse the start offset from `Content-Range: bytes <start>-<end>/<total>`.
fn content_range_start(v: &str) -> Option<u64> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    rest.split('-').next()?.trim().parse().ok()
}

/// Download `url` to `dest`, resuming a previous partial attempt.
/// `progress(done, total)` is called after every chunk.
pub async fn download_resumable(
    client: &reqwest::Client,
    url: &Url,
    dest: &Path,
    expect: Expect<'_>,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let part = part_path(dest);
    let what = url
        .path()
        .rsplit('/')
        .next()
        .unwrap_or("download")
        .to_string();

    // At most one restart: a resume that the server answers oddly falls back
    // to a clean full download.
    for attempt in 0..2 {
        let mut have = tokio::fs::metadata(&part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if let Some(size) = expect.size {
            if have > size {
                have = 0;
                let _ = tokio::fs::remove_file(&part).await;
            }
        }
        let complete = expect.size.is_some_and(|s| s == have && s > 0);
        if !complete {
            let mut req = client.get(url.clone());
            if have > 0 {
                req = req.header(RANGE, format!("bytes={have}-"));
            }
            let mut resp = req.send().await?;
            let status = resp.status();
            let append = if status == StatusCode::PARTIAL_CONTENT {
                let start = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(content_range_start);
                if start != Some(have) {
                    tracing::warn!(?start, have, "server returned unexpected range; restarting");
                    let _ = tokio::fs::remove_file(&part).await;
                    continue;
                }
                true
            } else if status == StatusCode::RANGE_NOT_SATISFIABLE && attempt == 0 {
                let _ = tokio::fs::remove_file(&part).await;
                continue;
            } else if status.is_success() {
                false
            } else {
                return Err(UpdateError::HttpStatus {
                    status: status.as_u16(),
                    url: url.to_string(),
                });
            };
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&part)
                .await?;
            let mut done = if append { have } else { 0 };
            let total = expect
                .size
                .or_else(|| resp.content_length().map(|l| l + done));
            progress(done, total);
            while let Some(chunk) = resp.chunk().await? {
                file.write_all(&chunk).await?;
                done += chunk.len() as u64;
                if expect.size.is_some_and(|s| done > s) {
                    drop(file);
                    let _ = tokio::fs::remove_file(&part).await;
                    return Err(UpdateError::SizeMismatch {
                        what,
                        expected: expect.size.unwrap_or(0),
                        actual: done,
                    });
                }
                progress(done, total);
            }
            file.flush().await?;
            file.sync_all().await?;
        }

        let len = tokio::fs::metadata(&part).await?.len();
        if let Some(size) = expect.size {
            if len != size {
                // Truncated transfer: keep the partial file for the next resume.
                return Err(UpdateError::SizeMismatch {
                    what,
                    expected: size,
                    actual: len,
                });
            }
        }
        if let Some(want) = expect.sha256 {
            let p = part.clone();
            let got = tokio::task::spawn_blocking(move || sha256_file(&p))
                .await
                .map_err(|e| UpdateError::Io(std::io::Error::other(e)))??;
            if let Err(e) = check_sha256(&what, want, &got) {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        }
        tokio::fs::rename(&part, dest).await?;
        return Ok(());
    }
    Err(UpdateError::Io(std::io::Error::other(format!(
        "could not download {url}: server rejected resume twice"
    ))))
}

/// Minimal HTTP/1.1 file server for tests: honours `Range`, and can cut the
/// connection after N body bytes on the first request to exercise resume.
#[cfg(test)]
pub(crate) mod testserver {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    pub struct Server {
        pub files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        /// Cut the body after this many bytes, once.
        pub cut_after: Arc<Mutex<Option<usize>>>,
        pub ignore_range: Arc<Mutex<bool>>,
        pub requests: Arc<AtomicUsize>,
        pub ranges_seen: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        pub fn put(&self, path: &str, data: Vec<u8>) {
            self.files.lock().unwrap().insert(path.to_string(), data);
        }

        /// Start serving; returns base URL like `http://127.0.0.1:1234`.
        pub async fn start(&self) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let me = me.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 1024];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        me.requests.fetch_add(1, Ordering::SeqCst);
                        let req = String::from_utf8_lossy(&buf).to_string();
                        let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                        let range = req.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("range")
                                .then(|| v.trim().to_string())
                        });
                        let data = me.files.lock().unwrap().get(&path).cloned();
                        let Some(data) = data else {
                            let _ = sock
                                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                .await;
                            return;
                        };
                        let mut start = 0usize;
                        let mut head = "HTTP/1.1 200 OK".to_string();
                        if let Some(r) = &range {
                            me.ranges_seen.lock().unwrap().push(r.clone());
                            if !*me.ignore_range.lock().unwrap() {
                                start = r
                                    .trim_start_matches("bytes=")
                                    .trim_end_matches('-')
                                    .parse()
                                    .unwrap_or(0);
                                if start >= data.len() {
                                    let _ = sock
                                        .write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                        .await;
                                    return;
                                }
                                head = format!(
                                    "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {start}-{}/{}",
                                    data.len() - 1,
                                    data.len()
                                );
                            }
                        }
                        let body = &data[start..];
                        let header = format!(
                            "{head}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(header.as_bytes()).await;
                        let cut = me.cut_after.lock().unwrap().take();
                        match cut {
                            Some(n) if n < body.len() => {
                                let _ = sock.write_all(&body[..n]).await;
                                let _ = sock.flush().await;
                                // Drop the socket mid-body.
                            }
                            _ => {
                                let _ = sock.write_all(body).await;
                            }
                        }
                        let _ = sock.shutdown().await;
                    });
                }
            });
            format!("http://{addr}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::Server;
    use super::*;
    use crate::sha256_hex;

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[tokio::test]
    async fn full_download_verifies_hash() {
        let srv = Server::default();
        let data = payload(100_000);
        srv.put("/a.bin", data.clone());
        let base = srv.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a.bin");
        let client = http_client("test").unwrap();
        let url = Url::parse(&format!("{base}/a.bin")).unwrap();
        let sha = sha256_hex(&data);
        let mut calls = 0;
        download_resumable(
            &client,
            &url,
            &dest,
            Expect {
                size: Some(data.len() as u64),
                sha256: Some(&sha),
            },
            &mut |_, _| calls += 1,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(calls > 0);
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn resumes_after_dropped_connection() {
        let srv = Server::default();
        let data = payload(200_000);
        srv.put("/b.bin", data.clone());
        *srv.cut_after.lock().unwrap() = Some(70_000);
        let base = srv.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("b.bin");
        let client = http_client("test").unwrap();
        let url = Url::parse(&format!("{base}/b.bin")).unwrap();
        let sha = sha256_hex(&data);
        let expect = Expect {
            size: Some(data.len() as u64),
            sha256: Some(&sha),
        };
        let first = download_resumable(&client, &url, &dest, expect.clone(), &mut |_, _| {}).await;
        assert!(first.is_err(), "first attempt should be truncated");
        let partial = std::fs::metadata(part_path(&dest)).unwrap().len();
        assert!(partial > 0 && partial < data.len() as u64);
        download_resumable(&client, &url, &dest, expect, &mut |_, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(
            srv.ranges_seen.lock().unwrap().as_slice(),
            [format!("bytes={partial}-")]
        );
    }

    #[tokio::test]
    async fn server_without_range_support_restarts() {
        let srv = Server::default();
        let data = payload(50_000);
        srv.put("/c.bin", data.clone());
        *srv.ignore_range.lock().unwrap() = true;
        let base = srv.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("c.bin");
        std::fs::write(part_path(&dest), &data[..1000]).unwrap();
        let client = http_client("test").unwrap();
        let url = Url::parse(&format!("{base}/c.bin")).unwrap();
        let sha = sha256_hex(&data);
        download_resumable(
            &client,
            &url,
            &dest,
            Expect {
                size: Some(data.len() as u64),
                sha256: Some(&sha),
            },
            &mut |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    #[tokio::test]
    async fn hash_mismatch_deletes_partial() {
        let srv = Server::default();
        srv.put("/d.bin", payload(1000));
        let base = srv.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("d.bin");
        let client = http_client("test").unwrap();
        let url = Url::parse(&format!("{base}/d.bin")).unwrap();
        let wrong = "0".repeat(64);
        let r = download_resumable(
            &client,
            &url,
            &dest,
            Expect {
                size: Some(1000),
                sha256: Some(&wrong),
            },
            &mut |_, _| {},
        )
        .await;
        assert!(matches!(r, Err(UpdateError::HashMismatch { .. })));
        assert!(!part_path(&dest).exists());
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn already_complete_partial_is_not_refetched() {
        let srv = Server::default();
        let data = payload(4096);
        srv.put("/e.bin", data.clone());
        let base = srv.start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("e.bin");
        std::fs::write(part_path(&dest), &data).unwrap();
        let client = http_client("test").unwrap();
        let url = Url::parse(&format!("{base}/e.bin")).unwrap();
        let sha = sha256_hex(&data);
        download_resumable(
            &client,
            &url,
            &dest,
            Expect {
                size: Some(4096),
                sha256: Some(&sha),
            },
            &mut |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(srv.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fetch_bytes_limits_and_404() {
        let srv = Server::default();
        srv.put("/m.json", b"{}".to_vec());
        let base = srv.start().await;
        let client = http_client("test").unwrap();
        let ok = fetch_bytes(&client, &Url::parse(&format!("{base}/m.json")).unwrap(), 10)
            .await
            .unwrap();
        assert_eq!(ok, b"{}");
        let too_big =
            fetch_bytes(&client, &Url::parse(&format!("{base}/m.json")).unwrap(), 1).await;
        assert!(too_big.is_err());
        let missing = fetch_bytes(&client, &Url::parse(&format!("{base}/nope")).unwrap(), 10).await;
        assert!(matches!(
            missing,
            Err(UpdateError::HttpStatus { status: 404, .. })
        ));
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(content_range_start("bytes 100-199/200"), Some(100));
        assert_eq!(content_range_start("bytes */200"), None);
    }
}
