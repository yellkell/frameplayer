//! How the installer reaches the headset: one async interface ([`Remote`])
//! over two SSH implementations.
//!
//! * [`TransportKind::System`]: the system OpenSSH client (`ssh`/`scp`, see
//!   [`crate::ssh`]). Default on Linux and macOS, where it is always present.
//! * [`TransportKind::Native`]: pure-Rust SSH (russh, ring crypto). Default on
//!   Windows, so nothing has to be installed there (OpenSSH Client is an
//!   optional Windows feature) and Windows OpenSSH's strict private-key ACL
//!   checks and path-quoting quirks never come into play. Commands run over
//!   exec channels; uploads/downloads stream through `cat` (no SFTP needed);
//!   port forwards use `direct-tcpip` channels.
//!
//! Both use the same key, the same `known_hosts` file (trust on first use)
//! and the same remote scripts (sent on stdin to `sh -s`). Override the
//! choice with `FRAMEPLAYER_SSH=system|native` or `--ssh`.

use crate::ssh::{shell_quote, SshTarget};
use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// Which SSH implementation to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    System,
    Native,
}

impl TransportKind {
    /// Native on Windows, the system client elsewhere (falling back to
    /// native when `ssh` is not installed).
    pub fn platform_default() -> Self {
        if cfg!(windows) || crate::ssh::locate_openssh().is_none() {
            Self::Native
        } else {
            Self::System
        }
    }

    /// `FRAMEPLAYER_SSH` if set and valid, else [`Self::platform_default`].
    pub fn from_env_or_default() -> Self {
        std::env::var("FRAMEPLAYER_SSH")
            .ok()
            .and_then(|v| v.parse::<TransportChoice>().ok())
            .map(TransportChoice::resolve)
            .unwrap_or_else(Self::platform_default)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system OpenSSH",
            Self::Native => "built-in SSH",
        }
    }
}

/// `--ssh` value: a kind or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportChoice {
    Auto,
    Kind(TransportKind),
}

impl TransportChoice {
    pub fn resolve(self) -> TransportKind {
        match self {
            Self::Auto => TransportKind::platform_default(),
            Self::Kind(k) => k,
        }
    }
}

impl FromStr for TransportChoice {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "system" | "openssh" => Ok(Self::Kind(TransportKind::System)),
            "native" | "builtin" | "built-in" | "russh" => Ok(Self::Kind(TransportKind::Native)),
            o => Err(format!(
                "unknown ssh transport {o:?} (auto, system, native)"
            )),
        }
    }
}

/// Captured result of a remote command.
#[derive(Debug, Clone, Default)]
pub struct RemoteOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl RemoteOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Line callback for streamed output.
pub type LineSink = Box<dyn FnMut(&str) + Send>;

/// A connection to one headset.
#[derive(Clone)]
pub enum Remote {
    System(SshTarget),
    Native(Arc<NativeSsh>),
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System(t) => write!(f, "Remote::System({})", t.destination()),
            Self::Native(n) => write!(f, "Remote::Native({})", n.target.destination()),
        }
    }
}

impl Remote {
    pub fn new(kind: TransportKind, target: SshTarget) -> Self {
        match kind {
            TransportKind::System => Self::System(target),
            TransportKind::Native => Self::Native(Arc::new(NativeSsh::new(target))),
        }
    }

    pub fn kind(&self) -> TransportKind {
        match self {
            Self::System(_) => TransportKind::System,
            Self::Native(_) => TransportKind::Native,
        }
    }

    pub fn target(&self) -> &SshTarget {
        match self {
            Self::System(t) => t,
            Self::Native(n) => &n.target,
        }
    }

    /// Log in and run `true`; the error says why it failed.
    pub async fn verify(&self) -> Result<()> {
        match self {
            Self::System(t) => {
                let t = t.clone();
                let out = tokio::task::spawn_blocking(move || {
                    t.ssh_command(Some("true"))
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .output()
                })
                .await?
                .context("running ssh")?;
                if !out.status.success() {
                    bail!(
                        "ssh login failed ({}): {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                Ok(())
            }
            Self::Native(n) => {
                let code = n.exec("true", None, |_| {}, |_| {}).await?;
                if code != 0 {
                    bail!("`true` exited with {code}");
                }
                Ok(())
            }
        }
    }

    /// Can we log in non-interactively?
    pub async fn check(&self) -> bool {
        match self.verify().await {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("ssh check failed: {e:#}");
                false
            }
        }
    }

    /// Run a script, capturing output, without failing on a non-zero exit.
    pub async fn run_script_status(&self, script: &str) -> Result<RemoteOutput> {
        match self {
            Self::System(t) => {
                let (t, s) = (t.clone(), script.to_string());
                let out = tokio::task::spawn_blocking(move || t.run_script_output(&s)).await??;
                Ok(RemoteOutput {
                    code: out.status.code().unwrap_or(255),
                    stdout: out.stdout,
                    stderr: out.stderr,
                })
            }
            Self::Native(n) => {
                let (mut so, mut se) = (Vec::new(), Vec::new());
                let code = n
                    .exec(
                        "sh -s",
                        Some(script.as_bytes()),
                        |d| so.extend_from_slice(d),
                        |d| se.extend_from_slice(d),
                    )
                    .await?;
                Ok(RemoteOutput {
                    code,
                    stdout: so,
                    stderr: se,
                })
            }
        }
    }

    /// Run a script; a non-zero exit is an error carrying the remote stderr.
    pub async fn run_script(&self, script: &str) -> Result<RemoteOutput> {
        let out = self.run_script_status(script).await?;
        if !out.success() {
            bail!(
                "remote command failed (exit {}): {}",
                out.code,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tracing::debug!(
            "remote stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out)
    }

    /// Run a script, handing each stdout line (and stderr line, prefixed
    /// with `! `) to `sink` as it arrives. Returns the exit code.
    pub async fn run_script_streaming(&self, script: &str, sink: LineSink) -> Result<i32> {
        match self {
            Self::System(t) => {
                let (t, s) = (t.clone(), script.to_string());
                tokio::task::spawn_blocking(move || t.run_script_lines(&s, sink)).await?
            }
            Self::Native(n) => {
                let sink = std::sync::Mutex::new(sink);
                let mut out = LineBuffer::default();
                let mut err = LineBuffer::default();
                let code = n
                    .exec(
                        "sh -s",
                        Some(script.as_bytes()),
                        |d| out.push(d, &mut |l| (sink.lock().unwrap())(l)),
                        |d| err.push(d, &mut |l| (sink.lock().unwrap())(&format!("! {l}"))),
                    )
                    .await?;
                out.finish(&mut |l| (sink.lock().unwrap())(l));
                err.finish(&mut |l| (sink.lock().unwrap())(&format!("! {l}")));
                Ok(code)
            }
        }
    }

    /// Copy a local file to `$HOME/<remote_rel>`. `progress(done, total)`
    /// is called for the native transport (scp gives no progress).
    pub async fn upload(
        &self,
        local: &Path,
        remote_rel: &str,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<()> {
        match self {
            Self::System(t) => {
                let (t, l, r) = (t.clone(), local.to_path_buf(), remote_rel.to_string());
                tokio::task::spawn_blocking(move || t.upload(&l, &r)).await?
            }
            Self::Native(n) => n.upload(local, remote_rel, progress).await,
        }
    }

    /// Copy `$HOME/<remote_rel>` to a local file.
    pub async fn download(&self, remote_rel: &str, local: &Path) -> Result<()> {
        match self {
            Self::System(t) => {
                let (t, l, r) = (t.clone(), local.to_path_buf(), remote_rel.to_string());
                tokio::task::spawn_blocking(move || t.download(&r, &l)).await?
            }
            Self::Native(n) => n.download(remote_rel, local).await,
        }
    }

    /// Forward a local port to `remote_host:remote_port` as seen from the
    /// headset. The forward lives as long as the returned [`Tunnel`].
    pub async fn open_tunnel(&self, remote_host: &str, remote_port: u16) -> Result<Tunnel> {
        match self {
            Self::System(t) => {
                let (t, h) = (t.clone(), remote_host.to_string());
                let tun =
                    tokio::task::spawn_blocking(move || t.open_tunnel(&h, remote_port)).await??;
                Ok(Tunnel {
                    local_port: tun.local_port,
                    _guard: TunnelGuard::Process(tun),
                })
            }
            Self::Native(n) => n.open_tunnel(remote_host, remote_port).await,
        }
    }
}

/// Splits a byte stream into lines.
#[derive(Default)]
struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    fn push(&mut self, data: &[u8], emit: &mut dyn FnMut(&str)) {
        self.buf.extend_from_slice(data);
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let s = String::from_utf8_lossy(&line);
            emit(s.trim_end_matches(['\n', '\r']));
        }
    }
    fn finish(&mut self, emit: &mut dyn FnMut(&str)) {
        if !self.buf.is_empty() {
            let s = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            emit(s.trim_end_matches('\r'));
        }
    }
}

/// A local port forward; closed on drop.
pub struct Tunnel {
    pub local_port: u16,
    _guard: TunnelGuard,
}

enum TunnelGuard {
    Process(#[allow(dead_code)] crate::ssh::Tunnel),
    Task(tokio::task::JoinHandle<()>),
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        if let TunnelGuard::Task(t) = self {
            t.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Native (russh) transport.

/// Pure-Rust SSH session, connected lazily and reused.
pub struct NativeSsh {
    pub target: SshTarget,
    conn: tokio::sync::Mutex<Option<Arc<russh::client::Handle<Client>>>>,
}

/// russh callback handler: host-key trust on first use in our known_hosts.
struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    key_changed: Arc<std::sync::atomic::AtomicBool>,
}

impl russh::client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = match key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            russh::keys::PublicKeyOrCertificate::Certificate(c) => {
                russh::keys::PublicKey::from(c.public_key().clone())
            }
        };
        match russh::keys::check_known_hosts_path(&self.host, self.port, &key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Ok(false) => {
                tracing::info!(
                    "trusting new host key for {}: {}",
                    self.host,
                    key.fingerprint(russh::keys::HashAlg::Sha256)
                );
                if let Err(e) = russh::keys::known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    &key,
                    &self.known_hosts,
                ) {
                    tracing::warn!("could not record host key: {e}");
                }
                Ok(true)
            }
            Err(e) => {
                tracing::warn!("host key check for {} failed: {e}", self.host);
                self.key_changed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(false)
            }
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

impl NativeSsh {
    pub fn new(target: SshTarget) -> Self {
        Self {
            target,
            conn: Default::default(),
        }
    }

    async fn handle(&self) -> Result<Arc<russh::client::Handle<Client>>> {
        let mut g = self.conn.lock().await;
        if let Some(h) = g.as_ref() {
            if !h.is_closed() {
                return Ok(h.clone());
            }
        }
        let h = Arc::new(self.connect().await?);
        *g = Some(h.clone());
        Ok(h)
    }

    async fn connect(&self) -> Result<russh::client::Handle<Client>> {
        let t = &self.target;
        let key = russh::keys::load_secret_key(&t.key, None)
            .with_context(|| format!("loading SSH key {}", t.key.display()))?;
        let config = Arc::new(russh::client::Config {
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 4,
            inactivity_timeout: None,
            ..Default::default()
        });
        let key_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler = Client {
            host: t.host.clone(),
            port: t.port,
            known_hosts: t.known_hosts.clone(),
            key_changed: key_changed.clone(),
        };
        tracing::debug!("native ssh: connecting to {}:{}", t.host, t.port);
        let host = t
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let mut h = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            russh::client::connect(config, (host.as_str(), t.port), handler),
        )
        .await
        {
            Err(_) => bail!("timed out connecting to {}:{}", t.host, t.port),
            Ok(Err(e)) if key_changed.load(std::sync::atomic::Ordering::SeqCst) => {
                return Err(anyhow::Error::new(HostKeyChanged {
                    host: t.host.clone(),
                    known_hosts: t.known_hosts.clone(),
                })
                .context(e.to_string()));
            }
            Ok(Err(e)) => {
                return Err(anyhow!(e)).context(format!("connecting to {}:{}", t.host, t.port))
            }
            Ok(Ok(h)) => h,
        };
        let auth = h
            .authenticate_publickey(
                t.user.clone(),
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None),
            )
            .await
            .context("SSH authentication")?;
        if !auth.success() {
            bail!(
                "the headset did not accept our key for {} (not paired yet?)",
                t.destination()
            );
        }
        Ok(h)
    }

    /// Run `command`, optionally feeding `stdin`, streaming output to the
    /// callbacks. Returns the exit code (255 for a signal).
    pub async fn exec(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        mut on_stdout: impl FnMut(&[u8]),
        mut on_stderr: impl FnMut(&[u8]),
    ) -> Result<i32> {
        let h = self.handle().await?;
        let mut ch = h
            .channel_open_session()
            .await
            .context("opening SSH session")?;
        ch.exec(true, command).await?;
        if let Some(data) = stdin {
            ch.data(data).await?;
        }
        ch.eof().await?;
        let mut code = None;
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => on_stdout(&data),
                russh::ChannelMsg::ExtendedData { data, .. } => on_stderr(&data),
                russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                russh::ChannelMsg::ExitSignal { signal_name, .. } => {
                    tracing::debug!("remote command killed by {signal_name:?}");
                    code = Some(255);
                }
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        code.ok_or_else(|| anyhow!("remote command ended without an exit status"))
    }

    async fn upload(
        &self,
        local: &Path,
        remote_rel: &str,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(local)
            .await
            .with_context(|| local.display().to_string())?;
        let total = f.metadata().await?.len();
        let h = self.handle().await?;
        let mut ch = h.channel_open_session().await?;
        let cmd = format!("cat > \"$HOME/\"{}", shell_quote(remote_rel));
        ch.exec(true, cmd).await?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            ch.data(&buf[..n]).await.context("upload interrupted")?;
            done += n as u64;
            progress(done, total);
        }
        ch.eof().await?;
        let mut code = None;
        let mut err = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        if code != Some(0) {
            bail!(
                "upload failed (exit {code:?}): {}",
                String::from_utf8_lossy(&err).trim()
            );
        }
        Ok(())
    }

    async fn download(&self, remote_rel: &str, local: &Path) -> Result<()> {
        let tmp = local.with_extension("part");
        let mut f = std::io::BufWriter::new(
            std::fs::File::create(&tmp).with_context(|| tmp.display().to_string())?,
        );
        let mut werr = None;
        let mut err = Vec::new();
        let code = self
            .exec(
                &format!("cat \"$HOME/\"{}", shell_quote(remote_rel)),
                None,
                |d| {
                    if werr.is_none() {
                        if let Err(e) = f.write_all(d) {
                            werr = Some(e);
                        }
                    }
                },
                |d| err.extend_from_slice(d),
            )
            .await?;
        if let Some(e) = werr {
            return Err(e.into());
        }
        f.flush()?;
        drop(f);
        if code != 0 {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "could not read {remote_rel} on the headset: {}",
                String::from_utf8_lossy(&err).trim()
            );
        }
        std::fs::rename(&tmp, local)?;
        Ok(())
    }

    async fn open_tunnel(&self, remote_host: &str, remote_port: u16) -> Result<Tunnel> {
        let h = self.handle().await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let local_port = listener.local_addr()?.port();
        let rhost = remote_host.to_string();
        let task = tokio::spawn(async move {
            while let Ok((mut sock, peer)) = listener.accept().await {
                let (h, rhost) = (h.clone(), rhost.clone());
                tokio::spawn(async move {
                    match h
                        .channel_open_direct_tcpip(
                            rhost.clone(),
                            remote_port as u32,
                            "127.0.0.1",
                            peer.port() as u32,
                        )
                        .await
                    {
                        Ok(ch) => {
                            let mut s = ch.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut sock, &mut s).await;
                        }
                        Err(e) => tracing::warn!("forward to {rhost}:{remote_port} failed: {e}"),
                    }
                });
            }
        });
        Ok(Tunnel {
            local_port,
            _guard: TunnelGuard::Task(task),
        })
    }
}

/// The headset presented a different host key than the one we recorded
/// (typically after a factory reset or reflash).
#[derive(Debug, thiserror::Error)]
#[error("the headset at {host} has a new identity (host key changed; recorded in {})", known_hosts.display())]
pub struct HostKeyChanged {
    pub host: String,
    pub known_hosts: PathBuf,
}

/// Remove every plain-text `known_hosts` entry for `host` (any port).
/// Used after the user re-approves pairing on the headset, which proves
/// physical access, so a reflashed headset doesn't lock us out. Returns the
/// number of lines removed.
pub fn forget_host(known_hosts: &Path, host: &str) -> Result<usize> {
    let text = match std::fs::read_to_string(known_hosts) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let (kept, removed) = filter_known_hosts(&text, host);
    if removed > 0 {
        std::fs::write(known_hosts, kept)?;
    }
    Ok(removed)
}

fn filter_known_hosts(text: &str, host: &str) -> (String, usize) {
    let mut removed = 0;
    let mut out = String::new();
    for line in text.lines() {
        let hosts = line.split_whitespace().next().unwrap_or("");
        let matches = !line.trim_start().starts_with('#')
            && hosts.split(',').any(|h| {
                h == host
                    || h.strip_prefix('[')
                        .and_then(|r| r.split_once("]:"))
                        .is_some_and(|(hh, _)| hh == host)
            });
        if matches {
            removed += 1;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, removed)
}

impl SshTarget {
    /// Run a script and capture its output whatever the exit status.
    pub fn run_script_output(&self, script: &str) -> Result<std::process::Output> {
        let mut child = self
            .ssh_command(Some("sh -s"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(script.as_bytes())?;
        Ok(child.wait_with_output()?)
    }

    /// Run a script, passing stdout lines (and stderr lines prefixed `! `)
    /// to `sink`. Returns the exit code.
    pub fn run_script_lines(&self, script: &str, mut sink: LineSink) -> Result<i32> {
        let mut child = self
            .ssh_command(Some("sh -s"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(script.as_bytes())?;
        let stderr = child.stderr.take().expect("piped");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let errt = std::thread::spawn(move || {
            for l in std::io::BufReader::new(stderr)
                .lines()
                .map_while(|l| l.ok())
            {
                let _ = tx.send(l);
            }
        });
        for line in std::io::BufReader::new(child.stdout.take().expect("piped"))
            .lines()
            .map_while(|l| l.ok())
        {
            sink(&line);
            while let Ok(e) = rx.try_recv() {
                sink(&format!("! {e}"));
            }
        }
        let _ = errt.join();
        while let Ok(e) = rx.try_recv() {
            sink(&format!("! {e}"));
        }
        Ok(child.wait()?.code().unwrap_or(255))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_choice_parsing() {
        assert_eq!(
            "native".parse::<TransportChoice>().unwrap(),
            TransportChoice::Kind(TransportKind::Native)
        );
        assert_eq!(
            "OpenSSH".parse::<TransportChoice>().unwrap(),
            TransportChoice::Kind(TransportKind::System)
        );
        assert_eq!(
            "auto".parse::<TransportChoice>().unwrap(),
            TransportChoice::Auto
        );
        assert!("telnet".parse::<TransportChoice>().is_err());
        if cfg!(windows) {
            assert_eq!(TransportKind::platform_default(), TransportKind::Native);
        }
    }

    #[test]
    fn line_buffer_splits_chunks() {
        let mut lb = LineBuffer::default();
        let mut got = Vec::new();
        lb.push(b"hel", &mut |l| got.push(l.to_string()));
        lb.push(b"lo\r\nwor", &mut |l| got.push(l.to_string()));
        lb.push(b"ld\n\nend", &mut |l| got.push(l.to_string()));
        lb.finish(&mut |l| got.push(l.to_string()));
        assert_eq!(got, ["hello", "world", "", "end"]);
    }

    #[test]
    fn forgets_known_hosts_entries() {
        let text = "192.168.1.5 ssh-ed25519 AAAA1\n[192.168.1.5]:2222 ssh-ed25519 AAAA2\n\
                    192.168.1.50 ssh-ed25519 AAAA3\nother,192.168.1.5 ssh-rsa AAAA4\n# 192.168.1.5 comment\n";
        let (kept, n) = filter_known_hosts(text, "192.168.1.5");
        assert_eq!(n, 3);
        assert_eq!(
            kept,
            "192.168.1.50 ssh-ed25519 AAAA3\n# 192.168.1.5 comment\n"
        );
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("kh");
        assert_eq!(forget_host(&p, "x").unwrap(), 0);
        std::fs::write(&p, text).unwrap();
        assert_eq!(forget_host(&p, "192.168.1.5").unwrap(), 3);
        assert!(!std::fs::read_to_string(&p).unwrap().contains("AAAA1"));
    }
}
