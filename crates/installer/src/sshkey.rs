//! Pure-Rust ed25519 SSH key management (no `ssh-keygen` needed).

use anyhow::{Context, Result};
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey};
use std::path::Path;

/// A loaded or freshly generated key pair.
#[derive(Debug, Clone)]
pub struct KeyPair {
    /// `ssh-ed25519 AAAA… comment` line for `authorized_keys` / `/register`.
    pub public_openssh: String,
    /// `SHA256:…` fingerprint, shown to the user so they can match it.
    pub fingerprint: String,
    /// True if the key was created by this call.
    pub created: bool,
}

/// Generate a new key in memory.
pub fn generate(comment: &str) -> Result<PrivateKey> {
    let mut key = PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)
        .context("generating ed25519 key")?;
    key.set_comment(comment);
    Ok(key)
}

/// Load `private_path` (OpenSSH format), creating it and `public_path` if
/// missing. The private key is written with mode 0600 on Unix.
pub fn load_or_create(private_path: &Path, public_path: &Path, comment: &str) -> Result<KeyPair> {
    let (key, created) = if private_path.exists() {
        let k = PrivateKey::read_openssh_file(private_path)
            .with_context(|| format!("reading SSH key {}", private_path.display()))?;
        (k, false)
    } else {
        if let Some(dir) = private_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let k = generate(comment)?;
        write_private(&k, private_path)?;
        (k, true)
    };
    if key.algorithm() != Algorithm::Ed25519 {
        anyhow::bail!("{} is not an ed25519 key", private_path.display());
    }
    let public_openssh = key.public_key().to_openssh()?;
    if created || !public_path.exists() {
        std::fs::write(public_path, format!("{public_openssh}\n"))?;
    }
    Ok(KeyPair {
        public_openssh,
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        created,
    })
}

fn write_private(k: &PrivateKey, path: &Path) -> Result<()> {
    let pem = k.to_openssh(LineEnding::LF)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(pem.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, pem.as_bytes())?;
        restrict_to_owner(path);
    }
    Ok(())
}

/// Windows OpenSSH refuses private keys that other accounts can read
/// ("UNPROTECTED PRIVATE KEY FILE"). Drop inherited ACEs and grant only the
/// current user. Best effort: only the system-OpenSSH transport cares, and
/// the built-in transport (the Windows default) never checks ACLs.
// [verify] `icacls <key> /inheritance:r /grant:r <USERNAME>:F` satisfies
// Win32-OpenSSH on a stock Windows 11 install (and with Microsoft accounts,
// where %USERNAME% is the short local name).
#[cfg(windows)]
fn restrict_to_owner(path: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(user) = std::env::var("USERNAME") else {
        return;
    };
    let r = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    if !r.is_ok_and(|s| s.success()) {
        tracing::warn!("could not restrict permissions on {}", path.display());
    }
}

#[cfg(not(any(unix, windows)))]
fn restrict_to_owner(_: &Path) {}

/// Default key comment: `frameplayer-install@<hostname>`.
pub fn default_comment() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "desktop".into());
    format!("frameplayer-install@{host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_then_reloads_same_key() {
        let d = tempfile::tempdir().unwrap();
        let (sk, pk) = (d.path().join("k"), d.path().join("k.pub"));
        let a = load_or_create(&sk, &pk, "test@host").unwrap();
        assert!(a.created);
        assert!(a.public_openssh.starts_with("ssh-ed25519 AAAA"));
        assert!(a.public_openssh.ends_with("test@host"));
        assert!(a.fingerprint.starts_with("SHA256:"));
        let b = load_or_create(&sk, &pk, "ignored").unwrap();
        assert!(!b.created);
        assert_eq!(a.public_openssh, b.public_openssh);
        assert_eq!(
            std::fs::read_to_string(&pk).unwrap().trim(),
            a.public_openssh
        );
        let pem = std::fs::read_to_string(&sk).unwrap();
        assert!(pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&sk).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn keys_are_unique() {
        let a = generate("a").unwrap();
        let b = generate("b").unwrap();
        assert_ne!(
            a.public_key().to_openssh().unwrap(),
            b.public_key().to_openssh().unwrap()
        );
    }
}
