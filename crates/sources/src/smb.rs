//! SMB 2/3 shares (`smb://[domain;]host[:port]/share/path`).
//!
//! With the `smb` feature this uses the pure-Rust `smb2` crate (NTLM auth,
//! signing/encryption, pipelined positioned reads), so nothing has to be
//! linked from the system. Without it the [`SmbSource`] type still exists
//! but `connect` fails with a clear "built without SMB support" error.

use crate::credentials::Credentials;
use crate::error::{Result, SourceError};
use percent_encoding::percent_decode_str;

/// A parsed `smb://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbTarget {
    pub host: String,
    pub port: u16,
    pub domain: Option<String>,
    pub user: Option<String>,
    pub share: String,
    /// Path inside the share, `/`-separated, no leading slash.
    pub path: String,
}

impl SmbTarget {
    pub fn addr(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Rebuild a URI for `path` on the same share.
    pub fn uri_for(&self, path: &str) -> String {
        let port = if self.port == 445 {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        let enc = |s: &str| {
            s.split('/')
                .map(|seg| percent_encoding::utf8_percent_encode(seg, URI_SEG).to_string())
                .collect::<Vec<_>>()
                .join("/")
        };
        let p = path.trim_matches('/');
        if p.is_empty() {
            format!("smb://{}{port}/{}", self.host, enc(&self.share))
        } else {
            format!("smb://{}{port}/{}/{}", self.host, enc(&self.share), enc(p))
        }
    }
}

const URI_SEG: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'?')
    .add(b'%')
    .add(b'"')
    .add(b'<')
    .add(b'>');

/// Parse `smb://[domain;][user@]host[:port]/share[/path]`.
pub fn parse_smb_uri(uri: &str) -> Result<SmbTarget> {
    let rest = uri
        .strip_prefix("smb://")
        .or_else(|| uri.strip_prefix("SMB://"))
        .ok_or_else(|| SourceError::InvalidUri(format!("not an smb:// URI: {uri}")))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (mut domain, mut user) = (None, None);
    if let Some(ui) = userinfo {
        let ui = percent_decode_str(ui).decode_utf8_lossy().into_owned();
        let ui = ui.split_once(':').map(|(u, _)| u.to_string()).unwrap_or(ui);
        match ui.split_once(';') {
            Some((d, u)) => {
                domain = Some(d.to_string());
                user = Some(u.to_string());
            }
            None => user = Some(ui),
        }
    }
    let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
        let (h, p) = h
            .split_once(']')
            .ok_or_else(|| SourceError::InvalidUri(uri.into()))?;
        (
            h.to_string(),
            p.strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(445),
        )
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse().map_err(|_| SourceError::InvalidUri(uri.into()))?,
            ),
            None => (hostport.to_string(), 445),
        }
    };
    if host.is_empty() {
        return Err(SourceError::InvalidUri(format!("missing host in {uri}")));
    }
    let path = percent_decode_str(path).decode_utf8_lossy().into_owned();
    let mut parts = path.splitn(2, '/');
    let share = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").trim_matches('/').to_string();
    if share.is_empty() {
        return Err(SourceError::InvalidUri(format!(
            "missing share name in {uri}"
        )));
    }
    Ok(SmbTarget {
        host,
        port,
        domain,
        user,
        share,
        path,
    })
}

#[cfg(feature = "smb")]
mod imp {
    use super::*;
    use crate::config::SourceKind;
    use crate::source::{Entry, RandomAccess, Source};
    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::Mutex;

    fn map_err(e: smb2::Error) -> SourceError {
        let s = e.to_string();
        let lower = s.to_ascii_lowercase();
        if lower.contains("logon")
            || lower.contains("access_denied")
            || lower.contains("access denied")
        {
            SourceError::Auth
        } else if lower.contains("not_found")
            || lower.contains("not found")
            || lower.contains("no such")
        {
            SourceError::NotFound(s)
        } else {
            SourceError::Protocol(format!("smb: {s}"))
        }
    }

    /// Windows FILETIME (100 ns since 1601) → Unix seconds.
    fn filetime_to_unix(ft: u64) -> Option<i64> {
        (ft > 116_444_736_000_000_000).then(|| ((ft - 116_444_736_000_000_000) / 10_000_000) as i64)
    }

    pub struct SmbSource {
        target: SmbTarget,
        inner: Mutex<(smb2::SmbClient, smb2::Tree)>,
    }

    impl SmbSource {
        pub async fn connect(uri: &str, creds: Option<Credentials>) -> Result<Self> {
            let target = parse_smb_uri(uri)?;
            let (user, pass, domain) = match &creds {
                Some(c) => (
                    c.username.clone(),
                    c.password.clone(),
                    c.domain.clone().or(target.domain.clone()),
                ),
                // Guest access.
                None => (
                    target.user.clone().unwrap_or_else(|| "guest".into()),
                    String::new(),
                    target.domain.clone(),
                ),
            };
            let config = smb2::ClientConfig {
                addr: target.addr(),
                username: user,
                password: pass,
                domain: domain.unwrap_or_default(),
                auto_reconnect: true,
                ..Default::default()
            };
            let mut client = smb2::SmbClient::connect(config).await.map_err(map_err)?;
            let tree = client.connect_share(&target.share).await.map_err(map_err)?;
            Ok(SmbSource {
                target,
                inner: Mutex::new((client, tree)),
            })
        }

        fn path_of(&self, uri: &str) -> Result<String> {
            if uri.is_empty() {
                return Ok(self.target.path.clone());
            }
            let t = parse_smb_uri(uri)?;
            if !t.host.eq_ignore_ascii_case(&self.target.host)
                || !t.share.eq_ignore_ascii_case(&self.target.share)
            {
                return Err(SourceError::InvalidUri(format!(
                    "{uri} is not on {}",
                    self.target.uri_for("")
                )));
            }
            Ok(t.path)
        }
    }

    #[async_trait]
    impl Source for SmbSource {
        fn kind(&self) -> SourceKind {
            SourceKind::Smb
        }

        fn root_uri(&self) -> String {
            self.target.uri_for(&self.target.path)
        }

        async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
            let path = self.path_of(dir)?;
            let mut g = self.inner.lock().await;
            let (client, tree) = &mut *g;
            let entries = client.list_directory(tree, &path).await.map_err(map_err)?;
            let mut out: Vec<Entry> = entries
                .into_iter()
                .filter(|e| e.name != "." && e.name != ".." && !e.name.starts_with('.'))
                .map(|e| {
                    let child = if path.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{path}/{}", e.name)
                    };
                    Entry {
                        uri: self.target.uri_for(&child),
                        is_dir: e.is_directory,
                        size: (!e.is_directory).then_some(e.size),
                        mtime: filetime_to_unix(e.modified.0),
                        name: e.name,
                        ..Default::default()
                    }
                })
                .collect();
            out.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(out)
        }

        async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
            let path = self.path_of(uri)?;
            let mut g = self.inner.lock().await;
            let (client, tree) = &mut *g;
            let reader = client
                .open_file_reader(tree, &path)
                .await
                .map_err(map_err)?;
            Ok(Box::new(SmbFile {
                size: reader.size(),
                reader: Some(reader),
            }))
        }
    }

    /// Positioned reads over one open SMB handle.
    pub struct SmbFile {
        reader: Option<smb2::FileReader>,
        size: u64,
    }

    #[async_trait]
    impl RandomAccess for SmbFile {
        async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
            let r = self.reader.as_ref().expect("reader present until drop");
            Ok(Bytes::from(
                r.read_at(offset, len as u64).await.map_err(map_err)?,
            ))
        }

        fn size(&self) -> Option<u64> {
            Some(self.size)
        }
    }

    impl Drop for SmbFile {
        fn drop(&mut self) {
            // Close the server handle asynchronously if a runtime is around.
            if let (Some(r), Ok(h)) = (self.reader.take(), tokio::runtime::Handle::try_current()) {
                h.spawn(async move {
                    let _ = r.close().await;
                });
            }
        }
    }
}

#[cfg(not(feature = "smb"))]
mod imp {
    use super::*;
    use crate::config::SourceKind;
    use crate::source::{Entry, RandomAccess, Source};
    use async_trait::async_trait;

    /// Placeholder present in builds without the `smb` feature.
    pub struct SmbSource {
        never: std::convert::Infallible,
    }

    impl SmbSource {
        pub async fn connect(uri: &str, _creds: Option<Credentials>) -> Result<Self> {
            parse_smb_uri(uri)?;
            Err(SourceError::Unsupported(
                "FramePlayer was built without SMB support (enable the `smb` cargo feature)".into(),
            ))
        }
    }

    #[async_trait]
    impl Source for SmbSource {
        fn kind(&self) -> SourceKind {
            SourceKind::Smb
        }
        fn root_uri(&self) -> String {
            match self.never {}
        }
        async fn list(&self, _dir: &str) -> Result<Vec<Entry>> {
            match self.never {}
        }
        async fn open(&self, _uri: &str) -> Result<Box<dyn RandomAccess>> {
            match self.never {}
        }
    }
}

pub use imp::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uris() {
        let t =
            parse_smb_uri("smb://WORKGROUP;deck@nas.local:1445/Videos/VR%20Stuff/2026").unwrap();
        assert_eq!(t.host, "nas.local");
        assert_eq!(t.port, 1445);
        assert_eq!(t.domain.as_deref(), Some("WORKGROUP"));
        assert_eq!(t.user.as_deref(), Some("deck"));
        assert_eq!(t.share, "Videos");
        assert_eq!(t.path, "VR Stuff/2026");
        assert_eq!(t.addr(), "nas.local:1445");
        assert_eq!(
            t.uri_for("VR Stuff/a.mp4"),
            "smb://nas.local:1445/Videos/VR%20Stuff/a.mp4"
        );
        let t = parse_smb_uri("smb://192.168.1.2/media").unwrap();
        assert_eq!((t.port, t.path.as_str(), t.user.clone()), (445, "", None));
        assert_eq!(t.uri_for(""), "smb://192.168.1.2/media");
        assert_eq!(
            parse_smb_uri("smb://[fe80::1]:445/s").unwrap().addr(),
            "[fe80::1]:445"
        );
        assert!(parse_smb_uri("smb://host").is_err());
        assert!(parse_smb_uri("http://host/share").is_err());
        // Round trip.
        let u = t.uri_for("a b/c#d.mp4");
        assert_eq!(parse_smb_uri(&u).unwrap().path, "a b/c#d.mp4");
    }

    #[cfg(not(feature = "smb"))]
    #[tokio::test]
    async fn disabled_build_reports_clearly() {
        let Err(e) = SmbSource::connect("smb://nas/share", None).await else {
            panic!("should fail")
        };
        assert!(e.to_string().contains("built without SMB support"), "{e}");
    }
}
