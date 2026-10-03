//! Minimal big-endian byte cursor for container parsing.

use crate::error::{Result, VideoError};

#[derive(Debug, Clone)]
pub struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bytes { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    pub fn pos(&self) -> usize {
        self.pos
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(VideoError::invalid(format!(
                "truncated: need {n} bytes, have {}",
                self.remaining()
            )));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    pub fn fourcc(&mut self) -> Result<[u8; 4]> {
        Ok(self.take(4)?.try_into().unwrap())
    }
    /// NUL-terminated string (or rest of buffer).
    pub fn cstring(&mut self) -> String {
        let rest = self.rest();
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.pos += (end + 1).min(rest.len());
        s
    }
}

/// MSB-first bit reader (for codec headers / exp-Golomb).
pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit: 0 }
    }
    pub fn read_bit(&mut self) -> Result<u32> {
        let byte = self
            .data
            .get(self.bit / 8)
            .ok_or_else(|| VideoError::invalid("bitstream truncated"))?;
        let v = (byte >> (7 - (self.bit % 8))) & 1;
        self.bit += 1;
        Ok(v as u32)
    }
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Ok(v)
    }
    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        if (self.bit + n).div_ceil(8) > self.data.len() {
            return Err(VideoError::invalid("bitstream truncated"));
        }
        self.bit += n;
        Ok(())
    }
    /// Unsigned exp-Golomb.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(VideoError::invalid("bad exp-golomb"));
            }
        }
        Ok(((1u64 << zeros) - 1 + self.read_bits(zeros)? as u64) as u32)
    }
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 → ue: 0, 1, 2, 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        // 011 | 00100 → ue 2, 3 → se −1, +2
        let data = [0b0110_0100];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.se().unwrap(), 2);
    }

    #[test]
    fn cursor_reads() {
        let d = [0, 1, 2, 3, 4, 5, 6, 7, b'a', b'b', 0, 9];
        let mut b = Bytes::new(&d);
        assert_eq!(b.u16().unwrap(), 1);
        assert_eq!(b.u24().unwrap(), 0x020304);
        assert_eq!(b.u8().unwrap(), 5);
        b.skip(2).unwrap();
        assert_eq!(b.cstring(), "ab");
        assert_eq!(b.u8().unwrap(), 9);
        assert!(b.u8().is_err());
    }
}

/// One ISO-BMFF box inside an in-memory buffer.
#[derive(Debug, Clone, Copy)]
pub struct BoxRef<'a> {
    pub kind: [u8; 4],
    /// Payload after the (possibly 64-bit / uuid) header.
    pub payload: &'a [u8],
    /// For `uuid` boxes, the 16-byte extended type.
    pub uuid: Option<[u8; 16]>,
}

/// Iterate the boxes packed in `data`. Stops at the first malformed header.
pub fn iter_boxes(data: &[u8]) -> impl Iterator<Item = BoxRef<'_>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > data.len() {
            return None;
        }
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let mut hdr = 8usize;
        let size = match size32 {
            0 => (data.len() - pos) as u64,
            1 => {
                if pos + 16 > data.len() {
                    return None;
                }
                hdr = 16;
                u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap())
            }
            s => s,
        };
        let mut uuid = None;
        if &kind == b"uuid" {
            if pos + hdr + 16 > data.len() {
                return None;
            }
            uuid = Some(data[pos + hdr..pos + hdr + 16].try_into().unwrap());
            hdr += 16;
        }
        if size < hdr as u64 || pos as u64 + size > data.len() as u64 {
            return None;
        }
        let payload = &data[pos + hdr..pos + size as usize];
        pos += size as usize;
        Some(BoxRef {
            kind,
            payload,
            uuid,
        })
    })
}

/// First child box of the given type.
pub fn find_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    iter_boxes(data)
        .find(|b| &b.kind == kind)
        .map(|b| b.payload)
}
