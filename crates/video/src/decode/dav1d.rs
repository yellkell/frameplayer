//! dav1d software AV1 decoder (feature `dav1d`).

use super::{CpuFrame, DecodedFrame, DecoderPath, PixelFormat, VideoDecoder};
use crate::error::{Result, VideoError};
use crate::packet::Packet;
use dav1d::{PixelLayout, PlanarImageComponent};
use fp_core::MediaTime;

pub struct Dav1dDecoder {
    dec: dav1d::Decoder,
    pending: bool,
    draining: bool,
    drained: bool,
}

fn err(e: dav1d::Error) -> VideoError {
    VideoError::Device(format!("dav1d: {e}"))
}

impl Dav1dDecoder {
    pub fn new(threads: usize) -> Result<Self> {
        let mut s = dav1d::Settings::new();
        s.set_n_threads(threads as u32);
        let dec = dav1d::Decoder::with_settings(&s).map_err(err)?;
        Ok(Dav1dDecoder {
            dec,
            pending: false,
            draining: false,
            drained: false,
        })
    }

    fn flush_pending(&mut self) -> Result<bool> {
        if !self.pending {
            return Ok(true);
        }
        match self.dec.send_pending_data() {
            Ok(()) => {
                self.pending = false;
                Ok(true)
            }
            Err(dav1d::Error::Again) => Ok(false),
            Err(e) => Err(err(e)),
        }
    }

    fn to_cpu(p: &dav1d::Picture) -> Result<CpuFrame> {
        if p.pixel_layout() != PixelLayout::I420 {
            return Err(VideoError::Unsupported(format!(
                "dav1d layout {:?}",
                p.pixel_layout()
            )));
        }
        let format = if p.bit_depth() > 8 {
            PixelFormat::I420P10
        } else {
            PixelFormat::I420
        };
        let mut planes = Vec::with_capacity(3);
        let mut strides = Vec::with_capacity(3);
        for c in [
            PlanarImageComponent::Y,
            PlanarImageComponent::U,
            PlanarImageComponent::V,
        ] {
            let plane = p.plane(c);
            planes.push(plane.as_ref().to_vec());
            strides.push(p.stride(c) as usize);
        }
        Ok(CpuFrame {
            format,
            width: p.width(),
            height: p.height(),
            planes,
            strides,
            pts: MediaTime(p.timestamp().unwrap_or(0)),
        })
    }
}

impl VideoDecoder for Dav1dDecoder {
    fn path(&self) -> DecoderPath {
        DecoderPath::Software {
            library: "dav1d".into(),
        }
    }

    fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
        if !self.flush_pending()? {
            return Ok(false);
        }
        match self.dec.send_data(
            pkt.data.clone(),
            None,
            Some(pkt.pts.0),
            Some(pkt.duration.0),
        ) {
            Ok(()) => {}
            // dav1d keeps the data; it is pushed on the next call.
            Err(dav1d::Error::Again) => self.pending = true,
            Err(e) => return Err(err(e)),
        }
        self.drained = false;
        self.draining = false;
        Ok(true)
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        self.flush_pending()?;
        match self.dec.get_picture() {
            Ok(p) => Ok(Some(DecodedFrame::Cpu(Self::to_cpu(&p)?))),
            Err(dav1d::Error::Again) => {
                if self.draining && !self.pending {
                    self.drained = true;
                }
                Ok(None)
            }
            Err(e) => Err(err(e)),
        }
    }

    fn drain(&mut self) -> Result<()> {
        self.draining = true;
        Ok(())
    }

    fn is_drained(&self) -> bool {
        self.drained
    }

    fn flush(&mut self) -> Result<()> {
        self.dec.flush();
        self.pending = false;
        self.draining = false;
        self.drained = false;
        Ok(())
    }
}
