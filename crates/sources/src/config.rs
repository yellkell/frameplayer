//! Persisted source definitions and the factory that turns them into live
//! [`Source`] objects.

use crate::credentials::Credentials;
use crate::error::{Result, SourceError};
use crate::http::{HttpAuth, HttpClient};
use crate::source::Source;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Local,
    Smb,
    WebDav,
    Http,
    Sftp,
    Dlna,
    DeoVr,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Local => "local",
            SourceKind::Smb => "smb",
            SourceKind::WebDav => "webdav",
            SourceKind::Http => "http",
            SourceKind::Sftp => "sftp",
            SourceKind::Dlna => "dlna",
            SourceKind::DeoVr => "deovr",
        }
    }

    pub fn parse(s: &str) -> Option<SourceKind> {
        Some(match s {
            "local" => SourceKind::Local,
            "smb" => SourceKind::Smb,
            "webdav" => SourceKind::WebDav,
            "http" => SourceKind::Http,
            "sftp" => SourceKind::Sftp,
            "dlna" => SourceKind::Dlna,
            "deovr" => SourceKind::DeoVr,
            _ => return None,
        })
    }
}

/// Guess the source kind from a URI scheme.
pub fn kind_for_uri(uri: &str) -> Option<SourceKind> {
    let scheme = uri.split_once("://")?.0.to_ascii_lowercase();
    Some(match scheme.as_str() {
        "file" => SourceKind::Local,
        "smb" => SourceKind::Smb,
        "webdav" | "webdavs" | "dav" | "davs" => SourceKind::WebDav,
        "http" | "https" => SourceKind::Http,
        "sftp" | "ssh" => SourceKind::Sftp,
        "dlna" => SourceKind::Dlna,
        "deovr+http" | "deovr+https" | "deovr" => SourceKind::DeoVr,
        _ => return None,
    })
}

/// A user-configured source as stored in config / the library DB.
/// Secrets are *not* stored here; see [`crate::CredentialStore`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Stable identifier, also the credential-store key.
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
    /// Root URI: `file:///home/deck/Videos`, `smb://nas/share/vr`,
    /// `webdavs://host/dav/`, `https://host/videos/`, `sftp://host/path`,
    /// `dlna://<device description URL>`, `https://xbvr:9999` (DeoVR feed).
    pub uri: String,
    /// SFTP host key fingerprint pinned on first connect (SHA-256, hex).
    #[serde(default)]
    pub pinned_host_key: Option<String>,
}

/// Build a live source from its configuration.
pub async fn connect(cfg: &SourceConfig, creds: Option<Credentials>) -> Result<Arc<dyn Source>> {
    let http_auth = creds
        .as_ref()
        .map(|c| HttpAuth::new(&c.username, &c.password));
    Ok(match cfg.kind {
        SourceKind::Local => Arc::new(crate::local::LocalSource::from_uri(&cfg.uri)?),
        SourceKind::Http => Arc::new(crate::http::HttpSource::new(
            HttpClient::new(http_auth)?,
            &cfg.uri,
        )?),
        SourceKind::WebDav => Arc::new(crate::webdav::WebDavSource::new(
            HttpClient::new(http_auth)?,
            &cfg.uri,
        )?),
        SourceKind::DeoVr => {
            let mut client = crate::deovr::DeoVrClient::new(HttpClient::new(http_auth)?, &cfg.uri)?;
            if let Some(c) = &creds {
                client = client.with_login(&c.username, &c.password);
            }
            Arc::new(crate::deovr::DeoVrSource::new(client))
        }
        SourceKind::Dlna => {
            let location = cfg.uri.strip_prefix("dlna://").unwrap_or(&cfg.uri);
            let location = if location.starts_with("http") {
                location.to_string()
            } else {
                format!("http://{location}")
            };
            Arc::new(crate::dlna::DlnaSource::connect(HttpClient::new(None)?, &location).await?)
        }
        SourceKind::Smb => Arc::new(crate::smb::SmbSource::connect(&cfg.uri, creds).await?),
        SourceKind::Sftp => Arc::new(
            crate::sftp::SftpSource::connect(&cfg.uri, creds, cfg.pinned_host_key.clone()).await?,
        ),
    })
}

/// Validate a config without connecting (URI scheme matches kind, parses).
pub fn validate(cfg: &SourceConfig) -> Result<()> {
    if cfg.kind == SourceKind::DeoVr || cfg.kind == SourceKind::Dlna {
        // These accept plain http(s) URLs too.
        url::Url::parse(
            cfg.uri
                .trim_start_matches("deovr+")
                .trim_start_matches("dlna://"),
        )
        .or_else(|_| {
            url::Url::parse(&format!("http://{}", cfg.uri.trim_start_matches("dlna://")))
        })?;
        return Ok(());
    }
    match kind_for_uri(&cfg.uri) {
        Some(k) if k == cfg.kind => Ok(()),
        _ => Err(SourceError::InvalidUri(format!(
            "{} is not a {} URI",
            cfg.uri,
            cfg.kind.as_str()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        assert_eq!(kind_for_uri("file:///x"), Some(SourceKind::Local));
        assert_eq!(kind_for_uri("WEBDAVS://h/"), Some(SourceKind::WebDav));
        assert_eq!(
            kind_for_uri("deovr+https://h/deovr/1"),
            Some(SourceKind::DeoVr)
        );
        assert_eq!(kind_for_uri("gopher://x"), None);
        assert_eq!(SourceKind::parse("smb"), Some(SourceKind::Smb));
    }

    #[test]
    fn validate_configs() {
        let mut c = SourceConfig {
            id: "a".into(),
            name: "A".into(),
            kind: SourceKind::Smb,
            uri: "smb://nas/share".into(),
            pinned_host_key: None,
        };
        assert!(validate(&c).is_ok());
        c.uri = "http://nas/".into();
        assert!(validate(&c).is_err());
        c.kind = SourceKind::DeoVr;
        assert!(validate(&c).is_ok());
    }
}
