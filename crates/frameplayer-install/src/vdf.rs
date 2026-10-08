//! Steam's binary VDF ("KeyValues binary") format, as used by
//! `userdata/<id>/config/shortcuts.vdf`.
//!
//! A document is a sequence of typed key/value pairs terminated by an end
//! marker. Each pair is one type byte, a NUL-terminated key, then the value:
//!
//! | byte   | value                                              |
//! |--------|----------------------------------------------------|
//! | `0x00` | nested map (pairs until `0x08`)                    |
//! | `0x01` | NUL-terminated string                              |
//! | `0x02` | 32-bit little-endian integer                       |
//! | `0x03` | 32-bit float (kept as raw bits)                    |
//! | `0x07` | 64-bit unsigned integer (rare; kept for round trip) |
//! | `0x08` | end of the current map                             |
//!
//! `shortcuts.vdf` itself only uses map, string and int32, but the other two
//! scalar types are read and written back unchanged so an unexpected field
//! never makes us corrupt the user's file. Any other type byte is an error:
//! we refuse to edit what we cannot reproduce exactly.

use thiserror::Error;

const T_MAP: u8 = 0x00;
const T_STRING: u8 = 0x01;
const T_INT32: u8 = 0x02;
const T_FLOAT32: u8 = 0x03;
const T_UINT64: u8 = 0x07;
const T_END: u8 = 0x08;

/// Nesting deeper than this is treated as a corrupt file.
const MAX_DEPTH: usize = 32;

/// A value in a binary VDF document.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Ordered key/value pairs (`0x00`).
    Map(Map),
    /// UTF-8 string (`0x01`).
    Str(String),
    /// Signed 32-bit integer (`0x02`).
    Int(i32),
    /// 32-bit float, stored as its raw bits (`0x03`).
    Float(u32),
    /// Unsigned 64-bit integer (`0x07`).
    U64(u64),
}

/// An ordered map. Key order is preserved so a parse/write round trip
/// reproduces the input byte for byte.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Map(pub Vec<(String, Value)>);

impl Map {
    /// An empty map.
    pub fn new() -> Self {
        Map(Vec::new())
    }

    /// The value for `key`, compared case-insensitively as Steam does.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// Mutable access to the value for `key` (case-insensitive).
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    }

    /// Sets `key` to `value`, replacing an existing entry in place (keeping
    /// its position and spelling) or appending a new one.
    pub fn set(&mut self, key: &str, value: Value) {
        match self.get_mut(key) {
            Some(v) => *v = value,
            None => self.0.push((key.to_string(), value)),
        }
    }

    /// The string value of `key`, if it is a string.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// The int32 value of `key`, if it is one.
    pub fn get_int(&self, key: &str) -> Option<i32> {
        match self.get(key) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }

    /// The nested map at `key`, if it is one.
    pub fn get_map(&self, key: &str) -> Option<&Map> {
        match self.get(key) {
            Some(Value::Map(m)) => Some(m),
            _ => None,
        }
    }

    /// Mutable nested map at `key`, if it is one.
    pub fn get_map_mut(&mut self, key: &str) -> Option<&mut Map> {
        match self.get_mut(key) {
            Some(Value::Map(m)) => Some(m),
            _ => None,
        }
    }
}

/// Why a binary VDF document could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VdfError {
    /// The data ended in the middle of a value.
    #[error("file ends unexpectedly at byte {0}")]
    Truncated(usize),
    /// A type byte we do not understand.
    #[error("unknown value type 0x{ty:02x} at byte {offset}")]
    UnknownType {
        /// The type byte.
        ty: u8,
        /// Where it was found.
        offset: usize,
    },
    /// A key or string is not valid UTF-8.
    #[error("text at byte {0} is not valid UTF-8")]
    NotUtf8(usize),
    /// Bytes follow the final end marker.
    #[error("unexpected data after the end of the document at byte {0}")]
    TrailingData(usize),
    /// Maps nested deeper than we accept.
    #[error("maps nested too deeply")]
    TooDeep,
    /// A key or string to be written contains a NUL byte.
    #[error("text {0:?} contains a NUL byte and cannot be stored")]
    NulInText(String),
}

/// Parses a whole binary VDF document into its top-level map.
///
/// An empty input is an empty document (a missing `shortcuts.vdf` and an
/// empty one mean the same thing).
pub fn parse(data: &[u8]) -> Result<Map, VdfError> {
    if data.is_empty() {
        return Ok(Map::new());
    }
    let mut r = Reader { data, pos: 0 };
    let map = r.map(0)?;
    if r.pos != data.len() {
        return Err(VdfError::TrailingData(r.pos));
    }
    Ok(map)
}

/// Serialises a top-level map (pairs followed by the closing `0x08`).
pub fn write(map: &Map) -> Result<Vec<u8>, VdfError> {
    let mut out = Vec::new();
    write_map(map, &mut out)?;
    Ok(out)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, VdfError> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or(VdfError::Truncated(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], VdfError> {
        let end = self
            .pos
            .checked_add(N)
            .ok_or(VdfError::Truncated(self.pos))?;
        let bytes: [u8; N] = self
            .data
            .get(self.pos..end)
            .and_then(|s| s.try_into().ok())
            .ok_or(VdfError::Truncated(self.pos))?;
        self.pos = end;
        Ok(bytes)
    }

    fn cstr(&mut self) -> Result<String, VdfError> {
        let start = self.pos;
        let len = self.data[start..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(VdfError::Truncated(self.data.len()))?;
        let s = std::str::from_utf8(&self.data[start..start + len])
            .map_err(|_| VdfError::NotUtf8(start))?
            .to_string();
        self.pos = start + len + 1;
        Ok(s)
    }

    fn map(&mut self, depth: usize) -> Result<Map, VdfError> {
        if depth > MAX_DEPTH {
            return Err(VdfError::TooDeep);
        }
        let mut map = Map::new();
        loop {
            let offset = self.pos;
            let ty = self.byte()?;
            if ty == T_END {
                return Ok(map);
            }
            let key = self.cstr()?;
            let value = match ty {
                T_MAP => Value::Map(self.map(depth + 1)?),
                T_STRING => Value::Str(self.cstr()?),
                T_INT32 => Value::Int(i32::from_le_bytes(self.take()?)),
                T_FLOAT32 => Value::Float(u32::from_le_bytes(self.take()?)),
                T_UINT64 => Value::U64(u64::from_le_bytes(self.take()?)),
                ty => return Err(VdfError::UnknownType { ty, offset }),
            };
            map.0.push((key, value));
        }
    }
}

fn put_cstr(s: &str, out: &mut Vec<u8>) -> Result<(), VdfError> {
    if s.contains('\0') {
        return Err(VdfError::NulInText(s.to_string()));
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

fn write_map(map: &Map, out: &mut Vec<u8>) -> Result<(), VdfError> {
    for (key, value) in &map.0 {
        let ty = match value {
            Value::Map(_) => T_MAP,
            Value::Str(_) => T_STRING,
            Value::Int(_) => T_INT32,
            Value::Float(_) => T_FLOAT32,
            Value::U64(_) => T_UINT64,
        };
        out.push(ty);
        put_cstr(key, out)?;
        match value {
            Value::Map(m) => write_map(m, out)?,
            Value::Str(s) => put_cstr(s, out)?,
            Value::Int(i) => out.extend_from_slice(&i.to_le_bytes()),
            Value::Float(f) => out.extend_from_slice(&f.to_le_bytes()),
            Value::U64(u) => out.extend_from_slice(&u.to_le_bytes()),
        }
    }
    out.push(T_END);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `shortcuts.vdf` as Steam writes it, built by hand byte by byte.
    fn steam_written() -> Vec<u8> {
        let mut v = Vec::new();
        v.push(0x00);
        v.extend_from_slice(b"shortcuts\0");
        v.push(0x00);
        v.extend_from_slice(b"0\0");
        v.push(0x02);
        v.extend_from_slice(b"appid\0");
        v.extend_from_slice(&(-123_456_789i32).to_le_bytes());
        v.push(0x01);
        v.extend_from_slice(b"AppName\0Some Game\0");
        v.push(0x01);
        v.extend_from_slice(b"Exe\0\"/usr/bin/game\"\0");
        v.push(0x00);
        v.extend_from_slice(b"tags\0");
        v.push(0x01);
        v.extend_from_slice(b"0\0favorite\0");
        v.push(0x08);
        v.push(0x07);
        v.extend_from_slice(b"Big\0");
        v.extend_from_slice(&u64::MAX.to_le_bytes());
        v.push(0x03);
        v.extend_from_slice(b"F\0");
        v.extend_from_slice(&1.5f32.to_bits().to_le_bytes());
        v.push(0x08); // end entry "0"
        v.push(0x08); // end "shortcuts"
        v.push(0x08); // end document
        v
    }

    #[test]
    fn round_trip_is_byte_exact() {
        let data = steam_written();
        let doc = parse(&data).unwrap();
        let entry = doc
            .get_map("shortcuts")
            .and_then(|s| s.get_map("0"))
            .unwrap();
        assert_eq!(entry.get_int("appid"), Some(-123_456_789));
        assert_eq!(entry.get_str("appname"), Some("Some Game"));
        assert_eq!(entry.get_str("Exe"), Some("\"/usr/bin/game\""));
        assert_eq!(
            entry.get_map("tags").and_then(|t| t.get_str("0")),
            Some("favorite")
        );
        assert_eq!(entry.get("Big"), Some(&Value::U64(u64::MAX)));
        assert_eq!(write(&doc).unwrap(), data);
    }

    #[test]
    fn empty_input_is_empty_document() {
        assert_eq!(parse(&[]).unwrap(), Map::new());
        assert_eq!(write(&Map::new()).unwrap(), vec![0x08]);
        assert_eq!(parse(&[0x08]).unwrap(), Map::new());
    }

    #[test]
    fn corrupt_input_is_rejected() {
        let data = steam_written();
        for cut in [1, 5, 20, data.len() - 1] {
            assert!(
                matches!(parse(&data[..cut]), Err(VdfError::Truncated(_))),
                "cut at {cut}"
            );
        }
        let mut extra = data.clone();
        extra.push(0);
        assert!(matches!(parse(&extra), Err(VdfError::TrailingData(_))));
        assert!(matches!(
            parse(&[0x05, b'k', 0, 0x08]),
            Err(VdfError::UnknownType { ty: 0x05, .. })
        ));
        assert!(matches!(
            parse(&[0x01, 0xff, 0, b'x', 0, 0x08]),
            Err(VdfError::NotUtf8(1))
        ));
        let deep: Vec<u8> = std::iter::repeat_n([0x00u8, b'a', 0], 100)
            .flatten()
            .collect();
        assert_eq!(parse(&deep), Err(VdfError::TooDeep));
    }

    #[test]
    fn set_replaces_in_place() {
        let mut m = Map::new();
        m.set("AppName", Value::Str("a".into()));
        m.set("Exe", Value::Str("b".into()));
        m.set("appname", Value::Str("c".into()));
        assert_eq!(m.0.len(), 2);
        assert_eq!(m.0[0], ("AppName".to_string(), Value::Str("c".into())));
        assert!(write(&m).is_ok());
        m.set("bad", Value::Str("x\0y".into()));
        assert!(matches!(write(&m), Err(VdfError::NulInText(_))));
    }
}
