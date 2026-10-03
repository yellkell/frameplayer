//! Source configuration, credentials, and their on-disk storage.
//!
//! Credentials live inside [`SourceConfig`] and are written in clear JSON to
//! a file only the user can read (mode 0600, folder 0700). SteamOS gaming
//! mode has no secret service; see `docs/OUTLINE.md` section 3.6.

use crate::error::{Error, Result};
use crate::urlutil::redact;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// User name and password for a network source.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// User name; for SMB may be `DOMAIN\user` or `user@domain`.
    pub username: String,
    /// Password, stored in clear in the 0600 config file.
    #[serde(default)]
    pub password: String,
}

impl Credentials {
    /// Builds credentials from a user name and password.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Credentials {
        Credentials {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// Which kind of source a configuration or [`crate::Source`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A local folder (internal storage, microSD, USB drive).
    Local,
    /// An HTTP(S) server with autoindex directory listings.
    Http,
    /// A WebDAV share.
    #[serde(rename = "webdav")]
    WebDav,
    /// A DLNA/UPnP media server.
    Dlna,
    /// A DeoVR JSON feed (XBVR, Stash, ...).
    #[serde(rename = "deovr")]
    DeoVr,
    /// A HereSphere JSON API (XBVR, Stash, ...).
    #[serde(rename = "heresphere")]
    HereSphere,
    /// An SMB2/3 share.
    Smb,
}

impl SourceKind {
    /// Short label for the UI.
    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::Local => "Local folder",
            SourceKind::Http => "HTTP",
            SourceKind::WebDav => "WebDAV",
            SourceKind::Dlna => "DLNA",
            SourceKind::DeoVr => "DeoVR feed",
            SourceKind::HereSphere => "HereSphere API",
            SourceKind::Smb => "SMB share",
        }
    }
}

/// A local folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Folder shown at the root of the source.
    pub root: PathBuf,
}

/// An HTTP(S) autoindex server or a WebDAV share.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Root folder URL. WebDAV also accepts `webdav(s)://` and `dav(s)://`.
    pub url: String,
    /// HTTP Basic credentials.
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Accept self-signed or otherwise invalid TLS certificates (common on
    /// home NAS boxes).
    #[serde(default)]
    pub insecure_tls: bool,
}

/// A DLNA/UPnP media server.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct DlnaConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name (usually the device's friendly name).
    pub name: String,
    /// Device description URL (the SSDP `LOCATION`).
    pub location: String,
    /// ContentDirectory control URL, when already known from discovery.
    /// Fetched from the device description when absent.
    #[serde(default)]
    pub control_url: Option<String>,
    /// ContentDirectory service type, e.g.
    /// `urn:schemas-upnp-org:service:ContentDirectory:1`.
    #[serde(default)]
    pub service_type: Option<String>,
}

/// A DeoVR JSON feed or HereSphere JSON API endpoint.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Feed URL, e.g. `http://xbvr:9999/deovr` or `http://xbvr:9999/heresphere`.
    pub url: String,
    /// Feed login (DeoVR form login / HereSphere JSON body) and HTTP Basic auth.
    #[serde(default)]
    pub credentials: Option<Credentials>,
    /// Accept invalid TLS certificates.
    #[serde(default)]
    pub insecure_tls: bool,
    /// Highest video height to pick among the encodings a scene offers
    /// (e.g. 2880 to skip 8K files). `None` picks the best.
    #[serde(default)]
    pub max_height: Option<u32>,
}

/// An SMB2/3 share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmbConfig {
    /// Stable identifier chosen by the app.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Server host name or IP address.
    pub host: String,
    /// TCP port; 445 when absent.
    #[serde(default)]
    pub port: Option<u16>,
    /// Share name.
    pub share: String,
    /// Folder inside the share shown at the root of the source
    /// (`/`-separated, empty for the share root).
    #[serde(default)]
    pub path: String,
    /// Login; guest access when absent.
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

fn debug_with_url(
    f: &mut fmt::Formatter<'_>,
    ty: &str,
    id: &str,
    name: &str,
    url: &str,
    creds: &Option<Credentials>,
) -> fmt::Result {
    f.debug_struct(ty)
        .field("id", &id)
        .field("name", &name)
        .field("url", &redact(url))
        .field("credentials", creds)
        .finish_non_exhaustive()
}

impl fmt::Debug for HttpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_with_url(
            f,
            "HttpConfig",
            &self.id,
            &self.name,
            &self.url,
            &self.credentials,
        )
    }
}

impl fmt::Debug for FeedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_with_url(
            f,
            "FeedConfig",
            &self.id,
            &self.name,
            &self.url,
            &self.credentials,
        )
    }
}

impl fmt::Debug for DlnaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DlnaConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("location", &redact(&self.location))
            .finish_non_exhaustive()
    }
}

/// Everything needed to rebuild a source, including credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    /// A local folder.
    Local(LocalConfig),
    /// An HTTP(S) server with autoindex listings.
    Http(HttpConfig),
    /// A WebDAV share.
    #[serde(rename = "webdav")]
    WebDav(HttpConfig),
    /// A DLNA/UPnP media server.
    Dlna(DlnaConfig),
    /// A DeoVR JSON feed.
    #[serde(rename = "deovr")]
    DeoVr(FeedConfig),
    /// A HereSphere JSON API.
    #[serde(rename = "heresphere")]
    HereSphere(FeedConfig),
    /// An SMB2/3 share.
    Smb(SmbConfig),
}

impl SourceConfig {
    /// Identifier of the configured source.
    pub fn id(&self) -> &str {
        match self {
            SourceConfig::Local(c) => &c.id,
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => &c.id,
            SourceConfig::Dlna(c) => &c.id,
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => &c.id,
            SourceConfig::Smb(c) => &c.id,
        }
    }

    /// Display name of the configured source.
    pub fn name(&self) -> &str {
        match self {
            SourceConfig::Local(c) => &c.name,
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => &c.name,
            SourceConfig::Dlna(c) => &c.name,
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => &c.name,
            SourceConfig::Smb(c) => &c.name,
        }
    }

    /// Kind of source this configuration builds.
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceConfig::Local(_) => SourceKind::Local,
            SourceConfig::Http(_) => SourceKind::Http,
            SourceConfig::WebDav(_) => SourceKind::WebDav,
            SourceConfig::Dlna(_) => SourceKind::Dlna,
            SourceConfig::DeoVr(_) => SourceKind::DeoVr,
            SourceConfig::HereSphere(_) => SourceKind::HereSphere,
            SourceConfig::Smb(_) => SourceKind::Smb,
        }
    }

    /// One-line description without secrets, for logs and the UI.
    pub fn describe(&self) -> String {
        let target = match self {
            SourceConfig::Local(c) => c.root.display().to_string(),
            SourceConfig::Http(c) | SourceConfig::WebDav(c) => redact(&c.url),
            SourceConfig::Dlna(c) => redact(&c.location),
            SourceConfig::DeoVr(c) | SourceConfig::HereSphere(c) => redact(&c.url),
            SourceConfig::Smb(c) => crate::smb::smb_url(&c.host, c.port, &c.share, &c.path),
        };
        format!("{} \"{}\" ({target})", self.kind().label(), self.name())
    }
}

/// On-disk layout of the sources file.
#[derive(Serialize, Deserialize)]
struct ConfigFile {
    version: u32,
    sources: Vec<SourceConfig>,
}

/// Default path of the sources file: `$XDG_CONFIG_HOME/frameplayer/sources.json`.
pub fn default_config_path() -> PathBuf {
    fp_core::dirs::config_dir().join("sources.json")
}

/// Writes source configurations (with credentials) as JSON readable only by
/// the user: the file gets mode 0600 and a newly created folder 0700. The
/// write is atomic (temporary file + rename).
pub fn save_configs(path: &Path, configs: &[SourceConfig]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let json = serde_json::to_vec_pretty(&ConfigFile {
        version: 1,
        sources: configs.to_vec(),
    })
    .map_err(|e| Error::Config(format!("cannot serialize sources: {e}")))?;

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // `mode` only applies on creation; tighten a leftover temp file too.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// Reads source configurations written by [`save_configs`]. A missing file
/// is an empty list. A file readable by others is tightened to 0600.
pub fn load_configs(path: &Path) -> Result<Vec<SourceConfig>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.permissions().mode() & 0o077 != 0 {
            log::warn!(
                "{} was readable by other users; restricting to 0600",
                path.display()
            );
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    // Accept the versioned object and a bare array.
    let value: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    let sources = if value.is_array() {
        serde_json::from_value(value)
    } else {
        serde_json::from_value::<ConfigFile>(value).map(|f| f.sources)
    };
    sources.map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<SourceConfig> {
        vec![
            SourceConfig::Local(LocalConfig {
                id: "l".into(),
                name: "Videos".into(),
                root: "/home/deck/Videos".into(),
            }),
            SourceConfig::WebDav(HttpConfig {
                id: "w".into(),
                name: "NAS".into(),
                url: "webdavs://nas/vr/".into(),
                credentials: Some(Credentials::new("bob", "hunter2")),
                insecure_tls: true,
            }),
            SourceConfig::DeoVr(FeedConfig {
                id: "d".into(),
                name: "XBVR".into(),
                url: "http://xbvr:9999/deovr".into(),
                credentials: None,
                insecure_tls: false,
                max_height: Some(2880),
            }),
            SourceConfig::Smb(SmbConfig {
                id: "s".into(),
                name: "Share".into(),
                host: "nas.local".into(),
                port: None,
                share: "media".into(),
                path: "vr".into(),
                credentials: Some(Credentials::new("bob", "hunter2")),
            }),
        ]
    }

    #[test]
    fn save_and_load_roundtrip_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/dir/sources.json");
        save_configs(&path, &sample()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
        assert_eq!(load_configs(&path).unwrap(), sample());
        // Overwrite keeps 0600 and leaves no temp file.
        save_configs(&path, &sample()[..1]).unwrap();
        assert_eq!(load_configs(&path).unwrap().len(), 1);
        assert!(!dir.path().join("sub/dir/sources.json.tmp").exists());
    }

    #[test]
    fn load_missing_is_empty_and_tightens_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.json");
        assert!(load_configs(&path).unwrap().is_empty());
        std::fs::write(&path, "[]").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_configs(&path).unwrap().is_empty());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::write(&path, "{nope").unwrap();
        assert!(matches!(load_configs(&path), Err(Error::Config(_))));
    }

    #[test]
    fn json_shape_is_tagged() {
        let j = serde_json::to_string(&sample()[1]).unwrap();
        assert!(j.contains("\"type\":\"webdav\""), "{j}");
        let j = serde_json::to_string(&sample()[2]).unwrap();
        assert!(j.contains("\"type\":\"deovr\""), "{j}");
        // Optional fields may be omitted.
        let c: SourceConfig =
            serde_json::from_str(r#"{"type":"http","id":"h","name":"H","url":"http://h/"}"#)
                .unwrap();
        assert_eq!(c.kind(), SourceKind::Http);
    }

    #[test]
    fn debug_and_describe_hide_passwords() {
        let mut configs = sample();
        configs.push(SourceConfig::Http(HttpConfig {
            id: "h".into(),
            name: "H".into(),
            url: "http://bob:hunter2@h/".into(),
            credentials: None,
            insecure_tls: false,
        }));
        for c in &configs {
            let dbg = format!("{c:?}");
            assert!(!dbg.contains("hunter2"), "{dbg}");
            assert!(!c.describe().contains("hunter2"), "{}", c.describe());
        }
        assert_eq!(configs[0].id(), "l");
        assert_eq!(configs[3].name(), "Share");
        assert!(configs[3].describe().contains("smb://nas.local/media/vr"));
    }
}
