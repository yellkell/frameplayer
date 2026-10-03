//! Talking to the headset: the [`Remote`] trait the installer is written
//! against, and [`SshRemote`], which drives the system `ssh` and `scp`.
//!
//! Using the system OpenSSH client (built into Windows 10+, macOS and
//! Linux) means we reuse the key and `Host frame` alias that Frame Control
//! (or FrameDrop, or Valve's Devkit Client) set up when pairing, and need no
//! SSH library.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{InstallError, Result};

/// Result of a command run on the headset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code, `None` if killed by a signal.
    pub code: Option<i32>,
    /// Standard output (lossy UTF-8).
    pub stdout: String,
    /// Standard error (lossy UTF-8).
    pub stderr: String,
}

impl CmdOutput {
    /// True for exit code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Operations the installer needs on the headset. Implemented by
/// [`SshRemote`] and by a fake in tests.
pub trait Remote {
    /// Runs a POSIX `sh` script on the headset and returns its output,
    /// whatever its exit code. Errors only when the connection fails.
    fn run(&mut self, script: &str) -> Result<CmdOutput>;
    /// Copies a local file to an absolute path on the headset.
    fn upload(&mut self, local: &Path, remote: &str) -> Result<()>;
    /// Copies an absolute path on the headset to a local file.
    fn download(&mut self, remote: &str, local: &Path) -> Result<()>;
    /// Human-readable name of the device, for messages.
    fn host(&self) -> &str;
}

/// Runs `script` and turns a non-zero exit into [`InstallError::Remote`].
pub fn run_checked(remote: &mut dyn Remote, what: &str, script: &str) -> Result<CmdOutput> {
    let out = remote.run(script)?;
    if out.success() {
        Ok(out)
    } else {
        Err(InstallError::Remote {
            what: what.to_string(),
            code: out.code,
            detail: last_lines(&out.stderr, &out.stdout),
        })
    }
}

fn last_lines(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines: Vec<&str> = text.trim().lines().collect();
    lines[lines.len().saturating_sub(6)..].join("\n")
}

/// Quotes `s` for a POSIX shell: wrapped in single quotes, with embedded
/// single quotes written as `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Checks that a remote path is absolute and uses only characters that are
/// safe both in a shell and in an `scp` target (letters, digits, `._+-/`),
/// with no `.` or `..` components. Our paths are built from `$HOME`, the
/// install directory and fixed names, so anything else means a surprising
/// home directory or a mistyped `--dir`.
pub fn check_remote_path(path: &str) -> Result<()> {
    let ok = path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-/".contains(&b))
        && !path.split('/').any(|c| c == "." || c == "..");
    if ok {
        Ok(())
    } else {
        Err(InstallError::BadPath(format!(
            "{path:?} must be an absolute path made of letters, digits and ._+-/ only"
        )))
    }
}

/// The real thing: system `ssh` and `scp`.
#[derive(Debug, Clone)]
pub struct SshRemote {
    host: String,
    options: Vec<String>,
    ssh: String,
    scp: String,
}

/// Seconds to wait for the TCP connection before giving up.
const CONNECT_TIMEOUT_SECS: u32 = 15;

impl SshRemote {
    /// A remote for `host`: an alias from `~/.ssh/config` (Frame Control
    /// creates `frame`) or `user@address`. `options` are extra `-o`
    /// settings such as `Port=2222` or `IdentityFile=~/.ssh/frame`.
    pub fn new(host: &str, options: &[String]) -> Result<Self> {
        if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
            return Err(InstallError::BadHost(host.to_string()));
        }
        for o in options {
            if !o.contains('=') || o.starts_with('-') {
                return Err(InstallError::BadSshOption(o.clone()));
            }
        }
        Ok(SshRemote {
            host: host.to_string(),
            options: options.to_vec(),
            ssh: "ssh".into(),
            scp: "scp".into(),
        })
    }

    /// Options shared by `ssh` and `scp`: never prompt (a hidden password
    /// prompt would hang the installer), time out, then the user's extras.
    fn common_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        ];
        for o in &self.options {
            args.push("-o".into());
            args.push(o.clone());
        }
        args
    }

    /// Arguments for `ssh` running a script fed on stdin (`sh -s`). Feeding
    /// the script on stdin avoids every layer of argument quoting,
    /// including Windows'.
    pub fn ssh_args(&self) -> Vec<String> {
        let mut args = vec!["-T".to_string()];
        args.extend(self.common_args());
        args.extend(["--".to_string(), self.host.clone(), "sh -s".to_string()]);
        args
    }

    /// Arguments for `scp` copying `from` to `to`, where one side is
    /// `host:path`.
    pub fn scp_args(&self, from: &str, to: &str) -> Vec<String> {
        let mut args = vec!["-q".to_string()];
        args.extend(self.common_args());
        args.extend(["--".to_string(), from.to_string(), to.to_string()]);
        args
    }

    /// Checks that `ssh` and `scp` can be started at all.
    pub fn check_tools(&self) -> Result<()> {
        for prog in [&self.ssh, &self.scp] {
            let started = Command::new(prog)
                .arg("-V")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if started.is_err() {
                return Err(InstallError::SshMissing(prog.clone()));
            }
        }
        Ok(())
    }

    fn spawn_error(&self, prog: &str, e: std::io::Error) -> InstallError {
        if e.kind() == std::io::ErrorKind::NotFound {
            InstallError::SshMissing(prog.to_string())
        } else {
            InstallError::Connect {
                host: self.host.clone(),
                detail: format!("cannot start {prog}: {e}"),
            }
        }
    }

    /// Runs scp in `cwd`, so the local side is a bare relative file name.
    fn scp(&self, cwd: &Path, from: &str, to: &str, what: String) -> Result<()> {
        let out = Command::new(&self.scp)
            .current_dir(cwd)
            .args(self.scp_args(from, to))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| self.spawn_error(&self.scp, e))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.code() == Some(255) && looks_like_connection_error(&stderr) {
            return Err(InstallError::Connect {
                host: self.host.clone(),
                detail: stderr,
            });
        }
        Err(InstallError::Copy {
            what,
            detail: stderr,
        })
    }
}

/// `ssh` exits with 255 for its own errors; scp uses 1 or 255. This tells
/// a failed connection from a failed copy by its message.
fn looks_like_connection_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "could not resolve",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "no route to host",
        "permission denied",
        "host key verification failed",
        "network is unreachable",
        "connection closed",
        "lost connection",
    ]
    .iter()
    .any(|m| s.contains(m))
}

/// Splits a local path into its directory and a file name scp cannot
/// mistake for `host:path`. Passing `C:\Users\...` straight to scp is
/// ambiguous (is `C` a host?), so scp runs in the directory and gets only
/// the name, prefixed with `./` if it contains a colon.
pub fn split_local(local: &Path) -> Result<(std::path::PathBuf, String)> {
    let bad = || InstallError::BadPath(format!("{} is not a file path", local.display()));
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(bad)?
        .to_string();
    let dir = match local.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let name = if name.contains(':') {
        format!("./{name}")
    } else {
        name
    };
    Ok((dir, name))
}

impl Remote for SshRemote {
    fn run(&mut self, script: &str) -> Result<CmdOutput> {
        let mut child = Command::new(&self.ssh)
            .args(self.ssh_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(&self.ssh, e))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A failed write means ssh already exited; its status says why.
            let _ = stdin.write_all(script.as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let out = child
            .wait_with_output()
            .map_err(|e| InstallError::Connect {
                host: self.host.clone(),
                detail: format!("ssh did not finish: {e}"),
            })?;
        let result = CmdOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        // 255 is ssh's own failure code; our scripts never use it.
        if result.code == Some(255) {
            return Err(InstallError::Connect {
                host: self.host.clone(),
                detail: result.stderr.trim().to_string(),
            });
        }
        Ok(result)
    }

    fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
        check_remote_path(remote)?;
        let (cwd, name) = split_local(local)?;
        self.scp(
            &cwd,
            &name,
            &format!("{}:{remote}", self.host),
            format!("{} to the headset", local.display()),
        )
    }

    fn download(&mut self, remote: &str, local: &Path) -> Result<()> {
        check_remote_path(remote)?;
        let (cwd, name) = split_local(local)?;
        self.scp(
            &cwd,
            &format!("{}:{remote}", self.host),
            &name,
            format!("{remote} from the headset"),
        )
    }

    fn host(&self) -> &str {
        &self.host
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Undoes [`sh_quote`] for one fully quoted word (test helper).
    pub(crate) fn sh_unquote(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix(r"\'") {
                out.push('\'');
                rest = r;
            } else if let Some(r) = rest.strip_prefix('\'') {
                let end = r.find('\'').expect("unterminated quote");
                out.push_str(&r[..end]);
                rest = &r[end + 1..];
            } else {
                panic!("unexpected text {rest:?}");
            }
        }
        out
    }

    /// Records everything and answers scripts from a rule list.
    #[derive(Default)]
    pub(crate) struct FakeRemote {
        /// (substring of the script, canned reply); first match wins.
        pub rules: Vec<(String, CmdOutput)>,
        /// Every script run, in order.
        pub scripts: Vec<String>,
        /// Remote files: path to contents.
        pub files: HashMap<String, Vec<u8>>,
        /// Every upload (local file name, remote path), in order.
        pub uploads: Vec<(PathBuf, String)>,
    }

    impl FakeRemote {
        pub fn on(&mut self, marker: &str, stdout: &str) {
            self.rules.push((
                marker.into(),
                CmdOutput {
                    code: Some(0),
                    stdout: stdout.into(),
                    stderr: String::new(),
                },
            ));
        }
        pub fn fail(&mut self, marker: &str, code: i32, stderr: &str) {
            self.rules.push((
                marker.into(),
                CmdOutput {
                    code: Some(code),
                    stdout: String::new(),
                    stderr: stderr.into(),
                },
            ));
        }
        pub fn ran(&self, needle: &str) -> bool {
            self.scripts.iter().any(|s| s.contains(needle))
        }
    }

    impl Remote for FakeRemote {
        fn run(&mut self, script: &str) -> Result<CmdOutput> {
            self.scripts.push(script.to_string());
            Ok(self
                .rules
                .iter()
                .find(|(m, _)| script.contains(m.as_str()))
                .map(|(_, o)| o.clone())
                .unwrap_or(CmdOutput {
                    code: Some(0),
                    ..CmdOutput::default()
                }))
        }
        fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
            check_remote_path(remote)?;
            let data = std::fs::read(local).unwrap();
            self.files.insert(remote.to_string(), data);
            self.uploads.push((local.to_path_buf(), remote.to_string()));
            Ok(())
        }
        fn download(&mut self, remote: &str, local: &Path) -> Result<()> {
            check_remote_path(remote)?;
            match self.files.get(remote) {
                Some(d) => {
                    std::fs::write(local, d).unwrap();
                    Ok(())
                }
                None => Err(InstallError::Copy {
                    what: remote.into(),
                    detail: "No such file".into(),
                }),
            }
        }
        fn host(&self) -> &str {
            "fake-frame"
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("abc"), "'abc'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        for s in ["", "a b", "it's", "''", "$HOME `x` \"y\"", "\\n"] {
            assert_eq!(sh_unquote(&sh_quote(s)), s);
        }
    }

    #[test]
    fn remote_path_rules() {
        assert!(check_remote_path("/home/steam/frameplayer").is_ok());
        assert!(check_remote_path("/home/steam/devkit-game/frame_player-1.0+x").is_ok());
        for bad in [
            "frameplayer",
            "~/frameplayer",
            "/home/steam/a b",
            "/home/steam/$(rm)",
            "/home/steam/../root",
            "/home/steam/./x",
            "/home/steam/x;y",
            "/home/steam/x'y",
        ] {
            assert!(check_remote_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ssh_command_construction() {
        let r = SshRemote::new("frame", &["Port=2222".into()]).unwrap();
        assert_eq!(
            r.ssh_args(),
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "Port=2222",
                "--",
                "frame",
                "sh -s"
            ]
        );
        assert_eq!(
            r.scp_args("a.zip", "frame:/home/steam/x.zip"),
            [
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "Port=2222",
                "--",
                "a.zip",
                "frame:/home/steam/x.zip"
            ]
        );
        assert!(SshRemote::new("steam@192.168.1.20", &[]).is_ok());
        for bad in ["", "-oProxyCommand=x", "a b"] {
            assert!(
                matches!(SshRemote::new(bad, &[]), Err(InstallError::BadHost(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(
            SshRemote::new("frame", &["-v".into()]),
            Err(InstallError::BadSshOption(_))
        ));
    }

    #[test]
    fn local_paths_are_split_for_scp() {
        let (d, n) = split_local(Path::new("/tmp/x/frameplayer-1.0.0.zip")).unwrap();
        assert_eq!(d, Path::new("/tmp/x"));
        assert_eq!(n, "frameplayer-1.0.0.zip");
        let (d, n) = split_local(Path::new("odd:name.zip")).unwrap();
        assert_eq!(d, Path::new("."));
        assert_eq!(n, "./odd:name.zip");
        assert!(split_local(Path::new("/")).is_err());
    }

    #[test]
    fn connection_error_detection() {
        assert!(looks_like_connection_error(
            "ssh: Could not resolve hostname frame: Name or service not known"
        ));
        assert!(looks_like_connection_error(
            "steam@10.0.0.2: Permission denied (publickey)."
        ));
        assert!(!looks_like_connection_error(
            "scp: /x: No space left on device"
        ));
    }

    #[test]
    fn run_checked_reports_failures() {
        let mut f = FakeRemote::default();
        f.fail("boom", 3, "line1\nbad thing happened\n");
        let err = run_checked(&mut f, "testing", "echo boom").unwrap_err();
        match err {
            InstallError::Remote { what, code, detail } => {
                assert_eq!(what, "testing");
                assert_eq!(code, Some(3));
                assert!(detail.ends_with("bad thing happened"));
            }
            other => panic!("{other:?}"),
        }
        assert!(run_checked(&mut f, "ok", "true").is_ok());
    }
}
