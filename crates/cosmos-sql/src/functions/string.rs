//! String functions.

use super::{f, fu, str_fn, FuncSpec, VARIADIC};
use crate::eval::{Ctx, MAX_VALUE_BYTES};
use crate::ops::{tri_and, tri_or};
use crate::value::Val;

pub(super) static FUNCS: &[FuncSpec] = &[
    f("CONCAT", 2, VARIADIC, concat),
    f("CONTAINS", 2, 3, |_, a| flag_pair(a, |s, t, ci| norm(s, ci).contains(&norm(t, ci)))),
    f("STARTSWITH", 2, 3, |_, a| flag_pair(a, |s, t, ci| norm(s, ci).starts_with(&norm(t, ci)))),
    f("ENDSWITH", 2, 3, |_, a| flag_pair(a, |s, t, ci| norm(s, ci).ends_with(&norm(t, ci)))),
    f("STRINGEQUALS", 2, 3, |_, a| flag_pair(a, |s, t, ci| norm(s, ci) == norm(t, ci))),
    f("INDEX_OF", 2, 3, index_of),
    f("LASTINDEXOF", 2, 3, last_index_of),
    f("LEFT", 2, 2, left),
    f("RIGHT", 2, 2, right),
    f("LENGTH", 1, 1, length),
    f("LOWER", 1, 1, lower),
    f("UPPER", 1, 1, upper),
    f("LTRIM", 1, 2, |_, a| trim(a, true, false)),
    f("RTRIM", 1, 2, |_, a| trim(a, false, true)),
    f("TRIM", 1, 2, |_, a| trim(a, true, true)),
    f("REPLACE", 3, 3, replace),
    f("REPLICATE", 2, 2, replicate),
    f("REVERSE", 1, 1, reverse),
    f("SUBSTRING", 3, 3, substring),
    f("TOSTRING", 1, 1, |_, a| Val::string(to_text(&a[0]))),
    f("STRINGJOIN", 2, 2, string_join),
    f("STRINGSPLIT", 2, 2, string_split),
    f("STRING_SPLIT", 2, 2, string_split),
    f("STRINGTOARRAY", 1, 1, |_, a| parse_json(&a[0], |v| matches!(v, Val::Arr(_)))),
    f("STRINGTOOBJECT", 1, 1, |_, a| parse_json(&a[0], |v| matches!(v, Val::Obj(_)))),
    f("STRINGTONUMBER", 1, 1, |_, a| parse_json(&a[0], |v| matches!(v, Val::Num(_)))),
    f("STRINGTOBOOLEAN", 1, 1, |_, a| parse_json(&a[0], |v| matches!(v, Val::Bool(_)))),
    f("STRINGTONULL", 1, 1, |_, a| parse_json(&a[0], |v| matches!(v, Val::Null))),
    f("REGEXMATCH", 2, 3, regex_match),
    fu("CONTAINS_ALL_CI", 2, VARIADIC, |_, a| multi(a, true, true)),
    fu("CONTAINS_ALL_CS", 2, VARIADIC, |_, a| multi(a, true, false)),
    fu("CONTAINS_ANY_CI", 2, VARIADIC, |_, a| multi(a, false, true)),
    fu("CONTAINS_ANY_CS", 2, VARIADIC, |_, a| multi(a, false, false)),
];

fn norm(s: &str, ci: bool) -> String {
    if ci {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Two strings plus an optional case-insensitive flag.
fn flag_pair(a: &[Val], op: fn(&str, &str, bool) -> bool) -> Val {
    let ci = match a.get(2) {
        None => false,
        Some(Val::Bool(b)) => *b,
        Some(_) => return Val::Undefined,
    };
    match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(t)) => Val::Bool(op(s, t, ci)),
        _ => Val::Undefined,
    }
}

fn multi(a: &[Val], all: bool, ci: bool) -> Val {
    let hay = match &a[0] {
        Val::Str(s) => norm(s, ci),
        _ => return Val::Undefined,
    };
    let mut acc = Some(all);
    for t in &a[1..] {
        let r = match t {
            Val::Str(t) => Some(hay.contains(&norm(t, ci))),
            Val::Undefined => None,
            _ => return Val::Undefined,
        };
        acc = if all { tri_and(acc, r) } else { tri_or(acc, r) };
    }
    crate::ops::from_tri(acc)
}

fn concat(ctx: &Ctx, a: &[Val]) -> Val {
    let total: usize = a.iter().map(|v| v.as_str().map(str::len).unwrap_or(0)).sum();
    if total > MAX_VALUE_BYTES {
        ctx.value_too_big();
        return Val::Undefined;
    }
    let mut out = String::new();
    for v in a {
        match v {
            Val::Str(s) => out.push_str(s),
            _ => return Val::Undefined,
        }
    }
    Val::string(out)
}

fn index_of(_: &Ctx, a: &[Val]) -> Val {
    let (s, t) = match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(t)) => (s, t),
        _ => return Val::Undefined,
    };
    let start = match a.get(2) {
        None => 0,
        Some(v) => match v.as_i64() {
            Some(i) => i.max(0) as usize,
            None => return Val::Undefined,
        },
    };
    let hay: Vec<char> = s.chars().collect();
    let needle: Vec<char> = t.chars().collect();
    if needle.len() > hay.len() {
        return Val::int(-1);
    }
    for i in start..=(hay.len() - needle.len()) {
        if hay[i..i + needle.len()] == needle[..] {
            return Val::int(i as i64);
        }
    }
    Val::int(-1)
}

fn last_index_of(_: &Ctx, a: &[Val]) -> Val {
    let (s, t) = match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(t)) => (s, t),
        _ => return Val::Undefined,
    };
    let hay: Vec<char> = s.chars().collect();
    let needle: Vec<char> = t.chars().collect();
    let start = match a.get(2) {
        None => hay.len() as i64 - 1,
        Some(v) => match v.as_i64() {
            Some(i) => i.min(hay.len() as i64 - 1),
            None => return Val::Undefined,
        },
    };
    let max_start = start + 1 - needle.len() as i64;
    let mut i = max_start;
    while i >= 0 {
        let u = i as usize;
        if u + needle.len() <= hay.len() && hay[u..u + needle.len()] == needle[..] {
            return Val::int(i);
        }
        i -= 1;
    }
    Val::int(-1)
}

fn count_arg(v: &Val) -> Option<usize> {
    let n = v.as_f64()?;
    if n.is_nan() {
        return None;
    }
    Some(if n <= 0.0 {
        0
    } else if n >= usize::MAX as f64 {
        usize::MAX
    } else {
        n as usize
    })
}

fn left(_: &Ctx, a: &[Val]) -> Val {
    match (&a[0], count_arg(&a[1])) {
        (Val::Str(s), Some(n)) => Val::string(s.chars().take(n).collect()),
        _ => Val::Undefined,
    }
}

fn right(_: &Ctx, a: &[Val]) -> Val {
    match (&a[0], count_arg(&a[1])) {
        (Val::Str(s), Some(n)) => {
            let total = s.chars().count();
            Val::string(s.chars().skip(total.saturating_sub(n)).collect())
        }
        _ => Val::Undefined,
    }
}

str_fn!(length, |s| Val::int(s.chars().count() as i64));
str_fn!(lower, |s| Val::string(s.to_lowercase()));
str_fn!(upper, |s| Val::string(s.to_uppercase()));
str_fn!(reverse, |s| Val::string(s.chars().rev().collect()));

fn trim(a: &[Val], front: bool, back: bool) -> Val {
    let s = match &a[0] {
        Val::Str(s) => s,
        _ => return Val::Undefined,
    };
    let set: Option<Vec<char>> = match a.get(1) {
        None => None,
        Some(Val::Str(c)) => Some(c.chars().collect()),
        Some(_) => return Val::Undefined,
    };
    let pred = |c: char| match &set {
        None => c.is_whitespace(),
        Some(set) => set.contains(&c),
    };
    let mut out: &str = s;
    if front {
        out = out.trim_start_matches(pred);
    }
    if back {
        out = out.trim_end_matches(pred);
    }
    Val::str(out)
}

fn replace(ctx: &Ctx, a: &[Val]) -> Val {
    match (&a[0], &a[1], &a[2]) {
        (Val::Str(s), Val::Str(from), Val::Str(to)) => {
            if from.is_empty() {
                Val::Str(s.clone())
            } else {
                // every match is replaced by `to`: say no before building a string that cannot be stored
                let matches = s.matches(&**from).count();
                let len = s.len() as i128 + matches as i128 * (to.len() as i128 - from.len() as i128);
                if len > MAX_VALUE_BYTES as i128 {
                    ctx.value_too_big();
                    return Val::Undefined;
                }
                Val::string(s.replace(&**from, to))
            }
        }
        _ => Val::Undefined,
    }
}

fn replicate(_: &Ctx, a: &[Val]) -> Val {
    let (s, n) = match (&a[0], a[1].as_f64()) {
        (Val::Str(s), Some(n)) => (s, n),
        _ => return Val::Undefined,
    };
    if !n.is_finite() || n < 0.0 {
        return Val::Undefined;
    }
    let n = n.trunc();
    if (s.chars().count() as f64) * n > 10_000.0 {
        return Val::Undefined;
    }
    Val::string(s.repeat(n as usize))
}

fn substring(_: &Ctx, a: &[Val]) -> Val {
    let s = match &a[0] {
        Val::Str(s) => s,
        _ => return Val::Undefined,
    };
    let (start, len) = match (a[1].as_f64(), a[2].as_f64()) {
        (Some(x), Some(y)) if !x.is_nan() && !y.is_nan() => (x, y),
        _ => return Val::Undefined,
    };
    if len < 0.0 {
        return Val::str("");
    }
    let start = if start < 0.0 { 0 } else { start as usize };
    let len = if len >= usize::MAX as f64 { usize::MAX } else { len as usize };
    Val::string(s.chars().skip(start).take(len).collect())
}

/// Text of a value as `TOSTRING` renders it.
pub(crate) fn to_text(v: &Val) -> String {
    match v {
        Val::Str(s) => s.to_string(),
        Val::Num(n) => {
            let x = n.f64();
            if x.is_nan() {
                "NaN".into()
            } else if x.is_infinite() {
                if x > 0.0 {
                    "Infinity".into()
                } else {
                    "-Infinity".into()
                }
            } else {
                v.to_json().map(|j| j.to_string()).unwrap_or_default()
            }
        }
        _ => v.to_json().map(|j| j.to_string()).unwrap_or_default(),
    }
}

fn string_join(ctx: &Ctx, a: &[Val]) -> Val {
    let (arr, sep) = match (&a[0], &a[1]) {
        (Val::Arr(arr), Val::Str(sep)) => (arr, sep),
        _ => return Val::Undefined,
    };
    let total: usize = arr.iter().map(|v| v.as_str().map(str::len).unwrap_or(0)).sum::<usize>() + sep.len().saturating_mul(arr.len());
    if total > MAX_VALUE_BYTES {
        ctx.value_too_big();
        return Val::Undefined;
    }
    let mut parts = Vec::with_capacity(arr.len());
    for v in arr.iter() {
        match v {
            Val::Str(s) => parts.push(s.to_string()),
            _ => return Val::Undefined,
        }
    }
    Val::string(parts.join(sep))
}

fn string_split(_: &Ctx, a: &[Val]) -> Val {
    match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(d)) => {
            if d.is_empty() {
                Val::arr(vec![Val::Str(s.clone())])
            } else {
                Val::arr(s.split(&**d).map(Val::str).collect())
            }
        }
        _ => Val::Undefined,
    }
}

/// Parses a JSON text; the result must satisfy `want`, else undefined.
fn parse_json(v: &Val, want: fn(&Val) -> bool) -> Val {
    let s = match v {
        Val::Str(s) => s,
        _ => return Val::Undefined,
    };
    match serde_json::from_str::<serde_json::Value>(s.trim()) {
        Ok(j) => {
            let val = Val::from_json(&j);
            if want(&val) {
                val
            } else {
                Val::Undefined
            }
        }
        Err(_) => Val::Undefined,
    }
}

fn regex_match(ctx: &Ctx, a: &[Val]) -> Val {
    let (s, p) = match (&a[0], &a[1]) {
        (Val::Str(s), Val::Str(p)) => (s, p),
        _ => return Val::Undefined,
    };
    let mods = match a.get(2) {
        None => "",
        Some(Val::Str(m)) => m,
        Some(_) => return Val::Undefined,
    };
    match ctx.regex(p, mods) {
        Some(re) => Val::Bool(re.is_match(s)),
        None => Val::Undefined,
    }
}
