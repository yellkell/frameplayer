//! Encrypted per-source credential storage (OUTLINE §3.6).
//!
//! Secrets are sealed with XChaCha20-Poly1305 under a random 256-bit key
//! kept in `$XDG_DATA_HOME/frameplayer/key` (mode 0600). Each entry uses a
//! fresh random 192-bit nonce and binds its source id as associated data,
//! so entries can't be swapped between sources.
//!
//! **Trade-off:** SteamOS gaming mode has no secret service / keyring, so
//! the key lives next to the data in the user's home directory. This
//! protects credentials in exported library zips, backups that skip the
//! key file, casual browsing of config files, and accidental sharing of
//! `credentials.json` — but *not* against anything that can read the
//! user's files (malware running as the user, or someone with the
//! unlocked headset / its storage). The key file is never included in
//! library exports, so restoring a backup on another device requires
//! re-entering passwords.

use crate::error::{Result, SourceError};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// Secrets for one source. Debug output redacts secrets; call
/// [`Zeroize::zeroize`] once a copy is no longer needed.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize, Zeroize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    /// SMB/NTLM domain or workgroup.
    #[serde(default)]
    pub domain: Option<String>,
    /// OpenSSH private key (PEM/OpenSSH format) for SFTP key auth.
    #[serde(default)]
    pub private_key: Option<String>,
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Self {
        Credentials {
            username: username.into(),
            password: password.into(),
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    ct: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    entries: BTreeMap<String, Sealed>,
}

/// `$XDG_DATA_HOME/frameplayer` (default `~/.local/share/frameplayer`).
pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("frameplayer")
}

/// Encrypted credential store.
pub struct CredentialStore {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
    file: StoreFile,
}

pub const KEY_FILE: &str = "key";
pub const STORE_FILE: &str = "credentials.json";

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    // Write to a temp file then rename so a crash never leaves a torn file.
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn load_or_create_key(path: &Path) -> Result<Zeroizing<[u8; 32]>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let md = std::fs::metadata(path)?;
            if md.permissions().mode() & 0o077 != 0 {
                tracing::warn!(
                    "{} had loose permissions; resetting to 0600",
                    path.display()
                );
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            let bytes = Zeroizing::new(bytes);
            let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                SourceError::Crypto(format!("{} is not a 32-byte key", path.display()))
            })?;
            Ok(Zeroizing::new(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = Zeroizing::new(rand::random::<[u8; 32]>());
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // create_new: never clobber a key another process just wrote.
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(mut f) => {
                    f.write_all(key.as_slice())?;
                    f.sync_all()?;
                    Ok(key)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create_key(path),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

impl CredentialStore {
    /// Open the store in [`data_dir`].
    pub fn open_default() -> Result<Self> {
        Self::open(&data_dir())
    }

    /// Open (creating the key if needed) the store in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_with_key_file(&dir.join(STORE_FILE), &dir.join(KEY_FILE))
    }

    pub fn open_with_key_file(store: &Path, key_file: &Path) -> Result<Self> {
        let key = load_or_create_key(key_file)?;
        let cipher = XChaCha20Poly1305::new(key.as_slice().into());
        let file = match std::fs::read(store) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreFile {
                version: 1,
                entries: BTreeMap::new(),
            },
            Err(e) => return Err(e.into()),
        };
        Ok(CredentialStore {
            path: store.to_path_buf(),
            cipher,
            file,
        })
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.file.entries.keys().map(String::as_str)
    }

    /// Decrypt the credentials for `source_id`.
    pub fn get(&self, source_id: &str) -> Result<Option<Credentials>> {
        let Some(sealed) = self.file.entries.get(source_id) else {
            return Ok(None);
        };
        let nonce = B64
            .decode(&sealed.nonce)
            .map_err(|e| SourceError::Crypto(e.to_string()))?;
        let ct = B64
            .decode(&sealed.ct)
            .map_err(|e| SourceError::Crypto(e.to_string()))?;
        if nonce.len() != 24 {
            return Err(SourceError::Crypto("bad nonce length".into()));
        }
        let pt = Zeroizing::new(
            self.cipher
                .decrypt(XNonce::from_slice(&nonce), Payload { msg: &ct, aad: source_id.as_bytes() })
                .map_err(|_| SourceError::Crypto(format!("cannot decrypt credentials for {source_id} (key changed or data corrupted)")))?,
        );
        Ok(Some(serde_json::from_slice(&pt)?))
    }

    /// Encrypt and persist credentials for `source_id`.
    pub fn set(&mut self, source_id: &str, creds: &Credentials) -> Result<()> {
        let pt = Zeroizing::new(serde_json::to_vec(creds)?);
        let nonce: [u8; 24] = rand::random();
        let ct = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &pt,
                    aad: source_id.as_bytes(),
                },
            )
            .map_err(|_| SourceError::Crypto("encryption failed".into()))?;
        self.file.entries.insert(
            source_id.to_string(),
            Sealed {
                nonce: B64.encode(nonce),
                ct: B64.encode(ct),
            },
        );
        self.save()
    }

    pub fn remove(&mut self, source_id: &str) -> Result<bool> {
        let had = self.file.entries.remove(source_id).is_some();
        if had {
            self.save()?;
        }
        Ok(had)
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(&self.path, &serde_json::to_vec_pretty(&self.file)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = CredentialStore::open(dir.path()).unwrap();
        let mut c = Credentials::new("deck", "hunter2");
        c.domain = Some("WORKGROUP".into());
        s.set("nas", &c).unwrap();
        assert_eq!(s.get("nas").unwrap(), Some(c.clone()));
        assert_eq!(s.get("other").unwrap(), None);

        let key_mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600);
        let store_mode = std::fs::metadata(dir.path().join(STORE_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(store_mode, 0o600);
        let raw = std::fs::read_to_string(dir.path().join(STORE_FILE)).unwrap();
        assert!(!raw.contains("hunter2") && !raw.contains("deck"));

        // Reopen with the same key.
        let s2 = CredentialStore::open(dir.path()).unwrap();
        assert_eq!(s2.get("nas").unwrap(), Some(c));
        assert_eq!(s2.ids().collect::<Vec<_>>(), ["nas"]);
    }

    #[test]
    fn tamper_and_swap_detected() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = CredentialStore::open(dir.path()).unwrap();
        s.set("a", &Credentials::new("u", "p")).unwrap();
        // Moving an entry to another id fails authentication (AAD binding).
        let sealed = s.file.entries.get("a").unwrap().clone();
        s.file.entries.insert("b".into(), sealed);
        assert!(matches!(s.get("b"), Err(SourceError::Crypto(_))));
        // A different key can't decrypt.
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let s3 = CredentialStore::open(dir.path()).unwrap();
        assert!(s3.get("a").is_err());
    }

    #[test]
    fn remove_and_redaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = CredentialStore::open(dir.path()).unwrap();
        s.set("x", &Credentials::new("u", "secret")).unwrap();
        assert!(s.remove("x").unwrap());
        assert!(!s.remove("x").unwrap());
        assert!(!format!("{:?}", Credentials::new("u", "secret")).contains("secret"));
    }

    #[test]
    fn loose_key_permissions_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join(KEY_FILE);
        std::fs::write(&key, [7u8; 32]).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        CredentialStore::open(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&key, [7u8; 5]).unwrap();
        assert!(CredentialStore::open(dir.path()).is_err());
    }
}
