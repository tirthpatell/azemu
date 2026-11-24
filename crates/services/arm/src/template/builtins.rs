//! The functions of the language that take their arguments as values, in one match (`call_strict`): strings, arithmetic,
//! comparison, arrays and objects that were there first. Newer ones are in `colls`, `dates`, `scope`, `lambda`.
//! Kept out of `call` so that its large stack frame is not part of the evaluator's recursion.

use serde_json::{json, Map, Value};

use super::*;

fn arith(name: &str, a: Option<i64>) -> Result<Value, EvalError> {
    match a {
        Some(n) => Ok(json!(n)),
        None => invalid(format!("{name}: the result does not fit in a 64-bit integer.")),
    }
}

#[inline(never)]
pub(super) fn call_strict(name: &str, lname: &str, v: Vec<Value>, env: &Env<'_>) -> Result<Value, EvalError> {
    let need = |n: usize| -> Result<(), EvalError> {
        if v.len() < n {
            err(format!("The function '{name}' needs at least {n} argument(s)."))
        } else {
            Ok(())
        }
    };
    match lname {
        "parameters" => {
            need(1)?;
            let n = s(&v[0]);
            env.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(&n)).map(|(_, x)| x.clone()).ok_or_else(|| {
                EvalError::Msg(format!(
                    "The template parameter '{n}' is not found. Please see https://aka.ms/arm-template/#parameters for usage details."
                ))
            })
        }
        "concat" => {
            if !v.is_empty() && v.iter().all(Value::is_array) {
                Ok(Value::Array(v.iter().flat_map(|x| x.as_array().cloned().unwrap_or_default()).collect()))
            } else {
                Ok(Value::String(v.iter().map(s).collect()))
            }
        }
        "format" => {
            need(1)?;
            let f = s(&v[0]);
            let mut out = String::new();
            let b = f.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'{' && b.get(i + 1) == Some(&b'{') {
                    out.push('{');
                    i += 2;
                } else if b[i] == b'}' && b.get(i + 1) == Some(&b'}') {
                    out.push('}');
                    i += 2;
                } else if b[i] == b'{' {
                    let end = f[i..].find('}').ok_or_else(|| EvalError::Msg("format: unmatched '{'".into()))? + i;
                    let spec = &f[i + 1..end];
                    let (idx, fmt) = spec.split_once(':').unwrap_or((spec, ""));
                    let n: usize = idx.trim().parse().map_err(|_| EvalError::Msg(format!("format: '{idx}' is not an index")))?;
                    let arg = n.checked_add(1).and_then(|k| v.get(k)).ok_or_else(|| EvalError::Msg(format!("format: no argument {n}")))?;
                    out.push_str(&format_one(arg, fmt)?);
                    i = end + 1;
                } else {
                    let ch = f[i..].chars().next().unwrap_or(' ');
                    out.push(ch);
                    i += ch.len_utf8();
                }
                if out.len() as u64 > limits::MAX_VALUE_BYTES {
                    return invalid(format!("format: the result is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
                }
            }
            Ok(Value::String(out))
        }
        "padleft" => {
            need(2)?;
            let t = s(&v[0]);
            let total = int(&v[1])?;
            if total > limits::MAX_VALUE_BYTES as i64 {
                return invalid(format!(
                    "padLeft: the total length {total} is larger than {} MB.",
                    limits::MAX_VALUE_BYTES / (1024 * 1024)
                ));
            }
            let pad = if v.len() > 2 { s(&v[2]).chars().next().unwrap_or(' ') } else { ' ' };
            let have = t.chars().count() as i64;
            let missing = (total - have).max(0) as usize;
            Ok(Value::String(std::iter::repeat_n(pad, missing).chain(t.chars()).collect()))
        }
        "tolower" => need(1).map(|_| Value::String(s(&v[0]).to_lowercase())),
        "toupper" => need(1).map(|_| Value::String(s(&v[0]).to_uppercase())),
        "trim" => need(1).map(|_| Value::String(s(&v[0]).trim().to_string())),
        "string" => need(1).and_then(|_| {
            if matches!(&v[0], Value::Number(n) if n.is_f64()) {
                return runtime("The template language function 'string' expects its first parameter to be one of the following types 'Integer, String, Boolean, Object, Array'. The provided value is of type 'Float'. Please see https://aka.ms/arm-functions for usage details.");
            }
            Ok(Value::String(if v[0].is_string() || v[0].is_boolean() { s(&v[0]) } else { v[0].to_string() }))
        }),
        "int" => need(1).and_then(|_| int(&v[0]).map(|n| json!(n))),
        "bool" => need(1).and_then(|_| truthy(&v[0]).map(|b| json!(b))),
        "length" => {
            need(1)?;
            Ok(json!(match &v[0] {
                Value::String(x) => x.chars().count(),
                Value::Array(a) => a.len(),
                Value::Object(o) => o.len(),
                _ => return err("length needs a string, array or object."),
            }))
        }
        "substring" => {
            need(2)?;
            let t: Vec<char> = s(&v[0]).chars().collect();
            let start = int(&v[1])?.max(0) as usize;
            let len = if v.len() > 2 { int(&v[2])?.max(0) as usize } else { t.len().saturating_sub(start) };
            if start > t.len() || start.checked_add(len).map(|end| end > t.len()).unwrap_or(true) {
                return err("substring: the index and length are out of range.");
            }
            Ok(Value::String(t[start..start + len].iter().collect()))
        }
        "replace" => {
            need(3)?;
            let (text, from, to) = (s(&v[0]), s(&v[1]), s(&v[2]));
            if from.is_empty() {
                return err("replace: the string to replace must not be empty.");
            }
            // Size of the answer, before it is built: a long `to` for every match could be far larger than any limit.
            let hits = text.matches(from.as_str()).count() as u64;
            let size = (text.len() as u64).saturating_sub(hits * from.len() as u64).saturating_add(hits.saturating_mul(to.len() as u64));
            if size > limits::MAX_VALUE_BYTES {
                return invalid(format!("replace: the result would be larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
            }
            Ok(Value::String(text.replace(&from, &to)))
        }
        "split" => {
            need(2)?;
            let text = s(&v[0]);
            let seps: Vec<String> = match &v[1] {
                Value::Array(a) => a.iter().map(s).collect(),
                other => vec![s(other)],
            };
            let mut parts = vec![text];
            for sep in seps.iter().filter(|x| !x.is_empty()) {
                let mut next: Vec<String> = Vec::new();
                let mut size = 0u64;
                for p in &parts {
                    for piece in p.split(sep.as_str()) {
                        size += 16 + piece.len() as u64;
                        if size > limits::MAX_VALUE_BYTES {
                            return invalid(format!("split: the result is larger than {} MB.", limits::MAX_VALUE_BYTES / (1024 * 1024)));
                        }
                        next.push(piece.to_string());
                    }
                }
                parts = next;
            }
            Ok(Value::Array(parts.into_iter().map(Value::String).collect()))
        }
        "contains" => {
            need(2)?;
            Ok(json!(match &v[0] {
                Value::String(t) => t.contains(&s(&v[1])),
                Value::Array(a) => a.iter().any(|x| eq_values(x, &v[1])),
                Value::Object(o) => o.keys().any(|k| k.eq_ignore_ascii_case(&s(&v[1]))),
                _ => false,
            }))
        }
        "startswith" => need(2).map(|_| json!(s(&v[0]).to_lowercase().starts_with(&s(&v[1]).to_lowercase()))),
        "endswith" => need(2).map(|_| json!(s(&v[0]).to_lowercase().ends_with(&s(&v[1]).to_lowercase()))),
        "base64" => {
            use base64::Engine;
            need(1).map(|_| Value::String(base64::engine::general_purpose::STANDARD.encode(s(&v[0]))))
        }
        "base64tostring" => {
            use base64::Engine;
            need(1)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s(&v[0]))
                .map_err(|_| EvalError::Msg("base64ToString: not valid base64".into()))?;
            Ok(Value::String(String::from_utf8_lossy(&bytes).to_string()))
        }
        "uniquestring" => {
            need(1)?;
            Ok(Value::String(crate::util::short_hash(&v.iter().map(s).collect::<Vec<_>>().join("-"), 13)))
        }
        "guid" => {
            need(1)?;
            let h = crate::util::short_hash(&v.iter().map(s).collect::<Vec<_>>().join("-"), 32);
            let hex: String = crate::util::derive_key(&h, 0)
                .bytes()
                .filter(|b| b.is_ascii_alphanumeric())
                .take(32)
                .map(|b| format!("{:x}", b % 16))
                .collect();
            Ok(Value::String(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])))
        }
        "equals" => need(2).map(|_| json!(eq_values(&v[0], &v[1]))),
        "not" => need(1).and_then(|_| truthy(&v[0]).map(|b| json!(!b))),
        "empty" => {
            need(1)?;
            Ok(json!(match &v[0] {
                Value::Null => true,
                Value::String(t) => t.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(o) => o.is_empty(),
                _ => false,
            }))
        }
        "less" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_lt()))),
        "lessorequals" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_le()))),
        "greater" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_gt()))),
        "greaterorequals" => need(2).and_then(|_| cmp(&v[0], &v[1]).map(|o| json!(o.is_ge()))),
        "add" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_add(int(&v[1])?))
        }
        "sub" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_sub(int(&v[1])?))
        }
        "mul" => {
            need(2)?;
            arith(name, int(&v[0])?.checked_mul(int(&v[1])?))
        }
        "div" => {
            need(2)?;
            let d = int(&v[1])?;
            if d == 0 {
                return invalid("div: division by zero.");
            }
            arith(name, int(&v[0])?.checked_div(d))
        }
        "mod" => {
            need(2)?;
            let d = int(&v[1])?;
            if d == 0 {
                return invalid("mod: division by zero.");
            }
            arith(name, int(&v[0])?.checked_rem(d))
        }
        "min" | "max" => {
            let nums: Vec<i64> = if v.len() == 1 && v[0].is_array() {
                v[0].as_array().map(|a| a.iter().filter_map(|x| int(x).ok()).collect()).unwrap_or_default()
            } else {
                v.iter().filter_map(|x| int(x).ok()).collect()
            };
            let r = if lname == "min" { nums.iter().min() } else { nums.iter().max() };
            r.map(|n| json!(n)).ok_or_else(|| EvalError::Msg(format!("{name}: no numbers given.")))
        }
        "range" => {
            need(2)?;
            let start = int(&v[0])?;
            let n = int(&v[1])?;
            if !(0..=limits::MAX_RANGE_COUNT).contains(&n) {
                return invalid(format!(
                    "range: the count {n} is not valid; it must be a non-negative integer up to {}.",
                    limits::MAX_RANGE_COUNT
                ));
            }
            match start.checked_add(n) {
                Some(end) if end <= limits::MAX_RANGE_END => {}
                _ => {
                    return invalid(format!(
                        "range: the sum of the start index {start} and the count {n} must not be greater than {}.",
                        limits::MAX_RANGE_END
                    ))
                }
            }
            Ok(Value::Array((0..n).map(|i| json!(start + i)).collect()))
        }
        "createarray" => Ok(Value::Array(v)),
        "createobject" => {
            let mut o = Map::new();
            for pair in v.chunks(2) {
                if pair.len() == 2 {
                    o.insert(s(&pair[0]), pair[1].clone());
                }
            }
            Ok(Value::Object(o))
        }
        "json" => need(1).and_then(|_| serde_json::from_str(&s(&v[0])).map_err(|e| EvalError::Msg(format!("json: {e}")))),
        "union" => {
            need(2)?;
            if v.iter().all(Value::is_array) {
                // Equal values (strings compared without case) are kept once; a set of canonical texts keeps it linear.
                let mut seen = std::collections::HashSet::new();
                let mut out: Vec<Value> = Vec::new();
                for a in v.iter().flat_map(|x| x.as_array().into_iter().flatten()) {
                    if seen.insert(limits::canonical(a)) {
                        out.push(a.clone());
                    }
                }
                Ok(Value::Array(out))
            } else {
                let mut out = Map::new();
                for o in &v {
                    if let Some(m) = o.as_object() {
                        for (k, x) in m {
                            out.insert(k.clone(), x.clone());
                        }
                    }
                }
                Ok(Value::Object(out))
            }
        }
        "first" => need(1).map(|_| match &v[0] {
            Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
            Value::String(t) => t.chars().next().map(|c| Value::String(c.to_string())).unwrap_or(Value::Null),
            _ => Value::Null,
        }),
        "last" => need(1).map(|_| match &v[0] {
            Value::Array(a) => a.last().cloned().unwrap_or(Value::Null),
            Value::String(t) => t.chars().last().map(|c| Value::String(c.to_string())).unwrap_or(Value::Null),
            _ => Value::Null,
        }),
        "take" | "skip" => {
            need(2)?;
            let n = usize::try_from(int(&v[1])?.max(0)).unwrap_or(usize::MAX);
            let take = lname == "take";
            Ok(match &v[0] {
                Value::Array(a) => {
                    Value::Array(if take { a.iter().take(n).cloned().collect() } else { a.iter().skip(n).cloned().collect() })
                }
                other => {
                    let t = s(other);
                    Value::String(if take { t.chars().take(n).collect() } else { t.chars().skip(n).collect() })
                }
            })
        }
        "copyindex" => {
            let (name_arg, offset) = match v.len() {
                0 => (None, 0),
                1 if v[0].is_number() => (None, int(&v[0])?),
                1 => (Some(s(&v[0])), 0),
                _ => (Some(s(&v[0])), int(&v[1])?),
            };
            let cur = match &name_arg {
                Some(n) => env.copy.iter().rev().find(|(c, _)| c.eq_ignore_ascii_case(n)),
                None => env.copy.last(),
            };
            match cur {
                Some((_, i)) => arith(name, i.checked_add(offset)),
                None => err("copyIndex() was used outside a copy loop."),
            }
        }
        "resourcegroup" => Ok(env.rg_info.clone()),
        "subscription" => Ok(env.sub_info.clone()),
        "tenant" => Ok(json!({"tenantId": env.tenant, "id": format!("/tenants/{}", env.tenant)})),
        "deployment" => Ok(env.deployment.clone()),
        "resourceid" => resource_id(env, &v, false),
        "subscriptionresourceid" => resource_id(env, &v, true),
        "lambdavariables" => {
            need(1)?;
            lambda::variable(env, &s(&v[0]))
        }
        "environment" => Ok(match env.cloud {
            Some(c) => c.clone(),
            None => cloud::environment("localhost"),
        }),
        other => {
            if let Some(r) = colls::call(name, other, &v) {
                return r;
            }
            if let Some(r) = dates::call(name, other, &v, env) {
                return r;
            }
            if let Some(r) = scope::call(name, other, &v, env) {
                return r;
            }
            err(format!("The template function '{name}' is not built in azemu."))
        }
    }
}

fn format_one(v: &Value, fmt: &str) -> Result<String, EvalError> {
    if let (Some(f), Some(spec)) = (fmt.chars().next(), fmt.get(1..)) {
        let digits: usize = match spec.parse() {
            Ok(d) => d,
            // digits that do not fit a number are certainly too wide
            Err(_) if !spec.is_empty() && spec.bytes().all(|b| b.is_ascii_digit()) => usize::MAX,
            Err(_) => 0,
        };
        if digits > limits::MAX_FORMAT_WIDTH {
            return invalid(format!("format: the width or precision {digits} is larger than {}.", limits::MAX_FORMAT_WIDTH));
        }
        match (f.to_ascii_uppercase(), v) {
            ('N' | 'F', Value::Number(n)) => {
                return Ok(format!("{:.*}", if spec.is_empty() { 2 } else { digits }, n.as_f64().unwrap_or(0.0)))
            }
            ('D', Value::Number(n)) => return Ok(format!("{:0>width$}", n.as_i64().unwrap_or(0), width = digits)),
            ('X', Value::Number(n)) => return Ok(format!("{:X}", n.as_i64().unwrap_or(0))),
            _ => {}
        }
    }
    Ok(s(v))
}
