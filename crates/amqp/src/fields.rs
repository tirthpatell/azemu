//! Helpers for reading the fields of described lists (performatives, terminus, outcomes...).

use crate::codec::DecodeError;
use crate::types::Value;

static NULL: Value = Value::Null;

/// The fields of a described list, with typed accessors that treat missing trailing fields as null.
pub struct Fields<'a>(pub &'a [Value]);

impl<'a> Fields<'a> {
    /// `v` must be a described list whose descriptor is the ulong `code` (or the symbol `name`).
    pub fn of(v: &'a Value, code: u64, name: &str) -> Result<Fields<'a>, DecodeError> {
        let d = v.as_described().ok_or(DecodeError::Invalid("expected a described value"))?;
        let ok = d.descriptor.as_u64() == Some(code) || d.descriptor.as_str() == Some(name);
        if !ok {
            return Err(DecodeError::Invalid("unexpected descriptor"));
        }
        match &d.value {
            Value::List(items) => Ok(Fields(items)),
            Value::Null => Ok(Fields(&[])),
            _ => Err(DecodeError::Invalid("described value is not a list")),
        }
    }

    pub fn get(&self, i: usize) -> &'a Value {
        self.0.get(i).unwrap_or(&NULL)
    }

    pub fn opt_u32(&self, i: usize) -> Result<Option<u32>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            v => v.as_u32().map(Some).ok_or(DecodeError::Invalid("expected uint")),
        }
    }
    pub fn u32_or(&self, i: usize, default: u32) -> Result<u32, DecodeError> {
        Ok(self.opt_u32(i)?.unwrap_or(default))
    }
    pub fn req_u32(&self, i: usize) -> Result<u32, DecodeError> {
        self.opt_u32(i)?.ok_or(DecodeError::Invalid("missing mandatory uint field"))
    }
    pub fn opt_u64(&self, i: usize) -> Result<Option<u64>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            v => v.as_u64().map(Some).ok_or(DecodeError::Invalid("expected ulong")),
        }
    }
    pub fn opt_u8(&self, i: usize) -> Result<Option<u8>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            v => v.as_u64().and_then(|x| u8::try_from(x).ok()).map(Some).ok_or(DecodeError::Invalid("expected ubyte")),
        }
    }
    pub fn opt_bool(&self, i: usize) -> Result<Option<bool>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            v => v.as_bool().map(Some).ok_or(DecodeError::Invalid("expected boolean")),
        }
    }
    pub fn bool_or(&self, i: usize, default: bool) -> Result<bool, DecodeError> {
        Ok(self.opt_bool(i)?.unwrap_or(default))
    }
    pub fn opt_string(&self, i: usize) -> Result<Option<String>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            v => v.as_str().map(|s| Some(s.to_string())).ok_or(DecodeError::Invalid("expected string")),
        }
    }
    pub fn req_string(&self, i: usize) -> Result<String, DecodeError> {
        self.opt_string(i)?.ok_or(DecodeError::Invalid("missing mandatory string field"))
    }
    pub fn opt_binary(&self, i: usize) -> Result<Option<Vec<u8>>, DecodeError> {
        match self.get(i) {
            Value::Null => Ok(None),
            Value::Binary(b) => Ok(Some(b.clone())),
            _ => Err(DecodeError::Invalid("expected binary")),
        }
    }
    pub fn symbols(&self, i: usize) -> Vec<String> {
        self.get(i).as_symbols()
    }
    pub fn opt_value(&self, i: usize) -> Option<Value> {
        match self.get(i) {
            Value::Null => None,
            v => Some(v.clone()),
        }
    }
}

/// Drop trailing nulls from a field list (AMQP allows omitting them).
pub fn trim(mut v: Vec<Value>) -> Vec<Value> {
    while matches!(v.last(), Some(Value::Null)) {
        v.pop();
    }
    v
}

pub fn opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map(Into::into).unwrap_or(Value::Null)
}

pub fn opt_sym(v: &Option<String>) -> Value {
    v.as_ref().map(|s| Value::Symbol(s.clone())).unwrap_or(Value::Null)
}

pub fn symbols(v: &[String]) -> Value {
    match v.len() {
        0 => Value::Null,
        _ => Value::Array(v.iter().map(|s| Value::Symbol(s.clone())).collect()),
    }
}

/// Build a described list from a code and its fields.
pub fn described(code: u64, fields: Vec<Value>) -> Value {
    Value::described_code(code, Value::List(trim(fields)))
}
