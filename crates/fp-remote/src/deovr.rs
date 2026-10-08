//! DeoVR-compatible remote-control API.
//!
//! Wire format (as implemented by DeoVR and spoken by ohdoki, MultiFunPlayer,
//! ScriptPlayer and similar tools): a TCP stream of messages, each a 4-byte
//! little-endian length followed by that many bytes of UTF-8 JSON. A
//! zero-length message is a keep-alive ping.
//!
//! The player sends a ping about every second and a status message
//! `{"path", "duration", "currentTime", "playbackSpeed", "playerState"}`
//! (`playerState` 0 = playing, 1 = paused) whenever status changes and at
//! least every second while playing; `{}` when nothing is open. Clients send
//! the same shape with any subset of fields as commands.

use crate::RemoteError;
use crate::hub::{RemoteEvent, Shared};
use crate::net::is_allowed_peer;
use crate::status::{ChangeTracker, command_is_valid, lock};
use fp_core::playback::now_ms;
use fp_core::{PlaybackStatus, PlayerCommand};
use serde_json::{Map, Value};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest message payload accepted from a client; bigger frames drop it.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// How long a client may take to deliver the rest of a frame it started.
const FRAME_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Socket read timeout: how often reader threads check for shutdown.
const READ_POLL: Duration = Duration::from_millis(200);
/// A client that cannot absorb a write for this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages queued per client before it counts as stuck and is dropped.
const CLIENT_QUEUE: usize = 64;
/// Broadcaster tick.
const TICK: Duration = Duration::from_millis(50);
/// Keep-alive and playing-status interval.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Why a DeoVR message or stream was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The frame header announced more than [`MAX_FRAME_LEN`] bytes.
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    /// The payload is not UTF-8.
    #[error("frame is not UTF-8")]
    NotUtf8,
    /// The payload is not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// The payload is JSON but not an object.
    #[error("message is not a JSON object")]
    NotObject,
    /// The peer closed the connection (possibly mid-frame).
    #[error("connection closed")]
    Closed,
    /// The peer started a frame and stopped sending.
    #[error("frame stalled")]
    Stalled,
    /// The server is shutting down.
    #[error("server stopping")]
    Stopped,
    /// Socket error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Prefixes `payload` with its little-endian length.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// One DeoVR message: any subset of the status fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeovrMessage {
    /// Media location (path or URL).
    pub path: Option<String>,
    /// Seconds.
    pub duration: Option<f64>,
    /// Seconds from the start.
    pub current_time: Option<f64>,
    /// 1.0 is normal speed.
    pub playback_speed: Option<f64>,
    /// 0 = playing, 1 = paused.
    pub player_state: Option<i64>,
}

impl DeovrMessage {
    /// The status message for `s` at wall-clock `now_ms`; all fields empty
    /// (`{}`) when nothing is open.
    pub fn from_status(s: &PlaybackStatus, now_ms: u64) -> DeovrMessage {
        if s.location.is_empty() {
            return DeovrMessage::default();
        }
        DeovrMessage {
            path: Some(s.location.clone()),
            duration: Some(s.duration.max(0.0)),
            current_time: Some(s.position_at(now_ms).max(0.0)),
            playback_speed: Some(if s.speed > 0.0 { s.speed } else { 1.0 }),
            player_state: Some(if s.playing { 0 } else { 1 }),
        }
    }

    /// Parses a client message leniently: unknown fields and fields of the
    /// wrong type are ignored, but the payload must be a UTF-8 JSON object.
    pub fn parse(payload: &[u8]) -> Result<DeovrMessage, ProtocolError> {
        let text = std::str::from_utf8(payload).map_err(|_| ProtocolError::NotUtf8)?;
        let value: Value =
            serde_json::from_str(text).map_err(|e| ProtocolError::Json(e.to_string()))?;
        let Value::Object(obj) = value else {
            return Err(ProtocolError::NotObject);
        };
        let num = |k: &str| obj.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
        Ok(DeovrMessage {
            path: obj
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .map(str::to_owned),
            duration: num("duration"),
            current_time: num("currentTime"),
            playback_speed: num("playbackSpeed"),
            player_state: num("playerState").map(|v| v.round() as i64),
        })
    }

    /// Serialises to the JSON object DeoVR sends, omitting absent fields.
    pub fn to_json(&self) -> String {
        let mut m = Map::new();
        if let Some(p) = &self.path {
            m.insert("path".into(), Value::from(p.as_str()));
        }
        let mut put = |k: &str, v: Option<f64>| {
            if let Some(v) = v {
                m.insert(k.into(), Value::from(v));
            }
        };
        put("duration", self.duration);
        put("currentTime", self.current_time);
        put("playbackSpeed", self.playback_speed);
        if let Some(s) = self.player_state {
            m.insert("playerState".into(), Value::from(s));
        }
        Value::Object(m).to_string()
    }

    /// Translates a client message into player commands, in the order the
    /// player should apply them: open, speed, seek, play/pause.
    ///
    /// A `path` equal to `current_location` does not reopen the file, so a
    /// client may send `{"path": ..., "currentTime": ...}` to seek within
    /// the open video. `duration` is informational and ignored.
    pub fn to_commands(&self, current_location: &str) -> Vec<PlayerCommand> {
        let mut out = Vec::new();
        if let Some(p) = &self.path
            && p != current_location
        {
            out.push(PlayerCommand::Open {
                location: p.clone(),
            });
        }
        if let Some(speed) = self.playback_speed {
            out.push(PlayerCommand::SetSpeed { speed });
        }
        if let Some(position) = self.current_time {
            out.push(PlayerCommand::Seek { position });
        }
        match self.player_state {
            Some(0) => out.push(PlayerCommand::Play),
            Some(1) => out.push(PlayerCommand::Pause),
            _ => {}
        }
        out.retain(command_is_valid);
        out
    }
}

/// Incremental frame decoder over a socket with a read timeout.
pub(crate) struct FrameReader {
    buf: Vec<u8>,
    max_len: usize,
    partial_since: Option<Instant>,
}

impl FrameReader {
    pub(crate) fn new(max_len: usize) -> FrameReader {
        FrameReader {
            buf: Vec::new(),
            max_len,
            partial_since: None,
        }
    }

    /// Returns the next complete payload (empty for a ping). Read timeouts
    /// on `r` are polling points: `stop` is checked, and a frame left
    /// incomplete for longer than `stall` is an error.
    pub(crate) fn read_frame<R: Read>(
        &mut self,
        r: &mut R,
        stall: Duration,
        stop: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, ProtocolError> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(frame);
            }
            if stop() {
                return Err(ProtocolError::Stopped);
            }
            if self.partial_since.is_some_and(|t| t.elapsed() > stall) {
                return Err(ProtocolError::Stalled);
            }
            match r.read(&mut chunk) {
                Ok(0) => return Err(ProtocolError::Closed),
                Ok(n) => {
                    if self.buf.is_empty() {
                        self.partial_since = Some(Instant::now());
                    }
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(ProtocolError::Io(e)),
            }
        }
    }

    fn take_frame(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let Some(header) = self.buf.first_chunk::<4>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*header) as usize;
        if len > self.max_len {
            return Err(ProtocolError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let frame = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        self.partial_since = if self.buf.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        Ok(Some(frame))
    }
}

type Frame = Arc<[u8]>;

struct Client {
    id: u64,
    tx: SyncSender<Frame>,
    stream: TcpStream,
}

/// Connected clients and their outgoing queues.
#[derive(Default)]
struct Registry {
    clients: Mutex<Vec<Client>>,
    next_id: AtomicU64,
}

impl Registry {
    fn add(&self, tx: SyncSender<Frame>, stream: TcpStream) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.clients).push(Client { id, tx, stream });
        id
    }

    fn remove(&self, id: u64) {
        lock(&self.clients).retain(|c| c.id != id);
    }

    fn len(&self) -> usize {
        lock(&self.clients).len()
    }

    /// Queues `frame` for every client without blocking. A client whose
    /// queue is full is stuck and gets disconnected.
    fn broadcast(&self, frame: &Frame) {
        lock(&self.clients).retain(|c| match c.tx.try_send(frame.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::warn!("DeoVR client {} is not reading; dropping it", c.id);
                let _ = c.stream.shutdown(Shutdown::Both);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    fn shutdown_all(&self) {
        for c in lock(&self.clients).drain(..) {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
    }
}

fn status_frame(s: &PlaybackStatus, now: u64) -> Frame {
    encode_frame(DeovrMessage::from_status(s, now).to_json().as_bytes()).into()
}

/// A running DeoVR API server.
pub(crate) struct DeovrServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    registry: Arc<Registry>,
    threads: Vec<JoinHandle<()>>,
}

impl DeovrServer {
    pub(crate) fn start(
        bind: SocketAddr,
        ctx: Arc<Shared>,
        max_clients: usize,
    ) -> Result<DeovrServer, RemoteError> {
        let listener =
            TcpListener::bind(bind).map_err(|source| RemoteError::Bind { addr: bind, source })?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(Registry::default());

        let mut server = DeovrServer {
            addr,
            stop: stop.clone(),
            registry: registry.clone(),
            threads: Vec::new(),
        };
        // Prime the tracker with the current status before any client can
        // connect: new clients get it directly from `start_client`, so the
        // broadcaster only sends what changes after that.
        let mut tracker = ChangeTracker::new(HEARTBEAT);
        let _ = tracker.poll(&ctx.status, now_ms());
        let (c, r, s) = (ctx.clone(), registry.clone(), stop.clone());
        server.threads.push(
            thread::Builder::new()
                .name("deovr-broadcast".into())
                .spawn(move || broadcast_loop(&c, &r, &s, tracker))?,
        );
        let accept = thread::Builder::new()
            .name("deovr-accept".into())
            .spawn(move || accept_loop(listener, &ctx, &registry, &stop, max_clients));
        match accept {
            Ok(h) => server.threads.push(h),
            Err(e) => {
                server.stop();
                return Err(e.into());
            }
        }
        log::info!("DeoVR remote API listening on {addr}");
        Ok(server)
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn client_count(&self) -> usize {
        self.registry.len()
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.registry.shutdown_all();
    }
}

impl Drop for DeovrServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(
    listener: TcpListener,
    ctx: &Arc<Shared>,
    registry: &Arc<Registry>,
    stop: &Arc<AtomicBool>,
    max_clients: usize,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !is_allowed_peer(peer.ip()) {
                    log::warn!("DeoVR API: refusing non-LAN peer {peer}");
                    continue;
                }
                if registry.len() >= max_clients {
                    log::warn!("DeoVR API: too many clients, refusing {peer}");
                    continue;
                }
                if let Err(e) = start_client(stream, ctx, registry, stop) {
                    log::warn!("DeoVR API: cannot serve {peer}: {e}");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(TICK),
            Err(e) => {
                log::warn!("DeoVR API: accept failed: {e}");
                thread::sleep(TICK);
            }
        }
    }
}

fn start_client(
    stream: TcpStream,
    ctx: &Arc<Shared>,
    registry: &Arc<Registry>,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(READ_POLL))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let peer = stream.peer_addr()?;
    let (tx, rx) = sync_channel::<Frame>(CLIENT_QUEUE);
    // The current status goes out first so the tool syncs immediately.
    let _ = tx.try_send(status_frame(&ctx.status.snapshot(), now_ms()));
    let writer = stream.try_clone()?;
    let id = registry.add(tx, stream.try_clone()?);

    if let Err(e) = thread::Builder::new()
        .name("deovr-write".into())
        .spawn(move || write_loop(writer, &rx))
    {
        registry.remove(id);
        return Err(e);
    }
    let (ctx, reg, stop) = (ctx.clone(), registry.clone(), stop.clone());
    let spawned = thread::Builder::new()
        .name("deovr-read".into())
        .spawn(move || {
            log::info!("DeoVR client {id} connected from {peer}");
            let mut stream = stream;
            let reason = read_loop(&mut stream, &ctx, &stop);
            log::info!("DeoVR client {id} ({peer}) disconnected: {reason}");
            reg.remove(id);
            let _ = stream.shutdown(Shutdown::Both);
        });
    if let Err(e) = spawned {
        registry.remove(id);
        return Err(e);
    }
    Ok(())
}

fn write_loop(mut stream: TcpStream, rx: &Receiver<Frame>) {
    for frame in rx {
        if stream.write_all(&frame).is_err() {
            break;
        }
    }
    // Wakes the reader so the client is unregistered promptly.
    let _ = stream.shutdown(Shutdown::Both);
}

/// Reads client messages until the connection ends; returns why.
fn read_loop(stream: &mut TcpStream, ctx: &Shared, stop: &AtomicBool) -> ProtocolError {
    let mut reader = FrameReader::new(MAX_FRAME_LEN);
    let stopping = || stop.load(Ordering::Acquire);
    loop {
        let payload = match reader.read_frame(stream, FRAME_STALL_TIMEOUT, &stopping) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if payload.is_empty() {
            continue; // keep-alive
        }
        let msg = match DeovrMessage::parse(&payload) {
            Ok(m) => m,
            Err(e) => return e,
        };
        for cmd in msg.to_commands(&ctx.status.location()) {
            ctx.emit(RemoteEvent::Command(cmd));
        }
    }
}

fn broadcast_loop(
    ctx: &Shared,
    registry: &Registry,
    stop: &AtomicBool,
    mut tracker: ChangeTracker,
) {
    let ping: Frame = encode_frame(&[]).into();
    let mut last_ping = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let now = now_ms();
        if let Some(snap) = tracker.poll(&ctx.status, now) {
            registry.broadcast(&status_frame(&snap, now));
        }
        if last_ping.elapsed() >= HEARTBEAT {
            registry.broadcast(&ping);
            last_ping = Instant::now();
        }
        thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A reader that hands out data in fixed-size pieces, then times out.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Err(io::Error::new(ErrorKind::WouldBlock, "timeout"));
            }
            let n = self.step.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn frames_round_trip_in_pieces() {
        let mut data = encode_frame(br#"{"a":1}"#);
        data.extend(encode_frame(b""));
        data.extend(encode_frame(br#"{"b":2}"#));
        for step in [1, 3, 7, 1000] {
            let mut r = Trickle {
                data: data.clone(),
                pos: 0,
                step,
            };
            let mut fr = FrameReader::new(MAX_FRAME_LEN);
            let s = Duration::from_secs(5);
            assert_eq!(fr.read_frame(&mut r, s, &never).unwrap(), br#"{"a":1}"#);
            assert_eq!(fr.read_frame(&mut r, s, &never).unwrap(), b"");
            assert_eq!(fr.read_frame(&mut r, s, &never).unwrap(), br#"{"b":2}"#);
        }
    }

    #[test]
    fn oversized_closed_and_stalled_frames_fail() {
        let mut fr = FrameReader::new(16);
        let mut r = Cursor::new(17u32.to_le_bytes().to_vec());
        assert!(matches!(
            fr.read_frame(&mut r, Duration::from_secs(1), &never),
            Err(ProtocolError::TooLarge(17))
        ));

        let mut fr = FrameReader::new(16);
        let mut r = Cursor::new(vec![5, 0, 0, 0, b'{']);
        assert!(matches!(
            fr.read_frame(&mut r, Duration::from_secs(1), &never),
            Err(ProtocolError::Closed)
        ));

        let mut fr = FrameReader::new(16);
        let mut r = Trickle {
            data: vec![5, 0],
            pos: 0,
            step: 2,
        };
        assert!(matches!(
            fr.read_frame(&mut r, Duration::ZERO, &never),
            Err(ProtocolError::Stalled)
        ));

        let mut fr = FrameReader::new(16);
        let mut r = Trickle {
            data: vec![],
            pos: 0,
            step: 1,
        };
        assert!(matches!(
            fr.read_frame(&mut r, Duration::ZERO, &|| true),
            Err(ProtocolError::Stopped)
        ));
    }

    #[test]
    fn status_messages() {
        assert_eq!(
            DeovrMessage::from_status(&PlaybackStatus::default(), 0).to_json(),
            "{}"
        );
        let s = PlaybackStatus {
            location: "/v/a.mp4".into(),
            title: "A".into(),
            duration: 120.0,
            position: 10.0,
            speed: 1.0,
            playing: true,
            sampled_at_ms: 1_000,
        };
        let v: Value =
            serde_json::from_str(&DeovrMessage::from_status(&s, 3_000).to_json()).unwrap();
        assert_eq!(v["path"], "/v/a.mp4");
        assert_eq!(v["duration"], 120.0);
        assert_eq!(v["currentTime"], 12.0);
        assert_eq!(v["playbackSpeed"], 1.0);
        assert!(v["playerState"].is_i64(), "playerState must be an integer");
        assert_eq!(v["playerState"], 0);
        let paused = PlaybackStatus {
            playing: false,
            ..s
        };
        let v: Value =
            serde_json::from_str(&DeovrMessage::from_status(&paused, 3_000).to_json()).unwrap();
        assert_eq!(v["playerState"], 1);
        assert_eq!(v["currentTime"], 10.0);
    }

    #[test]
    fn client_messages_become_commands() {
        let m = DeovrMessage::parse(
            br#"{"path":"/b.mp4","currentTime":12.5,"playerState":0,"playbackSpeed":1.5,"x":[1]}"#,
        )
        .unwrap();
        assert_eq!(
            m.to_commands("/a.mp4"),
            vec![
                PlayerCommand::Open {
                    location: "/b.mp4".into()
                },
                PlayerCommand::SetSpeed { speed: 1.5 },
                PlayerCommand::Seek { position: 12.5 },
                PlayerCommand::Play,
            ]
        );
        // Same path: no reopen, just the seek.
        let m = DeovrMessage::parse(br#"{"path":"/a.mp4","currentTime":3}"#).unwrap();
        assert_eq!(
            m.to_commands("/a.mp4"),
            vec![PlayerCommand::Seek { position: 3.0 }]
        );
        let m = DeovrMessage::parse(br#"{"playerState":1.0}"#).unwrap();
        assert_eq!(m.to_commands(""), vec![PlayerCommand::Pause]);
        // Wrong types and out-of-range values are ignored, not fatal.
        let m = DeovrMessage::parse(
            br#"{"path":5,"currentTime":"x","playerState":7,"playbackSpeed":-1,"duration":9}"#,
        )
        .unwrap();
        assert!(m.to_commands("").is_empty());
        assert_eq!(
            DeovrMessage::parse(b"{}").unwrap().to_commands("/a"),
            vec![]
        );
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert!(matches!(
            DeovrMessage::parse(b"nope"),
            Err(ProtocolError::Json(_))
        ));
        assert!(matches!(
            DeovrMessage::parse(b"[1,2]"),
            Err(ProtocolError::NotObject)
        ));
        assert!(matches!(
            DeovrMessage::parse(&[0x7b, 0xff, 0x7d]),
            Err(ProtocolError::NotUtf8)
        ));
    }
}
