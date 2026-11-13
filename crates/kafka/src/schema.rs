//! The message layouts and the codec that reads and writes them.
//!
//! A message is described as data (`schemas.rs`, generated from Kafka's own JSON definitions): fields in wire order,
//! each with the versions it exists in, the versions where it may be null, an optional tag, and a default. One
//! interpreter decodes a request or response body into a [`Struct`] and encodes one back, for every API and version:
//! plain and flexible versions (compact strings and arrays, tagged fields), all in one place.

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::value::{Struct, Value};
use crate::wire::{Reader, Writer};

#[derive(Clone, Copy, Debug)]
pub enum Ty {
    Bool,
    I8,
    I16,
    U16,
    I32,
    I64,
    F64,
    Uuid,
    Str,
    Bytes,
    Records,
    Array(&'static Ty),
    Struct(&'static [Field]),
}

#[derive(Clone, Copy, Debug)]
pub enum Def {
    Zero,
    Int(i64),
    Null,
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    pub min: i16,
    pub max: i16,
    pub null_min: i16,
    pub null_max: i16,
    /// -1 when the field is not tagged.
    pub tag: i32,
    pub def: Def,
}

#[allow(clippy::too_many_arguments)]
pub const fn f(name: &'static str, ty: Ty, min: i16, max: i16, null_min: i16, null_max: i16, tag: i32, def: Def) -> Field {
    Field { name, ty, min, max, null_min, null_max, tag, def }
}

pub struct MessageSchema {
    pub name: &'static str,
    pub api_key: i16,
    /// Versions the definition knows (the server serves a subset, see `api.rs`).
    pub min: i16,
    pub max: i16,
    /// First flexible version (compact encodings, tagged fields); `i16::MAX` when the message never is.
    pub flexible_from: i16,
    pub fields: &'static [Field],
}

impl MessageSchema {
    pub fn flexible(&self, version: i16) -> bool {
        version >= self.flexible_from
    }
}

pub fn request_schema(api_key: i16) -> Option<&'static MessageSchema> {
    crate::schemas::REQUESTS.iter().copied().find(|s| s.api_key == api_key)
}

pub fn response_schema(api_key: i16) -> Option<&'static MessageSchema> {
    crate::schemas::RESPONSES.iter().copied().find(|s| s.api_key == api_key)
}

fn in_range(f: &Field, v: i16) -> bool {
    v >= f.min && v <= f.max
}

fn nullable(f: &Field, v: i16) -> bool {
    v >= f.null_min && v <= f.null_max
}

fn default_value(f: &Field) -> Value {
    match (f.def, f.ty) {
        (Def::Null, _) => Value::Null,
        (Def::Int(i), Ty::Bool) => Value::Bool(i != 0),
        (Def::Int(i), _) => Value::Int(i),
        (Def::Zero, Ty::Bool) => Value::Bool(false),
        (Def::Zero, Ty::Str) => Value::Str(String::new()),
        (Def::Zero, Ty::Bytes | Ty::Records) => Value::Bytes(Bytes::new()),
        (Def::Zero, Ty::Array(_)) => Value::Array(Vec::new()),
        (Def::Zero, Ty::Uuid) => Value::Uuid([0; 16]),
        (Def::Zero, Ty::Struct(fields)) => {
            let mut s = Struct::new();
            for fl in fields {
                s.push(fl.name, default_value(fl));
            }
            Value::Struct(s)
        }
        (Def::Zero, _) => Value::Int(0),
    }
}

/// Values a decoded message may hold: one per byte of the frame.
struct Budget(usize);

impl Budget {
    fn spend(&mut self) -> Result<()> {
        if self.0 == 0 {
            return Err(Error::TooLarge("more values than bytes in the request"));
        }
        self.0 -= 1;
        Ok(())
    }
}

/// Decode a request or response body of `schema` at `version`. The reader must hold exactly the body.
pub fn decode(schema: &MessageSchema, version: i16, r: &mut Reader<'_>) -> Result<Struct> {
    if version < schema.min || version > schema.max {
        return Err(Error::Unsupported { api_key: schema.api_key, version });
    }
    let flex = schema.flexible(version);
    let mut budget = Budget(r.remaining().max(16));
    // Bytes left after the message are ignored, as Apache Kafka does: librdkafka (2.x) writes an all-topics Metadata
    // v9+ request with a 4-byte placeholder where the compact array null is one byte, so three zero bytes follow it.
    decode_struct(schema.fields, version, flex, r, &mut budget)
}

fn decode_struct(fields: &'static [Field], v: i16, flex: bool, r: &mut Reader<'_>, budget: &mut Budget) -> Result<Struct> {
    let mut s = Struct::new();
    for f in fields {
        if f.tag >= 0 || !in_range(f, v) {
            s.push(f.name, default_value(f));
            continue;
        }
        let val = decode_value(&f.ty, f, v, flex, r, budget)?;
        s.push(f.name, val);
    }
    if flex {
        skip_tagged(r)?;
    }
    Ok(s)
}

/// Tagged fields: a count, then (tag, size, bytes) each. None is needed by the emulator, so they are skipped.
fn skip_tagged(r: &mut Reader<'_>) -> Result<()> {
    let n = r.uvarint()?;
    if n as usize > r.remaining() {
        return Err(Error::Invalid("tagged field count over the bytes left"));
    }
    for _ in 0..n {
        let _tag = r.uvarint()?;
        let size = r.uvarint()? as usize;
        r.take(size)?;
    }
    Ok(())
}

fn length(r: &mut Reader<'_>, flex: bool, nullable: bool, what: &'static str) -> Result<Option<usize>> {
    if flex {
        let n = r.uvarint()?;
        if n == 0 {
            return if nullable { Ok(None) } else { Err(Error::Invalid(what)) };
        }
        Ok(Some((n - 1) as usize))
    } else {
        let n = r.i32()?;
        if n < 0 {
            return if n == -1 && nullable { Ok(None) } else { Err(Error::Invalid(what)) };
        }
        Ok(Some(n as usize))
    }
}

fn decode_value(ty: &Ty, f: &Field, v: i16, flex: bool, r: &mut Reader<'_>, budget: &mut Budget) -> Result<Value> {
    budget.spend()?;
    let null_ok = nullable(f, v);
    Ok(match ty {
        Ty::Bool => Value::Bool(r.u8()? != 0),
        Ty::I8 => Value::Int(r.i8()? as i64),
        Ty::I16 => Value::Int(r.i16()? as i64),
        Ty::U16 => Value::Int(r.u16()? as i64),
        Ty::I32 => Value::Int(r.i32()? as i64),
        Ty::I64 => Value::Int(r.i64()?),
        Ty::F64 => Value::Int(r.f64()? as i64),
        Ty::Uuid => {
            let b = r.take(16)?;
            let mut u = [0u8; 16];
            u.copy_from_slice(b);
            Value::Uuid(u)
        }
        Ty::Str => {
            let s = if flex { r.compact_string()? } else { r.string()? };
            match s {
                Some(s) => Value::Str(s.to_string()),
                None if null_ok => Value::Null,
                None => return Err(Error::Invalid("null string in a field that cannot be null")),
            }
        }
        Ty::Bytes | Ty::Records => {
            let b = if flex { r.compact_bytes()? } else { r.bytes()? };
            match b {
                Some(b) => Value::Bytes(Bytes::copy_from_slice(b)),
                None if null_ok || matches!(ty, Ty::Records) => Value::Null,
                None => return Err(Error::Invalid("null bytes in a field that cannot be null")),
            }
        }
        Ty::Array(elem) => {
            let n = length(r, flex, null_ok, "bad array length")?;
            match n {
                None => Value::Null,
                Some(n) => {
                    // every element takes at least one byte, so a count over the bytes left is a lie
                    if n > r.remaining() {
                        return Err(Error::Invalid("array count over the bytes left"));
                    }
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        items.push(decode_value(elem, &ELEM, v, flex, r, budget)?);
                    }
                    Value::Array(items)
                }
            }
        }
        Ty::Struct(fields) => Value::Struct(decode_struct(fields, v, flex, r, budget)?),
    })
}

/// What an array element is: not null, present in every version.
static ELEM: Field = f("", Ty::Bool, 0, i16::MAX, 1, 0, -1, Def::Zero);

/// Encode `s` as a body of `schema` at `version`. Missing fields take their defaults.
pub fn encode(schema: &MessageSchema, version: i16, s: &Struct, w: &mut Writer) {
    encode_struct(schema.fields, version, schema.flexible(version), s, w);
}

fn encode_struct(fields: &'static [Field], v: i16, flex: bool, s: &Struct, w: &mut Writer) {
    for f in fields {
        if f.tag >= 0 || !in_range(f, v) {
            continue;
        }
        let val = match s.get(f.name) {
            Value::Null => {
                if nullable(f, v) {
                    Value::Null
                } else {
                    default_value(f)
                }
            }
            other => other.clone(),
        };
        encode_value(&f.ty, f, v, flex, &val, w);
    }
    if flex {
        w.uvarint(0); // no tagged fields
    }
}

fn write_len(w: &mut Writer, flex: bool, n: Option<usize>) {
    match (flex, n) {
        (true, None) => w.uvarint(0),
        (true, Some(n)) => w.uvarint(n as u32 + 1),
        (false, None) => w.i32(-1),
        (false, Some(n)) => w.i32(n as i32),
    }
}

fn encode_value(ty: &Ty, f: &Field, v: i16, flex: bool, val: &Value, w: &mut Writer) {
    match ty {
        Ty::Bool => w.u8(val.as_bool() as u8),
        Ty::I8 => w.i8(val.as_i64() as i8),
        Ty::I16 => w.i16(val.as_i64() as i16),
        Ty::U16 => w.u16(val.as_i64() as u16),
        Ty::I32 => w.i32(val.as_i64() as i32),
        Ty::I64 => w.i64(val.as_i64()),
        Ty::F64 => w.f64(val.as_i64() as f64),
        Ty::Uuid => match val {
            Value::Uuid(u) => w.raw(u),
            _ => w.raw(&[0; 16]),
        },
        Ty::Str => {
            let s = val.as_str();
            let s = if s.is_none() && !nullable(f, v) { Some("") } else { s };
            if flex {
                w.compact_string(s)
            } else {
                w.string(s)
            }
        }
        Ty::Bytes | Ty::Records => {
            let b = val.as_bytes().map(|b| &b[..]);
            let b = if b.is_none() && !nullable(f, v) && !matches!(ty, Ty::Records) { Some(&[][..]) } else { b };
            if flex {
                w.compact_bytes(b)
            } else {
                w.bytes(b)
            }
        }
        Ty::Array(elem) => match val {
            Value::Array(items) => {
                write_len(w, flex, Some(items.len()));
                for it in items {
                    encode_value(elem, &ELEM, v, flex, it, w);
                }
            }
            _ => write_len(w, flex, if nullable(f, v) { None } else { Some(0) }),
        },
        Ty::Struct(fields) => {
            let empty = Struct::new();
            encode_struct(fields, v, flex, val.as_struct().unwrap_or(&empty), w)
        }
    }
}
