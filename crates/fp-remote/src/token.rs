//! The web remote's per-install pairing token.

use crate::RemoteError;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Random bytes in a token (hex-encoded to twice as many characters).
const TOKEN_BYTES: usize = 32;

/// Generates a fresh token: 32 random bytes from the OS, lower-case hex.
pub fn generate_token() -> Result<String, RemoteError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| RemoteError::Random(e.to_string()))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    s
}

/// A token we are willing to load from disk: at least 128 bits of hex.
fn valid_token(t: &str) -> bool {
    t.len() >= 32 && t.len() <= 256 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Loads the token at `path`, or creates and persists a new one (mode 0600)
/// when the file is missing or does not hold a valid token.
pub fn load_or_create(path: &Path) -> Result<String, RemoteError> {
    let err = |source| RemoteError::TokenFile {
        path: path.to_path_buf(),
        source,
    };
    match fs::read_to_string(path) {
        Ok(s) if valid_token(s.trim()) => {
            // Tighten permissions left loose by hand edits or old versions.
            if let Ok(meta) = fs::metadata(path) {
                if meta.permissions().mode() & 0o077 != 0 {
                    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(err)?;
                }
            }
            return Ok(s.trim().to_owned());
        }
        Ok(_) => log::warn!("{}: invalid pairing token, replacing it", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(err(e)),
    }
    let token = generate_token()?;
    store(path, &token)?;
    Ok(token)
}

/// Writes `token` to `path` atomically with owner-only permissions.
pub fn store(path: &Path, token: &str) -> Result<(), RemoteError> {
    let err = |source| RemoteError::TokenFile {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(err)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(err)?;
    // The file may have pre-existed with other permissions.
    f.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(err)?;
    f.write_all(token.as_bytes()).map_err(err)?;
    f.write_all(b"\n").map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    fs::rename(&tmp, path).map_err(err)
}

/// Compares two byte strings in time that depends only on their lengths,
/// so a network attacker cannot learn a token prefix by timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    // black_box keeps the optimiser from short-circuiting the fold.
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(valid_token(&a));
        assert_ne!(a, b);
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn persists_with_owner_only_permissions() {
        let dir = std::env::temp_dir().join(format!("fp-remote-token-{}", std::process::id()));
        let path = dir.join("sub/remote-token");
        let _ = fs::remove_dir_all(&dir);
        let t1 = load_or_create(&path).unwrap();
        let t2 = load_or_create(&path).unwrap();
        assert_eq!(t1, t2);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // Loosened permissions are tightened again; garbage is replaced.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_or_create(&path).unwrap(), t1);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::write(&path, "short").unwrap();
        let t3 = load_or_create(&path).unwrap();
        assert_ne!(t3, t1);
        assert!(valid_token(&t3));
        let _ = fs::remove_dir_all(&dir);
    }
}
