//! Partial document update (`PATCH`): add, set, replace, remove, incr, move, plus the
//! optional `condition`.

use azemu_cosmos_sql::{parse, Params};
use serde_json::Value;

use crate::documents::{self, Loc};
use crate::errors::{CosmosError, R};
use crate::out::{Cx, Out};
use crate::pk::DocKey;
use crate::state::{Container, Undo};

const MAX_OPERATIONS: usize = 10;

fn bad(msg: impl Into<String>) -> CosmosError {
    CosmosError::bad_request(msg)
}

fn segments(path: &str) -> R<Vec<String>> {
    if !path.starts_with('/') {
        return Err(bad(format!("Invalid patch path '{path}': it must start with '/'.")));
    }
    if path == "/" {
        return Err(bad("Invalid patch path '/'."));
    }
    Ok(path[1..].split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect())
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

fn parent_mut<'a>(doc: &'a mut Value, segs: &[String], path: &str) -> R<&'a mut Value> {
    let mut cur = doc;
    for s in &segs[..segs.len() - 1] {
        cur = match cur {
            Value::Object(m) => m.get_mut(s),
            Value::Array(a) => s.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
            _ => None,
        }
        .ok_or_else(|| bad(format!("The patch path '{path}' does not exist.")))?;
    }
    Ok(cur)
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Add,
    Set,
    Replace,
}

fn put(doc: &mut Value, segs: &[String], path: &str, value: Value, mode: Mode) -> R<()> {
    let last = segs[segs.len() - 1].clone();
    match parent_mut(doc, segs, path)? {
        Value::Object(m) => {
            if mode == Mode::Replace && !m.contains_key(&last) {
                return Err(bad(format!("The patch path '{path}' does not exist.")));
            }
            m.insert(last, value);
            Ok(())
        }
        Value::Array(a) => {
            if last == "-" && mode != Mode::Replace {
                a.push(value);
                return Ok(());
            }
            let i: usize = last.parse().map_err(|_| bad(format!("Invalid array index in patch path '{path}'.")))?;
            match mode {
                Mode::Add => {
                    if i > a.len() {
                        return Err(bad(format!("Array index out of range in patch path '{path}'.")));
                    }
                    a.insert(i, value);
                }
                Mode::Set | Mode::Replace => {
                    if i < a.len() {
                        a[i] = value;
                    } else if i == a.len() && mode == Mode::Set {
                        a.push(value);
                    } else {
                        return Err(bad(format!("Array index out of range in patch path '{path}'.")));
                    }
                }
            }
            Ok(())
        }
        _ => Err(bad(format!("The patch path '{path}' does not exist."))),
    }
}

fn take(doc: &mut Value, segs: &[String], path: &str) -> R<Value> {
    let last = segs[segs.len() - 1].clone();
    match parent_mut(doc, segs, path)? {
        Value::Object(m) => m.shift_remove(&last).ok_or_else(|| bad(format!("The patch path '{path}' does not exist."))),
        Value::Array(a) => {
            let i: usize = last.parse().map_err(|_| bad(format!("Invalid array index in patch path '{path}'.")))?;
            if i < a.len() {
                Ok(a.remove(i))
            } else {
                Err(bad(format!("Array index out of range in patch path '{path}'.")))
            }
        }
        _ => Err(bad(format!("The patch path '{path}' does not exist."))),
    }
}

fn get<'a>(doc: &'a Value, segs: &[String]) -> Option<&'a Value> {
    let mut cur = doc;
    for s in segs {
        cur = match cur {
            Value::Object(m) => m.get(s)?,
            Value::Array(a) => a.get(s.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn incr(doc: &mut Value, segs: &[String], path: &str, by: &Value) -> R<()> {
    if !by.is_number() {
        return Err(bad("The value of an incr operation must be a number."));
    }
    let current = get(doc, segs).cloned();
    let new = match current {
        None => by.clone(),
        Some(Value::Number(n)) => add_numbers(&n, by.as_number().ok_or_else(|| bad("incr needs a number"))?),
        Some(_) => return Err(bad(format!("The value at '{path}' is not a number."))),
    };
    put(doc, segs, path, new, Mode::Set)
}

fn add_numbers(a: &serde_json::Number, b: &serde_json::Number) -> Value {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        if let Some(s) = x.checked_add(y) {
            return Value::from(s);
        }
    }
    let s = a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0);
    crate::pk::num_json(s)
}

/// Apply the patch operations to a copy of the document.
pub fn apply(doc: &Value, ops: &[Value], protected: &[Vec<String>]) -> R<Value> {
    if ops.is_empty() {
        return Err(bad("The patch request has no operations."));
    }
    if ops.len() > MAX_OPERATIONS {
        return Err(bad(format!("The patch request has more than {MAX_OPERATIONS} operations.")));
    }
    let mut out = doc.clone();
    for op in ops {
        let name = op.get("op").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let path = op.get("path").and_then(Value::as_str).ok_or_else(|| bad("A patch operation needs a path."))?;
        let segs = segments(path)?;
        let touches = |s: &[String]| protected.iter().any(|p| overlaps(p, s));
        if touches(&segs) {
            return Err(bad(format!("Patch cannot modify the id or the partition key ('{path}').")));
        }
        if segs[0].starts_with('_') && ["_rid", "_self", "_etag", "_ts", "_attachments"].contains(&segs[0].as_str()) {
            return Err(bad(format!("Patch cannot modify the system property '{path}'.")));
        }
        let value = op.get("value");
        let need = || value.cloned().ok_or_else(|| bad(format!("The '{name}' operation needs a value.")));
        match name.as_str() {
            "add" => put(&mut out, &segs, path, need()?, Mode::Add)?,
            "set" => put(&mut out, &segs, path, need()?, Mode::Set)?,
            "replace" => put(&mut out, &segs, path, need()?, Mode::Replace)?,
            "remove" => {
                take(&mut out, &segs, path)?;
            }
            "incr" => incr(&mut out, &segs, path, &need()?)?,
            "move" => {
                let from = op.get("from").and_then(Value::as_str).ok_or_else(|| bad("A move operation needs a 'from' path."))?;
                let fsegs = segments(from)?;
                if touches(&fsegs) {
                    return Err(bad(format!("Patch cannot move the id or the partition key ('{from}').")));
                }
                if overlaps(&fsegs, &segs) && segs.len() > fsegs.len() {
                    return Err(bad("A move operation cannot move a value into itself."));
                }
                let v = take(&mut out, &fsegs, from)?;
                put(&mut out, &segs, path, v, Mode::Add)?;
            }
            other => return Err(bad(format!("Unsupported patch operation '{other}'."))),
        }
        // Each operation can add a nested value; without this check repeated patches would
        // build a document deeper than any request could carry, and walking it (clone, drop,
        // serialize) would overflow the stack.
        crate::limits::check_nesting(&out)?;
    }
    Ok(out)
}

/// The `condition` text ("from c where ...") evaluated against the document.
pub fn condition_holds(cx: &Cx, cond: &str, doc: &Value) -> R<bool> {
    let text = if cond.trim_start().to_ascii_lowercase().starts_with("select") { cond.to_string() } else { format!("SELECT * {cond}") };
    let q = parse(&text).map_err(|e| CosmosError::from_sql(&e))?;
    let rows = q.execute([doc], &Params::new(), &crate::query::eval_options(cx)).map_err(|e| CosmosError::from_sql(&e))?;
    Ok(!rows.is_empty())
}

/// Parsed patch body.
pub struct PatchBody {
    pub ops: Vec<Value>,
    pub condition: Option<String>,
}

/// The body is `{"operations": [...], "condition": "..."}`, or just the array of operations
/// (the JavaScript SDK sends that form when no condition is given).
pub fn parse_body(body: &Value) -> R<PatchBody> {
    if let Value::Array(ops) = body {
        return Ok(PatchBody { ops: ops.clone(), condition: None });
    }
    let ops =
        body.get("operations").and_then(Value::as_array).cloned().ok_or_else(|| bad("The patch body needs an 'operations' array."))?;
    let condition = body.get("condition").and_then(Value::as_str).map(str::to_string);
    Ok(PatchBody { ops, condition })
}

fn protected_paths(c: &Container) -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = c.pk.paths.iter().map(|p| p.trim_start_matches('/').split('/').map(str::to_string).collect()).collect();
    v.push(vec!["id".to_string()]);
    v
}

/// Patch one document in the container; shared by `PATCH` and batch. Returns the new JSON.
pub fn patch_doc(
    cx: &Cx,
    loc: &Loc,
    c: &mut Container,
    key: &DocKey,
    body: &Value,
    check_http_conditions: bool,
    undo: &mut Undo,
) -> R<Value> {
    let pb = parse_body(body)?;
    let existing = c.docs.get(key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
    if check_http_conditions {
        documents::check_write_preconditions(cx, existing)?;
    }
    if let Some(cond) = &pb.condition {
        if !condition_holds(cx, cond, &existing.value)? {
            return Err(CosmosError::precondition_failed());
        }
    }
    let new = apply(&existing.value, &pb.ops, &protected_paths(c))?;
    if crate::limits::json_size(&new) > documents::MAX_DOC_BYTES {
        return Err(CosmosError::too_large());
    }
    let p = documents::prepare(new, c, Some(&key.pk))?;
    let (value, _, _) = documents::store(cx, loc, c, p, undo)?;
    Ok(value)
}

pub fn handle(cx: &Cx, loc: &Loc, c: &mut Container, id: &str) -> R<Out> {
    c.purge_expired(cx.now);
    let key = documents::locate(cx, c, id)?;
    let body = cx.body_json()?;
    let mut undo = c.begin();
    let value = patch_doc(cx, loc, c, &key, &body, true, &mut undo)?;
    Ok(documents::doc_out(cx, c, 200, value, true))
}
