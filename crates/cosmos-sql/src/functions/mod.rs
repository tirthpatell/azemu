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
    unsupported("DOCUMENTID"),
    unsupported("ST_DISTANCE"),
    unsupported("ST_WITHIN"),
    unsupported("ST_INTERSECTS"),
    unsupported("ST_ISVALID"),
    unsupported("ST_ISVALIDDETAILED"),
    unsupported("ST_AREA"),
    unsupported("VECTORDISTANCE"),
    unsupported("FULLTEXTCONTAINS"),
    unsupported("FULLTEXTCONTAINSALL"),
    unsupported("FULLTEXTCONTAINSANY"),
    unsupported("FULLTEXTSCORE"),
    unsupported("RRF"),
];

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
