//! The AMQP 1.0 type system as a value tree.
//!
//! `Value` holds every primitive type (OASIS AMQP 1.0 part 1, section 1.6), the composite
//! containers (list, map, array) and described values. The compact wire encodings
//! (`smallint`, `uint0`, `list8`, ...) are a codec detail: decoding maps them all to the same
//! variant and encoding picks the smallest form.

use std::fmt;

/// A described value: `descriptor` is usually a symbol or a ulong.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub descriptor: Value,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Ubyte(u8),
    Ushort(u16),
    Uint(u32),
    Ulong(u64),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Decimal32([u8; 4]),
    Decimal64([u8; 8]),
    Decimal128([u8; 16]),
    Char(char),
    /// Milliseconds since the Unix epoch.
    Timestamp(i64),
    Uuid([u8; 16]),
    Binary(Vec<u8>),
    String(String),
    Symbol(String),
    List(Vec<Value>),
    /// Key/value pairs in wire order (AMQP maps are ordered).
    Map(Vec<(Value, Value)>),
    /// All elements share one constructor on the wire.
    Array(Vec<Value>),
    Described(Box<Described>),
}

impl Value {
    pub fn sym(s: impl Into<String>) -> Value {
        Value::Symbol(s.into())
    }
    pub fn str(s: impl Into<String>) -> Value {
        Value::String(s.into())
    }
    pub fn described(descriptor: Value, value: Value) -> Value {
        Value::Described(Box::new(Described { descriptor, value }))
    }
    /// A described value whose descriptor is a ulong code.
    pub fn described_code(code: u64, value: Value) -> Value {
        Value::described(Value::Ulong(code), value)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Any unsigned or non-negative signed integer as u64.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Ubyte(v) => Some(*v as u64),
            Value::Ushort(v) => Some(*v as u64),
            Value::Uint(v) => Some(*v as u64),
            Value::Ulong(v) => Some(*v),
            Value::Byte(v) if *v >= 0 => Some(*v as u64),
            Value::Short(v) if *v >= 0 => Some(*v as u64),
            Value::Int(v) if *v >= 0 => Some(*v as u64),
            Value::Long(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }
    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|v| u32::try_from(v).ok())
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Byte(v) => Some(*v as i64),
            Value::Short(v) => Some(*v as i64),
            Value::Int(v) => Some(*v as i64),
            Value::Long(v) => Some(*v),
            Value::Timestamp(v) => Some(*v),
            other => other.as_u64().and_then(|v| i64::try_from(v).ok()),
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            other => other.as_i64().map(|v| v as f64),
        }
    }
    /// String or symbol contents.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::Symbol(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(v) | Value::Array(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Map(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_described(&self) -> Option<&Described> {
        match self {
            Value::Described(d) => Some(d),
            _ => None,
        }
    }
    /// Look up a string/symbol key in a map value.
    pub fn map_get(&self, key: &str) -> Option<&Value> {
        self.as_map()?.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
    }

    /// `multiple` fields: a single symbol or an array/list of symbols.
    pub fn as_symbols(&self) -> Vec<String> {
        match self {
            Value::Symbol(s) | Value::String(s) => vec![s.clone()],
            Value::Array(v) | Value::List(v) => v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        }
    }

    /// Strip any described wrapper(s) and return the inner value.
    pub fn undescribed(&self) -> &Value {
        match self {
            Value::Described(d) => d.value.undescribed(),
            v => v,
        }
    }

    /// Descriptor as a ulong code, when it is one.
    pub fn descriptor_code(&self) -> Option<u64> {
        self.as_described().and_then(|d| d.descriptor.as_u64())
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Uint(v)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::Ulong(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Long(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Binary(v)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Symbol(s) => write!(f, ":{s}"),
            Value::Binary(b) => write!(f, "b[{}]", b.len()),
            Value::List(v) | Value::Array(v) => {
                write!(f, "[")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "]")
            }
            Value::Map(v) => {
                write!(f, "{{")?;
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {x}")?;
                }
                write!(f, "}}")
            }
            Value::Described(d) => write!(f, "@{}({})", d.descriptor, d.value),
            other => write!(f, "{other:?}"),
        }
    }
}
