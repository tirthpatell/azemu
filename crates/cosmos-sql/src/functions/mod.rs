//! Built-in system functions, split by family. Each family exports a `FUNCS` table.

use crate::eval::Ctx;
use crate::value::Val;

mod array;
mod math;
mod string;
mod time;
mod types;

/// Signature of a built-in implementation. Arguments are already evaluated.
pub(crate) type FuncImpl = fn(&Ctx, &[Val]) -> Val;

/// Registry entry for a built-in function.
#[derive(Debug)]
pub struct FuncSpec {
    pub(crate) name: &'static str,
    pub(crate) min: u8,
    pub(crate) max: u8,
    /// If false, an undefined argument makes the result undefined without calling `imp`.
    pub(crate) undef_ok: bool,
    /// Known function that azemu does not implement.
    pub(crate) unsupported: bool,
    pub(crate) imp: FuncImpl,
}

pub(crate) const VARIADIC: u8 = 255;

pub(crate) const fn f(name: &'static str, min: u8, max: u8, imp: FuncImpl) -> FuncSpec {
    FuncSpec { name, min, max, undef_ok: false, unsupported: false, imp }
}

/// Like [`f`], but the implementation sees undefined arguments.
pub(crate) const fn fu(name: &'static str, min: u8, max: u8, imp: FuncImpl) -> FuncSpec {
    FuncSpec { name, min, max, undef_ok: true, unsupported: false, imp }
}

fn never(_: &Ctx, _: &[Val]) -> Val {
    Val::Undefined
}

const fn unsupported(name: &'static str) -> FuncSpec {
    FuncSpec { name, min: 0, max: VARIADIC, undef_ok: true, unsupported: true, imp: never }
}

static MISC: &[FuncSpec] = &[
    f("IIF", 3, 3, iif),
    fu("CHOOSE", 2, VARIADIC, choose),
    f("DOCUMENTID", 1, 1, document_id),
    unsupported("ST_DISTANCE"),
    unsupported("ST_WITHIN"),
    unsupported("ST_INTERSECTS"),
    unsupported("ST_ISVALID"),
    unsupported("ST_ISVALIDDETAILED"),
    unsupported("ST_AREA"),
    f("VECTORDISTANCE", 2, 4, vector_distance),
    unsupported("FULLTEXTCONTAINS"),
    unsupported("FULLTEXTCONTAINSALL"),
    unsupported("FULLTEXTCONTAINSANY"),
    unsupported("FULLTEXTSCORE"),
];

/// `DOCUMENTID(c)`: the number inside the `_rid` of the item (its counter in the container).
fn document_id(_: &Ctx, a: &[Val]) -> Val {
    let rid = match a[0].get_prop("_rid") {
        Val::Str(s) => s,
        _ => return Val::Undefined,
    };
    // standard base64 (`-` stands for `/`); the counter is in the first seven of the last eight bytes, little endian
    let mut bytes: Vec<u8> = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in rid.chars() {
        let v = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' | '-' => 63,
            '=' => break,
            _ => return Val::Undefined,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bytes.len() < 16 {
        return Val::Undefined;
    }
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&bytes[bytes.len() - 8..]);
    eight[7] = 0;
    Val::int(i64::from_le_bytes(eight))
}

/// `VectorDistance(a, b [, bruteForce [, {distanceFunction: 'cosine' | 'dotproduct' | 'euclidean'}]])` over two numeric arrays of the
/// same length. There is no vector index here: every call is a brute force one.
fn vector_distance(_: &Ctx, a: &[Val]) -> Val {
    let nums = |v: &Val| -> Option<Vec<f64>> { v.as_arr()?.iter().map(Val::as_f64).collect() };
    let (x, y) = match (nums(&a[0]), nums(&a[1])) {
        (Some(x), Some(y)) if x.len() == y.len() && !x.is_empty() => (x, y),
        _ => return Val::Undefined,
    };
    let function = match a.get(3) {
        None => "cosine".to_string(),
        Some(o) => match o.get_prop("distanceFunction") {
            Val::Str(s) => s.to_ascii_lowercase(),
            Val::Undefined => "cosine".to_string(),
            _ => return Val::Undefined,
        },
    };
    let dot: f64 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
    match function.as_str() {
        "dotproduct" => Val::num(dot),
        "euclidean" => Val::num(x.iter().zip(&y).map(|(p, q)| (p - q) * (p - q)).sum::<f64>().sqrt()),
        "cosine" => {
            let nx: f64 = x.iter().map(|p| p * p).sum::<f64>().sqrt();
            let ny: f64 = y.iter().map(|p| p * p).sum::<f64>().sqrt();
            if nx == 0.0 || ny == 0.0 {
                return Val::Undefined;
            }
            Val::num(dot / (nx * ny))
        }
        _ => Val::Undefined,
    }
}

fn iif(_: &Ctx, a: &[Val]) -> Val {
    if a[0].as_bool() == Some(true) {
        a[1].clone()
    } else {
        a[2].clone()
    }
}

fn choose(_: &Ctx, a: &[Val]) -> Val {
    match a[0].as_i64() {
        Some(i) if i >= 1 && (i as usize) < a.len() => a[i as usize].clone(),
        _ => Val::Undefined,
    }
}

/// Finds a function by (case-insensitive) name.
pub(crate) fn lookup(name: &str) -> Option<&'static FuncSpec> {
    let up = name.to_ascii_uppercase();
    for table in [MISC, types::FUNCS, string::FUNCS, math::FUNCS, array::FUNCS, time::FUNCS] {
        if let Some(s) = table.iter().find(|s| s.name == up) {
            return Some(s);
        }
    }
    None
}

/// Calls a resolved function.
pub(crate) fn call(spec: &FuncSpec, ctx: &Ctx, args: &[Val]) -> Val {
    if !spec.undef_ok && args.iter().any(|a| a.is_undefined()) {
        return Val::Undefined;
    }
    (spec.imp)(ctx, args)
}

/// Declares `fn name(&Ctx, &[Val]) -> Val` over one string argument.
macro_rules! str_fn {
    ($name:ident, |$s:ident| $body:expr) => {
        fn $name(_: &Ctx, a: &[Val]) -> Val {
            match &a[0] {
                Val::Str($s) => $body,
                _ => Val::Undefined,
            }
        }
    };
}
/// Declares a function over one numeric argument (as f64).
macro_rules! num_fn {
    ($name:ident, |$x:ident| $body:expr) => {
        fn $name(_: &Ctx, a: &[Val]) -> Val {
            match a[0].as_f64() {
                Some($x) => $body,
                None => Val::Undefined,
            }
        }
    };
}
pub(crate) use num_fn;
pub(crate) use str_fn;
