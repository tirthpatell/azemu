//! Math, integer and bit functions.

use super::{f, num_fn, FuncSpec};
use crate::eval::Ctx;
use crate::value::{Num, Val};

pub(super) static FUNCS: &[FuncSpec] = &[
    f("ABS", 1, 1, abs),
    f("ACOS", 1, 1, acos),
    f("ASIN", 1, 1, asin),
    f("ATAN", 1, 1, atan),
    f("ATN2", 2, 2, |_, a| bin(a, |y, x| y.atan2(x))),
    f("CEILING", 1, 1, ceiling),
    f("COS", 1, 1, cos),
    f("COT", 1, 1, cot),
    f("DEGREES", 1, 1, degrees),
    f("EXP", 1, 1, exp),
    f("FLOOR", 1, 1, floor),
    f("LOG", 1, 2, log),
    f("LOG10", 1, 1, log10),
    f("PI", 0, 0, |_, _| Val::num(std::f64::consts::PI)),
    f("POWER", 2, 2, |_, a| bin(a, f64::powf)),
    f("RADIANS", 1, 1, radians),
    f("RAND", 0, 0, |c, _| Val::num(c.rand())),
    f("ROUND", 1, 2, round),
    f("SIGN", 1, 1, sign),
    f("SIN", 1, 1, sin),
    f("SQRT", 1, 1, sqrt),
    f("SQUARE", 1, 1, square),
    f("TAN", 1, 1, tan),
    f("TRUNC", 1, 1, trunc),
    f("NUMBERBIN", 1, 2, number_bin),
    f("INTADD", 2, 2, |_, a| int2(a, |x, y| x.checked_add(y))),
    f("INTSUB", 2, 2, |_, a| int2(a, |x, y| x.checked_sub(y))),
    f("INTMUL", 2, 2, |_, a| int2(a, |x, y| x.checked_mul(y))),
    f("INTDIV", 2, 2, |_, a| int2(a, |x, y| x.checked_div(y))),
    f("INTMOD", 2, 2, |_, a| int2(a, |x, y| x.checked_rem(y))),
    f("INTBITAND", 2, 2, |_, a| int2(a, |x, y| Some(x & y))),
    f("INTBITOR", 2, 2, |_, a| int2(a, |x, y| Some(x | y))),
    f("INTBITXOR", 2, 2, |_, a| int2(a, |x, y| Some(x ^ y))),
    f("INTBITLEFTSHIFT", 2, 2, |_, a| int2(a, |x, y| u32::try_from(y).ok().and_then(|s| x.checked_shl(s)))),
    f("INTBITRIGHTSHIFT", 2, 2, |_, a| int2(a, |x, y| u32::try_from(y).ok().and_then(|s| x.checked_shr(s)))),
    f("INTBITNOT", 1, 1, |_, a| match a[0].as_i64() {
        Some(x) => Val::int(!x),
        None => Val::Undefined,
    }),
];

fn bin(a: &[Val], op: fn(f64, f64) -> f64) -> Val {
    match (a[0].as_f64(), a[1].as_f64()) {
        (Some(x), Some(y)) => Val::num(op(x, y)),
        _ => Val::Undefined,
    }
}

fn int2(a: &[Val], op: fn(i64, i64) -> Option<i64>) -> Val {
    match (a[0].as_i64(), a[1].as_i64()) {
        (Some(x), Some(y)) => op(x, y).map(Val::int).unwrap_or(Val::Undefined),
        _ => Val::Undefined,
    }
}

fn abs(_: &Ctx, a: &[Val]) -> Val {
    match &a[0] {
        Val::Num(Num::Int(i)) => i.checked_abs().map(Val::int).unwrap_or_else(|| Val::num((*i as f64).abs())),
        Val::Num(Num::Float(x)) => Val::num(x.abs()),
        _ => Val::Undefined,
    }
}

/// Rounds an integer-valued input to itself, else applies `op`.
fn keep_int(v: &Val, op: fn(f64) -> f64) -> Val {
    match v {
        Val::Num(Num::Int(i)) => Val::int(*i),
        Val::Num(Num::Float(x)) => Val::num(op(*x)),
        _ => Val::Undefined,
    }
}

fn ceiling(_: &Ctx, a: &[Val]) -> Val {
    keep_int(&a[0], f64::ceil)
}
fn floor(_: &Ctx, a: &[Val]) -> Val {
    keep_int(&a[0], f64::floor)
}
fn trunc(_: &Ctx, a: &[Val]) -> Val {
    keep_int(&a[0], f64::trunc)
}

num_fn!(acos, |x| Val::num(x.acos()));
num_fn!(asin, |x| Val::num(x.asin()));
num_fn!(atan, |x| Val::num(x.atan()));
num_fn!(cos, |x| Val::num(x.cos()));
num_fn!(sin, |x| Val::num(x.sin()));
num_fn!(tan, |x| Val::num(x.tan()));
num_fn!(cot, |x| Val::num(1.0 / x.tan()));
num_fn!(degrees, |x| Val::num(x * 180.0 / std::f64::consts::PI));
num_fn!(radians, |x| Val::num(x * std::f64::consts::PI / 180.0));
num_fn!(exp, |x| Val::num(x.exp()));
num_fn!(log10, |x| Val::num(x.log10()));
num_fn!(sqrt, |x| Val::num(x.sqrt()));
num_fn!(sign, |x| Val::int(if x > 0.0 {
    1
} else if x < 0.0 {
    -1
} else {
    0
}));

fn square(_: &Ctx, a: &[Val]) -> Val {
    match &a[0] {
        Val::Num(Num::Int(i)) => i.checked_mul(*i).map(Val::int).unwrap_or_else(|| Val::num((*i as f64) * (*i as f64))),
        Val::Num(Num::Float(x)) => Val::num(x * x),
        _ => Val::Undefined,
    }
}

fn log(_: &Ctx, a: &[Val]) -> Val {
    let x = match a[0].as_f64() {
        Some(x) => x,
        None => return Val::Undefined,
    };
    match a.get(1) {
        None => Val::num(x.ln()),
        Some(b) => match b.as_f64() {
            Some(10.0) => Val::num(x.log10()),
            Some(2.0) => Val::num(x.log2()),
            Some(base) => Val::num(x.ln() / base.ln()),
            None => Val::Undefined,
        },
    }
}

fn round(_: &Ctx, a: &[Val]) -> Val {
    let x = match a[0].as_f64() {
        Some(x) => x,
        None => return Val::Undefined,
    };
    let digits = match a.get(1) {
        None => 0,
        Some(v) => match v.as_i64() {
            Some(d) if d.unsigned_abs() <= 308 => d,
            _ => return Val::Undefined,
        },
    };
    if digits == 0 {
        return keep_int(&a[0], f64::round);
    }
    if let Val::Num(Num::Int(i)) = &a[0] {
        if digits > 0 {
            return Val::int(*i);
        }
    }
    let p = 10f64.powi(digits.unsigned_abs() as i32);
    Val::num(if digits > 0 { (x * p).round() / p } else { (x / p).round() * p })
}

fn number_bin(_: &Ctx, a: &[Val]) -> Val {
    let x = match a[0].as_f64() {
        Some(x) => x,
        None => return Val::Undefined,
    };
    let size = match a.get(1) {
        None => 1.0,
        Some(v) => match v.as_f64() {
            Some(s) => s,
            None => return Val::Undefined,
        },
    };
    if size == 0.0 || !size.is_finite() {
        return Val::Undefined;
    }
    Val::num((x / size).floor() * size)
}
