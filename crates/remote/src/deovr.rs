//! DeoVR-compatible remote control over TCP (what ohdoki, ScriptPlayer, MultiFunPlayer and
//! other script players connect to).
//!
//! Framing: each packet is a 4-byte little-endian signed length followed by that many bytes of
//! UTF-8 JSON. A zero-length packet is a keepalive ping; clients send one about every second.
//! The player pushes its status about once per second:
//! `{"path": "...", "duration": 123.4, "currentTime": 5.6, "playbackSpeed": 1.0, "playerState": 0}`
//! where `playerState` is 0 = playing, 1 = paused. Clients control the player by sending the
//! same fields (any subset).
//!
//! [verify] Port 23554 and the field set follow DeoVR's published remote-control docs and the
//! client implementations in MultiFunPlayer / ScriptPlayer; re-test against ohdoki before
//! release. When nothing is loaded we send keepalive pings instead of a status object, which
//! the clients above treat as "no video".

use crate::net::is_lan_ip;
use crate::types::{PlayerStatus, RemoteCommand, RemoteLink};
use bytes::{Buf, BytesMut};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// DeoVR's default remote-control port.
pub const DEFAULT_PORT: u16 = 23554;
/// Largest JSON packet we accept.
pub const MAX_FRAME: usize = 1 << 20;

/// A decoded packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// Zero-length keepalive.
    Ping,
    /// JSON payload (UTF-8 validated).
    Json(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("frame length {0} out of range")]
    BadLength(i64),
    #[error("frame is not valid UTF-8")]
    Utf8,
}

/// Incremental decoder: feed it whatever the socket returns, pull complete frames out.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: BytesMut,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Bytes buffered but not yet decoded.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Next complete frame, `Ok(None)` if more data is needed. An error means the stream is
    /// corrupt and the connection should be dropped.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, CodecError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = i32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len < 0 || len as usize > MAX_FRAME {
            return Err(CodecError::BadLength(len as i64));
        }
        let len = len as usize;
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        self.buf.advance(4);
        if len == 0 {
            return Ok(Some(Frame::Ping));
        }
        let payload = self.buf.split_to(len);
        String::from_utf8(payload.to_vec())
            .map(|s| Some(Frame::Json(s)))
            .map_err(|_| CodecError::Utf8)
    }
}

/// Length-prefix a payload.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as i32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The keepalive packet.
pub const PING: [u8; 4] = [0, 0, 0, 0];

/// Status packet body (field names exactly as DeoVR sends them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoVrStatus {
    pub path: String,
    pub duration: f64,
    pub current_time: f64,
    pub playback_speed: f64,
    /// 0 = playing, 1 = paused.
    pub player_state: u8,
}

impl DeoVrStatus {
    /// `None` when nothing is loaded.
    pub fn from_status(s: &PlayerStatus) -> Option<Self> {
        let path = s.path.clone()?;
        Some(DeoVrStatus {
            path,
            duration: s.duration.unwrap_or(0.0),
            current_time: s.position,
            playback_speed: s.speed,
            player_state: if s.playing { 0 } else { 1 },
        })
    }
}

/// Client → player packet: any subset of the status fields.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoVrCommand {
    pub path: Option<String>,
    pub current_time: Option<f64>,
    pub playback_speed: Option<f64>,
    pub player_state: Option<u8>,
}

/// Seeks smaller than this relative to the current position are ignored, so clients that
/// echo the status back don't cause stutter.
pub const SEEK_EPSILON: f64 = 0.5;

/// Translate a client packet into app commands, relative to the current status so that
/// echoed fields are no-ops.
pub fn commands_from_json(
    json: &str,
    current: &PlayerStatus,
) -> Result<Vec<RemoteCommand>, serde_json::Error> {
    let c: DeoVrCommand = serde_json::from_str(json)?;
    let mut out = Vec::new();
    let mut opened = false;
    if let Some(p) = c.path.filter(|p| !p.is_empty()) {
        if current.path.as_deref() != Some(p.as_str()) {
            out.push(RemoteCommand::Open {
                uri: p,
                start: c.current_time.filter(|t| t.is_finite() && *t >= 0.0),
            });
            opened = true;
        }
    }
    if !opened {
        if let Some(t) = c.current_time.filter(|t| t.is_finite() && *t >= 0.0) {
            if (t - current.position).abs() > SEEK_EPSILON {
                out.push(RemoteCommand::Seek { seconds: t });
            }
        }
    }
    if let Some(s) = c.playback_speed.filter(|s| s.is_finite() && *s > 0.0) {
        if (s - current.speed).abs() > 1e-3 {
            out.push(RemoteCommand::SetSpeed { speed: s });
        }
    }
    match c.player_state {
        Some(0) if opened || !current.playing => out.push(RemoteCommand::Play),
        Some(1) if opened || current.playing => out.push(RemoteCommand::Pause),
        _ => {}
    }
    Ok(out)
}

/// Encode the packet to send for `status`: a status JSON frame, or a ping if nothing is loaded.
pub fn status_packet(status: &PlayerStatus) -> Vec<u8> {
    match DeoVrStatus::from_status(status) {
        Some(s) => encode_frame(
            serde_json::to_string(&s)
                .expect("status serialises")
                .as_bytes(),
        ),
        None => PING.to_vec(),
    }
}

/// Settings for the DeoVR server.
#[derive(Debug, Clone, PartialEq)]
pub struct DeoVrServerOptions {
    pub status_interval: Duration,
    /// Drop clients that send nothing (not even pings) for this long.
    pub idle_timeout: Option<Duration>,
}

impl Default for DeoVrServerOptions {
    fn default() -> Self {
        DeoVrServerOptions {
            status_interval: Duration::from_secs(1),
            idle_timeout: None,
        }
    }
}

/// Accept loop. Returns when `shutdown` flips to `true`.
pub async fn serve(
    listener: TcpListener,
    link: RemoteLink,
    opts: DeoVrServerOptions,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, peer)) => {
                    if !is_lan_ip(peer.ip()) {
                        tracing::warn!(%peer, "rejecting non-LAN DeoVR remote client");
                        continue;
                    }
                    tracing::info!(%peer, "DeoVR remote client connected");
                    let (link, opts, sd) = (link.clone(), opts.clone(), shutdown.clone());
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, peer, link, opts, sd).await {
                            tracing::debug!(%peer, "DeoVR client closed: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("DeoVR accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.changed() => if *shutdown.borrow() { return },
        }
    }
}

/// Fields whose change warrants an immediate (not periodic) status push.
fn significant(s: &PlayerStatus) -> (Option<String>, bool, u64) {
    (s.path.clone(), s.playing, s.speed.to_bits())
}

async fn handle_client(
    stream: TcpStream,
    peer: SocketAddr,
    mut link: RemoteLink,
    opts: DeoVrServerOptions,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; 8192];
    let mut tick = tokio::time::interval(opts.status_interval.max(Duration::from_millis(50)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_sig = significant(&link.status.borrow_and_update());
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            n = rd.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    return Ok(());
                }
                last_rx = tokio::time::Instant::now();
                dec.push(&buf[..n]);
                loop {
                    match dec.next_frame() {
                        Ok(None) => break,
                        Ok(Some(Frame::Ping)) => {}
                        Ok(Some(Frame::Json(j))) => {
                            let current = link.status.borrow().clone();
                            match commands_from_json(&j, &current) {
                                Ok(cmds) => {
                                    for c in cmds {
                                        if link.commands.send(c).await.is_err() {
                                            return Ok(());
                                        }
                                    }
                                }
                                Err(e) => tracing::debug!(%peer, "ignoring bad DeoVR packet {j:?}: {e}"),
                            }
                        }
                        Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    }
                }
            }
            _ = tick.tick() => {
                if let Some(t) = opts.idle_timeout {
                    if last_rx.elapsed() > t {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "client idle"));
                    }
                }
                let pkt = status_packet(&link.status.borrow());
                wr.write_all(&pkt).await?;
            }
            changed = link.status.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let s = link.status.borrow_and_update().clone();
                let sig = significant(&s);
                if sig != last_sig {
                    last_sig = sig;
                    wr.write_all(&status_packet(&s)).await?;
                }
            }
            _ = shutdown.changed() => if *shutdown.borrow() { return Ok(()) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::link;

    fn playing(path: &str, pos: f64) -> PlayerStatus {
        PlayerStatus {
            path: Some(path.into()),
            duration: Some(100.0),
            position: pos,
            speed: 1.0,
            playing: true,
            ..Default::default()
        }
    }

    #[test]
    fn decodes_partial_and_batched_frames() {
        let a = encode_frame(br#"{"path":"a"}"#);
        let b = encode_frame(br#"{"currentTime":3}"#);
        let mut stream = Vec::new();
        stream.extend_from_slice(&PING);
        stream.extend_from_slice(&a);
        stream.extend_from_slice(&PING);
        stream.extend_from_slice(&b);
        // Byte-at-a-time delivery.
        let mut dec = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in &stream {
            dec.push(std::slice::from_ref(byte));
            while let Some(f) = dec.next_frame().unwrap() {
                frames.push(f);
            }
        }
        let expect = vec![
            Frame::Ping,
            Frame::Json(r#"{"path":"a"}"#.into()),
            Frame::Ping,
            Frame::Json(r#"{"currentTime":3}"#.into()),
        ];
        assert_eq!(frames, expect);
        // Everything in one read.
        let mut dec = FrameDecoder::new();
        dec.push(&stream);
        let mut all = Vec::new();
        while let Some(f) = dec.next_frame().unwrap() {
            all.push(f);
        }
        assert_eq!(all, expect);
        assert_eq!(dec.pending(), 0);
        // Split across a length prefix boundary.
        let mut dec = FrameDecoder::new();
        dec.push(&stream[..6]);
        assert_eq!(dec.next_frame().unwrap(), Some(Frame::Ping));
        assert_eq!(dec.next_frame().unwrap(), None);
        dec.push(&stream[6..]);
        assert_eq!(dec.next_frame().unwrap(), Some(expect[1].clone()));
    }

    #[test]
    fn rejects_bad_frames() {
        let mut dec = FrameDecoder::new();
        dec.push(&(-1i32).to_le_bytes());
        assert_eq!(dec.next_frame(), Err(CodecError::BadLength(-1)));
        let mut dec = FrameDecoder::new();
        dec.push(&((MAX_FRAME as i32) + 1).to_le_bytes());
        assert!(matches!(dec.next_frame(), Err(CodecError::BadLength(_))));
        let mut dec = FrameDecoder::new();
        dec.push(&encode_frame(&[0xff, 0xfe]));
        assert_eq!(dec.next_frame(), Err(CodecError::Utf8));
    }

    #[test]
    fn encodes_little_endian() {
        assert_eq!(encode_frame(b"{}"), vec![2, 0, 0, 0, b'{', b'}']);
        let big = vec![b' '; 300];
        assert_eq!(&encode_frame(&big)[..4], &[0x2c, 0x01, 0, 0]);
    }

    #[test]
    fn status_json_shape() {
        let s = playing("http://nas/v.mp4", 12.5);
        let pkt = status_packet(&s);
        let len = u32::from_le_bytes(pkt[..4].try_into().unwrap()) as usize;
        let v: serde_json::Value = serde_json::from_slice(&pkt[4..4 + len]).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"path": "http://nas/v.mp4", "duration": 100.0, "currentTime": 12.5, "playbackSpeed": 1.0, "playerState": 0})
        );
        let paused = PlayerStatus {
            playing: false,
            ..s
        };
        assert_eq!(DeoVrStatus::from_status(&paused).unwrap().player_state, 1);
        assert_eq!(status_packet(&PlayerStatus::default()), PING.to_vec());
    }

    #[test]
    fn command_translation() {
        let cur = playing("a.mp4", 10.0);
        let c = |j: &str| commands_from_json(j, &cur).unwrap();
        assert_eq!(
            c(r#"{"path":"b.mp4","currentTime":5}"#),
            vec![RemoteCommand::Open {
                uri: "b.mp4".into(),
                start: Some(5.0)
            }]
        );
        assert_eq!(
            c(r#"{"path":"b.mp4","playerState":1}"#),
            vec![
                RemoteCommand::Open {
                    uri: "b.mp4".into(),
                    start: None
                },
                RemoteCommand::Pause
            ]
        );
        assert_eq!(
            c(r#"{"currentTime":42.0}"#),
            vec![RemoteCommand::Seek { seconds: 42.0 }]
        );
        assert_eq!(c(r#"{"playerState":1}"#), vec![RemoteCommand::Pause]);
        assert_eq!(c(r#"{"playerState":0}"#), vec![], "already playing");
        assert_eq!(
            c(r#"{"playbackSpeed":1.5}"#),
            vec![RemoteCommand::SetSpeed { speed: 1.5 }]
        );
        // An echoed status is a no-op.
        assert_eq!(
            c(
                r#"{"path":"a.mp4","duration":100,"currentTime":10.2,"playbackSpeed":1.0,"playerState":0}"#
            ),
            vec![]
        );
        assert_eq!(c(r#"{"currentTime":-3,"playbackSpeed":0}"#), vec![]);
        assert!(commands_from_json("[1]", &cur).is_err());
    }

    async fn read_frame(s: &mut TcpStream, dec: &mut FrameDecoder) -> Frame {
        let mut buf = [0u8; 1024];
        loop {
            if let Some(f) = dec.next_frame().unwrap() {
                return f;
            }
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "server closed");
            dec.push(&buf[..n]);
        }
    }

    #[tokio::test]
    async fn server_round_trip() {
        let (mut app, remote) = link(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sd_tx, sd_rx) = watch::channel(false);
        let opts = DeoVrServerOptions {
            status_interval: Duration::from_millis(100),
            idle_timeout: None,
        };
        let server = tokio::spawn(serve(listener, remote, opts, sd_rx));

        let mut c = TcpStream::connect(addr).await.unwrap();
        let mut dec = FrameDecoder::new();
        // Nothing loaded: the server pings.
        assert_eq!(read_frame(&mut c, &mut dec).await, Frame::Ping);

        // Client sends a ping and a command split over two writes, plus a second command in the
        // same write.
        let mut bytes = PING.to_vec();
        bytes.extend(encode_frame(br#"{"path":"x.mp4"}"#));
        bytes.extend(encode_frame(br#"{"playerState":0}"#));
        c.write_all(&bytes[..7]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.write_all(&bytes[7..]).await.unwrap();
        let cmd = app.commands.recv().await.unwrap();
        assert_eq!(
            cmd,
            RemoteCommand::Open {
                uri: "x.mp4".into(),
                start: None
            }
        );
        assert_eq!(app.commands.recv().await.unwrap(), RemoteCommand::Play);

        // App reports playback: client receives a status immediately (significant change).
        app.status.send_replace(playing("x.mp4", 1.0));
        let f = loop {
            match read_frame(&mut c, &mut dec).await {
                Frame::Json(j) => break j,
                Frame::Ping => continue,
            }
        };
        let st: DeoVrStatus = serde_json::from_str(&f).unwrap();
        assert_eq!((st.path.as_str(), st.player_state), ("x.mp4", 0));

        sd_tx.send(true).unwrap();
        server.await.unwrap();
    }
}
