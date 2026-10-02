//! Detached ed25519 signatures over release manifests (OUTLINE §3.6).
//!
//! * Manifest bytes are signed exactly as published; the client verifies the
//!   raw bytes *before* parsing JSON.
//! * Signature file (`stable.json.sig`): base64 of the 64-byte signature on
//!   the first non-empty line, optional trailing newline.
//! * Secret key: 64 hex chars (the 32-byte seed), e.g. the `FP_SIGNING_KEY`
//!   GitHub Actions secret. Public key: 64 hex chars.
//! * Trusted public keys are compiled in from `release-public-keys.txt` (one
//!   hex key per line, `#` comments) so a key can be rotated by shipping a
//!   release that trusts both old and new keys. Set `FP_RELEASE_PUBKEYS` at
//!   build time to override the file (comma or newline separated).

use crate::manifest::ReleaseManifest;
use crate::{Result, UpdateError};
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Trusted release keys compiled into this build.
pub const RELEASE_PUBLIC_KEYS: &str = match option_env!("FP_RELEASE_PUBKEYS") {
    Some(k) => k,
    None => include_str!("../release-public-keys.txt"),
};

/// Parse a list of hex public keys (comma/newline separated, `#` comments).
pub fn parse_public_keys(list: &str) -> Result<Vec<VerifyingKey>> {
    list.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .flat_map(|l| l.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse_public_key)
        .collect()
}

pub fn parse_public_key(hex_str: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = decode_hex32(hex_str, "public key")?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|e| UpdateError::BadSignature(format!("invalid public key: {e}")))
}

pub fn parse_secret_key(hex_str: &str) -> Result<SigningKey> {
    Ok(SigningKey::from_bytes(&decode_hex32(
        hex_str,
        "secret key",
    )?))
}

fn decode_hex32(s: &str, what: &str) -> Result<[u8; 32]> {
    let v = hex::decode(s.trim())
        .map_err(|e| UpdateError::BadSignature(format!("{what} is not hex: {e}")))?;
    v.try_into()
        .map_err(|_| UpdateError::BadSignature(format!("{what} must be 32 bytes")))
}

/// Generate a fresh signing key. Returns `(secret_hex, public_hex)`.
pub fn generate_keypair() -> (String, String) {
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    (
        hex::encode(sk.to_bytes()),
        hex::encode(sk.verifying_key().to_bytes()),
    )
}

pub fn public_key_hex(sk: &SigningKey) -> String {
    hex::encode(sk.verifying_key().to_bytes())
}

/// Sign `bytes`, returning the contents of the `.sig` file.
pub fn sign(sk: &SigningKey, bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(sk.sign(bytes).to_bytes()) + "\n"
}

fn decode_signature(sig_file: &[u8]) -> Result<Signature> {
    let text = std::str::from_utf8(sig_file)
        .map_err(|_| UpdateError::BadSignature("signature file is not UTF-8".into()))?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| UpdateError::BadSignature("empty signature file".into()))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(line)
        .map_err(|e| UpdateError::BadSignature(format!("signature is not base64: {e}")))?;
    let raw: [u8; 64] = raw
        .try_into()
        .map_err(|_| UpdateError::BadSignature("signature must be 64 bytes".into()))?;
    Ok(Signature::from_bytes(&raw))
}

/// Verifies manifests against a set of trusted keys.
#[derive(Debug, Clone)]
pub struct ManifestVerifier {
    keys: Vec<VerifyingKey>,
}

impl Default for ManifestVerifier {
    /// The keys compiled into this build.
    fn default() -> Self {
        let keys = parse_public_keys(RELEASE_PUBLIC_KEYS)
            .expect("release-public-keys.txt must contain valid hex ed25519 keys");
        assert!(!keys.is_empty(), "no release public keys compiled in");
        Self { keys }
    }
}

impl ManifestVerifier {
    /// Trust exactly these keys (tests, staging servers).
    pub fn with_keys(keys: Vec<VerifyingKey>) -> Self {
        Self { keys }
    }

    pub fn keys(&self) -> &[VerifyingKey] {
        &self.keys
    }

    /// Check a detached signature over raw bytes against any trusted key.
    pub fn verify(&self, bytes: &[u8], sig_file: &[u8]) -> Result<()> {
        let sig = decode_signature(sig_file)?;
        if self.keys.iter().any(|k| k.verify(bytes, &sig).is_ok()) {
            Ok(())
        } else {
            Err(UpdateError::BadSignature(
                "manifest not signed by a trusted release key".into(),
            ))
        }
    }

    /// Verify, then parse + validate the manifest.
    pub fn verify_and_parse(&self, bytes: &[u8], sig_file: &[u8]) -> Result<ReleaseManifest> {
        self.verify(bytes, sig_file)?;
        ReleaseManifest::parse(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_keys_parse() {
        let v = ManifestVerifier::default();
        assert!(!v.keys().is_empty());
    }

    #[test]
    fn sign_verify_roundtrip() {
        let (sk_hex, pk_hex) = generate_keypair();
        let sk = parse_secret_key(&sk_hex).unwrap();
        assert_eq!(public_key_hex(&sk), pk_hex);
        let v = ManifestVerifier::with_keys(vec![parse_public_key(&pk_hex).unwrap()]);
        let sig = sign(&sk, b"manifest bytes");
        v.verify(b"manifest bytes", sig.as_bytes()).unwrap();
        assert!(v.verify(b"manifest bytez", sig.as_bytes()).is_err());
    }

    #[test]
    fn wrong_key_rejected() {
        let (sk_hex, _) = generate_keypair();
        let (_, other_pk) = generate_keypair();
        let sk = parse_secret_key(&sk_hex).unwrap();
        let v = ManifestVerifier::with_keys(vec![parse_public_key(&other_pk).unwrap()]);
        assert!(v.verify(b"x", sign(&sk, b"x").as_bytes()).is_err());
        // The compiled-in placeholder key never verifies a random key's signature.
        assert!(ManifestVerifier::default()
            .verify(b"x", sign(&sk, b"x").as_bytes())
            .is_err());
    }

    #[test]
    fn key_rotation_list() {
        let (sk1, pk1) = generate_keypair();
        let (_, pk2) = generate_keypair();
        let list = format!("# old\n{pk2}\n{pk1} # new\n\n");
        let v = ManifestVerifier::with_keys(parse_public_keys(&list).unwrap());
        assert_eq!(v.keys().len(), 2);
        let sk = parse_secret_key(&sk1).unwrap();
        v.verify(b"m", sign(&sk, b"m").as_bytes()).unwrap();
        assert_eq!(parse_public_keys(&format!("{pk1},{pk2}")).unwrap().len(), 2);
    }

    #[test]
    fn malformed_signatures() {
        let v = ManifestVerifier::default();
        assert!(v.verify(b"x", b"").is_err());
        assert!(v.verify(b"x", b"not base64!!").is_err());
        assert!(v.verify(b"x", b"AAAA\n").is_err());
        assert!(parse_public_key("zz").is_err());
        assert!(parse_secret_key(&"ab".repeat(31)).is_err());
    }

    #[test]
    fn verify_and_parse_manifest() {
        let (sk_hex, pk_hex) = generate_keypair();
        let sk = parse_secret_key(&sk_hex).unwrap();
        let v = ManifestVerifier::with_keys(vec![parse_public_key(&pk_hex).unwrap()]);
        let json = crate::manifest::tests::sample_json();
        let sig = sign(&sk, json.as_bytes());
        let m = v.verify_and_parse(json.as_bytes(), sig.as_bytes()).unwrap();
        assert_eq!(m.version.to_string(), "0.2.0");
        let tampered = json.replace("0.2.0", "9.9.9");
        assert!(v
            .verify_and_parse(tampered.as_bytes(), sig.as_bytes())
            .is_err());
    }
}
