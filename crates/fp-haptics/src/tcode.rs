//! TCode v0.3 devices (OSR2, SR6 and other open-source strokers) over a USB
//! serial port, TCP or UDP.
//!
//! A command line holds one or more axis commands separated by spaces and
//! ends with `\n`: the channel (`L0`), a magnitude whose digits are the
//! fractional part of the position (`9999` = 0.9999), and optionally `I`
//! followed by the time in milliseconds to get there, e.g.
//! `L09999I250 R05000I250\n`. `DSTOP\n` stops all motion.
//!
//! Serial ports are configured 115200 8N1 raw through termios and written
//! non-blocking: if the port's buffer is full the line is dropped rather
//! than stalling the engine. Network transports reconnect at most once a
//! second after a failure.

use crate::axis::Axis;
use crate::device::{AxisMove, Device};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The stop command.
pub const TCODE_STOP: &str = "DSTOP\n";

/// Default serial speed.
pub const DEFAULT_BAUD: u32 = 115_200;

/// Formats a position as a 4-digit TCode magnitude (`0.5` → `"5000"`,
/// `1.0` → `"9999"`).
pub fn tcode_magnitude(pos: f32) -> String {
    let p = if pos.is_finite() {
        pos.clamp(0.0, 1.0)
    } else {
        0.5
    };
    format!("{:04}", (p * 9999.0).round() as u32)
}

/// Formats moves as one TCode line, e.g. `"L09999I250 R05000I250\n"`. A
/// zero duration omits the interval (move at the device's default speed).
pub fn format_tcode(moves: &[AxisMove]) -> String {
    let mut line = String::with_capacity(moves.len() * 12 + 1);
    for (i, m) in moves.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(m.axis.tcode());
        line.push_str(&tcode_magnitude(m.pos));
        if m.duration_ms > 0 {
            line.push('I');
            line.push_str(&m.duration_ms.to_string());
        }
    }
    line.push('\n');
    line
}

/// Where a TCode device is reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TcodeEndpoint {
    /// A serial device such as `/dev/ttyACM0`.
    Serial {
        /// Device path.
        path: PathBuf,
        /// Baud rate (115200 for every common firmware).
        baud: u32,
    },
    /// TCP `host:port`.
    Tcp {
        /// `host:port`.
        addr: String,
    },
    /// UDP `host:port` (one datagram per command line).
    Udp {
        /// `host:port`.
        addr: String,
    },
}

impl TcodeEndpoint {
    /// Parses `tcp://host:port`, `udp://host:port`, or a serial device path
    /// (optionally `serial://` prefixed; 115200 baud).
    pub fn parse(s: &str) -> Result<TcodeEndpoint> {
        let s = s.trim();
        if let Some(addr) = s.strip_prefix("tcp://") {
            Ok(TcodeEndpoint::Tcp {
                addr: addr.trim_end_matches('/').to_owned(),
            })
        } else if let Some(addr) = s.strip_prefix("udp://") {
            Ok(TcodeEndpoint::Udp {
                addr: addr.trim_end_matches('/').to_owned(),
            })
        } else {
            let path = s.strip_prefix("serial://").unwrap_or(s);
            if path.starts_with('/') {
                Ok(TcodeEndpoint::Serial {
                    path: PathBuf::from(path),
                    baud: DEFAULT_BAUD,
                })
            } else {
                Err(Error::Config(format!(
                    "TCode endpoint {s:?} is not tcp://host:port, udp://host:port or a /dev path"
                )))
            }
        }
    }

    /// A short description (`/dev/ttyACM0`, `tcp://10.0.0.5:8000`).
    pub fn describe(&self) -> String {
        match self {
            TcodeEndpoint::Serial { path, .. } => path.display().to_string(),
            TcodeEndpoint::Tcp { addr } => format!("tcp://{addr}"),
            TcodeEndpoint::Udp { addr } => format!("udp://{addr}"),
        }
    }
}

/// Configuration of a [`TcodeDevice`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TcodeConfig {
    /// Where the device is.
    pub endpoint: TcodeEndpoint,
    /// Axes the device has. TCode cannot report them, so the user picks:
    /// OSR2 is L0 R0 R1 R2, SR6 adds L1 L2.
    pub axes: Vec<Axis>,
    /// Minimum time between command lines, milliseconds.
    pub min_interval_ms: u32,
    /// Display name; defaults to `TCode (<endpoint>)`.
    pub name: Option<String>,
}

impl Default for TcodeConfig {
    fn default() -> Self {
        TcodeConfig {
            endpoint: TcodeEndpoint::Serial {
                path: PathBuf::from("/dev/ttyACM0"),
                baud: DEFAULT_BAUD,
            },
            axes: vec![Axis::L0, Axis::L1, Axis::L2, Axis::R0, Axis::R1, Axis::R2],
            min_interval_ms: 10,
            name: None,
        }
    }
}

enum Conn {
    Serial(File),
    Tcp(TcpStream),
    Udp(UdpSocket),
}

impl Conn {
    fn open(endpoint: &TcodeEndpoint) -> Result<Conn> {
        match endpoint {
            TcodeEndpoint::Serial { path, baud } => Ok(Conn::Serial(open_serial(path, *baud)?)),
            TcodeEndpoint::Tcp { addr } => {
                let sa = resolve(addr)?;
                let s = TcpStream::connect_timeout(&sa, Duration::from_millis(1500))?;
                s.set_nodelay(true)?;
                s.set_write_timeout(Some(Duration::from_millis(200)))?;
                Ok(Conn::Tcp(s))
            }
            TcodeEndpoint::Udp { addr } => {
                let sa = resolve(addr)?;
                let bind: SocketAddr = if sa.is_ipv4() {
                    SocketAddr::from(([0, 0, 0, 0], 0))
                } else {
                    SocketAddr::from(([0u16; 8], 0))
                };
                let s = UdpSocket::bind(bind)?;
                s.connect(sa)?;
                Ok(Conn::Udp(s))
            }
        }
    }

    fn send(&mut self, line: &[u8]) -> io::Result<()> {
        match self {
            Conn::Serial(f) => match f.write_all(line) {
                // Buffer full: drop the line rather than block the engine.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
                other => other,
            },
            Conn::Tcp(s) => s.write_all(line),
            Conn::Udp(s) => s.send(line).map(|_| ()),
        }
    }
}

fn resolve(addr: &str) -> Result<SocketAddr> {
    addr.to_socket_addrs()
        .map_err(|e| Error::Config(format!("cannot resolve {addr:?}: {e}")))?
        .next()
        .ok_or_else(|| Error::Config(format!("{addr:?} resolves to no address")))
}

/// A TCode device on a serial port or network socket.
pub struct TcodeDevice {
    config: TcodeConfig,
    name: String,
    conn: Option<Conn>,
    last_attempt: Instant,
}

impl TcodeDevice {
    /// Opens the endpoint. Fails if it cannot be opened now; later failures
    /// mark the device disconnected and it reconnects on its own.
    pub fn connect(config: TcodeConfig) -> Result<TcodeDevice> {
        let conn = Conn::open(&config.endpoint)?;
        let name = config
            .name
            .clone()
            .unwrap_or_else(|| format!("TCode ({})", config.endpoint.describe()));
        Ok(TcodeDevice {
            config,
            name,
            conn: Some(conn),
            last_attempt: Instant::now(),
        })
    }

    /// The configuration the device was opened with.
    pub fn config(&self) -> &TcodeConfig {
        &self.config
    }

    /// Sends a raw command line (a trailing `\n` is added when missing).
    pub fn send_line(&mut self, line: &str) -> Result<()> {
        if self.conn.is_none() && self.last_attempt.elapsed() >= Duration::from_secs(1) {
            self.last_attempt = Instant::now();
            self.conn = Conn::open(&self.config.endpoint).ok();
        }
        let Some(conn) = self.conn.as_mut() else {
            return Err(Error::NotConnected(self.config.endpoint.describe()));
        };
        let mut bytes = line.as_bytes().to_vec();
        if !line.ends_with('\n') {
            bytes.push(b'\n');
        }
        conn.send(&bytes).map_err(|e| {
            self.conn = None;
            self.last_attempt = Instant::now();
            Error::Io(e)
        })
    }
}

impl Device for TcodeDevice {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn axes(&self) -> Vec<Axis> {
        self.config.axes.clone()
    }

    fn move_to(&mut self, axis: Axis, pos: f32, duration_ms: u32) -> Result<()> {
        self.move_axes(&[AxisMove {
            axis,
            pos,
            duration_ms,
        }])
    }

    fn move_axes(&mut self, moves: &[AxisMove]) -> Result<()> {
        if moves.is_empty() {
            return Ok(());
        }
        let line = format_tcode(moves);
        self.send_line(&line)
    }

    fn stop(&mut self) -> Result<()> {
        self.send_line(TCODE_STOP)
    }

    fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    fn min_interval_ms(&self) -> u32 {
        self.config.min_interval_ms
    }
}

fn baud_constant(baud: u32) -> Result<libc::speed_t> {
    Ok(match baud {
        9600 => libc::B9600,
        19_200 => libc::B19200,
        38_400 => libc::B38400,
        57_600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        921_600 => libc::B921600,
        other => return Err(Error::Unsupported(format!("baud rate {other}"))),
    })
}

/// Opens a serial port for TCode: raw mode, 8 data bits, no parity, one
/// stop bit, no flow control, ignore modem lines, non-blocking.
pub fn open_serial(path: &Path, baud: u32) -> Result<File> {
    let speed = baud_constant(baud)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is an open descriptor owned by `file` for the duration of
    // these calls, and `tio` is a plain C struct fully initialised by
    // tcgetattr before use.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        libc::cfmakeraw(&mut tio);
        tio.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE | libc::CRTSCTS);
        tio.c_cflag |= libc::CS8 | libc::CLOCAL | libc::CREAD;
        tio.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
        tio.c_cc[libc::VMIN] = 0;
        tio.c_cc[libc::VTIME] = 0;
        if libc::cfsetispeed(&mut tio, speed) != 0 || libc::cfsetospeed(&mut tio, speed) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if libc::tcsetattr(fd, libc::TCSANOW, &tio) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
    }
    Ok(file)
}

/// A serial port that may have a TCode device on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialPortInfo {
    /// Device path, e.g. `/dev/ttyACM0`.
    pub path: PathBuf,
    /// The `/dev/serial/by-id` name when there is one (usually names the
    /// board, e.g. `usb-Espressif_ESP32...`).
    pub description: Option<String>,
}

/// Lists `/dev/ttyACM*` and `/dev/ttyUSB*`, sorted, with their
/// `/dev/serial/by-id` names.
pub fn list_serial_ports() -> Vec<SerialPortInfo> {
    list_serial_ports_in(Path::new("/dev"))
}

fn list_serial_ports_in(dev: &Path) -> Vec<SerialPortInfo> {
    let by_id: Vec<(PathBuf, String)> = std::fs::read_dir(dev.join("serial/by-id"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let target = std::fs::canonicalize(e.path()).ok()?;
                    Some((target, e.file_name().to_string_lossy().into_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut ports: Vec<SerialPortInfo> = std::fs::read_dir(dev)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.starts_with("ttyACM") || n.starts_with("ttyUSB")
                })
                .map(|e| {
                    let path = e.path();
                    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    let description = by_id
                        .iter()
                        .find(|(t, _)| *t == canonical)
                        .map(|(_, n)| n.clone());
                    SerialPortInfo { path, description }
                })
                .collect()
        })
        .unwrap_or_default();
    ports.sort_by(|a, b| a.path.cmp(&b.path));
    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;

    fn m(axis: Axis, pos: f32, duration_ms: u32) -> AxisMove {
        AxisMove {
            axis,
            pos,
            duration_ms,
        }
    }

    #[test]
    fn formats_commands() {
        assert_eq!(tcode_magnitude(1.0), "9999");
        assert_eq!(tcode_magnitude(0.0), "0000");
        assert_eq!(tcode_magnitude(0.5), "5000");
        assert_eq!(tcode_magnitude(0.0123), "0123");
        assert_eq!(tcode_magnitude(7.0), "9999");
        assert_eq!(tcode_magnitude(f32::NAN), "5000");
        assert_eq!(format_tcode(&[m(Axis::L0, 1.0, 250)]), "L09999I250\n");
        assert_eq!(
            format_tcode(&[
                m(Axis::L0, 0.25, 100),
                m(Axis::R1, 0.5, 100),
                m(Axis::V0, 0.0, 0)
            ]),
            "L02500I100 R15000I100 V00000\n"
        );
    }

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            TcodeEndpoint::parse("tcp://10.0.0.5:8000").unwrap(),
            TcodeEndpoint::Tcp {
                addr: "10.0.0.5:8000".into()
            }
        );
        assert_eq!(
            TcodeEndpoint::parse("udp://osr.local:8000/").unwrap(),
            TcodeEndpoint::Udp {
                addr: "osr.local:8000".into()
            }
        );
        assert_eq!(
            TcodeEndpoint::parse("/dev/ttyUSB1").unwrap(),
            TcodeEndpoint::Serial {
                path: "/dev/ttyUSB1".into(),
                baud: DEFAULT_BAUD
            }
        );
        assert!(TcodeEndpoint::parse("COM3").is_err());
        let cfg = TcodeConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<TcodeConfig>(&json).unwrap(), cfg);
    }

    #[test]
    fn tcp_transport_sends_lines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut lines = Vec::new();
            for line in BufReader::new(s).lines().take(2) {
                lines.push(line.unwrap());
            }
            lines
        });
        let mut dev = TcodeDevice::connect(TcodeConfig {
            endpoint: TcodeEndpoint::parse(&format!("tcp://{addr}")).unwrap(),
            axes: vec![Axis::L0, Axis::R0],
            ..Default::default()
        })
        .unwrap();
        assert!(dev.is_connected());
        assert_eq!(dev.name(), format!("TCode (tcp://{addr})"));
        dev.move_axes(&[m(Axis::L0, 1.0, 250), m(Axis::R0, 0.5, 250)])
            .unwrap();
        dev.stop().unwrap();
        assert_eq!(
            reader.join().unwrap(),
            vec!["L09999I250 R05000I250", "DSTOP"]
        );
    }

    #[test]
    fn udp_transport_sends_datagrams() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let addr = sock.local_addr().unwrap();
        let mut dev = TcodeDevice::connect(TcodeConfig {
            endpoint: TcodeEndpoint::Udp {
                addr: addr.to_string(),
            },
            name: Some("SR6".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(dev.name(), "SR6");
        dev.move_to(Axis::L1, 0.0, 40).unwrap();
        let mut buf = [0u8; 64];
        let n = sock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"L10000I40\n");
        dev.stop().unwrap();
        let n = sock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"DSTOP\n");
    }

    #[test]
    fn tcp_disconnect_is_reported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut dev = TcodeDevice::connect(TcodeConfig {
            endpoint: TcodeEndpoint::Tcp {
                addr: addr.to_string(),
            },
            ..Default::default()
        })
        .unwrap();
        let (s, _) = listener.accept().unwrap();
        drop(s);
        drop(listener);
        // The peer is gone: writes fail within a few attempts (the first may
        // still be buffered by the kernel).
        let mut failed = false;
        for _ in 0..50 {
            if dev.stop().is_err() {
                failed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(failed);
        assert!(!dev.is_connected());
        assert!(matches!(dev.stop(), Err(Error::NotConnected(_))));
    }

    #[test]
    fn serial_over_pty() {
        // A pseudo-terminal stands in for /dev/ttyACM0.
        // SAFETY: plain libc calls on a descriptor we own; ptsname_r writes
        // into a buffer we provide with its length.
        let (master, slave_path) = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0);
            assert_eq!(libc::grantpt(fd), 0);
            assert_eq!(libc::unlockpt(fd), 0);
            let mut buf = [0 as libc::c_char; 128];
            assert_eq!(libc::ptsname_r(fd, buf.as_mut_ptr(), buf.len()), 0);
            let name = std::ffi::CStr::from_ptr(buf.as_ptr())
                .to_string_lossy()
                .into_owned();
            (
                <File as std::os::fd::FromRawFd>::from_raw_fd(fd),
                PathBuf::from(name),
            )
        };
        let port = open_serial(&slave_path, DEFAULT_BAUD).unwrap();
        // SAFETY: tcgetattr fills the zeroed struct for an open descriptor.
        let tio = unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(port.as_raw_fd(), &mut tio), 0);
            tio
        };
        assert_eq!(tio.c_cflag & libc::CSIZE, libc::CS8);
        assert_eq!(tio.c_cflag & libc::PARENB, 0);
        assert_eq!(tio.c_cflag & libc::CSTOPB, 0);
        assert_eq!(tio.c_lflag & libc::ICANON, 0);
        // SAFETY: reads the speed field of an initialised termios.
        assert_eq!(unsafe { libc::cfgetospeed(&tio) }, libc::B115200);
        drop(port);
        assert!(open_serial(&slave_path, 12_345).is_err());

        let mut dev = TcodeDevice::connect(TcodeConfig {
            endpoint: TcodeEndpoint::Serial {
                path: slave_path,
                baud: DEFAULT_BAUD,
            },
            axes: vec![Axis::L0],
            ..Default::default()
        })
        .unwrap();
        dev.move_to(Axis::L0, 0.5, 100).unwrap();
        let mut got = Vec::new();
        let mut master = master;
        let mut buf = [0u8; 64];
        while !got.ends_with(b"\n") {
            let n = master.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"L05000I100\n");
    }

    #[test]
    fn enumerates_serial_ports() {
        let root = std::env::temp_dir().join(format!("fp-haptics-dev-{}", std::process::id()));
        // A stale directory from an earlier run with the same pid would make
        // the symlink below fail.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("serial/by-id")).unwrap();
        for n in ["ttyACM0", "ttyUSB0", "ttyS0", "null"] {
            std::fs::write(root.join(n), b"").unwrap();
        }
        std::os::unix::fs::symlink(
            root.join("ttyACM0"),
            root.join("serial/by-id/usb-Espressif_OSR2-if00"),
        )
        .unwrap();
        let ports = list_serial_ports_in(&root);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].path, root.join("ttyACM0"));
        assert_eq!(
            ports[0].description.as_deref(),
            Some("usb-Espressif_OSR2-if00")
        );
        assert_eq!(ports[1].path, root.join("ttyUSB0"));
        assert_eq!(ports[1].description, None);
        std::fs::remove_dir_all(&root).ok();
        // The real /dev listing must not fail even when empty.
        let _ = list_serial_ports();
    }
}
