//! A decoded or to-be-encoded message: a tree of named values. The layout comes from the schema, so one
//! representation serves every API in both directions.

use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    /// A null string, bytes or array, and the value of a field that is not there.
    #[default]
    Null,
    Bool(bool),
    /// Every integer width (int8 to int64) is held as an i64; the schema decides the width on the wire.
    Int(i64),
    Uuid([u8; 16]),
    Str(String),
    /// Bytes and records.
    Bytes(Bytes),
    Array(Vec<Value>),
    Struct(Struct),
}

pub static NULL: Value = Value::Null;

impl Value {
    pub fn as_i64(&self) -> i64 {
        match self {
            Value::Int(i) => *i,
            Value::Bool(b) => *b as i64,
            _ => 0,
        }
    }
    pub fn as_i32(&self) -> i32 {
        self.as_i64() as i32
    }
    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            _ => false,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_struct(&self) -> Option<&Struct> {
        match self {
            Value::Struct(s) => Some(s),
            _ => None,
        }
    }
    /// The elements of an array; empty for null.
    pub fn items(&self) -> &[Value] {
        match self {
            Value::Array(a) => a,
            _ => &[],
        }
    }
    /// The elements of an array of structs.
    pub fn structs(&self) -> impl Iterator<Item = &Struct> {
        self.items().iter().filter_map(Value::as_struct)
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

macro_rules! from_int {
    ($($t:ty),*) => { $(impl From<$t> for Value { fn from(v: $t) -> Value { Value::Int(v as i64) } })* };
}
from_int!(i8, i16, i32, i64, u16, u32);

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Str(v)
    }
}
impl From<Option<String>> for Value {
    fn from(v: Option<String>) -> Value {
        v.map(Value::Str).unwrap_or(Value::Null)
    }
}
impl From<Bytes> for Value {
    fn from(v: Bytes) -> Value {
        Value::Bytes(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Bytes(Bytes::from(v))
    }
}
impl From<Struct> for Value {
    fn from(v: Struct) -> Value {
        Value::Struct(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Value {
        Value::Array(v)
    }
}
impl From<Vec<Struct>> for Value {
    fn from(v: Vec<Struct>) -> Value {
        Value::Array(v.into_iter().map(Value::Struct).collect())
    }
}
impl From<Vec<i32>> for Value {
    fn from(v: Vec<i32>) -> Value {
        Value::Array(v.into_iter().map(|i| Value::Int(i as i64)).collect())
    }
}
impl From<Vec<String>> for Value {
    fn from(v: Vec<String>) -> Value {
        Value::Array(v.into_iter().map(Value::Str).collect())
    }
}

/// Named fields in wire order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Struct {
    fields: Vec<(&'static str, Value)>,
}

impl Struct {
    pub fn new() -> Struct {
        Struct::default()
    }

    pub fn with(mut self, name: &'static str, v: impl Into<Value>) -> Struct {
        self.set(name, v);
        self
    }

    pub fn set(&mut self, name: &'static str, v: impl Into<Value>) {
        let v = v.into();
        if let Some(slot) = self.fields.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = v;
        } else {
            self.fields.push((name, v));
        }
    }

    pub(crate) fn push(&mut self, name: &'static str, v: Value) {
        self.fields.push((name, v));
    }

    /// The field, or null when the struct has none of that name.
    pub fn get(&self, name: &str) -> &Value {
        self.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v).unwrap_or(&NULL)
    }

    pub fn i16(&self, name: &str) -> i16 {
        self.get(name).as_i64() as i16
    }
    pub fn i32(&self, name: &str) -> i32 {
        self.get(name).as_i64() as i32
    }
    pub fn i64(&self, name: &str) -> i64 {
        self.get(name).as_i64()
    }
    pub fn bool(&self, name: &str) -> bool {
        self.get(name).as_bool()
    }
    pub fn str(&self, name: &str) -> Option<&str> {
        self.get(name).as_str()
    }
    pub fn items(&self, name: &str) -> &[Value] {
        self.get(name).items()
    }
    pub fn structs(&self, name: &str) -> impl Iterator<Item = &Struct> {
        self.get(name).structs()
    }
    pub fn bytes(&self, name: &str) -> Option<&Bytes> {
        self.get(name).as_bytes()
    }
}
