//! Pairing this PC with a Steam Frame in Developer Mode, so `ssh` works.
//!
//! The headset runs Valve's devkit service (the on-device half of the
//! SteamOS Devkit Client) on port 32000. Pairing is one HTTP request:
//! `POST /register` with an RSA public key line followed by a fixed token
//! ([`DEVKIT_KEY_TOKEN`]). The service only accepts it while the headset
//! shows Steam Settings > Developer > Pair new host; then the key goes into
//! the `steamos` account's `authorized_keys`.
//!
//! What was verified on a Steam Frame: ed25519 keys are refused, RSA keys
//! work; outside pairing mode the service answers 403 with a JSON error
//! asking for pairing mode; in pairing mode it answers 200 `Registered`.
//!
//! [`pair`] makes (or reuses) `~/.ssh/id_rsa_frame_devkit` with the system
//! `ssh-keygen`, registers it, and writes a `Host frame` block into
//! `~/.ssh/config`, so the rest of the installer (and plain `ssh frame`)
//! just works.

use std::ffi::OsString;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{InstallError, Result};

/// TCP port of the headset's devkit service.
pub const DEVKIT_PORT: u16 = 32000;
/// Token Valve's devkit client appends to the key line it registers.
pub const DEVKIT_KEY_TOKEN: &str = "900b919520e4cf601998a71eec318fec";
/// Account the devkit service authorizes the key for.
pub const DEVKIT_USER: &str = "steamos";
/// The ssh alias pairing writes (and the installer's default `--host`).
pub const FRAME_ALIAS: &str = "frame";
/// Key file, in `~/.ssh`. RSA: the devkit service refuses ed25519 keys.
pub const KEY_FILE_NAME: &str = "id_rsa_frame_devkit";
/// Where pairing mode is in the headset's menus.
pub const PAIRING_MODE_PLACE: &str = "Steam Settings > Developer > Pair new host";

/// What the devkit service said to `POST /register`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// The key is registered; ssh works now.
    Accepted,
    /// The headset is not showing the pairing screen; retry once it is.
    NeedsPairingMode(String),
    /// Any other refusal (for example a key it cannot use).
    Refused {
        /// HTTP status.
        status: u16,
        /// The service's message.
        message: String,
    },
}

/// The body for `POST /register`: `ssh-rsa <base64> <comment> <token>`.
/// Only RSA keys are accepted by the headset, so anything else is refused
/// here with a clear message rather than by the headset with a vague one.
pub fn register_body(public_key_line: &str) -> Result<String> {
    let mut parts = public_key_line.split_whitespace();
    let (Some(algo), Some(blob)) = (parts.next(), parts.next()) else {
        return Err(InstallError::Key(format!(
            "{public_key_line:?} is not an ssh public key"
        )));
    };
    if algo != "ssh-rsa" {
        return Err(InstallError::Key(format!(
            "the headset only pairs with RSA keys, and this key is {algo}"
        )));
    }
    let comment = parts.collect::<Vec<_>>().join(" ");
    let comment = if comment.is_empty() {
        "frameplayer-install"
    } else {
        &comment
    };
    Ok(format!("{algo} {blob} {comment} {DEVKIT_KEY_TOKEN}\n"))
}

/// Sorts the devkit service's answer. Errors come as `{"error": "..."}`.
pub fn classify_register(status: u16, body: &str) -> RegisterOutcome {
    if (200..300).contains(&status) {
        return RegisterOutcome::Accepted;
    }
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(|e| e.trim().to_string()))
        .unwrap_or_else(|| body.trim().to_string());
    let lower = message.to_ascii_lowercase();
    if lower.contains("pairing mode") || lower.contains("pair new host") {
        RegisterOutcome::NeedsPairingMode(message)
    } else {
        RegisterOutcome::Refused { status, message }
    }
}

/// Checks a headset address (IP or host name) and drops IPv6 brackets.
pub fn check_address(address: &str) -> Result<String> {
    let a = address.trim();
    let a = a
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(a);
    let ok = !a.is_empty()
        && !a.starts_with(['-', '.'])
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b));
    if ok {
        Ok(a.to_string())
    } else {
        Err(InstallError::BadHost(address.to_string()))
    }
}

/// `http://<address>:<port>/register`, with IPv6 addresses in brackets.
pub fn register_url(address: &str, port: u16) -> String {
    if address.contains(':') {
        format!("http://[{address}]:{port}/register")
    } else {
        format!("http://{address}:{port}/register")
    }
}

/// Sends `body` to the devkit service at `address:port`.
pub fn register(address: &str, port: u16, body: &str) -> Result<RegisterOutcome> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        // A LAN address: never through a proxy from the environment.
        .proxy(None)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(concat!("frameplayer-install/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let failed = |e: ureq::Error| InstallError::Pair {
        address: address.to_string(),
        detail: format!("cannot reach its devkit service on port {port}: {e}"),
    };
    let mut resp = agent
        .post(register_url(address, port))
        .header("Content-Type", "text/plain")
        .send(body)
        .map_err(failed)?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok(classify_register(status, &text))
}

// ---------------------------------------------------------------------------
// The key

/// The pairing key on this PC.
#[derive(Debug, Clone)]
pub struct DevkitKey {
    /// Private key file.
    pub private: PathBuf,
    /// `ssh-rsa AAAA... comment`.
    pub public_line: String,
    /// True if this call created it.
    pub created: bool,
}

/// `~/.ssh` as the system OpenSSH sees it (`%USERPROFILE%\.ssh` on
/// Windows, `$HOME/.ssh` elsewhere).
pub fn ssh_dir() -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".ssh"))
        .ok_or_else(|| {
            InstallError::BadPath(format!("cannot find your home folder ({var} is not set)"))
        })
}

/// Creates `~/.ssh` if needed (private to the user on Unix).
fn create_ssh_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
        .map_err(|e| InstallError::io("cannot create", dir, e))
}

/// Key comment: `frameplayer-install@<computer name>`.
pub fn key_comment() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    if host.is_empty() {
        "frameplayer-install".into()
    } else {
        format!("frameplayer-install@{host}")
    }
}

fn pub_path(private: &Path) -> PathBuf {
    let mut s: OsString = private.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

/// Runs `ssh-keygen` with `args`; returns its standard output.
fn ssh_keygen(args: &[&std::ffi::OsStr]) -> Result<String> {
    let out = Command::new("ssh-keygen")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                InstallError::SshMissing("ssh-keygen".into())
            } else {
                InstallError::Key(format!("cannot start ssh-keygen: {e}"))
            }
        })?;
    if !out.status.success() {
        return Err(InstallError::Key(format!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Uses the RSA key at `private`, creating it (RSA 4096, no passphrase)
/// with `ssh-keygen` if it does not exist, and its `.pub` file if that is
/// missing.
pub fn ensure_key(private: &Path, comment: &str) -> Result<DevkitKey> {
    let public = pub_path(private);
    let created = !private.exists();
    if created {
        if let Some(dir) = private.parent() {
            create_ssh_dir(dir)?;
        }
        ssh_keygen(&[
            "-q".as_ref(),
            "-t".as_ref(),
            "rsa".as_ref(),
            "-b".as_ref(),
            "4096".as_ref(),
            "-N".as_ref(),
            "".as_ref(),
            "-C".as_ref(),
            comment.as_ref(),
            "-f".as_ref(),
            private.as_os_str(),
        ])?;
    }
    let public_line = match std::fs::read_to_string(&public) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let line = ssh_keygen(&["-y".as_ref(), "-f".as_ref(), private.as_os_str()])?;
            std::fs::write(&public, &line)
                .map_err(|e| InstallError::io("cannot write", &public, e))?;
            line
        }
        Err(e) => return Err(InstallError::io("cannot read", &public, e)),
    };
    let public_line = public_line.lines().next().unwrap_or("").trim().to_string();
    if !public_line.starts_with("ssh-rsa ") {
        return Err(InstallError::Key(format!(
            "{} is not an RSA key, and the headset only pairs with RSA keys. Move it \
             (and its .pub file) out of the way and pair again to create a new one.",
            private.display()
        )));
    }
    Ok(DevkitKey {
        private: private.to_path_buf(),
        public_line,
        created,
    })
}

// ---------------------------------------------------------------------------
// ~/.ssh/config

/// A config line's keyword (lower-cased) and arguments, or `None` for blank
/// and comment lines. Accepts `Key value` and `Key=value`.
fn keyword(line: &str) -> Option<(String, &str)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let end = t
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(t.len());
    let rest = t[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    Some((t[..end].to_ascii_lowercase(), rest))
}

fn starts_block(line: &str) -> bool {
    matches!(keyword(line), Some((k, _)) if k == "host" || k == "match")
}

fn is_alias_line(line: &str, alias: &str) -> bool {
    matches!(keyword(line), Some((k, args)) if k == "host"
        && args.split_whitespace().count() == 1
        && args.eq_ignore_ascii_case(alias))
}

/// Line ranges of the `Host <alias>` blocks (only those naming exactly that
/// one alias). A block runs to the next `Host`/`Match` line; blank and
/// comment lines just before that line belong to the next block.
fn alias_blocks(lines: &[&str], alias: &str) -> Vec<Range<usize>> {
    let close = |start: usize, mut end: usize| {
        while end > start + 1 && keyword(lines[end - 1]).is_none() {
            end -= 1;
        }
        start..end
    };
    let mut blocks = Vec::new();
    let mut open: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if starts_block(line) {
            if let Some(start) = open.take() {
                blocks.push(close(start, i));
            }
            if is_alias_line(line, alias) {
                open = Some(i);
            }
        }
    }
    if let Some(start) = open {
        blocks.push(close(start, lines.len()));
    }
    blocks
}

/// The `Host` block pairing writes.
pub fn host_block(alias: &str, address: &str, identity_file: &str) -> Vec<String> {
    vec![
        format!("Host {alias}"),
        format!("    HostName {address}"),
        format!("    User {DEVKIT_USER}"),
        format!("    IdentityFile {identity_file}"),
        "    IdentitiesOnly yes".into(),
        // The installer runs ssh with BatchMode, which cannot answer the
        // first-connection host key question; a *changed* key still fails.
        "    StrictHostKeyChecking accept-new".into(),
    ]
}

/// Returns `config` with `block` as the only `Host <alias>` block.
///
/// An existing block is replaced where it is (and any further copies are
/// dropped). Otherwise the block goes before the first `Host`/`Match` line,
/// so a later `Host *` cannot override it and options at the top of the
/// file stay global. Everything else, including comments and Windows line
/// endings, is kept.
pub fn set_host_block(config: &str, alias: &str, block: &[String]) -> String {
    let (bom, text) = match config.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", config),
    };
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.lines().collect();
    let blocks = alias_blocks(&lines, alias);
    let block: Vec<&str> = block.iter().map(String::as_str).collect();

    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + block.len() + 2);
    if let Some(first) = blocks.first() {
        let mut i = 0;
        while i < lines.len() {
            if let Some(b) = blocks.iter().find(|b| b.start == i) {
                if b.start == first.start {
                    out.extend(&block);
                }
                i = b.end;
            } else {
                out.push(lines[i]);
                i += 1;
            }
        }
    } else {
        // Before the first block, keeping comments written just above it
        // with that block.
        let mut at = lines
            .iter()
            .position(|l| starts_block(l))
            .unwrap_or(lines.len());
        if at < lines.len() {
            while at > 0 && lines[at - 1].trim_start().starts_with('#') {
                at -= 1;
            }
        }
        out.extend(&lines[..at]);
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push("");
        }
        out.extend(&block);
        if at < lines.len() {
            if !lines[at].trim().is_empty() {
                out.push("");
            }
            out.extend(&lines[at..]);
        }
    }
    let mut s = String::from(bom);
    for line in out {
        s.push_str(line);
        s.push_str(nl);
    }
    s
}

/// The value of `key` in the `Host <alias>` block, if there is one.
pub fn host_block_value(config: &str, alias: &str, key: &str) -> Option<String> {
    let text = config.strip_prefix('\u{feff}').unwrap_or(config);
    let lines: Vec<&str> = text.lines().collect();
    let block = alias_blocks(&lines, alias).into_iter().next()?;
    lines[block].iter().find_map(|l| match keyword(l) {
        Some((k, v)) if k.eq_ignore_ascii_case(key) && !v.is_empty() => Some(v.to_string()),
        _ => None,
    })
}

/// Writes the `Host <alias>` block into the ssh config at `path`, creating
/// the file if needed. The file is replaced atomically, so a failure never
/// leaves half a config.
pub fn update_ssh_config(path: &Path, alias: &str, block: &[String]) -> Result<()> {
    let old = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(InstallError::io("cannot read", path, e)),
    };
    let new = set_host_block(&old, alias, block);
    if new == old {
        return Ok(());
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    create_ssh_dir(dir)?;
    // A symlinked config (dotfile managers) is written through, not replaced.
    let is_link = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_link {
        return std::fs::write(path, new).map_err(|e| InstallError::io("cannot write", path, e));
    }
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| InstallError::io("cannot write a file in", dir, e))?;
    tmp.write_all(new.as_bytes())
        .map_err(|e| InstallError::io("cannot write", tmp.path().to_path_buf(), e))?;
    tmp.persist(path)
        .map_err(|e| InstallError::io("cannot replace", path, e.error))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Deciding when to pair, and the whole flow

/// True when an ssh failure looks like "this PC is not paired yet": login
/// refused, the `frame` alias unknown, or no ssh server listening (the
/// headset enables it when it pairs). A timeout or unreachable network is
/// a different problem that pairing cannot fix.
pub fn pairing_might_help(err: &InstallError) -> bool {
    let InstallError::Connect { detail, .. } = err else {
        return false;
    };
    let d = detail.to_ascii_lowercase();
    [
        "permission denied",
        "too many authentication failures",
        "could not resolve hostname",
        "connection refused",
    ]
    .iter()
    .any(|m| d.contains(m))
}

/// The headset's address as far as `host` (`--host`) and the ssh config
/// tell: the part after `user@`, an address given directly, or the
/// `HostName` of an alias.
pub fn address_for_host(host: &str, ssh_config: &str) -> Option<String> {
    let target = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let target = target
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(target);
    if target.contains(['.', ':']) {
        return check_address(target).ok();
    }
    host_block_value(ssh_config, target, "hostname").and_then(|a| check_address(&a).ok())
}

/// Reads `~/.ssh/config`, or an empty string.
pub fn read_ssh_config() -> String {
    ssh_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("config")).ok())
        .unwrap_or_default()
}

/// True when both stdin and stdout are a terminal, so questions can be
/// asked.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Prints `question` and reads one line; `None` at end of input.
pub fn ask(question: &str) -> Option<String> {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Pairs this PC with the headset at `address`: makes or reuses
/// `~/.ssh/id_rsa_frame_devkit`, registers it with the headset's devkit
/// service and writes the `Host frame` block into `~/.ssh/config`.
///
/// With `interactive`, it first asks the user to open the pairing screen
/// and press Enter, and asks again while the headset says it is not in
/// pairing mode. Otherwise it tries once.
pub fn pair(address: &str, interactive: bool) -> Result<()> {
    let address = check_address(address)?;
    let dir = ssh_dir()?;
    let key = ensure_key(&dir.join(KEY_FILE_NAME), &key_comment())?;
    if key.created {
        println!("Created the pairing key {}.", key.private.display());
    } else {
        println!("Using the pairing key {}.", key.private.display());
    }
    let body = register_body(&key.public_line)?;

    let pair_error = |detail: String| InstallError::Pair {
        address: address.clone(),
        detail,
    };
    if interactive {
        println!("\nOn the headset, open {PAIRING_MODE_PLACE} and leave that screen showing.");
    }
    loop {
        if interactive && ask("Press Enter when it is showing (Ctrl+C to stop) ").is_none() {
            return Err(pair_error("cancelled".into()));
        }
        println!("Pairing with {address} ...");
        match register(&address, DEVKIT_PORT, &body)? {
            RegisterOutcome::Accepted => break,
            RegisterOutcome::NeedsPairingMode(msg) if interactive => {
                println!("The headset is not in pairing mode yet ({msg}).");
                println!("Open {PAIRING_MODE_PLACE} on the headset, then try again.");
            }
            RegisterOutcome::NeedsPairingMode(msg) => {
                return Err(pair_error(format!(
                    "the headset is not in pairing mode: {msg}"
                )));
            }
            RegisterOutcome::Refused { status, message } => {
                return Err(pair_error(format!(
                    "the headset refused the key (HTTP {status}): {message}"
                )));
            }
        }
    }

    let config = dir.join("config");
    update_ssh_config(
        &config,
        FRAME_ALIAS,
        &host_block(FRAME_ALIAS, &address, &format!("~/.ssh/{KEY_FILE_NAME}")),
    )?;
    println!(
        "Paired. {} now has `Host {FRAME_ALIAS}` ({DEVKIT_USER}@{address}), so \
         `ssh {FRAME_ALIAS}` reaches the headset.",
        config.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::net::TcpListener;

    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAACAQC7 me@pc";

    #[test]
    fn register_body_is_key_line_plus_token() {
        assert_eq!(
            register_body(&format!("{RSA}\n")).unwrap(),
            "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAACAQC7 me@pc \
             900b919520e4cf601998a71eec318fec\n"
        );
        // No comment: one is supplied, so the token stays the last word.
        assert_eq!(
            register_body("ssh-rsa AAAAB3Nza").unwrap(),
            "ssh-rsa AAAAB3Nza frameplayer-install 900b919520e4cf601998a71eec318fec\n"
        );
        // Comments with spaces survive (single-spaced).
        assert_eq!(
            register_body("ssh-rsa AAAA  my  laptop").unwrap(),
            "ssh-rsa AAAA my laptop 900b919520e4cf601998a71eec318fec\n"
        );
        let e = register_body("ssh-ed25519 AAAAC3Nza me@pc").unwrap_err();
        assert!(e.to_string().contains("only pairs with RSA"), "{e}");
        assert!(register_body("").is_err());
        assert!(register_body("ssh-rsa").is_err());
    }

    #[test]
    fn register_answers() {
        assert_eq!(
            classify_register(200, "Registered"),
            RegisterOutcome::Accepted
        );
        // What a Frame says when it is not on the pairing screen.
        let not_pairing = r#"{"error": "devkit approve-ssh-key: please put the Steam client in pairing mode: Settings -> Developer -> Pair new host\n"}"#;
        assert!(matches!(
            classify_register(403, not_pairing),
            RegisterOutcome::NeedsPairingMode(m) if m.ends_with("Pair new host")
        ));
        assert_eq!(
            classify_register(403, r#"{"error": "Failed to write the ssh key"}"#),
            RegisterOutcome::Refused {
                status: 403,
                message: "Failed to write the ssh key".into()
            }
        );
        assert_eq!(
            classify_register(500, " boom \n"),
            RegisterOutcome::Refused {
                status: 500,
                message: "boom".into()
            }
        );
    }

    #[test]
    fn addresses() {
        assert_eq!(check_address(" 192.168.0.68 ").unwrap(), "192.168.0.68");
        assert_eq!(
            check_address("steamframe.local").unwrap(),
            "steamframe.local"
        );
        assert_eq!(check_address("[fe80::1]").unwrap(), "fe80::1");
        for bad in [
            "",
            "a b",
            "-oProxyCommand",
            "x/y",
            "steamos@1.2.3.4",
            "a?b",
            "[]",
        ] {
            assert!(check_address(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            register_url("192.168.0.68", DEVKIT_PORT),
            "http://192.168.0.68:32000/register"
        );
        assert_eq!(register_url("fe80::1", 1), "http://[fe80::1]:1/register");
    }

    /// Serves one HTTP request on localhost; returns the port and a handle
    /// yielding the raw request.
    fn one_shot_server(
        status: &'static str,
        body: &'static str,
    ) -> (u16, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req);
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text[..h]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if req.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8(req).unwrap()
        });
        (port, handle)
    }

    #[test]
    fn register_posts_the_body() {
        let (port, server) = one_shot_server("200 OK", "Registered");
        let body = register_body(RSA).unwrap();
        assert_eq!(
            register("127.0.0.1", port, &body).unwrap(),
            RegisterOutcome::Accepted
        );
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /register HTTP/1.1\r\n"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: text/plain")
        );
        assert!(req.ends_with(&format!("\r\n\r\n{body}")), "{req}");

        let (port, server) = one_shot_server(
            "403 Forbidden",
            r#"{"error": "please put the Steam client in pairing mode"}"#,
        );
        assert!(matches!(
            register("127.0.0.1", port, &body).unwrap(),
            RegisterOutcome::NeedsPairingMode(_)
        ));
        server.join().unwrap();
    }

    #[test]
    fn register_reports_unreachable_service() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = register("127.0.0.1", port, "x").unwrap_err();
        assert!(matches!(err, InstallError::Pair { .. }), "{err:?}");
        assert!(err.hint().unwrap().contains(PAIRING_MODE_PLACE));
    }

    fn block(address: &str) -> Vec<String> {
        host_block(FRAME_ALIAS, address, "~/.ssh/id_rsa_frame_devkit")
    }

    const BLOCK_10: &str = "Host frame\n    HostName 10.0.0.5\n    User steamos\n    \
        IdentityFile ~/.ssh/id_rsa_frame_devkit\n    IdentitiesOnly yes\n    \
        StrictHostKeyChecking accept-new\n";

    #[test]
    fn ssh_config_new_file() {
        assert_eq!(set_host_block("", "frame", &block("10.0.0.5")), BLOCK_10);
        // Only global options: appended after them.
        assert_eq!(
            set_host_block("ServerAliveInterval 30\n", "frame", &block("10.0.0.5")),
            format!("ServerAliveInterval 30\n\n{BLOCK_10}")
        );
    }

    #[test]
    fn ssh_config_inserts_before_other_hosts() {
        let config = "Include ~/.ssh/extra\n\n# my server\nHost server\n    User me\n\n\
                      Host *\n    User nobody\n";
        let got = set_host_block(config, "frame", &block("10.0.0.5"));
        assert_eq!(
            got,
            format!(
                "Include ~/.ssh/extra\n\n{BLOCK_10}\n# my server\nHost server\n    User me\n\n\
                 Host *\n    User nobody\n"
            )
        );
        // Idempotent.
        assert_eq!(set_host_block(&got, "frame", &block("10.0.0.5")), got);
    }

    #[test]
    fn ssh_config_replaces_existing_block() {
        let config = "Host server\n    User me\n\n\
                      # Frame Control\n\
                      Host frame\n    HostName 192.168.1.9\n    User steam\n    \
                      IdentityFile ~/.ssh/id_ed25519\n\n\
                      # work\nHost=work\n    User w\n\
                      HOST frame\n    Port 2222\n\
                      Host frame other\n    User x\n";
        let got = set_host_block(config, "frame", &block("10.0.0.5"));
        assert_eq!(
            got,
            format!(
                "Host server\n    User me\n\n# Frame Control\n{BLOCK_10}\n\
                 # work\nHost=work\n    User w\n\
                 Host frame other\n    User x\n"
            )
        );
        assert_eq!(
            host_block_value(&got, "frame", "HostName").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            host_block_value(config, "frame", "hostname").as_deref(),
            Some("192.168.1.9")
        );
        assert_eq!(
            host_block_value(config, "work", "user").as_deref(),
            Some("w")
        );
        assert_eq!(host_block_value(config, "nope", "user"), None);
    }

    #[test]
    fn ssh_config_keeps_crlf_and_bom() {
        let config = "\u{feff}Host frame\r\n  HostName 1.2.3.4\r\n\r\nHost b\r\n  User u\r\n";
        let got = set_host_block(config, "frame", &block("10.0.0.5"));
        assert_eq!(
            got,
            format!(
                "\u{feff}{}\r\nHost b\r\n  User u\r\n",
                BLOCK_10.replace('\n', "\r\n")
            )
        );
    }

    #[test]
    fn ssh_config_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config");
        update_ssh_config(&path, "frame", &block("10.0.0.5")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BLOCK_10);
        std::fs::write(&path, "Host a\n  User b\n").unwrap();
        update_ssh_config(&path, "frame", &block("10.0.0.6")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("Host frame\n    HostName 10.0.0.6\n"),
            "{text}"
        );
        assert!(text.ends_with("\nHost a\n  User b\n"), "{text}");
    }

    #[test]
    fn when_to_pair() {
        let connect = |detail: &str| InstallError::Connect {
            host: "frame".into(),
            detail: detail.into(),
        };
        assert!(pairing_might_help(&connect(
            "steamos@192.168.0.68: Permission denied (publickey)."
        )));
        assert!(pairing_might_help(&connect(
            "ssh: Could not resolve hostname frame: No such host is known."
        )));
        assert!(pairing_might_help(&connect(
            "ssh: connect to host 192.168.0.68 port 22: Connection refused"
        )));
        assert!(!pairing_might_help(&connect(
            "ssh: connect to host 192.168.0.68 port 22: Connection timed out"
        )));
        assert!(!pairing_might_help(&InstallError::Requirement(
            "permission denied".into()
        )));
    }

    #[test]
    fn address_from_host_or_config() {
        let config = "Host frame\n  HostName 192.168.0.68\n";
        assert_eq!(
            address_for_host("frame", config).as_deref(),
            Some("192.168.0.68")
        );
        assert_eq!(address_for_host("frame", ""), None);
        assert_eq!(
            address_for_host("steam@10.0.0.2", config).as_deref(),
            Some("10.0.0.2")
        );
        assert_eq!(
            address_for_host("steamframe.local", "").as_deref(),
            Some("steamframe.local")
        );
        assert_eq!(
            address_for_host("steamos@[fe80::1]", "").as_deref(),
            Some("fe80::1")
        );
    }

    fn have_ssh_keygen() -> bool {
        Command::new("ssh-keygen")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[test]
    fn key_is_created_reused_and_checked() {
        if !have_ssh_keygen() {
            eprintln!("ssh-keygen not found; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join(".ssh").join(KEY_FILE_NAME);
        let a = ensure_key(&private, "test@pc").unwrap();
        assert!(a.created);
        assert!(
            a.public_line.starts_with("ssh-rsa AAAA"),
            "{}",
            a.public_line
        );
        assert!(a.public_line.ends_with(" test@pc"), "{}", a.public_line);
        let b = ensure_key(&private, "ignored").unwrap();
        assert!(!b.created);
        assert_eq!(a.public_line, b.public_line);
        // A lost .pub file is recreated from the private key.
        std::fs::remove_file(pub_path(&private)).unwrap();
        let c = ensure_key(&private, "ignored").unwrap();
        assert_eq!(
            c.public_line.split_whitespace().nth(1),
            a.public_line.split_whitespace().nth(1)
        );
        assert!(pub_path(&private).is_file());

        // An ed25519 key under that name is refused.
        let ed = dir.path().join("ed");
        ssh_keygen(&[
            "-q".as_ref(),
            "-t".as_ref(),
            "ed25519".as_ref(),
            "-N".as_ref(),
            "".as_ref(),
            "-f".as_ref(),
            ed.as_os_str(),
        ])
        .unwrap();
        let e = ensure_key(&ed, "x").unwrap_err();
        assert!(e.to_string().contains("not an RSA key"), "{e}");
    }

    #[test]
    fn comment_has_no_spaces() {
        let c = key_comment();
        assert!(c.starts_with("frameplayer-install"));
        assert!(!c.contains(char::is_whitespace));
    }
}
