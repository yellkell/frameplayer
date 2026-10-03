//! SSH/SCP via the system OpenSSH client (shipped with macOS, Windows 10+
//! and every Linux desktop), so the installer has no native dependencies.
//!
//! Host keys are trust-on-first-use (`StrictHostKeyChecking=accept-new`) in
//! our own `known_hosts`, never the user's, so a re-flashed headset only
//! affects this tool. Remote scripts are sent on stdin to `sh -s`, which
//! avoids quoting differences between Windows and Unix local shells.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// Everything needed to reach the headset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    pub ssh_bin: PathBuf,
    pub scp_bin: PathBuf,
}

/// POSIX single-quote a string for the remote shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Locate an executable on PATH (`.exe` appended on Windows).
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
}

/// Find the OpenSSH client: `ssh`/`scp` on PATH, else (Windows) the
/// optional-feature install in `%SystemRoot%\System32\OpenSSH`, which is
/// sometimes missing from PATH.
pub fn locate_openssh() -> Option<(PathBuf, PathBuf)> {
    if let (Some(s), Some(c)) = (find_binary("ssh"), find_binary("scp")) {
        return Some((s, c));
    }
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let dir = PathBuf::from(root).join("System32").join("OpenSSH");
        let (s, c) = (dir.join("ssh.exe"), dir.join("scp.exe"));
        if s.is_file() && c.is_file() {
            return Some((s, c));
        }
    }
    None
}

/// How to get OpenSSH when it is missing (shown to the user).
pub const OPENSSH_MISSING_HELP: &str = "the OpenSSH client (ssh, scp) was not found.\n\
     Windows: Settings > System > Optional features > View features / Add a feature > \"OpenSSH Client\" \
     (or just use the built-in transport: --ssh native).\n\
     Linux: install the openssh-client package.";

/// `-o Key=Value` with the value double-quoted when it contains whitespace
/// (OpenSSH splits e.g. `UserKnownHostsFile` values on spaces, which breaks
/// Windows profiles like `C:\Users\Jane Doe`).
fn opt_path(key: &str, p: &Path) -> String {
    let v = p.display().to_string();
    if v.chars().any(char::is_whitespace) {
        format!("{key}=\"{v}\"")
    } else {
        format!("{key}={v}")
    }
}

impl SshTarget {
    pub fn new(host: &str, user: &str, port: u16, key: &Path, known_hosts: &Path) -> Self {
        let (ssh_bin, scp_bin) = match locate_openssh() {
            Some((s, c)) if cfg!(windows) => (s, c),
            _ => (PathBuf::from("ssh"), PathBuf::from("scp")),
        };
        Self {
            host: host.to_string(),
            user: user.to_string(),
            port,
            key: key.to_path_buf(),
            known_hosts: known_hosts.to_path_buf(),
            ssh_bin,
            scp_bin,
        }
    }

    /// Fail early with a helpful message if OpenSSH is missing.
    pub fn ensure_client_available() -> Result<()> {
        if locate_openssh().is_none() {
            bail!(OPENSSH_MISSING_HELP);
        }
        Ok(())
    }

    fn common_opts(&self) -> Vec<String> {
        let mut v = vec!["-i".into(), self.key.display().to_string()];
        for o in [
            "BatchMode=yes".to_string(),
            "IdentitiesOnly=yes".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "HashKnownHosts=no".into(),
            opt_path("UserKnownHostsFile", &self.known_hosts),
            "ConnectTimeout=10".into(),
            "ServerAliveInterval=15".into(),
            "LogLevel=ERROR".into(),
        ] {
            v.push("-o".into());
            v.push(o);
        }
        v
    }

    /// `user@host` for ssh.
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// `user@host:` prefix for scp (IPv6 literals bracketed).
    fn scp_destination(&self, remote_path: &str) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}@{}:{}", self.user, host, remote_path)
    }

    /// `ssh … user@host [remote]`.
    pub fn ssh_command(&self, remote: Option<&str>) -> Command {
        self.ssh_command_with(&[], remote)
    }

    /// Like [`Self::ssh_command`] with extra ssh flags (e.g. `-t`).
    pub fn ssh_command_with(&self, extra: &[&str], remote: Option<&str>) -> Command {
        let mut c = Command::new(&self.ssh_bin);
        c.args(extra)
            .arg("-p")
            .arg(self.port.to_string())
            .args(self.common_opts())
            .arg(self.destination());
        if let Some(r) = remote {
            c.arg("--").arg(r);
        }
        c
    }

    /// `scp … local user@host:remote` (remote path relative to $HOME).
    pub fn scp_command(&self, local: &Path, remote_path: &str) -> Command {
        let mut c = Command::new(&self.scp_bin);
        c.arg("-P")
            .arg(self.port.to_string())
            .args(self.common_opts())
            .arg("-q");
        c.arg(local).arg(self.scp_destination(remote_path));
        c
    }

    /// `scp … user@host:remote local` (remote path relative to $HOME).
    pub fn scp_download_command(&self, remote_path: &str, local: &Path) -> Command {
        let mut c = Command::new(&self.scp_bin);
        c.arg("-P")
            .arg(self.port.to_string())
            .args(self.common_opts())
            .arg("-q");
        c.arg(self.scp_destination(remote_path)).arg(local);
        c
    }

    /// `ssh -N -L 127.0.0.1:<local>:<rhost>:<rport> …`.
    pub fn tunnel_command(&self, local_port: u16, remote_host: &str, remote_port: u16) -> Command {
        let mut c = Command::new(&self.ssh_bin);
        c.arg("-N")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!(
                "127.0.0.1:{local_port}:{remote_host}:{remote_port}"
            ))
            .arg("-p")
            .arg(self.port.to_string())
            .args(self.common_opts())
            .arg(self.destination());
        c
    }

    /// An OpenSSH `config` block so shell tools can use `ssh -F <file> <alias>`
    /// (tools/frame.sh does this).
    pub fn to_ssh_config(&self, alias: &str) -> String {
        format!(
            "Host {alias}\n  HostName {}\n  User {}\n  Port {}\n  IdentityFile \"{}\"\n  IdentitiesOnly yes\n  \
             UserKnownHostsFile \"{}\"\n  StrictHostKeyChecking accept-new\n  ServerAliveInterval 15\n  LogLevel ERROR\n",
            self.host,
            self.user,
            self.port,
            self.key.display(),
            self.known_hosts.display()
        )
    }

    /// Can we log in non-interactively?
    pub fn check(&self) -> bool {
        self.ssh_command(Some("true"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn spawn_script(&self, script: &str, stdout: Stdio) -> Result<Child> {
        let mut child = self
            .ssh_command(Some("sh -s"))
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(script.as_bytes())?;
        Ok(child)
    }

    /// Run a script remotely, capturing output. Non-zero exit is an error
    /// carrying the remote stderr.
    pub fn run_script(&self, script: &str) -> Result<Output> {
        let out = self
            .spawn_script(script, Stdio::piped())?
            .wait_with_output()?;
        if !out.status.success() {
            bail!(
                "remote command failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }

    /// Run a script remotely with stdout streamed to ours (logs -f).
    pub fn run_script_streaming(&self, script: &str) -> Result<ExitStatus> {
        let mut c = self.spawn_script(script, Stdio::inherit())?;
        Ok(c.wait()?)
    }

    /// Copy a local file to `$HOME/<remote_path>` by streaming it into
    /// `cat` over ssh. (Not scp: OpenSSH 9+ scp needs the SFTP subsystem,
    /// and this keeps both transports identical.)
    pub fn upload(&self, local: &Path, remote_path: &str) -> Result<()> {
        let file = std::fs::File::open(local).with_context(|| local.display().to_string())?;
        let cmd = format!("cat > \"$HOME/\"{}", shell_quote(remote_path));
        let out = self
            .ssh_command(Some(&cmd))
            .stdin(Stdio::from(file))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running ssh")?;
        if !out.status.success() {
            bail!(
                "upload failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// Copy `$HOME/<remote_path>` to a local file (via `cat` over ssh).
    pub fn download(&self, remote_path: &str, local: &Path) -> Result<()> {
        let tmp = local.with_extension("part");
        let file = std::fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
        let cmd = format!("cat \"$HOME/\"{}", shell_quote(remote_path));
        let out = self
            .ssh_command(Some(&cmd))
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::piped())
            .output()
            .context("running ssh")?;
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "download of {remote_path} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        std::fs::rename(&tmp, local)?;
        Ok(())
    }

    /// Open a local port forward and wait until it accepts connections.
    pub fn open_tunnel(&self, remote_host: &str, remote_port: u16) -> Result<Tunnel> {
        let local_port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let child = self
            .tunnel_command(local_port, remote_host, remote_port)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting ssh tunnel")?;
        let mut t = Tunnel { child, local_port };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = t.child.try_wait()? {
                bail!("ssh tunnel exited early ({status})");
            }
            if std::net::TcpStream::connect(("127.0.0.1", local_port)).is_ok() {
                return Ok(t);
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        bail!("timed out opening ssh tunnel to {remote_host}:{remote_port}")
    }
}

/// A running `ssh -L` forward; killed on drop.
#[derive(Debug)]
pub struct Tunnel {
    child: Child,
    pub local_port: u16,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget::new(
            "192.168.1.20",
            "steamos",
            22,
            Path::new("/k/id"),
            Path::new("/k/known_hosts"),
        )
    }

    fn args(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("plain/path-1.0"), "plain/path-1.0");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn ssh_command_shape() {
        let c = target().ssh_command(Some("sh -s"));
        let a = args(&c);
        assert_eq!(c.get_program(), "ssh");
        assert_eq!(&a[..2], ["-p", "22"]);
        assert!(a.windows(2).any(|w| w == ["-i", "/k/id"]));
        assert!(a.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(a.contains(&"UserKnownHostsFile=/k/known_hosts".to_string()));
        assert!(a.contains(&"BatchMode=yes".to_string()));
        assert_eq!(&a[a.len() - 3..], ["steamos@192.168.1.20", "--", "sh -s"]);
    }

    #[test]
    fn scp_command_shape_and_ipv6() {
        let mut t = target();
        let a = args(&t.scp_command(Path::new("/tmp/fp.tar.gz"), "devkit-game/.upload.tar.gz"));
        assert_eq!(&a[..2], ["-P", "22"]);
        assert_eq!(
            &a[a.len() - 2..],
            [
                "/tmp/fp.tar.gz",
                "steamos@192.168.1.20:devkit-game/.upload.tar.gz"
            ]
        );
        t.host = "fe80::1%en0".into();
        let a = args(&t.scp_command(Path::new("f"), "x"));
        assert_eq!(a.last().unwrap(), "steamos@[fe80::1%en0]:x");
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let t = SshTarget::new(
            "h",
            "u",
            22,
            Path::new("/k/id"),
            Path::new("C:/Users/Jane Doe/AppData/kh"),
        );
        let a = args(&t.ssh_command(None));
        assert!(a.contains(&"UserKnownHostsFile=\"C:/Users/Jane Doe/AppData/kh\"".to_string()));
        assert!(a.contains(&"HashKnownHosts=no".to_string()));
        let d =
            args(&t.scp_download_command("frameplayer-probe-report.json", Path::new("out.json")));
        assert_eq!(
            &d[d.len() - 2..],
            ["u@h:frameplayer-probe-report.json", "out.json"]
        );
    }

    #[test]
    fn ssh_config_block() {
        let c = target().to_ssh_config("frame");
        assert!(c.starts_with("Host frame\n  HostName 192.168.1.20\n  User steamos\n  Port 22\n"));
        assert!(c.contains("IdentityFile \"/k/id\""));
        assert!(c.contains("StrictHostKeyChecking accept-new"));
    }

    #[test]
    fn tunnel_command_shape() {
        let a = args(&target().tunnel_command(40000, "127.0.0.1", 8080));
        assert!(a
            .windows(2)
            .any(|w| w == ["-L", "127.0.0.1:40000:127.0.0.1:8080"]));
        assert!(a.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert_eq!(a[0], "-N");
    }
}
