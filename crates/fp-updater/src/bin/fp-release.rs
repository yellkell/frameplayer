//! `fp-release`: maintainer tool for FramePlayer releases.
//!
//! ```text
//! fp-release keygen [--out PREFIX]
//! fp-release sign MANIFEST.json --key PREFIX.key
//! fp-release verify MANIFEST.json --pub PREFIX.pub
//! fp-release manifest --version X --zip PATH --url-base URL [--arch aarch64]
//!                     [--notes FILE] [--channel stable|beta] [--published RFC3339]
//!                     [--out manifest.json]
//! fp-release framedrop --name FramePlayer --zip PATH --url URL
//!                      [--out framedrop.json] [--manifest-url URL]
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fp_updater::framedrop::{
    FramedropManifest, frame_control_install_link, framedrop_install_link_unverified,
};
use fp_updater::manifest::{MANIFEST_NAME, public_key_from_bytes};
use fp_updater::{
    Artifact, Channel, ReleaseManifest, SigningKey, Version, hash_file, inspect_zip, sign_manifest,
    verify_manifest,
};

const USAGE: &str = "\
fp-release: build, sign and check FramePlayer release metadata

USAGE:
  fp-release keygen [--out PREFIX]
      Create a signing key pair: PREFIX.key (secret, keep offline) and
      PREFIX.pub. Prints the line to paste into fp-updater's lib.rs.
      PREFIX defaults to frameplayer-release.

  fp-release sign MANIFEST.json --key PREFIX.key
      Write MANIFEST.json.sig (base64 ed25519 signature of the exact bytes).

  fp-release verify MANIFEST.json --pub PREFIX.pub
      Check MANIFEST.json.sig and the manifest contents.

  fp-release manifest --version X --zip PATH --url-base URL [--arch aarch64]
                      [--notes FILE] [--channel stable|beta]
                      [--published RFC3339] [--out manifest.json]
      Write the release manifest. The artifact URL is URL-BASE/<zip file name>.
      The zip is checked (safe paths, binary present, VERSION equals X).

  fp-release framedrop --name NAME --zip PATH --url URL
                       [--out framedrop.json] [--manifest-url URL]
      Write a framedrop.install/v1 manifest for Frame Control / FrameDrop,
      validated against the installers' rules. With --manifest-url, also
      print the one-click install links.
";

type CliResult<T = ()> = Result<T, String>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> CliResult {
    let Some((cmd, rest)) = args.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    match cmd.as_str() {
        "keygen" => keygen(&Args::parse(rest, &["out"])?),
        "sign" => sign(&Args::parse(rest, &["key"])?),
        "verify" => verify(&Args::parse(rest, &["pub"])?),
        "manifest" => manifest(&Args::parse(
            rest,
            &[
                "version",
                "zip",
                "url-base",
                "arch",
                "notes",
                "channel",
                "published",
                "out",
            ],
        )?),
        "framedrop" => framedrop(&Args::parse(
            rest,
            &["name", "zip", "url", "out", "manifest-url"],
        )?),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

/// `--flag value` options plus positional arguments.
struct Args {
    opts: HashMap<String, String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(args: &[String], allowed: &[&str]) -> CliResult<Args> {
        let mut opts = HashMap::new();
        let mut positional = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let (name, value) = match name.split_once('=') {
                    Some((n, v)) => (n.to_string(), v.to_string()),
                    None => {
                        let v = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
                        (name.to_string(), v.clone())
                    }
                };
                if !allowed.contains(&name.as_str()) {
                    return Err(format!("unknown option --{name}"));
                }
                if opts.insert(name.clone(), value).is_some() {
                    return Err(format!("--{name} given twice"));
                }
            } else {
                positional.push(a.clone());
            }
        }
        Ok(Args { opts, positional })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.opts.get(name).map(String::as_str)
    }

    fn need(&self, name: &str) -> CliResult<&str> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn one_positional(&self, what: &str) -> CliResult<&str> {
        match self.positional.as_slice() {
            [p] => Ok(p),
            [] => Err(format!("missing {what}")),
            _ => Err(format!("expected one {what}, got {:?}", self.positional)),
        }
    }
}

fn read_key_file(path: &Path) -> CliResult<[u8; 32]> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let b64: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let bytes = BASE64
        .decode(b64)
        .map_err(|e| format!("{} is not a base64 key: {e}", path.display()))?;
    bytes.as_slice().try_into().map_err(|_| {
        format!(
            "{} holds {} bytes, expected 32",
            path.display(),
            bytes.len()
        )
    })
}

fn write_new(path: &Path, contents: &str, secret: bool) -> CliResult {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if secret {
            opts.mode(0o600);
        }
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut f = opts.open(path).map_err(|e| {
        format!(
            "cannot create {} (refusing to overwrite): {e}",
            path.display()
        )
    })?;
    f.write_all(contents.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn write_file(path: &Path, contents: &[u8]) -> CliResult {
    fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Formats the Rust line holding the public key, for pasting into lib.rs.
fn rust_key_literal(public: &[u8; 32]) -> String {
    let bytes: Vec<String> = public.iter().map(|b| format!("0x{b:02x}")).collect();
    let mut lines = String::new();
    for chunk in bytes.chunks(8) {
        lines.push_str("    ");
        lines.push_str(&chunk.join(", "));
        lines.push_str(",\n");
    }
    format!("pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = Some([\n{lines}]);")
}

fn keygen(args: &Args) -> CliResult {
    let prefix = args.get("out").unwrap_or("frameplayer-release");
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("no system randomness: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    let public = key.verifying_key().to_bytes();
    let secret_path = PathBuf::from(format!("{prefix}.key"));
    let public_path = PathBuf::from(format!("{prefix}.pub"));
    write_new(
        &secret_path,
        &format!(
            "# FramePlayer release signing key (ed25519 seed, base64). KEEP SECRET.\n{}\n",
            BASE64.encode(seed)
        ),
        true,
    )?;
    write_new(
        &public_path,
        &format!(
            "# FramePlayer release public key (ed25519, base64)\n{}\n",
            BASE64.encode(public)
        ),
        false,
    )?;
    println!(
        "Wrote {} (secret) and {}.",
        secret_path.display(),
        public_path.display()
    );
    println!("Paste this into crates/fp-updater/src/lib.rs, replacing the existing constant:\n");
    println!("{}", rust_key_literal(&public));
    Ok(())
}

fn sign(args: &Args) -> CliResult {
    let manifest_path = PathBuf::from(args.one_positional("manifest file")?);
    let seed = read_key_file(Path::new(args.need("key")?))?;
    let key = SigningKey::from_bytes(&seed);
    let bytes = fs::read(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: ReleaseManifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a release manifest: {e}", manifest_path.display()))?;
    manifest.validate().map_err(|e| e.to_string())?;
    let sig = sign_manifest(&bytes, &key);
    // Self-check before writing.
    verify_manifest(&bytes, &sig, &key.verifying_key()).map_err(|e| e.to_string())?;
    let sig_path = PathBuf::from(format!("{}.sig", manifest_path.display()));
    write_file(&sig_path, sig.as_bytes())?;
    println!(
        "Signed {} {} -> {}",
        manifest.name,
        manifest.version,
        sig_path.display()
    );
    Ok(())
}

fn verify(args: &Args) -> CliResult {
    let manifest_path = PathBuf::from(args.one_positional("manifest file")?);
    let public = read_key_file(Path::new(args.need("pub")?))?;
    let key = public_key_from_bytes(&public).map_err(|e| e.to_string())?;
    let bytes = fs::read(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let sig_path = PathBuf::from(format!("{}.sig", manifest_path.display()));
    let sig = fs::read_to_string(&sig_path)
        .map_err(|e| format!("cannot read {}: {e}", sig_path.display()))?;
    let m = verify_manifest(&bytes, &sig, &key).map_err(|e| e.to_string())?;
    println!(
        "OK: {} {} ({}), {} artifact(s)",
        m.name,
        m.version,
        m.channel,
        m.artifacts.len()
    );
    Ok(())
}

fn zip_file_name(zip: &Path) -> CliResult<String> {
    zip.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no usable file name", zip.display()))
}

fn manifest(args: &Args) -> CliResult {
    let version = Version::parse(args.need("version")?)
        .map_err(|e| format!("--version is not semver: {e}"))?;
    let zip = PathBuf::from(args.need("zip")?);
    let url_base = args.need("url-base")?.trim_end_matches('/');
    let arch = args.get("arch").unwrap_or("aarch64");
    let channel: Channel = match args.get("channel") {
        Some(c) => c.parse()?,
        None if version.pre.is_empty() => Channel::Stable,
        None => Channel::Beta,
    };
    let notes = match args.get("notes") {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("cannot read notes {p}: {e}"))?,
        None => String::new(),
    };
    let published = match args.get("published") {
        Some(p) => p.to_string(),
        None => rfc3339_utc(SystemTime::now()),
    };

    let info = inspect_zip(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    if info.version != version {
        return Err(format!(
            "{} contains VERSION {}, but --version is {version}",
            zip.display(),
            info.version
        ));
    }
    if !info.has_launcher {
        eprintln!("warning: the zip has no frameplayer.sh launcher");
    }
    let (size, sha256) = hash_file(&zip).map_err(|e| e.to_string())?;
    let m = ReleaseManifest {
        name: MANIFEST_NAME.into(),
        version,
        channel,
        published,
        notes,
        artifacts: vec![Artifact {
            arch: arch.to_string(),
            url: format!("{url_base}/{}", zip_file_name(&zip)?),
            sha256,
            size,
        }],
    };
    m.validate().map_err(|e| e.to_string())?;
    let mut json = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
    json.push('\n');
    let out = PathBuf::from(args.get("out").unwrap_or("manifest.json"));
    write_file(&out, json.as_bytes())?;
    println!(
        "Wrote {} for {} {} ({}). Sign it next: fp-release sign {} --key <file>.key",
        out.display(),
        m.name,
        m.version,
        m.channel,
        out.display()
    );
    Ok(())
}

fn framedrop(args: &Args) -> CliResult {
    let name = args.need("name")?;
    let zip = PathBuf::from(args.need("zip")?);
    let url = args.need("url")?;
    let info = inspect_zip(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    if !info.has_launcher {
        return Err(format!(
            "{} has no frameplayer.sh, which the installers launch",
            zip.display()
        ));
    }
    let root = info.root.unwrap_or_default();
    let exe = if root.is_empty() {
        "frameplayer.sh".to_string()
    } else {
        format!("{root}/frameplayer.sh")
    };
    let (size, sha256) = hash_file(&zip).map_err(|e| e.to_string())?;
    let m = FramedropManifest::for_zip(name, url, &sha256, size, &exe);
    let json = m
        .to_json()
        .map_err(|e| format!("manifest would be rejected: {e}"))?;
    let out = PathBuf::from(args.get("out").unwrap_or("framedrop.json"));
    write_file(&out, json.as_bytes())?;
    println!(
        "Wrote {} ({name} {}, exe {exe})",
        out.display(),
        info.version
    );
    if let Some(murl) = args.get("manifest-url") {
        println!("Frame Control link: {}", frame_control_install_link(murl));
        println!(
            "FrameDrop link (UNVERIFIED scheme, check before publishing): {}",
            framedrop_install_link_unverified(murl)
        );
    }
    Ok(())
}

/// `SystemTime` as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn dates() {
        assert_eq!(rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_709_210_096); // leap day 2024
        assert_eq!(rfc3339_utc(t), "2024-02-29T12:34:56Z");
        assert!(fp_updater::manifest::looks_like_rfc3339(&rfc3339_utc(
            SystemTime::now()
        )));
    }

    #[test]
    fn key_literal_is_valid_rust_shape() {
        let lit = rust_key_literal(&[0xab; 32]);
        assert!(lit.starts_with("pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = Some(["));
        assert_eq!(lit.matches("0xab").count(), 32);
    }

    #[test]
    fn arg_parsing() {
        let a: Vec<String> = ["m.json", "--key", "k", "--out=x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = Args::parse(&a, &["key", "out"]).unwrap();
        assert_eq!(p.get("key"), Some("k"));
        assert_eq!(p.get("out"), Some("x"));
        assert_eq!(p.one_positional("f").unwrap(), "m.json");
        assert!(Args::parse(&a, &["key"]).is_err());
        assert!(Args::parse(&["--key".to_string()], &["key"]).is_err());
    }

    #[test]
    fn keygen_sign_verify_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("k").display().to_string();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        run(&s(&["keygen", "--out", &prefix])).unwrap();
        // Refuses to overwrite.
        assert!(run(&s(&["keygen", "--out", &prefix])).is_err());

        let zip = dir.path().join("frameplayer-1.2.3-aarch64.zip");
        {
            use zip::write::SimpleFileOptions;
            let mut w = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
            let exec = SimpleFileOptions::default().unix_permissions(0o755);
            w.start_file("frameplayer/frameplayer", exec).unwrap();
            w.write_all(b"bin").unwrap();
            w.start_file("frameplayer/frameplayer.sh", exec).unwrap();
            w.write_all(b"#!/bin/sh").unwrap();
            w.start_file("frameplayer/VERSION", SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"1.2.3\n").unwrap();
            w.finish().unwrap();
        }
        let zip_s = zip.display().to_string();
        let out = dir.path().join("manifest.json").display().to_string();
        // Version must match the zip.
        assert!(
            run(&s(&[
                "manifest",
                "--version",
                "1.2.4",
                "--zip",
                &zip_s,
                "--url-base",
                "https://example.com/r",
                "--out",
                &out,
            ]))
            .is_err()
        );
        run(&s(&[
            "manifest",
            "--version",
            "1.2.3",
            "--zip",
            &zip_s,
            "--url-base",
            "https://example.com/r/",
            "--out",
            &out,
        ]))
        .unwrap();
        let m: ReleaseManifest = serde_json::from_slice(&fs::read(&out).unwrap()).unwrap();
        assert_eq!(
            m.artifacts[0].url,
            "https://example.com/r/frameplayer-1.2.3-aarch64.zip"
        );
        assert_eq!(m.channel, Channel::Stable);

        run(&s(&["sign", &out, "--key", &format!("{prefix}.key")])).unwrap();
        run(&s(&["verify", &out, "--pub", &format!("{prefix}.pub")])).unwrap();
        // Tamper and verification fails.
        let tampered = fs::read_to_string(&out).unwrap().replace("1.2.3", "1.2.9");
        fs::write(&out, tampered).unwrap();
        assert!(run(&s(&["verify", &out, "--pub", &format!("{prefix}.pub")])).is_err());

        let fd = dir.path().join("framedrop.json").display().to_string();
        run(&s(&[
            "framedrop",
            "--name",
            "FramePlayer",
            "--zip",
            &zip_s,
            "--url",
            "https://example.com/r/frameplayer-1.2.3-aarch64.zip",
            "--out",
            &fd,
        ]))
        .unwrap();
        let fdm = fp_updater::framedrop::parse_framedrop_manifest(&fs::read(&fd).unwrap()).unwrap();
        assert_eq!(
            fdm.files[0].exe.as_deref(),
            Some("frameplayer/frameplayer.sh")
        );
        // Local URLs are refused before writing.
        assert!(
            run(&s(&[
                "framedrop",
                "--name",
                "FramePlayer",
                "--zip",
                &zip_s,
                "--url",
                "https://192.168.1.5/f.zip",
                "--out",
                &fd,
            ]))
            .is_err()
        );
    }
}
