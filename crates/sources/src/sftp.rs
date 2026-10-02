//! SFTP (`sftp://[user@]host[:port]/path`).
//!
//! With the `sftp` feature this uses pure-Rust russh + russh-sftp (ring
//! crypto backend). Host keys are trust-on-first-use: pass the SHA-256
//! fingerprint stored in [`crate::SourceConfig::pinned_host_key`]; when
//! none is pinned the key is accepted and its fingerprint logged so the
//! app can persist it (see [`SftpSource::host_key_fingerprint`]).
//! Without the feature, `connect` fails with "built without SFTP support".

use crate::credentials::Credentials;
use crate::error::{Result, SourceError};
use percent_encoding::percent_decode_str;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpTarget {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    /// Absolute remote path (`/` if none given).
    pub path: String,
}

impl SftpTarget {
    pub fn uri_for(&self, path: &str) -> String {
        let port = if self.port == 22 {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        let user = self
            .user
            .as_ref()
            .map(|u| format!("{u}@"))
            .unwrap_or_default();
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let enc: String = path
            .split('/')
            .map(|s| percent_encoding::utf8_percent_encode(s, SEG).to_string())
            .collect::<Vec<_>>()
            .join("/");
        format!(
            "sftp://{user}{host}{port}{}{enc}",
            if path.starts_with('/') { "" } else { "/" }
        )
    }
}

const SEG: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'?')
    .add(b'%');

pub fn parse_sftp_uri(uri: &str) -> Result<SftpTarget> {
    let u = url::Url::parse(uri)?;
    if u.scheme() != "sftp" && u.scheme() != "ssh" {
        return Err(SourceError::InvalidUri(format!(
            "not an sftp:// URI: {uri}"
        )));
    }
    let host = u
        .host_str()
        .ok_or_else(|| SourceError::InvalidUri(format!("missing host in {uri}")))?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let user = (!u.username().is_empty()).then(|| {
        percent_decode_str(u.username())
            .decode_utf8_lossy()
            .into_owned()
    });
    let path = percent_decode_str(u.path())
        .decode_utf8_lossy()
        .into_owned();
    Ok(SftpTarget {
        host,
        port: u.port().unwrap_or(22),
        user,
        path: if path.is_empty() { "/".into() } else { path },
    })
}

#[cfg(feature = "sftp")]
mod imp {
    use super::*;
    use crate::config::SourceKind;
    use crate::source::{Entry, RandomAccess, Source};
    use async_trait::async_trait;
    use bytes::Bytes;
    use parking_lot::Mutex as PlMutex;
    use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
    use russh_sftp::client::SftpSession;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    struct Handler {
        pinned: Option<String>,
        seen: Arc<PlMutex<Option<String>>>,
    }

    impl russh::client::Handler for Handler {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            key: &PublicKeyOrCertificate,
        ) -> std::result::Result<bool, Self::Error> {
            let fp = match key {
                PublicKeyOrCertificate::PublicKey { key, .. } => {
                    key.fingerprint(HashAlg::Sha256).to_string()
                }
                PublicKeyOrCertificate::Certificate(c) => {
                    c.public_key().fingerprint(HashAlg::Sha256).to_string()
                }
            };
            *self.seen.lock() = Some(fp.clone());
            Ok(match &self.pinned {
                Some(p) => p == &fp,
                None => {
                    tracing::info!("SFTP: trusting new host key {fp} (TOFU)");
                    true
                }
            })
        }
    }

    fn sftp_err(e: russh_sftp::client::error::Error) -> SourceError {
        let s = e.to_string();
        if s.to_ascii_lowercase().contains("no such file") {
            SourceError::NotFound(s)
        } else {
            SourceError::Protocol(format!("sftp: {s}"))
        }
    }

    pub struct SftpSource {
        target: SftpTarget,
        session: SftpSession,
        fingerprint: Option<String>,
        _handle: russh::client::Handle<Handler>,
    }

    impl SftpSource {
        pub async fn connect(
            uri: &str,
            creds: Option<Credentials>,
            pinned_host_key: Option<String>,
        ) -> Result<Self> {
            let target = parse_sftp_uri(uri)?;
            let config = Arc::new(russh::client::Config::default());
            let seen = Arc::new(PlMutex::new(None));
            let handler = Handler {
                pinned: pinned_host_key.clone(),
                seen: seen.clone(),
            };
            let mut handle =
                russh::client::connect(config, (target.host.as_str(), target.port), handler)
                    .await
                    .map_err(|e| {
                        if pinned_host_key.is_some()
                            && seen.lock().as_ref() != pinned_host_key.as_ref()
                        {
                            SourceError::Protocol(format!(
                                "SFTP host key changed for {} (got {:?})",
                                target.host,
                                seen.lock()
                            ))
                        } else {
                            SourceError::Protocol(format!("ssh connect: {e}"))
                        }
                    })?;
            let creds = creds.unwrap_or_default();
            let user = if creds.username.is_empty() {
                target.user.clone().unwrap_or_else(|| "deck".into())
            } else {
                creds.username.clone()
            };
            let mut ok = false;
            if let Some(pem) = creds.private_key.as_deref() {
                let pass = (!creds.password.is_empty()).then_some(creds.password.as_str());
                let key = russh::keys::decode_secret_key(pem, pass)
                    .map_err(|e| SourceError::Crypto(format!("private key: {e}")))?;
                let hash = handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten();
                ok = handle
                    .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(|e| SourceError::Protocol(e.to_string()))?
                    .success();
            }
            if !ok && !creds.password.is_empty() {
                ok = handle
                    .authenticate_password(&user, creds.password.as_str())
                    .await
                    .map_err(|e| SourceError::Protocol(e.to_string()))?
                    .success();
            }
            if !ok {
                return Err(SourceError::Auth);
            }
            let channel = handle
                .channel_open_session()
                .await
                .map_err(|e| SourceError::Protocol(e.to_string()))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| SourceError::Protocol(e.to_string()))?;
            let session = SftpSession::new(channel.into_stream())
                .await
                .map_err(sftp_err)?;
            let fingerprint = seen.lock().clone();
            Ok(SftpSource {
                target,
                session,
                fingerprint,
                _handle: handle,
            })
        }

        /// `SHA256:...` fingerprint of the server key seen at connect, for
        /// pinning in the source config.
        pub fn host_key_fingerprint(&self) -> Option<&str> {
            self.fingerprint.as_deref()
        }

        fn path_of(&self, uri: &str) -> Result<String> {
            if uri.is_empty() {
                return Ok(self.target.path.clone());
            }
            let t = parse_sftp_uri(uri)?;
            if !t.host.eq_ignore_ascii_case(&self.target.host) || t.port != self.target.port {
                return Err(SourceError::InvalidUri(format!(
                    "{uri} is not on {}",
                    self.target.host
                )));
            }
            Ok(t.path)
        }
    }

    #[async_trait]
    impl Source for SftpSource {
        fn kind(&self) -> SourceKind {
            SourceKind::Sftp
        }

        fn root_uri(&self) -> String {
            self.target.uri_for(&self.target.path)
        }

        async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
            let path = self.path_of(dir)?;
            let rd = self
                .session
                .read_dir(path.clone())
                .await
                .map_err(sftp_err)?;
            let mut out: Vec<Entry> = rd
                .filter_map(|e| {
                    let name = e.file_name();
                    if name.starts_with('.') {
                        return None;
                    }
                    let md = e.metadata();
                    let is_dir = md.is_dir();
                    let child = format!("{}/{}", path.trim_end_matches('/'), name);
                    Some(Entry {
                        uri: self.target.uri_for(&child),
                        is_dir,
                        size: (!is_dir).then_some(md.size).flatten(),
                        mtime: md.mtime.map(i64::from),
                        name,
                        ..Default::default()
                    })
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
            let file = self.session.open(path).await.map_err(sftp_err)?;
            let size = file.metadata().await.ok().and_then(|m| m.size);
            Ok(Box::new(SftpFile {
                file: tokio::sync::Mutex::new(file),
                size,
            }))
        }
    }

    /// An open remote file; positioned reads are serialized seek+read.
    pub struct SftpFile {
        file: tokio::sync::Mutex<russh_sftp::client::fs::File>,
        size: Option<u64>,
    }

    #[async_trait]
    impl RandomAccess for SftpFile {
        async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
            if self.size.is_some_and(|s| offset >= s) || len == 0 {
                return Ok(Bytes::new());
            }
            let mut f = self.file.lock().await;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                let n = f.read(&mut buf[done..]).await?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            buf.truncate(done);
            Ok(Bytes::from(buf))
        }

        fn size(&self) -> Option<u64> {
            self.size
        }
    }
}

#[cfg(not(feature = "sftp"))]
mod imp {
    use super::*;
    use crate::config::SourceKind;
    use crate::source::{Entry, RandomAccess, Source};
    use async_trait::async_trait;

    /// Placeholder present in builds without the `sftp` feature.
    pub struct SftpSource {
        never: std::convert::Infallible,
    }

    impl SftpSource {
        pub async fn connect(
            uri: &str,
            _creds: Option<Credentials>,
            _pinned_host_key: Option<String>,
        ) -> Result<Self> {
            parse_sftp_uri(uri)?;
            Err(SourceError::Unsupported(
                "FramePlayer was built without SFTP support (enable the `sftp` cargo feature)"
                    .into(),
            ))
        }

        pub fn host_key_fingerprint(&self) -> Option<&str> {
            match self.never {}
        }
    }

    #[async_trait]
    impl Source for SftpSource {
        fn kind(&self) -> SourceKind {
            SourceKind::Sftp
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
        let t = parse_sftp_uri("sftp://deck@nas:2222/srv/vr%20videos").unwrap();
        assert_eq!(
            t,
            SftpTarget {
                host: "nas".into(),
                port: 2222,
                user: Some("deck".into()),
                path: "/srv/vr videos".into()
            }
        );
        assert_eq!(
            t.uri_for("/srv/vr videos/a.mp4"),
            "sftp://deck@nas:2222/srv/vr%20videos/a.mp4"
        );
        assert_eq!(
            parse_sftp_uri(&t.uri_for("/x y/#1.mp4")).unwrap().path,
            "/x y/#1.mp4"
        );
        let t = parse_sftp_uri("sftp://host").unwrap();
        assert_eq!((t.port, t.path.as_str()), (22, "/"));
        assert!(parse_sftp_uri("ftp://host/").is_err());
    }

    #[cfg(not(feature = "sftp"))]
    #[tokio::test]
    async fn disabled_build_reports_clearly() {
        let Err(e) = SftpSource::connect("sftp://h/", None, None).await else {
            panic!()
        };
        assert!(e.to_string().contains("built without SFTP support"));
    }
}
