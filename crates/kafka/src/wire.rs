//! Byte-level readers and writers: big-endian integers, zig-zag varints, unsigned varints, plain and compact strings
//! and byte arrays. Every read checks what is left first.

use crate::error::{Error, Result};

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.buf[self.pos..];
        self.pos = self.buf.len();
        s
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    pub fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(self.i32()? as u32)
    }
    pub fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    /// Unsigned LEB128, at most 5 bytes (32 bits).
    pub fn uvarint(&mut self) -> Result<u32> {
        let mut v: u64 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                if v > u32::MAX as u64 {
                    return Err(Error::Invalid("unsigned varint over 32 bits"));
                }
                return Ok(v as u32);
            }
        }
        Err(Error::Invalid("unsigned varint longer than 5 bytes"))
    }

    /// Zig-zag varint (record fields), at most 5 bytes.
    pub fn varint(&mut self) -> Result<i32> {
        let mut v: u64 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                if v > u32::MAX as u64 {
                    return Err(Error::Invalid("varint over 32 bits"));
                }
                let v = v as u32;
                return Ok(((v >> 1) as i32) ^ -((v & 1) as i32));
            }
        }
        Err(Error::Invalid("varint longer than 5 bytes"))
    }

    /// Zig-zag varlong, at most 10 bytes.
    pub fn varlong(&mut self) -> Result<i64> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(((v >> 1) as i64) ^ -((v & 1) as i64));
            }
        }
        Err(Error::Invalid("varlong longer than 10 bytes"))
    }

    /// STRING / NULLABLE_STRING: int16 length, -1 is null.
    pub fn string(&mut self) -> Result<Option<&'a str>> {
        let n = self.i16()?;
        if n < 0 {
            return if n == -1 { Ok(None) } else { Err(Error::Invalid("negative string length")) };
        }
        self.utf8(n as usize).map(Some)
    }

    /// COMPACT_STRING: unsigned varint length + 1, 0 is null.
    pub fn compact_string(&mut self) -> Result<Option<&'a str>> {
        let n = self.uvarint()?;
        if n == 0 {
            return Ok(None);
        }
        self.utf8((n - 1) as usize).map(Some)
    }

    fn utf8(&mut self, n: usize) -> Result<&'a str> {
        let b = self.take(n)?;
        std::str::from_utf8(b).map_err(|_| Error::Invalid("string is not UTF-8"))
    }

    /// BYTES / NULLABLE_BYTES: int32 length, -1 is null.
    pub fn bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let n = self.i32()?;
        if n < 0 {
            return if n == -1 { Ok(None) } else { Err(Error::Invalid("negative bytes length")) };
        }
        self.take(n as usize).map(Some)
    }

    pub fn compact_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let n = self.uvarint()?;
        if n == 0 {
            return Ok(None);
        }
        self.take((n - 1) as usize).map(Some)
    }
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }
    pub fn with_capacity(n: usize) -> Writer {
        Writer { buf: Vec::with_capacity(n) }
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }
    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.i64(v.to_bits() as i64);
    }
    pub fn uvarint(&mut self, mut v: u32) {
        while v >= 0x80 {
            self.buf.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    pub fn varint(&mut self, v: i32) {
        self.uvarint(((v << 1) ^ (v >> 31)) as u32);
    }
    pub fn varlong(&mut self, v: i64) {
        let mut z = ((v << 1) ^ (v >> 63)) as u64;
        while z >= 0x80 {
            self.buf.push((z as u8 & 0x7F) | 0x80);
            z >>= 7;
        }
        self.buf.push(z as u8);
    }
    pub fn string(&mut self, s: Option<&str>) {
        match s {
            None => self.i16(-1),
            Some(s) => {
                self.i16(s.len().min(i16::MAX as usize) as i16);
                self.raw(&s.as_bytes()[..s.len().min(i16::MAX as usize)]);
            }
        }
    }
    pub fn compact_string(&mut self, s: Option<&str>) {
        match s {
            None => self.uvarint(0),
            Some(s) => {
                self.uvarint(s.len() as u32 + 1);
                self.raw(s.as_bytes());
            }
        }
    }
    pub fn bytes(&mut self, b: Option<&[u8]>) {
        match b {
            None => self.i32(-1),
            Some(b) => {
                self.i32(b.len() as i32);
                self.raw(b);
            }
        }
    }
    pub fn compact_bytes(&mut self, b: Option<&[u8]>) {
        match b {
            None => self.uvarint(0),
            Some(b) => {
                self.uvarint(b.len() as u32 + 1);
                self.raw(b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_and_match_known_bytes() {
        // from the Protocol Buffers / Kafka zig-zag description: 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, 150 -> 0xAC 0x02 (unsigned)
        for (v, enc) in [(0i32, vec![0u8]), (-1, vec![1]), (1, vec![2]), (-2, vec![3]), (64, vec![0x80, 0x01]), (-65, vec![0x81, 0x01])] {
            let mut w = Writer::new();
            w.varint(v);
            assert_eq!(w.buf, enc, "encode {v}");
            assert_eq!(Reader::new(&enc).varint().unwrap(), v);
        }
        let mut w = Writer::new();
        w.uvarint(150);
        assert_eq!(w.buf, vec![0x96, 0x01]);
        for v in [0, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut w = Writer::new();
            w.uvarint(v);
            assert_eq!(Reader::new(&w.buf).uvarint().unwrap(), v);
        }
        for v in [0i64, -1, 1, i64::MAX, i64::MIN, 123456789012345] {
            let mut w = Writer::new();
            w.varlong(v);
            assert_eq!(Reader::new(&w.buf).varlong().unwrap(), v);
        }
        for v in [i32::MAX, i32::MIN] {
            let mut w = Writer::new();
            w.varint(v);
            assert_eq!(Reader::new(&w.buf).varint().unwrap(), v);
        }
    }

    #[test]
    fn overlong_varints_are_refused() {
        assert!(Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]).uvarint().is_err());
        assert!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).uvarint().is_err()); // over 32 bits
        assert!(Reader::new(&[0x80; 11]).varlong().is_err());
        assert_eq!(Reader::new(&[0x80]).uvarint(), Err(Error::Truncated));
    }

    #[test]
    fn strings_check_length_first() {
        assert_eq!(Reader::new(&[0, 3, b'a', b'b']).string(), Err(Error::Truncated));
        assert_eq!(Reader::new(&[0xFF, 0xFF]).string().unwrap(), None);
        assert!(Reader::new(&[0xFF, 0xFE]).string().is_err());
        assert_eq!(Reader::new(&[3, b'a', b'b']).compact_string().unwrap(), Some("ab"));
        assert_eq!(Reader::new(&[0]).compact_string().unwrap(), None);
        // a huge declared length is just a truncation, nothing is allocated for it
        assert_eq!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).compact_string(), Err(Error::Truncated));
        assert!(Reader::new(&[0, 2, 0xC3, 0x28]).string().is_err());
        assert_eq!(Reader::new(&[0x7F, 0xFF, 0xFF, 0xFF, 1]).bytes(), Err(Error::Truncated));
    }
}
