//! Encoding and decoding of AMQP 1.0 values (OASIS AMQP 1.0 part 1, section 1.6 "Type System"
//! and section 1.5 "Encodings").
//!
//! The decoder never allocates more than the input justifies: every count is checked against
//! the bytes that remain, nesting is limited, and every read is bounds checked. Besides that
//! every decode runs on a budget of values (the length of the input plus `VALUE_ALLOWANCE`):
//! each decoded value costs one, including the elements of an array of zero-width elements,
//! which take no bytes at all. When the budget is gone the decode fails, so the memory a decode
//! can ask for is a constant times the bytes it was given, however the input is nested.

use crate::types::{Described, Value};

/// Maximum nesting of lists, maps, arrays and described values.
pub const MAX_DEPTH: usize = 64;
/// Upper bound on the element count of an array of zero-width elements (`null`, `true`, ...).
const MAX_ZERO_WIDTH_ARRAY: usize = 1 << 16;
/// Longest descriptor name accepted on the elements of an array.
const MAX_ARRAY_DESCRIPTOR: usize = 255;
/// Values a decode may produce beyond one per input byte. Every value that takes bytes costs at
/// least one byte of input, so this only matters for zero-width array elements.
pub const VALUE_ALLOWANCE: usize = 1024;

/// How many values a decode of `len` bytes may produce.
pub fn value_budget(len: usize) -> usize {
    len.saturating_add(VALUE_ALLOWANCE)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended in the middle of a value.
    Truncated,
    InvalidConstructor(u8),
    Invalid(&'static str),
    TooDeep,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated AMQP data"),
            DecodeError::InvalidConstructor(c) => write!(f, "invalid AMQP type constructor 0x{c:02x}"),
            DecodeError::Invalid(s) => write!(f, "invalid AMQP data: {s}"),
            DecodeError::TooDeep => write!(f, "AMQP value nested too deeply"),
        }
    }
}
impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

/// Encode `v` with the smallest encoding of each type.
pub fn encode(v: &Value, out: &mut Vec<u8>) {
    if let Value::Described(d) = v {
        out.push(0x00);
        encode(&d.descriptor, out);
        encode(&d.value, out);
        return;
    }
    let pos = out.len();
    out.push(0);
    let ctor = encode_body(v, false, out);
    out[pos] = ctor;
}

pub fn encode_to_vec(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode(v, &mut out);
    out
}

fn put_var(out: &mut Vec<u8>, bytes: &[u8], small: u8, large: u8, wide: bool) -> u8 {
    if !wide && bytes.len() <= 255 {
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
        small
    } else {
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
        large
    }
}

/// Writes the body of `v` (everything after the constructor byte) and returns the constructor.
/// `wide` forces the fixed 4/8 byte and 32-bit-size forms, which array elements need so that
/// every element shares one constructor.
fn encode_body(v: &Value, wide: bool, out: &mut Vec<u8>) -> u8 {
    match v {
        Value::Null => 0x40,
        Value::Bool(b) => {
            if wide {
                out.push(*b as u8);
                0x56
            } else if *b {
                0x41
            } else {
                0x42
            }
        }
        Value::Ubyte(x) => {
            out.push(*x);
            0x50
        }
        Value::Ushort(x) => {
            out.extend_from_slice(&x.to_be_bytes());
            0x60
        }
        Value::Uint(x) => {
            if !wide && *x == 0 {
                0x43
            } else if !wide && *x <= 255 {
                out.push(*x as u8);
                0x52
            } else {
                out.extend_from_slice(&x.to_be_bytes());
                0x70
            }
        }
        Value::Ulong(x) => {
            if !wide && *x == 0 {
                0x44
            } else if !wide && *x <= 255 {
                out.push(*x as u8);
                0x53
            } else {
                out.extend_from_slice(&x.to_be_bytes());
                0x80
            }
        }
        Value::Byte(x) => {
            out.push(*x as u8);
            0x51
        }
        Value::Short(x) => {
            out.extend_from_slice(&x.to_be_bytes());
            0x61
        }
        Value::Int(x) => {
            if !wide && (-128..=127).contains(x) {
                out.push(*x as i8 as u8);
                0x54
            } else {
                out.extend_from_slice(&x.to_be_bytes());
                0x71
            }
        }
        Value::Long(x) => {
            if !wide && (-128..=127).contains(x) {
                out.push(*x as i8 as u8);
                0x55
            } else {
                out.extend_from_slice(&x.to_be_bytes());
                0x81
            }
        }
        Value::Float(x) => {
            out.extend_from_slice(&x.to_be_bytes());
            0x72
        }
        Value::Double(x) => {
            out.extend_from_slice(&x.to_be_bytes());
            0x82
        }
        Value::Decimal32(b) => {
            out.extend_from_slice(b);
            0x74
        }
        Value::Decimal64(b) => {
            out.extend_from_slice(b);
            0x84
        }
        Value::Decimal128(b) => {
            out.extend_from_slice(b);
            0x94
        }
        Value::Char(c) => {
            out.extend_from_slice(&(*c as u32).to_be_bytes());
            0x73
        }
        Value::Timestamp(x) => {
            out.extend_from_slice(&x.to_be_bytes());
            0x83
        }
        Value::Uuid(b) => {
            out.extend_from_slice(b);
            0x98
        }
        Value::Binary(b) => put_var(out, b, 0xa0, 0xb0, wide),
        Value::String(s) => put_var(out, s.as_bytes(), 0xa1, 0xb1, wide),
        Value::Symbol(s) => put_var(out, s.as_bytes(), 0xa3, 0xb3, wide),
        Value::List(items) => {
            if items.is_empty() && !wide {
                return 0x45;
            }
            let mut body = Vec::new();
            for i in items {
                encode(i, &mut body);
            }
            put_compound(out, &body, items.len(), 0xc0, 0xd0, wide)
        }
        Value::Map(pairs) => {
            let mut body = Vec::new();
            for (k, x) in pairs {
                encode(k, &mut body);
                encode(x, &mut body);
            }
            put_compound(out, &body, pairs.len() * 2, 0xc1, 0xd1, wide)
        }
        Value::Array(items) => encode_array(items, wide, out),
        // A described value nested in an array element is handled by encode_array; reaching
        // here means a described value inside another constructor context, which cannot happen.
        Value::Described(_) => 0x40,
    }
}

fn put_compound(out: &mut Vec<u8>, body: &[u8], count: usize, small: u8, large: u8, wide: bool) -> u8 {
    if !wide && count <= 255 && body.len() < 255 {
        out.push((body.len() + 1) as u8);
        out.push(count as u8);
        out.extend_from_slice(body);
        small
    } else {
        out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(&(count as u32).to_be_bytes());
        out.extend_from_slice(body);
        large
    }
}

fn encode_array(items: &[Value], wide: bool, out: &mut Vec<u8>) -> u8 {
    // Constructor of the first element decides; elements of another type degrade the array to a
    // list so the output is always valid.
    let first = match items.first() {
        Some(f) => f,
        None => {
            // Empty array: any constructor will do. Use sym32.
            return put_compound_array(out, &[], 0, &[0xb3], wide);
        }
    };
    let (descriptor, first_inner) = match first {
        Value::Described(d) => (Some(&d.descriptor), &d.value),
        other => (None, other),
    };
    let mut body = Vec::new();
    let mut ctor = 0u8;
    let mut ok = true;
    for (i, item) in items.iter().enumerate() {
        let inner = match (item, descriptor) {
            (Value::Described(d), Some(desc)) if &d.descriptor == desc => &d.value,
            (Value::Described(_), _) | (_, Some(_)) => {
                ok = false;
                break;
            }
            (other, None) => other,
        };
        let c = encode_body(inner, true, &mut body);
        if i == 0 {
            ctor = c;
        } else if c != ctor {
            ok = false;
            break;
        }
    }
    let _ = first_inner;
    if !ok {
        return encode_body(&Value::List(items.to_vec()), wide, out);
    }
    let mut ctor_bytes = Vec::new();
    if let Some(desc) = descriptor {
        ctor_bytes.push(0x00);
        encode(desc, &mut ctor_bytes);
    }
    ctor_bytes.push(ctor);
    put_compound_array(out, &body, items.len(), &ctor_bytes, wide)
}

fn put_compound_array(out: &mut Vec<u8>, body: &[u8], count: usize, ctor: &[u8], wide: bool) -> u8 {
    if !wide && count <= 255 && body.len() + ctor.len() < 255 {
        out.push((body.len() + ctor.len() + 1) as u8);
        out.push(count as u8);
        out.extend_from_slice(ctor);
        out.extend_from_slice(body);
        0xe0
    } else {
        out.extend_from_slice(&((body.len() + ctor.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(&(count as u32).to_be_bytes());
        out.extend_from_slice(ctor);
        out.extend_from_slice(body);
        0xf0
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

/// A cursor over input bytes.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Values this reader (and the readers made from it) may still produce.
    budget: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0, budget: value_budget(buf.len()) }
    }
    /// A reader over `buf` that spends from a budget the caller keeps across several decodes.
    pub fn with_budget(buf: &'a [u8], budget: usize) -> Self {
        Reader { buf, pos: 0, budget }
    }
    /// Values this reader may still produce.
    pub fn budget(&self) -> usize {
        self.budget
    }
    fn charge(&mut self, n: usize) -> Result<(), DecodeError> {
        if n > self.budget {
            return Err(DecodeError::Invalid("too many values for the size of the data"));
        }
        self.budget -= n;
        Ok(())
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.arr()?))
    }
}

/// Decode one value from the front of `buf`; returns the value and the bytes consumed.
pub fn decode(buf: &[u8]) -> Result<(Value, usize), DecodeError> {
    let mut budget = value_budget(buf.len());
    decode_budgeted(buf, &mut budget)
}

/// Like `decode`, but spends from `budget` (see `value_budget`) and leaves what is left in it, so
/// several decodes over parts of one input can share one budget.
pub fn decode_budgeted(buf: &[u8], budget: &mut usize) -> Result<(Value, usize), DecodeError> {
    let mut r = Reader::with_budget(buf, *budget);
    let result = decode_value(&mut r);
    *budget = r.budget;
    let v = result?;
    Ok((v, r.position()))
}

/// Decode a value that must fill `buf` exactly.
pub fn decode_exact(buf: &[u8]) -> Result<Value, DecodeError> {
    let (v, n) = decode(buf)?;
    if n != buf.len() {
        return Err(DecodeError::Invalid("trailing bytes"));
    }
    Ok(v)
}

pub fn decode_value(r: &mut Reader<'_>) -> Result<Value, DecodeError> {
    decode_depth(r, 0)
}

fn decode_depth(r: &mut Reader<'_>, depth: usize) -> Result<Value, DecodeError> {
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    r.charge(1)?;
    let code = r.u8()?;
    if code == 0x00 {
        let descriptor = decode_depth(r, depth + 1)?;
        let value = decode_depth(r, depth + 1)?;
        return Ok(Value::Described(Box::new(Described { descriptor, value })));
    }
    decode_with_ctor(r, code, depth)
}

fn utf8(b: &[u8]) -> Result<String, DecodeError> {
    String::from_utf8(b.to_vec()).map_err(|_| DecodeError::Invalid("invalid utf-8"))
}

fn decode_with_ctor(r: &mut Reader<'_>, code: u8, depth: usize) -> Result<Value, DecodeError> {
    // Array elements come here without passing `decode_depth`, so arrays of arrays of arrays ...
    // need this check too (each level costs only 9 bytes: 100,000 levels fit in one frame).
    if depth > MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    Ok(match code {
        0x40 => Value::Null,
        0x41 => Value::Bool(true),
        0x42 => Value::Bool(false),
        0x56 => match r.u8()? {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(DecodeError::Invalid("boolean not 0 or 1")),
        },
        0x43 => Value::Uint(0),
        0x44 => Value::Ulong(0),
        0x45 => Value::List(Vec::new()),
        0x50 => Value::Ubyte(r.u8()?),
        0x51 => Value::Byte(r.u8()? as i8),
        0x52 => Value::Uint(r.u8()? as u32),
        0x53 => Value::Ulong(r.u8()? as u64),
        0x54 => Value::Int(r.u8()? as i8 as i32),
        0x55 => Value::Long(r.u8()? as i8 as i64),
        0x60 => Value::Ushort(u16::from_be_bytes(r.arr()?)),
        0x61 => Value::Short(i16::from_be_bytes(r.arr()?)),
        0x70 => Value::Uint(u32::from_be_bytes(r.arr()?)),
        0x71 => Value::Int(i32::from_be_bytes(r.arr()?)),
        0x72 => Value::Float(f32::from_be_bytes(r.arr()?)),
        0x73 => Value::Char(char::from_u32(u32::from_be_bytes(r.arr()?)).ok_or(DecodeError::Invalid("bad char"))?),
        0x74 => Value::Decimal32(r.arr()?),
        0x80 => Value::Ulong(u64::from_be_bytes(r.arr()?)),
        0x81 => Value::Long(i64::from_be_bytes(r.arr()?)),
        0x82 => Value::Double(f64::from_be_bytes(r.arr()?)),
        0x83 => Value::Timestamp(i64::from_be_bytes(r.arr()?)),
        0x84 => Value::Decimal64(r.arr()?),
        0x94 => Value::Decimal128(r.arr()?),
        0x98 => Value::Uuid(r.arr()?),
        0xa0 => {
            let n = r.u8()? as usize;
            Value::Binary(r.take(n)?.to_vec())
        }
        0xb0 => {
            let n = r.u32()? as usize;
            Value::Binary(r.take(n)?.to_vec())
        }
        0xa1 => {
            let n = r.u8()? as usize;
            Value::String(utf8(r.take(n)?)?)
        }
        0xb1 => {
            let n = r.u32()? as usize;
            Value::String(utf8(r.take(n)?)?)
        }
        0xa3 => {
            let n = r.u8()? as usize;
            Value::Symbol(utf8(r.take(n)?)?)
        }
        0xb3 => {
            let n = r.u32()? as usize;
            Value::Symbol(utf8(r.take(n)?)?)
        }
        0xc0 | 0xd0 | 0xc1 | 0xd1 | 0xe0 | 0xf0 => {
            let small = code & 0x10 == 0;
            let (size, count) = if small { (r.u8()? as usize, r.u8()? as usize) } else { (r.u32()? as usize, r.u32()? as usize) };
            let width = if small { 1 } else { 4 };
            if size < width || size - width > r.remaining() {
                return Err(DecodeError::Truncated);
            }
            let body = r.take(size - width)?;
            let mut inner = Reader::with_budget(body, r.budget);
            let value = decode_compound(&mut inner, code, count, depth)?;
            r.budget = inner.budget;
            value
        }
        other => return Err(DecodeError::InvalidConstructor(other)),
    })
}

fn decode_compound(inner: &mut Reader<'_>, code: u8, count: usize, depth: usize) -> Result<Value, DecodeError> {
    Ok(match code {
        0xc0 | 0xd0 => {
            // each element takes at least one byte
            if count > inner.remaining() {
                return Err(DecodeError::Invalid("list count exceeds body"));
            }
            if count > inner.budget {
                return Err(DecodeError::Invalid("too many values for the size of the data"));
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_depth(inner, depth + 1)?);
            }
            check_consumed(inner)?;
            Value::List(items)
        }
        0xc1 | 0xd1 => {
            if !count.is_multiple_of(2) {
                return Err(DecodeError::Invalid("map with odd element count"));
            }
            if count > inner.remaining() {
                return Err(DecodeError::Invalid("map count exceeds body"));
            }
            if count > inner.budget {
                return Err(DecodeError::Invalid("too many values for the size of the data"));
            }
            let mut pairs = Vec::with_capacity(count / 2);
            for _ in 0..count / 2 {
                let k = decode_depth(inner, depth + 1)?;
                let v = decode_depth(inner, depth + 1)?;
                pairs.push((k, v));
            }
            check_consumed(inner)?;
            Value::Map(pairs)
        }
        _ => decode_array(inner, count, depth)?,
    })
}

fn check_consumed(r: &Reader<'_>) -> Result<(), DecodeError> {
    if r.is_empty() {
        Ok(())
    } else {
        Err(DecodeError::Invalid("compound value size does not match its contents"))
    }
}

fn is_zero_width(code: u8) -> bool {
    matches!(code, 0x40..=0x45)
}

fn decode_array(r: &mut Reader<'_>, count: usize, depth: usize) -> Result<Value, DecodeError> {
    // element constructor, possibly described
    let mut descriptor = None;
    let mut code = r.u8()?;
    if code == 0x00 {
        let d = decode_depth(r, depth + 1)?;
        // Every element of the array carries its own copy of the descriptor, so a long or nested
        // descriptor multiplies. The type system allows only a symbol or a ulong here.
        match &d {
            Value::Ulong(_) => {}
            Value::Symbol(s) | Value::String(s) if s.len() <= MAX_ARRAY_DESCRIPTOR => {}
            _ => return Err(DecodeError::Invalid("array descriptor must be a ulong or a short symbol")),
        }
        descriptor = Some(d);
        code = r.u8()?;
        if code == 0x00 {
            return Err(DecodeError::Invalid("nested described array constructor"));
        }
    }
    if is_zero_width(code) {
        if count > MAX_ZERO_WIDTH_ARRAY {
            return Err(DecodeError::Invalid("array of zero-width elements too large"));
        }
    } else if count > r.remaining() {
        return Err(DecodeError::Invalid("array count exceeds body"));
    }
    // The elements cost nothing in the input when they are zero-width, so they are paid for here,
    // before the vector is made.
    r.charge(count)?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let v = decode_with_ctor(r, code, depth + 1)?;
        items.push(match &descriptor {
            Some(d) => Value::Described(Box::new(Described { descriptor: d.clone(), value: v })),
            None => v,
        });
    }
    check_consumed(r)?;
    Ok(Value::Array(items))
}
