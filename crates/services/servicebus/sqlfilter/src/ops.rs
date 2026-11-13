//! Typed operators. `None` means "unknown".

use crate::ast::BinOp;
use crate::error::EvalError;
use crate::value::Value;

pub(crate) type Val = Option<Value>;

fn cmp_ord<T: PartialOrd>(op: BinOp, a: T, b: T) -> Option<bool> {
    Some(match op {
        BinOp::Eq => a == b,
        BinOp::Ne => a != b,
        BinOp::Lt => a < b,
        BinOp::Le => a <= b,
        BinOp::Gt => a > b,
        BinOp::Ge => a >= b,
        _ => return None,
    })
}

/// Comparison with C#-style promotion: integers and doubles compare as
/// doubles when mixed; strings compare ordinally; booleans support only
/// `=` and `<>`; incompatible types (for example a string and a number)
/// are unknown, so neither `=` nor `<>` is true.
pub(crate) fn compare(op: BinOp, l: &Value, r: &Value) -> Option<bool> {
    match (l, r) {
        (Value::Long(a), Value::Long(b)) => cmp_ord(op, a, b),
        (Value::Long(a), Value::Double(b)) => cmp_ord(op, *a as f64, *b),
        (Value::Double(a), Value::Long(b)) => cmp_ord(op, *a, *b as f64),
        (Value::Double(a), Value::Double(b)) => cmp_ord(op, a, b),
        (Value::Str(a), Value::Str(b)) => cmp_ord(op, a, b),
        (Value::Timestamp(a), Value::Timestamp(b)) => cmp_ord(op, a, b),
        (Value::Bool(a), Value::Bool(b)) => match op {
            BinOp::Eq => Some(a == b),
            BinOp::Ne => Some(a != b),
            _ => None,
        },
        _ => None,
    }
}

fn concat_text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Long(n) => Some(n.to_string()),
        Value::Double(f) => Some(f.to_string()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.to_string()),
        _ => None,
    }
}

fn float_op(op: BinOp, a: f64, b: f64) -> Value {
    Value::Double(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        _ => a % b,
    })
}

/// Arithmetic (`+ - * / %`). Integer arithmetic wraps on overflow
/// (C# unchecked); integer division or remainder by zero is an error;
/// double division by zero gives infinity/NaN. `+` with a string on either
/// side concatenates, as in C#. Timestamps support `ts - ts` (milliseconds,
/// as a `Long`) and `ts +/- long` (milliseconds).
pub(crate) fn arith(op: BinOp, l: &Value, r: &Value) -> Result<Val, EvalError> {
    let v = match (l, r) {
        (Value::Long(a), Value::Long(b)) => match op {
            BinOp::Add => Value::Long(a.wrapping_add(*b)),
            BinOp::Sub => Value::Long(a.wrapping_sub(*b)),
            BinOp::Mul => Value::Long(a.wrapping_mul(*b)),
            _ if *b == 0 => return Err(EvalError::new("attempted to divide by zero")),
            BinOp::Div => Value::Long(a.wrapping_div(*b)),
            _ => Value::Long(a.wrapping_rem(*b)),
        },
        (Value::Long(a), Value::Double(b)) => float_op(op, *a as f64, *b),
        (Value::Double(a), Value::Long(b)) => float_op(op, *a, *b as f64),
        (Value::Double(a), Value::Double(b)) => float_op(op, *a, *b),
        (Value::Str(_), _) | (_, Value::Str(_)) if op == BinOp::Add => match (concat_text(l), concat_text(r)) {
            (Some(a), Some(b)) => Value::Str(a + &b),
            _ => return Ok(None),
        },
        (Value::Timestamp(a), Value::Timestamp(b)) if op == BinOp::Sub => Value::Long(a.wrapping_sub(*b)),
        (Value::Timestamp(a), Value::Long(b)) if op == BinOp::Add => Value::Timestamp(a.wrapping_add(*b)),
        (Value::Timestamp(a), Value::Long(b)) if op == BinOp::Sub => Value::Timestamp(a.wrapping_sub(*b)),
        (Value::Long(a), Value::Timestamp(b)) if op == BinOp::Add => Value::Timestamp(a.wrapping_add(*b)),
        _ => return Ok(None),
    };
    Ok(Some(v))
}

pub(crate) fn negate(v: &Value) -> Val {
    match v {
        Value::Long(n) => Some(Value::Long(n.wrapping_neg())),
        Value::Double(f) => Some(Value::Double(-f)),
        _ => None,
    }
}

/// A random version-4 GUID in the usual 8-4-4-4-12 text form.
pub(crate) fn new_guid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(n);
    h1.write_u64(nanos);
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(nanos.rotate_left(17));
    h2.write_u64(n);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&h1.finish().to_le_bytes());
    bytes[8..].copy_from_slice(&h2.finish().to_le_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", hex[0..4].concat(), hex[4..6].concat(), hex[6..8].concat(), hex[8..10].concat(), hex[10..16].concat())
}
