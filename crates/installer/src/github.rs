//! Find and download the newest FramePlayer build from GitHub Releases,
//! pre-releases included.
//!
//! `releases/latest` skips pre-releases, so we list
//! `GET /repos/<owner>/<repo>/releases` and pick the newest non-draft
//! release that carries an aarch64 tarball (`frameplayer-<ver>-aarch64.tar.gz`,
//! or `.tar.zst`). A sibling `<tarball>.sha256` asset, when present, is used
//! to verify the download. No token is needed (60 requests/hour/IP).

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use url::Url;

/// Public release list of the project.
pub const RELEASES_API: &str = "https://api.github.com/repos/yellkell/frameplayer/releases";

/// The API URL, overridable with `FRAMEPLAYER_RELEASES_API` (testing, forks).
pub fn releases_api() -> String {
    std::env::var("FRAMEPLAYER_RELEASES_API").unwrap_or_else(|_| RELEASES_API.to_string())
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

/// The build we will install.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedRelease {
    pub tag: String,
    /// Version parsed from the tarball name.
    pub version: String,
    pub prerelease: bool,
    pub tarball: GhAsset,
    pub sha256: Option<GhAsset>,
}

pub fn parse_releases(json: &[u8]) -> Result<Vec<GhRelease>> {
    serde_json::from_slice(json).context("GitHub returned an unexpected release list")
}

/// Version part of `frameplayer-<ver>-<arch>.tar.(gz|zst)`, if the name matches.
pub fn tarball_version<'a>(name: &'a str, arch: &str) -> Option<&'a str> {
    let rest = name.strip_prefix("frameplayer-")?;
    let stem = rest
        .strip_suffix(".tar.gz")
        .or_else(|| rest.strip_suffix(".tar.zst"))?;
    let ver = stem.strip_suffix(&format!("-{arch}"))?;
    (!ver.is_empty() && fp_updater::layout::validate_version(ver).is_ok()).then_some(ver)
}

/// Newest non-draft release (by publish date, pre-releases included) with a
/// tarball for `arch`. `.tar.gz` is preferred over `.tar.zst`.
pub fn select_release(releases: &[GhRelease], arch: &str) -> Option<SelectedRelease> {
    let mut candidates: Vec<&GhRelease> = releases.iter().filter(|r| !r.draft).collect();
    // ISO-8601 UTC timestamps sort lexically; a stable sort keeps API order
    // (newest first) for ties and missing dates.
    candidates.sort_by(|a, b| {
        let ka = a.published_at.as_deref().or(a.created_at.as_deref());
        let kb = b.published_at.as_deref().or(b.created_at.as_deref());
        kb.cmp(&ka)
    });
    candidates.into_iter().find_map(|r| {
        let pick = |ext: &str| {
            r.assets
                .iter()
                .find(|a| a.name.ends_with(ext) && tarball_version(&a.name, arch).is_some())
        };
        let tarball = pick(".tar.gz").or_else(|| pick(".tar.zst"))?.clone();
        let version = tarball_version(&tarball.name, arch)?.to_string();
        let sha_name = format!("{}.sha256", tarball.name);
        let sha256 = r.assets.iter().find(|a| a.name == sha_name).cloned();
        Some(SelectedRelease {
            tag: r.tag_name.clone(),
            version,
            prerelease: r.prerelease,
            tarball,
            sha256,
        })
    })
}

/// Hex digest from a `sha256sum`-style file: the line naming `file`, else
/// the first line. Returns lower-case hex.
pub fn parse_sha256_file(text: &str, file: &str) -> Option<String> {
    let valid = |h: &str| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
    let mut first = None;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(hash) = it.next().filter(|h| valid(h)) else {
            continue;
        };
        let name = it.next().map(|n| n.trim_start_matches('*'));
        if name.is_some_and(|n| n == file || n.ends_with(&format!("/{file}"))) {
            return Some(hash.to_ascii_lowercase());
        }
        first.get_or_insert_with(|| hash.to_ascii_lowercase());
    }
    first
}

/// Query GitHub and choose a release.
pub async fn latest_release(http: &reqwest::Client, arch: &str) -> Result<SelectedRelease> {
    let api = releases_api();
    let resp = http
        .get(&api)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .with_context(|| format!("could not reach {api} (is this PC online?)"))?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub is rate-limiting this network ({status}); wait an hour or put the release tarball next to this program");
    }
    if !status.is_success() {
        bail!("{api} returned {status}");
    }
    let body = resp.bytes().await?;
    let releases = parse_releases(&body)?;
    select_release(&releases, arch).ok_or_else(|| {
        anyhow!(
            "no FramePlayer release with an {arch} build was found on GitHub ({} release(s) checked)",
            releases.len()
        )
    })
}

/// Download the selected tarball into `dir` (reusing a verified cached
/// copy), checking size and, if published, SHA-256. Returns the path and
/// whether the checksum was verified.
pub async fn download_release(
    http: &reqwest::Client,
    sel: &SelectedRelease,
    dir: &Path,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(PathBuf, bool)> {
    let expected = match &sel.sha256 {
        Some(a) => {
            let url = Url::parse(&a.browser_download_url)?;
            let text = fp_updater::download::fetch_bytes(http, &url, 64 * 1024).await?;
            Some(
                parse_sha256_file(&String::from_utf8_lossy(&text), &sel.tarball.name)
                    .ok_or_else(|| anyhow!("{} does not contain a SHA-256", a.name))?,
            )
        }
        None => None,
    };
    let dest = dir.join(&sel.tarball.name);
    if let Some(h) = &expected {
        if dest.is_file() && fp_updater::sha256_file(&dest)?.eq_ignore_ascii_case(h) {
            return Ok((dest, true));
        }
    }
    let url = Url::parse(&sel.tarball.browser_download_url)?;
    fp_updater::download::download_resumable(
        http,
        &url,
        &dest,
        fp_updater::download::Expect {
            size: (sel.tarball.size > 0).then_some(sel.tarball.size),
            sha256: expected.as_deref(),
        },
        progress,
    )
    .await?;
    Ok((dest, expected.is_some()))
}

/// A release tarball placed next to the installer (offline installs and
/// testing): the highest version among `frameplayer-*-<arch>.tar.*` in `dirs`.
pub fn local_tarball(dirs: &[PathBuf], arch: &str) -> Option<PathBuf> {
    let mut best: Option<(semver::Version, PathBuf)> = None;
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(v) = tarball_version(&name, arch) else {
                continue;
            };
            let v = semver::Version::parse(v).unwrap_or(semver::Version::new(0, 0, 0));
            if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
                best = Some((v, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
      {"tag_name":"v0.3.0-alpha.2","name":"draft","draft":true,"prerelease":true,
       "published_at":null,"created_at":"2026-10-05T00:00:00Z",
       "assets":[{"name":"frameplayer-0.3.0-alpha.2-aarch64.tar.gz","browser_download_url":"https://x/d","size":1}]},
      {"tag_name":"v0.2.0-alpha.1","prerelease":true,"draft":false,"published_at":"2026-10-03T10:00:00Z",
       "assets":[
         {"name":"frameplayer-install-windows-x86_64.exe","browser_download_url":"https://x/exe","size":7000000},
         {"name":"frameplayer-0.2.0-alpha.1-aarch64.tar.gz.sha256","browser_download_url":"https://x/sha","size":100},
         {"name":"frameplayer-0.2.0-alpha.1-aarch64.tar.gz","browser_download_url":"https://x/tgz","size":12345678},
         {"name":"SHA256SUMS","browser_download_url":"https://x/sums","size":300}]},
      {"tag_name":"v0.1.0","prerelease":false,"draft":false,"published_at":"2026-09-01T10:00:00Z",
       "assets":[{"name":"frameplayer-0.1.0-aarch64.tar.zst","browser_download_url":"https://x/old","size":5}]},
      {"tag_name":"v0.2.1-docs","prerelease":false,"draft":false,"published_at":"2026-10-04T10:00:00Z",
       "assets":[{"name":"notes.txt","browser_download_url":"https://x/n","size":5}]}
    ]"#;

    #[test]
    fn selects_newest_prerelease_with_tarball() {
        let rels = parse_releases(FIXTURE.as_bytes()).unwrap();
        assert_eq!(rels.len(), 4);
        let s = select_release(&rels, "aarch64").unwrap();
        assert_eq!(s.tag, "v0.2.0-alpha.1");
        assert_eq!(s.version, "0.2.0-alpha.1");
        assert!(s.prerelease);
        assert_eq!(s.tarball.browser_download_url, "https://x/tgz");
        assert_eq!(s.tarball.size, 12345678);
        assert_eq!(s.sha256.unwrap().browser_download_url, "https://x/sha");
        assert!(select_release(&rels, "x86_64").is_none());
    }

    #[test]
    fn falls_back_to_older_zst_release() {
        let mut rels = parse_releases(FIXTURE.as_bytes()).unwrap();
        rels.retain(|r| r.tag_name != "v0.2.0-alpha.1");
        let s = select_release(&rels, "aarch64").unwrap();
        assert_eq!(s.version, "0.1.0");
        assert!(s.tarball.name.ends_with(".tar.zst"));
        assert!(s.sha256.is_none());
        assert!(select_release(&[], "aarch64").is_none());
    }

    #[test]
    fn asset_name_matching() {
        assert_eq!(
            tarball_version("frameplayer-1.2.3-aarch64.tar.gz", "aarch64"),
            Some("1.2.3")
        );
        assert_eq!(
            tarball_version("frameplayer-1.2.3-rc.1-aarch64.tar.zst", "aarch64"),
            Some("1.2.3-rc.1")
        );
        assert_eq!(
            tarball_version("frameplayer-1.2.3-aarch64.tar.gz.sha256", "aarch64"),
            None
        );
        assert_eq!(
            tarball_version("frameplayer-install-linux-aarch64", "aarch64"),
            None
        );
        assert_eq!(
            tarball_version("frameplayer-1.2.3-from-1.2.2.fpd", "aarch64"),
            None
        );
        assert_eq!(
            tarball_version("frameplayer--aarch64.tar.gz", "aarch64"),
            None
        );
        assert_eq!(
            tarball_version("frameplayer-../x-aarch64.tar.gz", "aarch64"),
            None
        );
    }

    #[test]
    fn sha256_file_parsing() {
        let h = "a".repeat(64);
        let g = "B".repeat(64);
        let text = format!("{g}  other.tar.gz\n{h}  frameplayer-1-aarch64.tar.gz\n");
        assert_eq!(
            parse_sha256_file(&text, "frameplayer-1-aarch64.tar.gz").unwrap(),
            h
        );
        assert_eq!(parse_sha256_file(&text, "missing").unwrap(), "b".repeat(64));
        assert_eq!(parse_sha256_file(&format!("{h}\n"), "x").unwrap(), h);
        assert_eq!(
            parse_sha256_file(&format!("{g} *dir/f.tgz"), "f.tgz").unwrap(),
            "b".repeat(64)
        );
        assert!(parse_sha256_file("nothing here", "x").is_none());
    }

    #[test]
    fn finds_local_tarball_highest_version() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            "frameplayer-0.1.0-aarch64.tar.gz",
            "frameplayer-0.2.0-alpha.1-aarch64.tar.gz",
            "frameplayer-0.10.0-x86_64.tar.gz",
            "frameplayer-install.exe",
        ] {
            std::fs::write(d.path().join(n), b"x").unwrap();
        }
        let p =
            local_tarball(&[PathBuf::from("/nonexistent"), d.path().into()], "aarch64").unwrap();
        assert!(p.ends_with("frameplayer-0.2.0-alpha.1-aarch64.tar.gz"));
        assert!(local_tarball(&[PathBuf::from("/nonexistent")], "aarch64").is_none());
    }

    #[tokio::test]
    async fn downloads_and_verifies_against_local_server() {
        use std::io::{Read, Write};
        let payload = b"pretend tarball".to_vec();
        let sum = fp_updater::sha256_hex(&payload);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (p2, s2) = (payload.clone(), sum.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body: Vec<u8> = if req.starts_with("GET /sha") {
                    format!("{s2}  frameplayer-9.9.9-aarch64.tar.gz\n").into_bytes()
                } else {
                    p2.clone()
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&body);
            }
        });
        let base = format!("http://127.0.0.1:{port}");
        let sel = SelectedRelease {
            tag: "v9.9.9".into(),
            version: "9.9.9".into(),
            prerelease: true,
            tarball: GhAsset {
                name: "frameplayer-9.9.9-aarch64.tar.gz".into(),
                browser_download_url: format!("{base}/tgz"),
                size: payload.len() as u64,
            },
            sha256: Some(GhAsset {
                name: "frameplayer-9.9.9-aarch64.tar.gz.sha256".into(),
                browser_download_url: format!("{base}/sha"),
                size: 0,
            }),
        };
        let d = tempfile::tempdir().unwrap();
        let http = reqwest::Client::new();
        let (p, verified) = download_release(&http, &sel, d.path(), &mut |_, _| {})
            .await
            .unwrap();
        assert!(verified);
        assert_eq!(std::fs::read(&p).unwrap(), payload);
        // Second call: cached copy verified via the .sha256, no tarball GET.
        let (p2, _) = download_release(&http, &sel, d.path(), &mut |_, _| {})
            .await
            .unwrap();
        assert_eq!(p, p2);
    }
}
