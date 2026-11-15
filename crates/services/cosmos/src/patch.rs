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

const LEARN: &str = "Learn more: https://aka.ms/cosmosdbpatchdocs";

/// An error of operation number `n` (counted from 1), worded as Azure does (checked in November 2026).
fn op_error(n: usize, text: impl std::fmt::Display) -> CosmosError {
    bad(format!("For Operation({n}): {text}"))
}

/// What an operation was working on when it failed: the operation's number and the path text.
struct At<'a> {
    n: usize,
    /// "Add Operation" for `add`, "Given Operation" for the others: Azure's wording of the missing-parent error.
    name: &'a str,
}

fn segments(path: &str) -> R<Vec<String>> {
    if !path.starts_with('/') {
        return Err(bad("One of the specified inputs is invalid"));
    }
    Ok(path[1..].split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect())
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

/// The container of the last segment: every segment but the last must name an existing object or array element, else the
/// path "cannot be created recursively".
fn parent_mut<'a>(doc: &'a mut Value, segs: &[String], at: &At) -> R<&'a mut Value> {
    let mut cur = doc;
    for s in &segs[..segs.len() - 1] {
        cur = match cur {
            Value::Object(m) => m.get_mut(s),
            Value::Array(a) => s.parse::<usize>().ok().and_then(|i| a.get_mut(i)),
            _ => None,
        }
        .ok_or_else(|| {
            op_error(
                at.n,
                format!(
                    "{} can only create a child object of an existing node(array or object) and cannot create path recursively, no path found beyond: '{s}'. {LEARN}.",
                    at.name
                ),
            )
        })?;
    }
    Ok(cur)
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Add,
    Set,
    Replace,
}

fn out_of_bounds(at: &At, index: &str) -> CosmosError {
    op_error(at.n, format!("Index('{index}') to operate on is out of array bounds. Learn More: https://aka.ms/cosmosdbpatchdocs"))
}

fn not_an_index(at: &At, index: &str) -> CosmosError {
    op_error(
        at.n,
        format!(
            "Invalid navigation for array({index}) - Trying to navigate Array node using item which is not suitable for given context. {LEARN}"
        ),
    )
}

fn absent(at: &At, name: &str) -> CosmosError {
    op_error(at.n, format!("Path(from '{name}') on which patch operation has to be applied is absent. {LEARN}"))
}

fn put(doc: &mut Value, segs: &[String], at: &At, value: Value, mode: Mode) -> R<()> {
    let last = segs[segs.len() - 1].clone();
    match parent_mut(doc, segs, at)? {
        Value::Object(m) => {
            if mode == Mode::Replace && !m.contains_key(&last) {
                return Err(op_error(at.n, format!("Node('{last}') to be replaced is absent. {LEARN}")));
            }
            m.insert(last, value);
            Ok(())
        }
        Value::Array(a) => {
            if last == "-" && mode != Mode::Replace {
                a.push(value);
                return Ok(());
            }
            let i: usize = last.parse().map_err(|_| not_an_index(at, &last))?;
            match mode {
                Mode::Add => {
                    if i > a.len() {
                        return Err(out_of_bounds(at, &last));
                    }
                    a.insert(i, value);
                }
                Mode::Set | Mode::Replace => {
                    if i < a.len() {
                        a[i] = value;
                    } else if i == a.len() && mode == Mode::Set {
                        a.push(value);
                    } else {
                        return Err(out_of_bounds(at, &last));
                    }
                }
            }
            Ok(())
        }
        _ => Err(op_error(at.n, format!("{} can only create a child object of an existing node(array or object). {LEARN}", at.name))),
    }
}

fn take(doc: &mut Value, segs: &[String], at: &At) -> R<Value> {
    let last = segs[segs.len() - 1].clone();
    match parent_mut(doc, segs, at).map_err(|_| absent(at, &last))? {
        Value::Object(m) => m.shift_remove(&last).ok_or_else(|| absent(at, &last)),
        Value::Array(a) => {
            let i: usize = last.parse().map_err(|_| not_an_index(at, &last))?;
            if i < a.len() {
                Ok(a.remove(i))
            } else {
                Err(out_of_bounds(at, &last))
            }
        }
        _ => Err(absent(at, &last)),
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

fn incr(doc: &mut Value, segs: &[String], at: &At, by: &Value) -> R<()> {
    let Some(by_n) = by.as_number() else {
        return Err(op_error(at.n, format!("The Increment patch operation requested has an unsupported data type. {LEARN}")));
    };
    let current = get(doc, segs).cloned();
    let new = match current {
        None => by.clone(),
        Some(Value::Number(n)) => add_numbers(&n, by_n),
        Some(_) => {
            let last = segs[segs.len() - 1].clone();
            return Err(op_error(at.n, format!("Node('{last}') is not a number. {LEARN}")));
        }
    };
    put(doc, segs, at, new, Mode::Set)
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
pub fn apply(doc: &Value, ops: &[Value]) -> R<Value> {
    if ops.is_empty() {
        return Err(bad("The patch request has no operations."));
    }
    if ops.len() > MAX_OPERATIONS {
        return Err(bad(format!("The number of patch operations cannot exceed '{MAX_OPERATIONS}'.")));
    }
    let required = |name: &str| bad(format!("The required property '{name}' could not be found in patch request."));
    let mut out = doc.clone();
    for (index, op) in ops.iter().enumerate() {
        let n = index + 1;
        let name = op.get("op").and_then(Value::as_str).ok_or_else(|| required("op"))?.to_ascii_lowercase();
        if !["add", "set", "replace", "remove", "incr", "move"].contains(&name.as_str()) {
            let given = op.get("op").and_then(Value::as_str).unwrap_or("");
            return Err(bad(format!("Invalid value of the patch operation 'op' property in patch request: '{given}'")));
        }
        let path = op.get("path").and_then(Value::as_str).ok_or_else(|| required("path"))?;
        let segs = segments(path)?;
        let value = op.get("value");
        let need = || value.cloned().ok_or_else(|| required("value"));
        // the id and the system properties are refused by name; a change to the partition key is caught when the document is
        // checked against the header (it is a "partition key mismatch" there)
        let first = segs[0].as_str();
        if first == "id" && segs.len() == 1 || ["_rid", "_self", "_etag", "_ts", "_attachments"].contains(&first) {
            return Err(bad(format!("Invalid patch request: Cannot patch system property '{first}'.")));
        }
        let at = At { n, name: if name == "add" { "Add Operation" } else { "Given Operation" } };
        match name.as_str() {
            "add" => put(&mut out, &segs, &at, need()?, Mode::Add)?,
            "set" => put(&mut out, &segs, &at, need()?, Mode::Set)?,
            "replace" => put(&mut out, &segs, &at, need()?, Mode::Replace)?,
            "remove" => {
                take(&mut out, &segs, &at)?;
            }
            "incr" => incr(&mut out, &segs, &at, &need()?)?,
            "move" => {
                let from = op.get("from").and_then(Value::as_str).ok_or_else(|| required("from"))?;
                let fsegs = segments(from)?;
                if fsegs[0] == "id" && fsegs.len() == 1 {
                    return Err(bad("Invalid patch request: Cannot patch system property 'id'."));
                }
                if overlaps(&fsegs, &segs) && segs.len() > fsegs.len() {
                    return Err(op_error(n, "A move operation cannot move a value into itself."));
                }
                let v = take(&mut out, &fsegs, &at)?;
                put(&mut out, &segs, &at, v, Mode::Add)?;
            }
            _ => unreachable!("the name was checked above"),
        }
        // Each operation can add a nested value; without this check repeated patches would
        // build a document deeper than any request could carry, and walking it (clone, drop,
        // serialize) would overflow the stack. Azure answers 500 to such a document (it answers 400 to a request body).
        if crate::limits::check_nesting(&out).is_err() {
            return Err(CosmosError::unknown());
        }
    }
    Ok(out)
}

/// The `condition` text ("from c where ...") evaluated against the document.
pub fn condition_holds(cx: &Cx, cond: &str, doc: &Value) -> R<bool> {
    let text = if cond.trim_start().to_ascii_lowercase().starts_with("select") { cond.to_string() } else { format!("SELECT * {cond}") };
    let q = parse(&text).map_err(|e| condition_error(&e))?;
    let rows = q.execute([doc], &Params::new(), &crate::query::eval_options(cx)).map_err(|e| condition_error(&e))?;
    Ok(!rows.is_empty())
}

/// A bad `condition` is a 400 whose one error is the compiler's own document, as text:
/// `{"Errors":["{\"errors\":[{\"severity\":\"Error\",\"code\":1001,\"message\":\"...\"}]}"]}`.
fn condition_error(e: &azemu_cosmos_sql::SqlError) -> CosmosError {
    let code = e.code.trim_start_matches("SC").parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(e.code.clone()));
    bad(serde_json::json!({ "errors": [{ "severity": "Error", "code": code, "message": e.message }] }).to_string())
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
    if !body.is_object() {
        return Err(bad("Invalid patch request: the payload must be either an array or an object."));
    }
    let ops = body
        .get("operations")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| bad("The required property 'operations' could not be found in patch request."))?;
    let condition = body.get("condition").and_then(Value::as_str).map(str::to_string);
    Ok(PatchBody { ops, condition })
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
    let existing = c.docs.get(key).ok_or_else(CosmosError::item_not_found)?;
    if check_http_conditions {
        documents::check_write_preconditions(cx, existing)?;
    }
    if let Some(cond) = &pb.condition {
        if !condition_holds(cx, cond, &existing.value)? {
            // sub-status 1110: the patch condition is not met
            return Err(CosmosError::precondition_failed().sub(1110));
        }
    }
    let new = apply(&existing.value, &pb.ops)?;
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
    let body = match cx.body_json() {
        Ok(b) => b,
        Err(e) if e.status == 400 => return Err(bad("Invalid patch request: check syntax of patch spec.")),
        Err(e) => return Err(e),
    };
    // a patch with no operations changes nothing: 304 with no body (the condition and the preconditions are not looked at)
    if let Ok(pb) = parse_body(&body) {
        if pb.ops.is_empty() {
            return Ok(Out::empty(304).lsn(c.lsn));
        }
    }
    let mut undo = c.begin();
    let value = patch_doc(cx, loc, c, &key, &body, true, &mut undo)?;
    Ok(documents::doc_out(cx, c, 200, value, true))
}
