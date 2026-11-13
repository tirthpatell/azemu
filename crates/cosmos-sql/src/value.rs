//! Internal value model: JSON plus `undefined`, with integers kept apart from floats
//! and non-finite numbers (which JSON cannot hold).

use serde_json::Value;
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    pub fn f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
    /// Integer value if the number is integral and in the i64 range.
    pub fn as_i64(self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(i),
            Num::Float(f) => {
                if f.is_finite() && f.fract() == 0.0 && (-(2f64.powi(63))..2f64.powi(63)).contains(&f) {
                    Some(f as i64)
                } else {
                    None
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Val {
    Undefined,
    Null,
    Bool(bool),
    Num(Num),
    Str(Arc<str>),
    Arr(Arc<Vec<Val>>),
    Obj(Arc<Vec<(Arc<str>, Val)>>),
    /// A stored document, shared with the container and not converted. Only a row of the FROM stage
    /// (`Env::vals`) and the output of a query that starts at the container hold one: property paths
    /// (`c.a.b[0]`) are read straight from the JSON (`Ctx::nav_root`), `SELECT *` and `SELECT VALUE c`
    /// hand the same document on, and everything else (`Ctx::eval` of a bare alias, `canon_key`, ...)
    /// converts it with `forced`. So a query that only filters and counts never copies the documents.
    Root(Arc<Value>),
}

const TWO53: f64 = 9007199254740992.0;

impl Val {
    pub fn int(i: i64) -> Val {
        Val::Num(Num::Int(i))
    }
    /// Float result; integral values below 2^53 become integers (`3.0` -> `3`).
    pub fn num(f: f64) -> Val {
        if f.is_finite() && f.fract() == 0.0 && f.abs() < TWO53 {
            Val::Num(Num::Int(f as i64))
        } else {
            Val::Num(Num::Float(f))
        }
    }
    pub fn str(s: &str) -> Val {
        Val::Str(Arc::from(s))
    }
    pub fn string(s: String) -> Val {
        Val::Str(Arc::from(s))
    }
    pub fn arr(v: Vec<Val>) -> Val {
        Val::Arr(Arc::new(v))
    }
    /// Builds an object; a repeated key keeps its first position and takes the last value.
    /// Undefined values are skipped.
    pub fn object(pairs: Vec<(Arc<str>, Val)>) -> Val {
        let mut out: Vec<(Arc<str>, Val)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            if matches!(v, Val::Undefined) {
                continue;
            }
            if let Some(slot) = out.iter_mut().find(|(ek, _)| **ek == *k) {
                slot.1 = v;
            } else {
                out.push((k, v));
            }
        }
        Val::Obj(Arc::new(out))
    }
    pub fn is_undefined(&self) -> bool {
        matches!(self, Val::Undefined)
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(n.f64()),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Val::Num(n) => n.as_i64(),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<Val>> {
        match self {
            Val::Arr(a) => Some(a),
            _ => None,
        }
    }
    /// The value as a plain tree: a shared document is converted, everything else is cloned (cheap, shared).
    pub fn forced(&self) -> Val {
        match self {
            Val::Root(d) => Val::from_json(d),
            other => other.clone(),
        }
    }

    /// Rough size in bytes of this value while a query holds it, counting what it points to (a shared
    /// document counts as a pointer: the container owns it). Stops adding once `cap` is passed, so a
    /// huge value costs nothing to measure.
    pub fn est_bytes(&self, cap: usize) -> usize {
        fn go(v: &Val, total: &mut usize, cap: usize) {
            *total += std::mem::size_of::<Val>();
            if *total > cap {
                return;
            }
            match v {
                Val::Str(s) => *total += s.len() + 16,
                Val::Arr(a) => {
                    *total += 32;
                    for x in a.iter() {
                        go(x, total, cap);
                        if *total > cap {
                            return;
                        }
                    }
                }
                Val::Obj(o) => {
                    *total += 32;
                    for (k, x) in o.iter() {
                        *total += k.len() + 16;
                        go(x, total, cap);
                        if *total > cap {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
        let mut total = 0;
        go(self, &mut total, cap);
        total
    }

    pub fn get_prop(&self, key: &str) -> Val {
        match self {
            Val::Root(d) => d.as_object().and_then(|m| m.get(key)).map(Val::from_json).unwrap_or(Val::Undefined),
            Val::Obj(o) => o.iter().find(|(k, _)| &**k == key).map(|(_, v)| v.clone()).unwrap_or(Val::Undefined),
            _ => Val::Undefined,
        }
    }
    pub fn get_index(&self, idx: &Val) -> Val {
        if let Val::Root(d) = self {
            return match json_index(d, idx) {
                Some(v) => Val::from_json(v),
                None => Val::Undefined,
            };
        }
        match (self, idx) {
            (Val::Arr(a), Val::Num(n)) => match n.as_i64() {
                Some(i) if i >= 0 => a.get(i as usize).cloned().unwrap_or(Val::Undefined),
                _ => Val::Undefined,
            },
            (Val::Obj(_), Val::Str(s)) => self.get_prop(s),
            _ => Val::Undefined,
        }
    }

    pub fn from_json(v: &Value) -> Val {
        match v {
            Value::Null => Val::Null,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Val::Num(Num::Int(i))
                } else {
                    Val::Num(Num::Float(n.as_f64().unwrap_or(f64::NAN)))
                }
            }
            Value::String(s) => Val::str(s),
            Value::Array(a) => Val::arr(a.iter().map(Val::from_json).collect()),
            Value::Object(o) => Val::Obj(Arc::new(o.iter().map(|(k, v)| (Arc::from(k.as_str()), Val::from_json(v))).collect())),
        }
    }

    /// `None` for undefined. Non-finite numbers become `null`.
    pub fn to_json(&self) -> Option<Value> {
        Some(match self {
            Val::Undefined => return None,
            Val::Root(d) => (**d).clone(),
            Val::Null => Value::Null,
            Val::Bool(b) => Value::Bool(*b),
            Val::Num(Num::Int(i)) => Value::from(*i),
            Val::Num(Num::Float(f)) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
            Val::Str(s) => Value::String(s.to_string()),
            Val::Arr(a) => Value::Array(a.iter().filter_map(|v| v.to_json()).collect()),
            Val::Obj(o) => {
                let mut m = serde_json::Map::new();
                for (k, v) in o.iter() {
                    if let Some(j) = v.to_json() {
                        m.insert(k.to_string(), j);
                    }
                }
                Value::Object(m)
            }
        })
    }
}

/// `value[idx]` on stored JSON, with the rules of `Val::get_index`.
pub(crate) fn json_index<'v>(v: &'v Value, idx: &Val) -> Option<&'v Value> {
    match (v, idx) {
        (Value::Array(a), Val::Num(n)) => match n.as_i64() {
            Some(i) if i >= 0 => a.get(i as usize),
            _ => None,
        },
        (Value::Object(m), Val::Str(s)) => m.get(&**s),
        _ => None,
    }
}

/// Order of types: undefined < null < boolean < number < string < array < object.
pub(crate) fn type_rank(v: &Val) -> u8 {
    match v {
        Val::Root(d) => type_rank(&Val::from_json(d)),
        Val::Undefined => 0,
        Val::Null => 1,
        Val::Bool(_) => 2,
        Val::Num(_) => 3,
        Val::Str(_) => 4,
        Val::Arr(_) => 5,
        Val::Obj(_) => 6,
    }
}

pub(crate) fn cmp_num(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Int(x), Num::Int(y)) => x.cmp(&y),
        _ => {
            let (x, y) = (a.f64(), b.f64());
            match x.partial_cmp(&y) {
                Some(o) => o,
                None => match (x.is_nan(), y.is_nan()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Less,
                    _ => Ordering::Greater,
                },
            }
        }
    }
}

/// Total order used by ORDER BY, MIN/MAX and DISTINCT helpers.
pub(crate) fn cmp_total(a: &Val, b: &Val) -> Ordering {
    if matches!(a, Val::Root(_)) || matches!(b, Val::Root(_)) {
        return cmp_total(&a.forced(), &b.forced());
    }
    let (ra, rb) = (type_rank(a), type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Val::Bool(x), Val::Bool(y)) => x.cmp(y),
        (Val::Num(x), Val::Num(y)) => cmp_num(*x, *y),
        (Val::Str(x), Val::Str(y)) => x.cmp(y),
        (Val::Arr(x), Val::Arr(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let o = cmp_total(p, q);
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
        (Val::Obj(x), Val::Obj(y)) => {
            let sx = sorted_entries(x);
            let sy = sorted_entries(y);
            for (p, q) in sx.iter().zip(sy.iter()) {
                let o = p.0.cmp(q.0).then_with(|| cmp_total(p.1, q.1));
                if o != Ordering::Equal {
                    return o;
                }
            }
            sx.len().cmp(&sy.len())
        }
        _ => Ordering::Equal,
    }
}

fn sorted_entries(o: &[(Arc<str>, Val)]) -> Vec<(&str, &Val)> {
    let mut v: Vec<(&str, &Val)> = o.iter().map(|(k, v)| (&**k, v)).collect();
    v.sort_by(|a, b| a.0.cmp(b.0));
    v
}

/// Deep equality: numbers by value, objects ignoring key order.
pub(crate) fn values_equal(a: &Val, b: &Val) -> bool {
    if matches!(a, Val::Root(_)) || matches!(b, Val::Root(_)) {
        return values_equal(&a.forced(), &b.forced());
    }
    match (a, b) {
        (Val::Undefined, Val::Undefined) | (Val::Null, Val::Null) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Num(x), Val::Num(y)) => {
            let (fx, fy) = (x.f64(), y.f64());
            if fx.is_nan() || fy.is_nan() {
                false
            } else {
                cmp_num(*x, *y) == Ordering::Equal
            }
        }
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Arr(x), Val::Arr(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| values_equal(p, q)),
        (Val::Obj(x), Val::Obj(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| y.iter().find(|(k2, _)| k2 == k).map(|(_, v2)| values_equal(v, v2)).unwrap_or(false))
        }
        _ => false,
    }
}

/// Canonical text of a value, equal for values that are `values_equal` (used for
/// DISTINCT, GROUP BY and set functions).
pub(crate) fn canon_key(v: &Val) -> String {
    let mut s = String::new();
    canon_into(v, &mut s);
    s
}

fn canon_into(v: &Val, out: &mut String) {
    match v {
        Val::Root(d) => canon_into(&Val::from_json(d), out),
        Val::Undefined => out.push('u'),
        Val::Null => out.push('n'),
        Val::Bool(b) => out.push(if *b { 't' } else { 'f' }),
        Val::Num(n) => {
            out.push('#');
            match n.as_i64() {
                Some(i) => out.push_str(&i.to_string()),
                None => out.push_str(&format!("{:?}", n.f64())),
            }
            out.push(';');
        }
        Val::Str(s) => {
            out.push('"');
            out.push_str(&s.len().to_string());
            out.push(':');
            out.push_str(s);
        }
        Val::Arr(a) => {
            out.push('[');
            for x in a.iter() {
                canon_into(x, out);
                out.push(',');
            }
            out.push(']');
        }
        Val::Obj(o) => {
            out.push('{');
            for (k, x) in sorted_entries(o) {
                out.push_str(&k.len().to_string());
                out.push(':');
                out.push_str(k);
                out.push('=');
                canon_into(x, out);
                out.push(',');
            }
            out.push('}');
        }
    }
}
