//! Operator semantics: comparison, arithmetic, bitwise, string concatenation.

use crate::ast::BinaryOp;
use crate::value::{cmp_num, cmp_total, values_equal, Num, Val};
use std::cmp::Ordering;

/// Comparison of two defined-type values per the documented table:
/// different types or undefined give undefined; objects and arrays only support `=`.
pub(crate) fn compare(op: BinaryOp, a: &Val, b: &Val) -> Val {
    use BinaryOp::*;
    let ord = match (a, b) {
        (Val::Null, Val::Null) => Ordering::Equal,
        (Val::Bool(_), Val::Bool(_)) | (Val::Str(_), Val::Str(_)) => cmp_total(a, b),
        (Val::Num(x), Val::Num(y)) => {
            if x.f64().is_nan() || y.f64().is_nan() {
                return Val::Bool(matches!(op, Ne));
            }
            cmp_num(*x, *y)
        }
        (Val::Arr(_), Val::Arr(_)) | (Val::Obj(_), Val::Obj(_)) => {
            return if op == Eq { Val::Bool(values_equal(a, b)) } else { Val::Undefined };
        }
        _ => return Val::Undefined,
    };
    Val::Bool(match op {
        Eq => ord == Ordering::Equal,
        Ne => ord != Ordering::Equal,
        Lt => ord == Ordering::Less,
        Le => ord != Ordering::Greater,
        Gt => ord == Ordering::Greater,
        Ge => ord != Ordering::Less,
        _ => return Val::Undefined,
    })
}

pub(crate) fn arith(op: BinaryOp, a: &Val, b: &Val) -> Val {
    let (x, y) = match (a, b) {
        (Val::Num(x), Val::Num(y)) => (*x, *y),
        _ => return Val::Undefined,
    };
    use BinaryOp::*;
    if let (Num::Int(p), Num::Int(q)) = (x, y) {
        let r = match op {
            Add => p.checked_add(q),
            Sub => p.checked_sub(q),
            Mul => p.checked_mul(q),
            Div => {
                if q != 0 && p.checked_rem(q) == Some(0) {
                    p.checked_div(q)
                } else {
                    None
                }
            }
            Mod => {
                if q != 0 {
                    p.checked_rem(q)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(r) = r {
            return Val::int(r);
        }
    }
    let (p, q) = (x.f64(), y.f64());
    Val::num(match op {
        Add => p + q,
        Sub => p - q,
        Mul => p * q,
        Div => p / q,
        Mod => p % q,
        _ => return Val::Undefined,
    })
}

pub(crate) fn negate(a: &Val) -> Val {
    match a {
        Val::Num(Num::Int(i)) => match i.checked_neg() {
            Some(n) => Val::int(n),
            None => Val::num(-(*i as f64)),
        },
        Val::Num(Num::Float(f)) => Val::num(-*f),
        _ => Val::Undefined,
    }
}

/// JavaScript `ToInt32`.
pub(crate) fn to_int32(f: f64) -> i32 {
    if !f.is_finite() {
        return 0;
    }
    let t = f.trunc();
    let m = t.rem_euclid(4294967296.0);
    (m as u64 as u32) as i32
}

pub(crate) fn bitwise(op: BinaryOp, a: &Val, b: &Val) -> Val {
    let (x, y) = match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (to_int32(x), to_int32(y)),
        _ => return Val::Undefined,
    };
    use BinaryOp::*;
    let sh = (y as u32) & 31;
    match op {
        BitAnd => Val::int((x & y) as i64),
        BitOr => Val::int((x | y) as i64),
        BitXor => Val::int((x ^ y) as i64),
        Shl => Val::int(x.wrapping_shl(sh) as i64),
        Shr => Val::int((x >> sh) as i64),
        UShr => Val::int(((x as u32) >> sh) as i64),
        _ => Val::Undefined,
    }
}

pub(crate) fn bit_not(a: &Val) -> Val {
    match a.as_f64() {
        Some(x) => Val::int((!to_int32(x)) as i64),
        None => Val::Undefined,
    }
}

pub(crate) fn concat(a: &Val, b: &Val) -> Val {
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => {
            let mut s = String::with_capacity(x.len() + y.len());
            s.push_str(x);
            s.push_str(y);
            Val::string(s)
        }
        _ => Val::Undefined,
    }
}

/// Three-valued AND of two results (`None` = undefined).
pub(crate) fn tri(v: &Val) -> Option<bool> {
    v.as_bool()
}

pub(crate) fn from_tri(v: Option<bool>) -> Val {
    match v {
        Some(b) => Val::Bool(b),
        None => Val::Undefined,
    }
}

pub(crate) fn tri_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(crate) fn tri_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
