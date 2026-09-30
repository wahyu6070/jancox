//! The bits of protobuf wire format that `payload.bin` manifests need:
//! varints, length-delimited fields (strings, bytes, messages) and fixed
//! 32/64-bit fields, decoded and encoded by hand (no codegen).

use std::io;

use crate::fs::invalid;

/// One field of a message: its number and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn as_u64(&self) -> io::Result<u64> {
        match *self {
            Value::Varint(v) | Value::Fixed64(v) => Ok(v),
            Value::Fixed32(v) => Ok(v as u64),
            Value::Bytes(_) => Err(invalid("protobuf: expected a number")),
        }
    }

    pub fn as_bytes(&self) -> io::Result<&'a [u8]> {
        match *self {
            Value::Bytes(b) => Ok(b),
            _ => Err(invalid("protobuf: expected bytes")),
        }
    }

    pub fn as_string(&self) -> io::Result<String> {
        String::from_utf8(self.as_bytes()?.to_vec()).map_err(|_| invalid("protobuf: bad UTF-8"))
    }
}

fn varint(b: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b
            .get(*pos)
            .ok_or_else(|| invalid("protobuf: truncated varint"))?;
        *pos += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c < 0x80 {
            return Ok(v);
        }
    }
    Err(invalid("protobuf: varint too long"))
}

fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> io::Result<&'a [u8]> {
    let s = b
        .get(*pos..pos.saturating_add(n))
        .ok_or_else(|| invalid("protobuf: truncated field"))?;
    *pos += n;
    Ok(s)
}

/// The fields of a message, in wire order.
pub fn fields(msg: &[u8]) -> io::Result<Vec<(u32, Value<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < msg.len() {
        let key = varint(msg, &mut pos)?;
        let field = (key >> 3) as u32;
        let value = match key & 7 {
            0 => Value::Varint(varint(msg, &mut pos)?),
            1 => Value::Fixed64(u64::from_le_bytes(
                take(msg, &mut pos, 8)?.try_into().unwrap(),
            )),
            2 => {
                let n = varint(msg, &mut pos)?;
                let n = usize::try_from(n).map_err(|_| invalid("protobuf: field too long"))?;
                Value::Bytes(take(msg, &mut pos, n)?)
            }
            5 => Value::Fixed32(u32::from_le_bytes(
                take(msg, &mut pos, 4)?.try_into().unwrap(),
            )),
            t => return Err(invalid(format!("protobuf: unsupported wire type {}", t))),
        };
        out.push((field, value));
    }
    Ok(out)
}

/// Encodes a message field by field.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer::default()
    }

    fn raw_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    pub fn varint(&mut self, field: u32, v: u64) -> &mut Self {
        self.raw_varint((field as u64) << 3);
        self.raw_varint(v);
        self
    }

    pub fn bytes(&mut self, field: u32, b: &[u8]) -> &mut Self {
        self.raw_varint((field as u64) << 3 | 2);
        self.raw_varint(b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }

    pub fn string(&mut self, field: u32, s: &str) -> &mut Self {
        self.bytes(field, s.as_bytes())
    }

    pub fn message(&mut self, field: u32, m: &Writer) -> &mut Self {
        self.bytes(field, &m.buf)
    }

    pub fn fixed32(&mut self, field: u32, v: u32) -> &mut Self {
        self.raw_varint((field as u64) << 3 | 5);
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes a field as it was read.
    pub fn value(&mut self, field: u32, v: &Value) -> &mut Self {
        match *v {
            Value::Varint(n) => self.varint(field, n),
            Value::Bytes(b) => self.bytes(field, b),
            Value::Fixed32(n) => self.fixed32(field, n),
            Value::Fixed64(n) => {
                self.raw_varint((field as u64) << 3 | 1);
                self.buf.extend_from_slice(&n.to_le_bytes());
                self
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut inner = Writer::new();
        inner.varint(1, 300);
        let mut w = Writer::new();
        w.string(1, "system")
            .varint(2, u64::MAX)
            .message(3, &inner)
            .bytes(4, &[]);
        let f = fields(&w.buf).unwrap();
        assert_eq!(f.len(), 4);
        assert_eq!(f[0], (1, Value::Bytes(b"system")));
        assert_eq!(f[1].1.as_u64().unwrap(), u64::MAX);
        let sub = fields(f[2].1.as_bytes().unwrap()).unwrap();
        assert_eq!(sub, [(1, Value::Varint(300))]);
        assert_eq!(f[3], (4, Value::Bytes(&[])));
        // 300 = 0xac 0x02
        assert_eq!(&inner.buf, &[0x08, 0xac, 0x02]);
        assert!(fields(&[0x0a, 0x05, 1]).is_err());
        assert!(fields(&[0x0b]).is_err());
    }
}
