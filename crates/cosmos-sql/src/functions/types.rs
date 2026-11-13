//! Type-checking functions.

use super::{f, fu, FuncSpec};
use crate::eval::Ctx;
use crate::value::{Num, Val};

pub(super) static FUNCS: &[FuncSpec] = &[
    fu("IS_DEFINED", 1, 1, |_, a| Val::Bool(!a[0].is_undefined())),
    fu("IS_NULL", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Null))),
    fu("IS_ARRAY", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Arr(_)))),
    fu("IS_BOOL", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Bool(_)))),
    // Not in the documentation (the function is IS_BOOL), but the Java SDK writes `IS_BOOLEAN` in the filter it builds to resume an
    // ORDER BY query from a continuation token; without it such a page request fails with SC2005.
    fu("IS_BOOLEAN", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Bool(_)))),
    fu("IS_NUMBER", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Num(_)))),
    fu("IS_OBJECT", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Obj(_)))),
    fu("IS_STRING", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Str(_)))),
    fu("IS_PRIMITIVE", 1, 1, |_, a| Val::Bool(matches!(a[0], Val::Null | Val::Bool(_) | Val::Num(_) | Val::Str(_)))),
    f("IS_INTEGER", 1, 1, is_integer),
    f("IS_FINITE_NUMBER", 1, 1, |_, a| Val::Bool(matches!(&a[0], Val::Num(n) if n.f64().is_finite()))),
];

fn is_integer(_: &Ctx, a: &[Val]) -> Val {
    Val::Bool(match &a[0] {
        Val::Num(Num::Int(_)) => true,
        Val::Num(n) => n.as_i64().is_some(),
        _ => false,
    })
}
