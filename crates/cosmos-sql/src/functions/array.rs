//! Array, set and object-conversion functions.

use super::{f, fu, FuncSpec, VARIADIC};
use crate::eval::Ctx;

/// Most elements ARRAY_CONCAT may build (a 2 MB document holds at most about a million).
const MAX_ARRAY_ELEMENTS: usize = 1_000_000;
use crate::ops::{from_tri, tri_and, tri_or};
use crate::value::{canon_key, cmp_total, values_equal, Val};
use std::collections::HashSet;

pub(super) static FUNCS: &[FuncSpec] = &[
    f("ARRAY_CONCAT", 2, VARIADIC, array_concat),
    f("ARRAY_CONTAINS", 2, 3, array_contains),
    fu("ARRAY_CONTAINS_ALL", 2, VARIADIC, |_, a| contains_multi(a, true)),
    fu("ARRAY_CONTAINS_ANY", 2, VARIADIC, |_, a| contains_multi(a, false)),
    f("ARRAY_LENGTH", 1, 1, |_, a| match &a[0] {
        Val::Arr(x) => Val::int(x.len() as i64),
        _ => Val::Undefined,
    }),
    f("ARRAY_SLICE", 2, 3, array_slice),
    f("ARRAY_SUM", 1, 1, |_, a| numbers(&a[0]).map(|n| Val::num(n.iter().sum())).unwrap_or(Val::Undefined)),
    f("ARRAY_AVG", 1, 1, array_avg),
    f("ARRAY_MEDIAN", 1, 1, array_median),
    f("ARRAY_MIN", 1, 1, |_, a| min_max(&a[0], false)),
    f("ARRAY_MAX", 1, 1, |_, a| min_max(&a[0], true)),
    f("SETUNION", 2, 2, set_union),
    f("SETINTERSECT", 2, 2, set_intersect),
    f("SETDIFFERENCE", 2, 2, set_difference),
    f("SETEQUAL", 2, 2, set_equal),
    f("OBJECTTOARRAY", 1, 3, object_to_array),
];

fn array_concat(ctx: &Ctx, a: &[Val]) -> Val {
    let total: usize = a.iter().map(|v| v.as_arr().map(Vec::len).unwrap_or(0)).sum();
    if total > MAX_ARRAY_ELEMENTS {
        ctx.value_too_big();
        return Val::Undefined;
    }
    let mut out = Vec::new();
    for v in a {
        match v {
            Val::Arr(x) => out.extend(x.iter().cloned()),
            _ => return Val::Undefined,
        }
    }
    Val::arr(out)
}

/// Every field of `search` appears in `elem` with a (partially) matching value.
fn partial(elem: &Val, search: &Val) -> bool {
    match (elem, search) {
        (Val::Obj(_), Val::Obj(s)) => s.iter().all(|(k, sv)| {
            let ev = elem.get_prop(k);
            !ev.is_undefined() && partial(&ev, sv)
        }),
        _ => values_equal(elem, search),
    }
}

fn array_contains(_: &Ctx, a: &[Val]) -> Val {
    let arr = match &a[0] {
        Val::Arr(x) => x,
        _ => return Val::Undefined,
    };
    let part = match a.get(2) {
        None => false,
        Some(Val::Bool(b)) => *b,
        Some(_) => return Val::Undefined,
    };
    Val::Bool(arr.iter().any(|e| if part { partial(e, &a[1]) } else { values_equal(e, &a[1]) }))
}

fn contains_multi(a: &[Val], all: bool) -> Val {
    let arr = match &a[0] {
        Val::Arr(x) => x,
        _ => return Val::Undefined,
    };
    let mut acc = Some(all);
    for t in &a[1..] {
        let r = if t.is_undefined() { None } else { Some(arr.iter().any(|e| values_equal(e, t))) };
        acc = if all { tri_and(acc, r) } else { tri_or(acc, r) };
    }
    from_tri(acc)
}

fn array_slice(_: &Ctx, a: &[Val]) -> Val {
    let arr = match &a[0] {
        Val::Arr(x) => x,
        _ => return Val::Undefined,
    };
    let n = arr.len() as i64;
    let start = match a[1].as_i64() {
        Some(s) => s,
        None => return Val::Undefined,
    };
    let start = if start < 0 { (n + start).max(0) } else { start.min(n) };
    let len = match a.get(2) {
        None => n - start,
        Some(v) => match v.as_i64() {
            Some(l) => l.max(0),
            None => return Val::Undefined,
        },
    };
    let end = start.saturating_add(len).min(n);
    Val::arr(arr[start as usize..end as usize].to_vec())
}

/// Numbers of an array (undefined skipped); `None` if the array is empty or holds a non-number.
fn numbers(v: &Val) -> Option<Vec<f64>> {
    let arr = v.as_arr()?;
    let mut out = Vec::new();
    for e in arr.iter() {
        match e {
            Val::Num(n) => out.push(n.f64()),
            Val::Undefined => {}
            _ => return None,
        }
    }
    Some(out)
}

fn array_avg(_: &Ctx, a: &[Val]) -> Val {
    match numbers(&a[0]) {
        Some(n) if !n.is_empty() => Val::num(n.iter().sum::<f64>() / n.len() as f64),
        _ => Val::Undefined,
    }
}

fn array_median(_: &Ctx, a: &[Val]) -> Val {
    match numbers(&a[0]) {
        Some(mut n) if !n.is_empty() => {
            n.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
            let m = n.len() / 2;
            if n.len() % 2 == 1 {
                Val::num(n[m])
            } else {
                Val::num((n[m - 1] + n[m]) / 2.0)
            }
        }
        _ => Val::Undefined,
    }
}

fn min_max(v: &Val, max: bool) -> Val {
    let arr = match v.as_arr() {
        Some(a) => a,
        None => return Val::Undefined,
    };
    let mut best: Option<&Val> = None;
    for e in arr.iter() {
        if matches!(e, Val::Undefined | Val::Arr(_) | Val::Obj(_)) {
            if e.is_undefined() {
                continue;
            }
            return Val::Undefined;
        }
        best = match best {
            None => Some(e),
            Some(b) => {
                let o = cmp_total(e, b);
                if (max && o == std::cmp::Ordering::Greater) || (!max && o == std::cmp::Ordering::Less) {
                    Some(e)
                } else {
                    Some(b)
                }
            }
        };
    }
    best.cloned().unwrap_or(Val::Undefined)
}

fn two_arrays(a: &[Val]) -> Option<(&Vec<Val>, &Vec<Val>)> {
    match (&a[0], &a[1]) {
        (Val::Arr(x), Val::Arr(y)) => Some((x, y)),
        _ => None,
    }
}

fn dedup(items: impl Iterator<Item = Val>) -> Vec<Val> {
    let mut seen = HashSet::new();
    items.filter(|v| seen.insert(canon_key(v))).collect()
}

fn set_union(_: &Ctx, a: &[Val]) -> Val {
    match two_arrays(a) {
        Some((x, y)) => Val::arr(dedup(x.iter().chain(y.iter()).cloned())),
        None => Val::Undefined,
    }
}

/// Elements found in both arrays, in the order of the second array.
fn set_intersect(_: &Ctx, a: &[Val]) -> Val {
    match two_arrays(a) {
        Some((x, y)) => {
            let first: HashSet<String> = x.iter().map(canon_key).collect();
            Val::arr(dedup(y.iter().filter(|v| first.contains(&canon_key(v))).cloned()))
        }
        None => Val::Undefined,
    }
}

fn set_difference(_: &Ctx, a: &[Val]) -> Val {
    match two_arrays(a) {
        Some((x, y)) => {
            let second: HashSet<String> = y.iter().map(canon_key).collect();
            Val::arr(dedup(x.iter().filter(|v| !second.contains(&canon_key(v))).cloned()))
        }
        None => Val::Undefined,
    }
}

fn set_equal(_: &Ctx, a: &[Val]) -> Val {
    match two_arrays(a) {
        Some((x, y)) => {
            let sx: HashSet<String> = x.iter().map(canon_key).collect();
            let sy: HashSet<String> = y.iter().map(canon_key).collect();
            Val::Bool(sx == sy)
        }
        None => Val::Undefined,
    }
}

fn object_to_array(_: &Ctx, a: &[Val]) -> Val {
    let obj = match &a[0] {
        Val::Obj(o) => o,
        _ => return Val::Undefined,
    };
    let (kn, vn) = match (a.get(1), a.get(2)) {
        (None, None) => ("k".to_string(), "v".to_string()),
        (Some(Val::Str(k)), Some(Val::Str(v))) => (k.to_string(), v.to_string()),
        _ => return Val::Undefined,
    };
    Val::arr(
        obj.iter().map(|(k, v)| Val::object(vec![(kn.as_str().into(), Val::Str(k.clone())), (vn.as_str().into(), v.clone())])).collect(),
    )
}
