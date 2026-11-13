//! Values of the query language and the conversions between them.

use std::cmp::Ordering;

use super::datetime::{self, Ts};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A JSON field that is not in the record (`IS MISSING`). Left out of JSON output.
    Missing,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Ts(Ts),
    Array(Vec<Value>),
    /// Fields in the order they were written.
    Object(Vec<(String, Value)>),
}

/// What went wrong while evaluating one row. A row that fails is skipped and reported as a non-fatal error
/// (`name`, `message`), except `Fatal`, which ends the query.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalError {
    pub name: &'static str,
    pub message: String,
    pub fatal: bool,
}

impl EvalError {
    pub fn new(name: &'static str, message: impl Into<String>) -> EvalError {
        EvalError { name, message: message.into(), fatal: false }
    }

    pub fn fatal(name: &'static str, message: impl Into<String>) -> EvalError {
        EvalError { name, message: message.into(), fatal: true }
    }
}

pub type EvalResult<T> = Result<T, EvalError>;

impl Value {
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Null | Value::Missing)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Missing => "MISSING",
            Value::Null => "NULL",
            Value::Bool(_) => "BOOLEAN",
            Value::Int(_) => "INT",
            Value::Float(_) => "FLOAT",
            Value::Str(_) => "STRING",
            Value::Ts(_) => "TIMESTAMP",
            Value::Array(_) => "ARRAY",
            Value::Object(_) => "OBJECT",
        }
    }

    /// The text of a scalar, the way it is written into delimited output and `CAST(.. AS STRING)`.
    /// Arrays and objects are written as JSON.
    pub fn to_text(&self) -> String {
        match self {
            Value::Missing | Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => float_text(*f),
            Value::Str(s) => s.clone(),
            Value::Ts(t) => datetime::format(t),
            Value::Array(_) | Value::Object(_) => {
                let mut out = Vec::new();
                super::json::write(self, &mut out);
                String::from_utf8_lossy(&out).into_owned()
            }
        }
    }
}

/// `1.5`, `3` (no `.0`), `1e300` as a plain number; NaN and infinities as their names.
pub fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        format!("{f}")
    }
}

/// A string read as a number: an integer when it is one, a float otherwise.
pub fn parse_number(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Some(Value::Int(i));
    }
    // Rust accepts `inf`, `nan` and `infinity`; the language does not read those from data
    if t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')) {
        if let Ok(f) = t.parse::<f64>() {
            if f.is_finite() {
                return Some(Value::Float(f));
            }
        }
    }
    None
}

pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn cast_error(from: &str, to: &str) -> EvalError {
    EvalError::new("CastFailed", format!("Cannot convert the value to {to}: {from}"))
}

/// Number for arithmetic and ordering: a string that holds a number is read as one.
pub fn to_number(v: &Value) -> EvalResult<Value> {
    match v {
        Value::Int(_) | Value::Float(_) => Ok(v.clone()),
        Value::Str(s) => parse_number(s).ok_or_else(|| cast_error(&format!("'{}' is not a number", short(s)), "a number")),
        Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
        other => Err(cast_error(other.type_name(), "a number")),
    }
}

fn short(s: &str) -> String {
    s.chars().take(40).collect()
}

pub fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

/// `CAST(v AS ty)`.
pub fn cast(v: &Value, ty: CastType) -> EvalResult<Value> {
    if v.is_nullish() {
        return Ok(Value::Null);
    }
    match ty {
        CastType::Int => match v {
            Value::Int(_) => Ok(v.clone()),
            Value::Float(f) if f.is_finite() && f.abs() < 9.2e18 => Ok(Value::Int(f.trunc() as i64)),
            Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
            Value::Str(s) => match parse_number(s) {
                Some(Value::Int(i)) => Ok(Value::Int(i)),
                Some(Value::Float(f)) if f.abs() < 9.2e18 => Ok(Value::Int(f.trunc() as i64)),
                _ => Err(cast_error(&format!("'{}'", short(s)), "INT")),
            },
            other => Err(cast_error(other.type_name(), "INT")),
        },
        CastType::Float => match v {
            Value::Int(i) => Ok(Value::Float(*i as f64)),
            Value::Float(_) => Ok(v.clone()),
            Value::Str(s) => match parse_number(s) {
                Some(n) => Ok(Value::Float(as_f64(&n))),
                None => Err(cast_error(&format!("'{}'", short(s)), "FLOAT")),
            },
            other => Err(cast_error(other.type_name(), "FLOAT")),
        },
        CastType::Str => Ok(Value::Str(v.to_text())),
        CastType::Bool => match v {
            Value::Bool(_) => Ok(v.clone()),
            Value::Int(i) => Ok(Value::Bool(*i != 0)),
            Value::Str(s) => parse_bool(s).map(Value::Bool).ok_or_else(|| cast_error(&format!("'{}'", short(s)), "BOOLEAN")),
            other => Err(cast_error(other.type_name(), "BOOLEAN")),
        },
        CastType::Timestamp => match v {
            Value::Ts(_) => Ok(v.clone()),
            Value::Str(s) => datetime::parse(s).map(Value::Ts).ok_or_else(|| cast_error(&format!("'{}'", short(s)), "TIMESTAMP")),
            other => Err(cast_error(other.type_name(), "TIMESTAMP")),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastType {
    Int,
    Float,
    Str,
    Bool,
    Timestamp,
}

/// Comparison of two values. Different kinds are made alike first: a string next to a number is read as a
/// number, next to a timestamp as a timestamp, next to a boolean as a boolean (data read from CSV is all
/// text). `Ok(None)` when either side is NULL or MISSING (the comparison is unknown).
pub fn compare(a: &Value, b: &Value) -> EvalResult<Option<Ordering>> {
    if a.is_nullish() || b.is_nullish() {
        return Ok(None);
    }
    use Value::*;
    Ok(match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Int(_) | Float(_), Int(_) | Float(_)) => as_f64(a).partial_cmp(&as_f64(b)),
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Ts(x), Ts(y)) => x.partial_cmp(y),
        (Str(_), Int(_) | Float(_)) | (Int(_) | Float(_), Str(_)) => {
            let (x, y) = (to_number(a)?, to_number(b)?);
            return compare(&x, &y);
        }
        (Str(s), Ts(_)) => return compare(&cast(&Value::Str(s.clone()), CastType::Timestamp)?, b),
        (Ts(_), Str(s)) => return compare(a, &cast(&Value::Str(s.clone()), CastType::Timestamp)?),
        (Str(s), Bool(_)) => return compare(&cast(&Value::Str(s.clone()), CastType::Bool)?, b),
        (Bool(_), Str(s)) => return compare(a, &cast(&Value::Str(s.clone()), CastType::Bool)?),
        (x, y) => return Err(EvalError::new("CastFailed", format!("Cannot compare {} with {}.", x.type_name(), y.type_name()))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_next_to_numbers_are_read_as_numbers() {
        assert_eq!(compare(&Value::Str("300".into()), &Value::Int(250)).unwrap(), Some(Ordering::Greater));
        assert_eq!(compare(&Value::Str("1.5".into()), &Value::Float(1.5)).unwrap(), Some(Ordering::Equal));
        assert!(compare(&Value::Str("abc".into()), &Value::Int(1)).is_err());
        assert_eq!(compare(&Value::Str("b".into()), &Value::Str("a".into())).unwrap(), Some(Ordering::Greater));
        assert_eq!(compare(&Value::Null, &Value::Int(1)).unwrap(), None);
    }

    #[test]
    fn casts() {
        assert_eq!(cast(&Value::Str(" 12 ".into()), CastType::Int).unwrap(), Value::Int(12));
        assert_eq!(cast(&Value::Str("12.9".into()), CastType::Int).unwrap(), Value::Int(12));
        assert!(cast(&Value::Str("nan".into()), CastType::Float).is_err());
        assert_eq!(cast(&Value::Int(3), CastType::Str).unwrap(), Value::Str("3".into()));
        assert_eq!(cast(&Value::Str("TRUE".into()), CastType::Bool).unwrap(), Value::Bool(true));
        assert_eq!(cast(&Value::Null, CastType::Int).unwrap(), Value::Null);
        assert_eq!(float_text(2.5), "2.5");
        assert_eq!(float_text(3.0), "3");
    }
}
