//! SMB2/3 shares (Windows, Samba, NAS boxes).
//!
//! # Implementation choice
//!
//! This uses [`smb`](https://crates.io/crates/smb) (smb-rs), a maintained
//! pure-Rust SMB 2.0.2-3.1.1 client with NTLM authentication, signing and
//! encryption, in its blocking `multi_threaded` mode (no async runtime). It
//! cross-compiles for `aarch64-unknown-linux-gnu` with `cargo zigbuild`
//! without any system library, so the `libsmbclient`-via-`libloading`
//! fallback is not needed. Build with `--no-default-features` to leave SMB
//! out; [`SmbSource::new`] then fails with [`Error::SmbUnavailable`].
//!
//! Locations are `smb://host[:port]/share/dir/file` with percent-encoded
//! segments and no credentials. Files are read through the shared block
//! cache with read-ahead, in requests no larger than the server's
//! negotiated maximum read size.

use crate::config::{SmbConfig, SourceKind};
use crate::error::{Error, Result};
use crate::urlutil::{encode_segment, percent_decode};
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry};
use std::sync::Arc;

/// Builds `smb://host[:port]/share/path` with every segment
/// percent-encoded. `path` is `/`-separated and may be empty.
pub fn smb_url(host: &str, port: Option<u16>, share: &str, path: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]") // IPv6 literal
    } else {
        host.to_string()
    };
    let mut out = format!("smb://{host}");
    if let Some(p) = port.filter(|p| *p != 445) {
        out.push_str(&format!(":{p}"));
    }
    out.push('/');
    out.push_str(&encode_segment(share));
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(&encode_segment(seg));
    }
    out
}

/// A location split into share and decoded path segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmbLocation {
    /// Host name or address (IPv6 without brackets).
    pub host: String,
    /// TCP port, when given.
    pub port: Option<u16>,
    /// Share name.
    pub share: String,
    /// Decoded path segments inside the share.
    pub path: Vec<String>,
}

impl SmbLocation {
    /// Parses `smb://host[:port]/share/path`. Credentials in the URL are
    /// ignored (they belong in the configuration).
    pub fn parse(location: &str) -> Result<SmbLocation> {
        let rest = location
            .get(..6)
            .filter(|p| p.eq_ignore_ascii_case("smb://"))
            .map(|_| &location[6..])
            .ok_or_else(|| Error::invalid(location, "expected an smb:// URL"))?;
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6
                .split_once(']')
                .ok_or_else(|| Error::invalid(location, "bad IPv6 host"))?;
            (h.to_string(), after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (authority.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => Some(
                p.parse::<u16>()
                    .map_err(|_| Error::invalid(location, "bad port"))?,
            ),
            None => None,
        };
        let mut segs = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(percent_decode);
        let share = segs
            .next()
            .ok_or_else(|| Error::invalid(location, "no share name"))?;
        let path: Vec<String> = segs.collect();
        if host.is_empty() {
            return Err(Error::invalid(location, "no host"));
        }
        if path.iter().any(|s| s == ".." || s.contains('\\')) {
            return Err(Error::invalid(location, "path escapes the share"));
        }
        Ok(SmbLocation {
            host,
            port,
            share,
            path,
        })
    }

    /// Back to an `smb://` URL.
    pub fn to_url(&self) -> String {
        smb_url(&self.host, self.port, &self.share, &self.path.join("/"))
    }

    /// Path inside the share with backslashes (empty for the share root).
    pub fn share_path(&self) -> String {
        self.path.join("\\")
    }
}

/// An SMB2/3 share.
pub struct SmbSource {
    id: String,
    name: String,
    config: SmbConfig,
    root: String,
    #[cfg(feature = "smb")]
    client: std::sync::Mutex<Option<Arc<imp::Session>>>,
}

impl std::fmt::Debug for SmbSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbSource")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("credentials", &self.config.credentials)
            .finish_non_exhaustive()
    }
}

impl SmbSource {
    /// Creates the source. No connection is made until it is used.
    pub fn new(config: &SmbConfig) -> Result<SmbSource> {
        if config.host.trim().is_empty() || config.share.trim().is_empty() {
            return Err(Error::Config("SMB source needs a host and a share".into()));
        }
        if !cfg!(feature = "smb") {
            return Err(Error::SmbUnavailable(
                "this build of FramePlayer was compiled without SMB support".into(),
            ));
        }
        Ok(SmbSource {
            id: config.id.clone(),
            name: config.name.clone(),
            root: smb_url(&config.host, config.port, &config.share, &config.path),
            config: config.clone(),
            #[cfg(feature = "smb")]
            client: std::sync::Mutex::new(None),
        })
    }

    /// URL listed by `list(None)`.
    pub fn root(&self) -> &str {
        &self.root
    }

    fn check_same_share(&self, loc: &SmbLocation) -> Result<()> {
        let same = loc.host.eq_ignore_ascii_case(&self.config.host)
            && loc.share.eq_ignore_ascii_case(&self.config.share)
            && loc.port.unwrap_or(445) == self.config.port.unwrap_or(445);
        if same {
            Ok(())
        } else {
            Err(Error::invalid(&loc.to_url(), "not on this source's share"))
        }
    }
}

impl Source for SmbSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Smb
    }

    fn describe(&self) -> String {
        match &self.config.credentials {
            Some(c) => format!("SMB share {} as {}", self.root, c.username),
            None => format!("SMB share {} (guest)", self.root),
        }
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let loc = SmbLocation::parse(location.unwrap_or(&self.root))?;
        self.check_same_share(&loc)?;
        let mut out = Vec::new();
        for item in self.list_dir(&loc)? {
            if item.name.starts_with('.') {
                continue;
            }
            let mut child = loc.clone();
            child.path.push(item.name.clone());
            let url = child.to_url();
            out.push(if item.is_dir {
                dir_entry(item.name, format!("{url}/"), item.modified)
            } else {
                file_entry(item.name, url, Some(item.size), item.modified)
            });
        }
        sort_entries(&mut out);
        Ok(out)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        let loc = SmbLocation::parse(location)?;
        self.check_same_share(&loc)?;
        if loc.path.is_empty() {
            return Err(Error::invalid(
                location,
                "a share cannot be opened as a file",
            ));
        }
        self.open_file(&loc)
    }

    fn parent(&self, location: &str) -> Option<String> {
        let mut loc = SmbLocation::parse(location).ok()?;
        loc.path.pop()?;
        Some(format!("{}/", loc.to_url()))
    }
}

/// One directory entry as the SMB layer reports it.
#[cfg_attr(not(feature = "smb"), allow(dead_code))]
struct DirItem {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<i64>,
}

#[cfg(not(feature = "smb"))]
impl SmbSource {
    fn list_dir(&self, _loc: &SmbLocation) -> Result<Vec<DirItem>> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }

    fn open_file(&self, _loc: &SmbLocation) -> Result<Arc<dyn ByteSource>> {
        Err(Error::SmbUnavailable("compiled without SMB support".into()))
    }
}

#[cfg(feature = "smb")]
mod imp {
    //! The smb-rs backed implementation.

    use super::{DirItem, SmbLocation, SmbSource};
    use crate::cache::{CacheOptions, CachedSource, RangeFetch, lock};
    use crate::error::{Error, Result};
    use fp_core::source::ByteSource;
    use smb::{
        Client, ClientConfig, ConnectionConfig, DirAccessMask, FileAccessMask, FileCreateArgs,
        FileDirectoryInformation, GetLen, ReadAt, Status, UncPath,
    };
    use std::io;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    /// Seconds between 1601-01-01 and 1970-01-01.
    const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

    /// Fallback read size when the negotiated maximum is unknown (the SMB
    /// 2.0.2 limit).
    const DEFAULT_MAX_READ: usize = 64 << 10;

    /// A client connected to the configured share.
    pub(super) struct Session {
        client: Client,
        max_read: usize,
    }

    /// Maps an smb-rs error, keeping "not found" and "access denied"
    /// distinguishable.
    fn map_err(e: smb::Error, what: &str) -> Error {
        if let smb::Error::ReceivedErrorMessage(status, _) = &e {
            match *status {
                Status::U32_LOGON_FAILURE | Status::U32_ACCESS_DENIED => {
                    return Error::Auth(what.to_string());
                }
                Status::U32_OBJECT_NAME_NOT_FOUND
                | Status::U32_OBJECT_PATH_NOT_FOUND
                | Status::U32_BAD_NETWORK_NAME => return Error::NotFound(what.to_string()),
                _ => {}
            }
        }
        Error::Smb(format!("{what}: {e}"))
    }

    fn unc(loc: &SmbLocation) -> Result<UncPath> {
        let server = loc.host.clone();
        let share = UncPath::from_str(&format!(r"\\{server}\{}", loc.share))
            .map_err(|e| Error::invalid(&loc.to_url(), e.to_string()))?;
        let path = loc.share_path();
        Ok(if path.is_empty() {
            share
        } else {
            share.with_path(&path)
        })
    }

    impl SmbSource {
        fn session(&self) -> Result<Arc<Session>> {
            let mut guard = lock(&self.client);
            if let Some(s) = guard.as_ref() {
                return Ok(s.clone());
            }
            let guest = self.config.credentials.is_none();
            let config = ClientConfig {
                connection: ConnectionConfig {
                    port: self.config.port,
                    timeout: Some(Duration::from_secs(15)),
                    allow_unsigned_guest_access: guest,
                    ..ConnectionConfig::default()
                },
                ..ClientConfig::default()
            };
            let client = Client::new(config);
            let loc = SmbLocation {
                host: self.config.host.clone(),
                port: self.config.port,
                share: self.config.share.clone(),
                path: Vec::new(),
            };
            let share = unc(&loc)?;
            let (user, pass) = match &self.config.credentials {
                Some(c) => (c.username.clone(), c.password.clone()),
                None => ("Guest".to_string(), String::new()),
            };
            log::info!("connecting to {} as {user}", self.root);
            client
                .share_connect(&share, &user, pass)
                .map_err(|e| map_err(e, &self.root))?;
            let max_read = client
                .get_connection(&self.config.host)
                .ok()
                .and_then(|c| c.conn_info().map(|i| i.negotiation.max_read_size as usize))
                .filter(|n| *n >= 4096)
                .unwrap_or(DEFAULT_MAX_READ);
            let session = Arc::new(Session { client, max_read });
            *guard = Some(session.clone());
            Ok(session)
        }

        /// Runs `op` with a session; after a connection-level failure the
        /// session is dropped so the next call reconnects.
        fn with_session<T>(&self, op: impl Fn(&Session) -> Result<T>) -> Result<T> {
            let session = self.session()?;
            let r = op(&session);
            if let Err(Error::Smb(_)) = &r {
                *lock(&self.client) = None;
            }
            r
        }

        pub(super) fn list_dir(&self, loc: &SmbLocation) -> Result<Vec<DirItem>> {
            let path = unc(loc)?;
            let url = loc.to_url();
            self.with_session(|s| {
                let access = DirAccessMask::new()
                    .with_list_directory(true)
                    .with_synchronize(true);
                let args = FileCreateArgs::make_open_existing(access.into());
                let res = s
                    .client
                    .create_file(&path, &args)
                    .map_err(|e| map_err(e, &url))?;
                let dir = match res {
                    smb::Resource::Directory(d) => d,
                    _ => return Err(Error::invalid(&url, "not a folder")),
                };
                let mut out = Vec::new();
                for info in dir
                    .query::<FileDirectoryInformation>("*")
                    .map_err(|e| map_err(e, &url))?
                {
                    let info = info.map_err(|e| map_err(e, &url))?;
                    let name = info.file_name.to_string();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let modified = (!info.last_write_time.is_zero()).then(|| {
                        info.last_write_time.since_epoch().as_secs() as i64 - FILETIME_UNIX_OFFSET
                    });
                    out.push(DirItem {
                        name,
                        is_dir: info.file_attributes.directory(),
                        size: info.end_of_file,
                        modified,
                    });
                }
                Ok(out)
            })
        }

        pub(super) fn open_file(&self, loc: &SmbLocation) -> Result<Arc<dyn ByteSource>> {
            let path = unc(loc)?;
            let url = loc.to_url();
            let (file, max_read) = self.with_session(|s| {
                let args = FileCreateArgs::make_open_existing(
                    FileAccessMask::new().with_generic_read(true),
                );
                let res = s
                    .client
                    .create_file(&path, &args)
                    .map_err(|e| map_err(e, &url))?;
                match res {
                    smb::Resource::File(f) => Ok((f, s.max_read)),
                    _ => Err(Error::invalid(&url, "not a file")),
                }
            })?;
            let size = file.get_len().map_err(|e| map_err(e, &url))?;
            let fetcher = SmbFetcher {
                file,
                size,
                max_read,
                url,
            };
            Ok(Arc::new(CachedSource::new(
                fetcher,
                &CacheOptions::default(),
            )))
        }
    }

    /// Reads ranges of one open SMB file.
    struct SmbFetcher {
        file: smb::File,
        size: u64,
        max_read: usize,
        url: String,
    }

    impl RangeFetch for SmbFetcher {
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            if offset >= self.size {
                return Ok(Vec::new());
            }
            let len = len.min((self.size - offset) as usize);
            let mut out = vec![0u8; len];
            let mut done = 0;
            while done < len {
                let chunk = (len - done).min(self.max_read);
                let n = self
                    .file
                    .read_at(&mut out[done..done + chunk], offset + done as u64)
                    .map_err(|e| io::Error::other(format!("{}: {e}", self.url)))?;
                if n == 0 {
                    break;
                }
                done += n;
            }
            out.truncate(done);
            Ok(out)
        }

        fn size(&self) -> Option<u64> {
            Some(self.size)
        }

        fn describe(&self) -> String {
            self.url.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credentials;

    #[test]
    fn urls_roundtrip() {
        assert_eq!(smb_url("nas", None, "media", ""), "smb://nas/media");
        assert_eq!(
            smb_url("nas", Some(4445), "My Share", "/vr/Clip #1.mp4"),
            "smb://nas:4445/My%20Share/vr/Clip%20%231.mp4"
        );
        assert_eq!(smb_url("fe80::1", Some(445), "s", ""), "smb://[fe80::1]/s");
        let l = SmbLocation::parse("smb://u:pw@nas:4445/My%20Share/vr/Clip%20%231.mp4").unwrap();
        assert_eq!(l.host, "nas");
        assert_eq!(l.port, Some(4445));
        assert_eq!(l.share, "My Share");
        assert_eq!(l.path, ["vr", "Clip #1.mp4"]);
        assert_eq!(l.share_path(), "vr\\Clip #1.mp4");
        assert_eq!(l.to_url(), "smb://nas:4445/My%20Share/vr/Clip%20%231.mp4");
        let v6 = SmbLocation::parse("SMB://[fe80::1]:139/s/").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("fe80::1", Some(139)));
        assert!(v6.path.is_empty());
        assert!(SmbLocation::parse("smb://nas").is_err());
        assert!(SmbLocation::parse("http://nas/s").is_err());
        assert!(SmbLocation::parse("smb://nas/s/../x").is_err());
        assert!(SmbLocation::parse("smb://nas:x/s").is_err());
    }

    fn config() -> SmbConfig {
        SmbConfig {
            id: "s".into(),
            name: "NAS".into(),
            host: "nas.local".into(),
            port: None,
            share: "media".into(),
            path: "vr/new".into(),
            credentials: Some(Credentials::new("bob", "hunter2")),
        }
    }

    #[test]
    fn source_basics_without_network() {
        let s = SmbSource::new(&config());
        if !cfg!(feature = "smb") {
            assert!(matches!(s, Err(Error::SmbUnavailable(_))));
            return;
        }
        let s = s.unwrap();
        assert_eq!(s.root(), "smb://nas.local/media/vr/new");
        assert!(!s.describe().contains("hunter2"));
        assert!(!format!("{s:?}").contains("hunter2"));
        assert_eq!(
            s.parent("smb://nas.local/media/vr/new/a.mp4").as_deref(),
            Some("smb://nas.local/media/vr/new/")
        );
        assert_eq!(s.parent("smb://nas.local/media"), None);
        assert!(matches!(
            s.list(Some("smb://other/media/")),
            Err(Error::InvalidLocation { .. })
        ));
        assert!(matches!(
            s.open("smb://nas.local/media"),
            Err(Error::InvalidLocation { .. })
        ));
        let mut bad = config();
        bad.share.clear();
        assert!(matches!(SmbSource::new(&bad), Err(Error::Config(_))));
    }

    #[cfg(feature = "smb")]
    #[test]
    fn unreachable_server_is_an_error_not_a_hang() {
        let mut c = config();
        // Port 9 on localhost: connection refused straight away.
        c.host = "127.0.0.1".into();
        c.port = Some(9);
        let s = SmbSource::new(&c).unwrap();
        let err = s.list(None).unwrap_err();
        assert!(matches!(err, Error::Smb(_)), "{err:?}");
        assert!(!err.to_string().contains("hunter2"));
    }
}
