//! PGS (Blu-ray Presentation Graphic Stream) decoding to RGBA bitmaps.
//!
//! A display set is `PCS` (composition: canvas size, object placements),
//! `WDS` (windows), `PDS` (YCrCb+A palette), `ODS` (RLE bitmap, possibly
//! split over several segments) and `END`. Matroska packets contain bare
//! segments; `.sup` files prefix each with `"PG"` + pts + dts.

use super::{Cue, CueContent, SubtitleBitmap, SubtitleTrack, SubtitleUpdate, OPEN_END};
use crate::bytes::Bytes;
use crate::error::{Result, VideoError};
use fp_core::MediaTime;
use std::collections::HashMap;

const PDS: u8 = 0x14;
const ODS: u8 = 0x15;
const PCS: u8 = 0x16;
const WDS: u8 = 0x17;
const END: u8 = 0x80;

#[derive(Debug, Clone, Copy)]
struct Placement {
    object_id: u16,
    x: u16,
    y: u16,
    crop: Option<(u16, u16, u16, u16)>,
}

#[derive(Debug, Clone, Default)]
struct Composition {
    width: u16,
    height: u16,
    palette_id: u8,
    objects: Vec<Placement>,
}

#[derive(Debug, Clone, Default)]
struct ObjectData {
    width: u16,
    height: u16,
    rle: Vec<u8>,
}

/// Stateful PGS decoder (palettes and objects persist across display sets).
#[derive(Debug, Default)]
pub struct PgsDecoder {
    palettes: HashMap<u8, [[u8; 4]; 256]>,
    objects: HashMap<u16, ObjectData>,
    pcs: Option<(Composition, MediaTime)>,
}

/// BT.709 limited-range YCbCr → RGB.
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let y = (y as f32 - 16.0) * 1.164;
    let cb = cb as f32 - 128.0;
    let cr = cr as f32 - 128.0;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    [
        q(y + 1.793 * cr),
        q(y - 0.213 * cb - 0.533 * cr),
        q(y + 2.112 * cb),
    ]
}

/// Decode PGS run-length data into palette indices.
pub fn decode_rle(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height];
    let (mut x, mut y, mut i) = (0usize, 0usize, 0usize);
    let mut put = |x: &mut usize, y: usize, color: u8, n: usize| {
        for _ in 0..n {
            if *x < width && y < height {
                out[y * width + *x] = color;
            }
            *x += 1;
        }
    };
    while i < data.len() && y < height {
        let b = data[i];
        i += 1;
        if b != 0 {
            put(&mut x, y, b, 1);
            continue;
        }
        let Some(&f) = data.get(i) else { break };
        i += 1;
        if f == 0 {
            x = 0;
            y += 1;
            continue;
        }
        let long = f & 0x40 != 0;
        let colored = f & 0x80 != 0;
        let mut n = (f & 0x3f) as usize;
        if long {
            n = (n << 8) | *data.get(i).unwrap_or(&0) as usize;
            i += 1;
        }
        let color = if colored {
            let c = *data.get(i).unwrap_or(&0);
            i += 1;
            c
        } else {
            0
        };
        put(&mut x, y, color, n);
    }
    out
}

impl PgsDecoder {
    fn handle(
        &mut self,
        kind: u8,
        body: &[u8],
        pts: MediaTime,
        out: &mut Vec<SubtitleUpdate>,
    ) -> Result<()> {
        let mut b = Bytes::new(body);
        match kind {
            PCS => {
                let width = b.u16()?;
                let height = b.u16()?;
                b.skip(1)?; // frame rate
                b.skip(2)?; // composition number
                let state = b.u8()?;
                b.skip(1)?; // palette update flag
                let palette_id = b.u8()?;
                let n = b.u8()?;
                if state & 0x80 != 0 {
                    // Epoch start: forget cached objects.
                    self.objects.clear();
                }
                let mut objects = Vec::new();
                for _ in 0..n {
                    let object_id = b.u16()?;
                    b.skip(1)?;
                    let flags = b.u8()?;
                    let x = b.u16()?;
                    let y = b.u16()?;
                    let crop = if flags & 0x80 != 0 {
                        Some((b.u16()?, b.u16()?, b.u16()?, b.u16()?))
                    } else {
                        None
                    };
                    objects.push(Placement {
                        object_id,
                        x,
                        y,
                        crop,
                    });
                }
                self.pcs = Some((
                    Composition {
                        width,
                        height,
                        palette_id,
                        objects,
                    },
                    pts,
                ));
            }
            PDS => {
                let id = b.u8()?;
                b.skip(1)?;
                let pal = self.palettes.entry(id).or_insert([[0; 4]; 256]);
                while b.remaining() >= 5 {
                    let idx = b.u8()?;
                    let (y, cr, cb, a) = (b.u8()?, b.u8()?, b.u8()?, b.u8()?);
                    let rgb = ycbcr_to_rgb(y, cb, cr);
                    pal[idx as usize] = [rgb[0], rgb[1], rgb[2], a];
                }
            }
            ODS => {
                let id = b.u16()?;
                b.skip(1)?; // version
                let seq = b.u8()?;
                if seq & 0x80 != 0 {
                    let _len = b.u24()?;
                    let width = b.u16()?;
                    let height = b.u16()?;
                    self.objects.insert(
                        id,
                        ObjectData {
                            width,
                            height,
                            rle: b.rest().to_vec(),
                        },
                    );
                } else if let Some(o) = self.objects.get_mut(&id) {
                    o.rle.extend_from_slice(b.rest());
                }
            }
            WDS => {}
            END => {
                let Some((comp, at)) = self.pcs.take() else {
                    return Ok(());
                };
                // Any new display set ends what was on screen.
                out.push(SubtitleUpdate::CloseOpen(at));
                let palette = self
                    .palettes
                    .get(&comp.palette_id)
                    .copied()
                    .unwrap_or([[0; 4]; 256]);
                for p in &comp.objects {
                    let Some(obj) = self.objects.get(&p.object_id) else {
                        continue;
                    };
                    let (w, h) = (obj.width as usize, obj.height as usize);
                    if w == 0 || h == 0 {
                        continue;
                    }
                    let idx = decode_rle(&obj.rle, w, h);
                    let (cx, cy, cw, ch) = p.crop.map_or((0, 0, w, h), |(x, y, cw, ch)| {
                        (
                            x as usize,
                            y as usize,
                            (cw as usize).min(w),
                            (ch as usize).min(h),
                        )
                    });
                    let mut rgba = Vec::with_capacity(cw * ch * 4);
                    for yy in cy..(cy + ch).min(h) {
                        for xx in cx..(cx + cw).min(w) {
                            rgba.extend_from_slice(&palette[idx[yy * w + xx] as usize]);
                        }
                    }
                    let bw = (cx + cw).min(w) - cx;
                    let bh = (cy + ch).min(h) - cy;
                    out.push(SubtitleUpdate::Add(Cue {
                        start: at,
                        end: OPEN_END,
                        content: CueContent::Bitmap(SubtitleBitmap {
                            x: p.x as u32,
                            y: p.y as u32,
                            width: bw as u32,
                            height: bh as u32,
                            rgba,
                            canvas_width: comp.width as u32,
                            canvas_height: comp.height as u32,
                        }),
                    }));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Decode a Matroska PGS packet (bare segments) at `pts`.
    pub fn decode_segments(&mut self, data: &[u8], pts: MediaTime) -> Vec<SubtitleUpdate> {
        let mut out = Vec::new();
        let mut p = 0;
        while p + 3 <= data.len() {
            let kind = data[p];
            let len = u16::from_be_bytes([data[p + 1], data[p + 2]]) as usize;
            let body = &data[p + 3..(p + 3 + len).min(data.len())];
            if let Err(e) = self.handle(kind, body, pts, &mut out) {
                tracing::debug!("bad PGS segment 0x{kind:02x}: {e}");
            }
            p += 3 + len;
        }
        out
    }
}

/// Parse a `.sup` file.
pub fn parse_sup(data: &[u8]) -> Result<SubtitleTrack> {
    let mut dec = PgsDecoder::default();
    let mut track = SubtitleTrack::default();
    let mut p = 0;
    let mut out = Vec::new();
    while p + 13 <= data.len() {
        if &data[p..p + 2] != b"PG" {
            return Err(VideoError::invalid(format!("bad PGS sync at {p}")));
        }
        let pts90 = u32::from_be_bytes(data[p + 2..p + 6].try_into().unwrap());
        let kind = data[p + 10];
        let len = u16::from_be_bytes([data[p + 11], data[p + 12]]) as usize;
        let body = &data[p + 13..(p + 13 + len).min(data.len())];
        let pts = MediaTime::from_timebase(pts90 as i64, 1, 90_000);
        dec.handle(kind, body, pts, &mut out)?;
        for u in out.drain(..) {
            track.apply(u);
        }
        p += 13 + len;
    }
    Ok(track)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    fn display_set(objects: bool) -> Vec<u8> {
        let mut pcs = vec![0x07, 0x80, 0x04, 0x38, 0x10, 0, 1, 0x80, 0, 0];
        if objects {
            pcs.push(1);
            pcs.extend_from_slice(&[0, 1, 0, 0, 0, 100, 0, 200]);
        } else {
            pcs.push(0);
        }
        let pds = [0u8, 0, 1, 235, 128, 128, 255, 2, 16, 128, 128, 128];
        // 4×2 object: row 0 = 1,1,1,1 (run), row 1 = 2,0,0,2.
        let rle = [
            0x00, 0x84, 0x01, 0x00, 0x00, 0x02, 0x00, 0x02, 0x02, 0x00, 0x00,
        ];
        let mut ods = vec![0, 1, 0, 0xc0];
        ods.extend_from_slice(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
        ods.extend_from_slice(&[0, 4, 0, 2]);
        ods.extend_from_slice(&rle);
        let mut v = seg(PCS, &pcs);
        if objects {
            v.extend(seg(WDS, &[1, 0, 0, 100, 0, 200, 0, 4, 0, 2]));
            v.extend(seg(PDS, &pds));
            v.extend(seg(ODS, &ods));
        }
        v.extend(seg(END, &[]));
        v
    }

    #[test]
    fn rle_decoding() {
        let rle = [
            0x00, 0x84, 0x01, 0x00, 0x00, 0x02, 0x00, 0x02, 0x02, 0x00, 0x00,
        ];
        assert_eq!(decode_rle(&rle, 4, 2), vec![1, 1, 1, 1, 2, 0, 0, 2]);
        // Long run of colour 0: 0x00, 0x41, 0x00 → 256 pixels.
        let px = decode_rle(&[0x00, 0x41, 0x00, 0x00, 0x00], 300, 1);
        assert!(px.iter().all(|&p| p == 0));
    }

    #[test]
    fn display_sets_to_cues() {
        let mut d = PgsDecoder::default();
        let mut track = SubtitleTrack::default();
        for u in d.decode_segments(&display_set(true), MediaTime::from_millis(1000)) {
            track.apply(u);
        }
        for u in d.decode_segments(&display_set(false), MediaTime::from_millis(3000)) {
            track.apply(u);
        }
        assert_eq!(track.len(), 1);
        let c = &track.cues()[0];
        assert_eq!(
            (c.start, c.end),
            (MediaTime::from_millis(1000), MediaTime::from_millis(3000))
        );
        let CueContent::Bitmap(b) = &c.content else {
            panic!()
        };
        assert_eq!(
            (b.x, b.y, b.width, b.height, b.canvas_width, b.canvas_height),
            (100, 200, 4, 2, 1920, 1080)
        );
        assert_eq!(
            &b.rgba[..4],
            &[255, 255, 255, 255],
            "palette 1 = opaque white"
        );
        assert_eq!(b.rgba[4 * 5 + 3], 0, "index 0 transparent");
        assert_eq!(&b.rgba[4 * 4..4 * 4 + 4], &[0, 0, 0, 128]);
        assert!(track.active_at(MediaTime::from_millis(2000)).len() == 1);
    }

    #[test]
    fn sup_file() {
        let mut file = Vec::new();
        for (set, pts90) in [
            (display_set(true), 90_000u32),
            (display_set(false), 270_000),
        ] {
            let mut p = 0;
            while p < set.len() {
                let len = u16::from_be_bytes([set[p + 1], set[p + 2]]) as usize;
                file.extend_from_slice(b"PG");
                file.extend_from_slice(&pts90.to_be_bytes());
                file.extend_from_slice(&0u32.to_be_bytes());
                file.extend_from_slice(&set[p..p + 3 + len]);
                p += 3 + len;
            }
        }
        let t = parse_sup(&file).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.cues()[0].end, MediaTime::from_secs_f64(3.0));
        assert!(parse_sup(b"XX0000000000000").is_err());
    }
}
