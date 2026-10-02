//! TCode output for OSR2 / SR6 style devices (TCode v0.3).
//!
//! A command is the channel name, a magnitude written as the fractional digits of a 0–1 value
//! (`L05000` = 0.5000, `L0500` = 0.500), and an optional interval `I<ms>` or speed `S<n>`.
//! Several commands go on one line separated by spaces and the line ends with `\n`, which is
//! when the device executes them together: `L05000I100 R02500I100\n`. `DSTOP` halts all motion.
//!
//! Transports: TCP (Wi-Fi firmware such as the ESP32 OSR TCode server), UDP (one datagram per
//! line, the MultiFunPlayer convention) and, behind the `serial` feature, USB serial.

use crate::device::{AxisTarget, DeviceInfo, HapticDevice, StreamStyle};
use crate::funscript::Axis;
use crate::{HapticsError, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;

/// Per-axis hardware limits, as fractions of full travel (0..=1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisLimits {
    pub min: f64,
    pub max: f64,
}

impl Default for AxisLimits {
    fn default() -> Self {
        AxisLimits { min: 0.0, max: 1.0 }
    }
}

/// Formatting settings.
#[derive(Debug, Clone, PartialEq)]
pub struct TCodeFormat {
    /// Magnitude digits: 4 gives 0–9999 (the common OSR setting), 3 gives 0–999.
    pub precision: u8,
    pub limits: BTreeMap<Axis, AxisLimits>,
}

impl Default for TCodeFormat {
    fn default() -> Self {
        TCodeFormat {
            precision: 4,
            limits: BTreeMap::new(),
        }
    }
}

impl TCodeFormat {
    /// Scale `value` (0..=1) into the axis limits and the magnitude range.
    pub fn magnitude(&self, axis: Axis, value: f64) -> u32 {
        let lim = self.limits.get(&axis).copied().unwrap_or_default();
        let v = value.clamp(0.0, 1.0);
        let v = lim.min.clamp(0.0, 1.0) + v * (lim.max.clamp(0.0, 1.0) - lim.min.clamp(0.0, 1.0));
        let max = 10u32.pow(self.precision.clamp(1, 9) as u32) - 1;
        (v * max as f64).round().clamp(0.0, max as f64) as u32
    }

    /// One command, e.g. `L05000I100`.
    pub fn command(&self, axis: Axis, value: f64, interval_ms: Option<u32>) -> String {
        let mut s = String::with_capacity(12);
        let digits = self.precision.clamp(1, 9) as usize;
        let _ = write!(
            s,
            "{}{:0digits$}",
            axis.tcode(),
            self.magnitude(axis, value)
        );
        if let Some(i) = interval_ms.filter(|i| *i > 0) {
            let _ = write!(s, "I{i}");
        }
        s
    }

    /// A full line for several axes, newline-terminated.
    pub fn line(&self, targets: &[AxisTarget]) -> String {
        let mut s = targets
            .iter()
            .map(|t| self.command(t.axis, t.position, Some(t.duration_ms)))
            .collect::<Vec<_>>()
            .join(" ");
        s.push('\n');
        s
    }

    /// Line moving every listed axis to its rest position (positions 0.5, intensities 0).
    pub fn home_line(&self, axes: &[Axis], interval_ms: u32) -> String {
        let targets: Vec<AxisTarget> = axes
            .iter()
            .map(|&axis| AxisTarget {
                axis,
                position: if axis.is_position() { 0.5 } else { 0.0 },
                duration_ms: interval_ms,
            })
            .collect();
        self.line(&targets)
    }
}

/// Stop all motion.
pub const STOP: &str = "DSTOP\n";
/// Ask the firmware for its TCode version.
pub const QUERY_VERSION: &str = "D1\n";

/// Where TCode lines go.
#[derive(Debug, Clone, PartialEq)]
pub enum TCodeTransport {
    Tcp(SocketAddr),
    Udp(SocketAddr),
    /// Serial port path and baud rate (requires the `serial` feature). OSR firmware uses 115200.
    Serial {
        path: String,
        baud: u32,
    },
}

enum Link {
    Tcp(tokio::net::TcpStream),
    Udp(tokio::net::UdpSocket),
    #[cfg(feature = "serial")]
    Serial(std::sync::mpsc::Sender<Vec<u8>>),
}

impl Link {
    async fn open(t: &TCodeTransport) -> Result<Link> {
        match t {
            TCodeTransport::Tcp(addr) => {
                let s = tokio::net::TcpStream::connect(addr).await?;
                s.set_nodelay(true)?;
                Ok(Link::Tcp(s))
            }
            TCodeTransport::Udp(addr) => {
                let bind: SocketAddr = if addr.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                }
                .parse()
                .expect("static");
                let s = tokio::net::UdpSocket::bind(bind).await?;
                s.connect(addr).await?;
                Ok(Link::Udp(s))
            }
            #[cfg(feature = "serial")]
            TCodeTransport::Serial { path, baud } => {
                // serialport is blocking; a dedicated writer thread keeps it off the runtime.
                let mut port = serialport::new(path, *baud)
                    .timeout(std::time::Duration::from_millis(200))
                    .open()
                    .map_err(|e| HapticsError::Device(format!("serial {path}: {e}")))?;
                let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
                std::thread::Builder::new()
                    .name("tcode-serial".into())
                    .spawn(move || {
                        use std::io::Write;
                        while let Ok(buf) = rx.recv() {
                            if port.write_all(&buf).and_then(|_| port.flush()).is_err() {
                                break;
                            }
                        }
                    })?;
                Ok(Link::Serial(tx))
            }
            #[cfg(not(feature = "serial"))]
            TCodeTransport::Serial { .. } => Err(HapticsError::Unsupported(
                "serial TCode requires building with the `serial` feature".into(),
            )),
        }
    }

    async fn write(&mut self, line: &str) -> Result<()> {
        match self {
            Link::Tcp(s) => s.write_all(line.as_bytes()).await.map_err(Into::into),
            Link::Udp(s) => s.send(line.as_bytes()).await.map(drop).map_err(Into::into),
            #[cfg(feature = "serial")]
            Link::Serial(tx) => tx
                .send(line.as_bytes().to_vec())
                .map_err(|_| HapticsError::NotConnected),
        }
    }
}

/// Settings for [`TCodeDevice`].
#[derive(Debug, Clone, PartialEq)]
pub struct TCodeConfig {
    pub transport: TCodeTransport,
    pub format: TCodeFormat,
    /// Axes the device has (OSR2: L0 R1 R2; SR6: L0 L1 L2 R0 R1 R2).
    pub axes: Vec<Axis>,
    /// Stream interval; also used as the `I` value of every command.
    pub update_interval_ms: u32,
}

impl TCodeConfig {
    pub fn osr2(transport: TCodeTransport) -> Self {
        TCodeConfig {
            transport,
            format: TCodeFormat::default(),
            axes: vec![Axis::L0, Axis::R1, Axis::R2],
            update_interval_ms: 20,
        }
    }

    pub fn sr6(transport: TCodeTransport) -> Self {
        TCodeConfig {
            axes: vec![Axis::L0, Axis::L1, Axis::L2, Axis::R0, Axis::R1, Axis::R2],
            ..TCodeConfig::osr2(transport)
        }
    }
}

/// [`HapticDevice`] that streams interpolated multi-axis TCode lines.
pub struct TCodeDevice {
    cfg: TCodeConfig,
    link: Option<Link>,
}

impl TCodeDevice {
    pub fn new(cfg: TCodeConfig) -> Self {
        TCodeDevice { cfg, link: None }
    }

    /// Write a raw line (must end in `\n`).
    pub async fn write_line(&mut self, line: &str) -> Result<()> {
        let link = self.link.as_mut().ok_or(HapticsError::NotConnected)?;
        link.write(line).await
    }
}

#[async_trait]
impl HapticDevice for TCodeDevice {
    fn info(&self) -> DeviceInfo {
        let name = match &self.cfg.transport {
            TCodeTransport::Tcp(a) => format!("TCode tcp://{a}"),
            TCodeTransport::Udp(a) => format!("TCode udp://{a}"),
            TCodeTransport::Serial { path, .. } => format!("TCode {path}"),
        };
        DeviceInfo {
            name,
            axes: self
                .cfg
                .axes
                .iter()
                .map(|&a| (a, StreamStyle::Interpolated))
                .collect(),
            script_sync: false,
            script_sync_any_speed: false,
            latency_ms: 0,
            update_interval_ms: self.cfg.update_interval_ms.max(1),
        }
    }

    async fn connect(&mut self) -> Result<()> {
        let mut link = Link::open(&self.cfg.transport).await?;
        link.write(&self.cfg.format.home_line(&self.cfg.axes, 500))
            .await?;
        self.link = Some(link);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        if self.link.is_some() {
            let _ = self.write_line(STOP).await;
        }
        self.link = None;
        Ok(())
    }

    async fn send(&mut self, targets: &[AxisTarget]) -> Result<()> {
        let targets: Vec<AxisTarget> = targets
            .iter()
            .filter(|t| self.cfg.axes.contains(&t.axis))
            .copied()
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        let line = self.cfg.format.line(&targets);
        self.write_line(&line).await
    }

    async fn stop(&mut self) -> Result<()> {
        self.write_line(STOP).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[test]
    fn command_strings() {
        let f = TCodeFormat::default();
        assert_eq!(f.command(Axis::L0, 0.5, Some(100)), "L05000I100");
        assert_eq!(f.command(Axis::L0, 1.0, None), "L09999");
        assert_eq!(f.command(Axis::R2, 0.0, Some(0)), "R20000");
        assert_eq!(f.command(Axis::V0, 1.7, Some(20)), "V09999I20", "clamped");
        let f3 = TCodeFormat {
            precision: 3,
            ..Default::default()
        };
        assert_eq!(f3.command(Axis::L0, 0.5005, Some(100)), "L0500I100");
        assert_eq!(f3.command(Axis::L0, 0.0005, Some(100)), "L0000I100");
    }

    #[test]
    fn limits_and_lines() {
        let mut f = TCodeFormat::default();
        f.limits.insert(Axis::L0, AxisLimits { min: 0.2, max: 0.8 });
        assert_eq!(f.magnitude(Axis::L0, 0.0), 2000);
        assert_eq!(f.magnitude(Axis::L0, 1.0), 7999);
        assert_eq!(f.magnitude(Axis::L0, 0.5), 5000);
        let line = f.line(&[
            AxisTarget {
                axis: Axis::L0,
                position: 1.0,
                duration_ms: 50,
            },
            AxisTarget {
                axis: Axis::R1,
                position: 0.25,
                duration_ms: 50,
            },
        ]);
        assert_eq!(line, "L07999I50 R12500I50\n");
        assert_eq!(
            f.home_line(&[Axis::R0, Axis::V0], 500),
            "R05000I500 V00000I500\n"
        );
    }

    #[tokio::test]
    async fn tcp_transport_writes_lines() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(s).lines();
            let mut got = Vec::new();
            while let Ok(Some(l)) = lines.next_line().await {
                got.push(l);
            }
            got
        });
        let mut dev = TCodeDevice::new(TCodeConfig::osr2(TCodeTransport::Tcp(addr)));
        dev.connect().await.unwrap();
        dev.send(&[
            AxisTarget {
                axis: Axis::L0,
                position: 0.25,
                duration_ms: 20,
            },
            AxisTarget {
                axis: Axis::L1,
                position: 0.9,
                duration_ms: 20,
            }, // not an OSR2 axis
        ])
        .await
        .unwrap();
        dev.disconnect().await.unwrap();
        let got = server.await.unwrap();
        assert_eq!(
            got,
            vec!["L05000I500 R15000I500 R25000I500", "L02500I20", "DSTOP"]
        );
    }

    #[tokio::test]
    async fn udp_transport_sends_datagrams() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        let mut dev = TCodeDevice::new(TCodeConfig {
            axes: vec![Axis::L0],
            ..TCodeConfig::osr2(TCodeTransport::Udp(addr))
        });
        dev.connect().await.unwrap();
        dev.send(&[AxisTarget {
            axis: Axis::L0,
            position: 1.0,
            duration_ms: 20,
        }])
        .await
        .unwrap();
        let mut buf = [0u8; 64];
        let n = sock.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"L05000I500\n");
        let n = sock.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"L09999I20\n");
    }

    #[cfg(not(feature = "serial"))]
    #[tokio::test]
    async fn serial_requires_feature() {
        let mut dev = TCodeDevice::new(TCodeConfig::osr2(TCodeTransport::Serial {
            path: "/dev/ttyUSB0".into(),
            baud: 115200,
        }));
        assert!(matches!(
            dev.connect().await,
            Err(HapticsError::Unsupported(_))
        ));
    }
}
