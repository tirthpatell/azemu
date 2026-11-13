//! Batch requests (`POST .../docs` with `x-ms-cosmos-is-batch-request`), up to 100 operations. The body is a JSON
//! array (Python, JavaScript, Java) or, from the .NET SDK, a HybridRow record stream (`hybridrow.rs`, read into the
//! same JSON operations before the lock is taken; the answer is then HybridRow too).
//!
//! - Transactional batch (`x-ms-cosmos-batch-atomic: true`, or the header absent): one logical
//!   partition, named by `x-ms-documentdb-partitionkey`; all operations or none. A failure rolls
//!   everything back and answers 207 with the failing status for that operation and 424 for the rest.
//! - Bulk (`x-ms-cosmos-batch-atomic: false`, what the JavaScript, Java and .NET bulk executors send): the
//!   request is addressed to a partition key range (`x-ms-documentdb-partitionkeyrangeid`), and every
//!   operation carries its own `partitionKey` (a string holding a JSON array). Each operation
//!   succeeds or fails alone. With `x-ms-cosmos-batch-continue-on-error: false` the first failure
//!   stops the run and the operations after it answer 424. The status is 200 when every operation
//!   worked and 207 otherwise.

use azemu_core::etag;
use serde_json::{json, Value};

use crate::documents::{self, Loc};
use crate::errors::{CosmosError, R};
use crate::out::{doc_charge, Cx, Out};
use crate::patch;
use crate::pk::DocKey;
use crate::state::{Container, Doc, Undo};

pub const MAX_OPS: usize = 100;

fn conditions(op: &Value, existing: &Doc) -> R<()> {
    if let Some(m) = op.get("ifMatch").and_then(Value::as_str) {
        if m != "*" && !etag::eq(m, &existing.etag) {
            return Err(CosmosError::precondition_failed());
        }
    }
    if let Some(m) = op.get("ifNoneMatch").and_then(Value::as_str) {
        if m == "*" || etag::eq(m, &existing.etag) {
            return Err(CosmosError::precondition_failed());
        }
    }
    Ok(())
}

fn result_ok(status: u16, value: Option<Value>) -> Value {
    let mut r = json!({ "statusCode": status });
    let size = value.as_ref().and_then(|v| serde_json::to_vec(v).ok()).map(|v| v.len()).unwrap_or(0);
    r["requestCharge"] = json!(doc_charge(status != 200, size));
    if let Some(v) = value {
        if let Some(e) = v.get("_etag") {
            r["eTag"] = e.clone();
        }
        r["resourceBody"] = v;
    }
    r
}

fn run_op(cx: &Cx, loc: &Loc, c: &mut Container, pkv: &crate::pk::PkValue, op: &Value, undo: &mut Undo) -> R<Value> {
    let mut r = run_op_inner(cx, loc, c, pkv, op, undo)?;
    // `minimalReturnPreference` (the .NET SDK with EnableContentResponseOnWrite = false): a write answers without the body
    if op.get("minimalReturnPreference").and_then(Value::as_bool) == Some(true) {
        let write = matches!(op.get("operationType").and_then(Value::as_str), Some("create" | "upsert" | "replace" | "patch"));
        if write {
            if let Some(o) = r.as_object_mut() {
                o.remove("resourceBody");
            }
        }
    }
    Ok(r)
}

fn run_op_inner(cx: &Cx, loc: &Loc, c: &mut Container, pkv: &crate::pk::PkValue, op: &Value, undo: &mut Undo) -> R<Value> {
    let kind = op.get("operationType").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let id = op.get("id").and_then(Value::as_str).map(str::to_string);
    let body = op.get("resourceBody");
    let find_key = |id: &Option<String>| -> R<DocKey> {
        let id = id.clone().ok_or_else(|| CosmosError::bad_request("The batch operation needs an id."))?;
        Ok(DocKey { pk: pkv.clone(), id })
    };
    match kind.as_str() {
        "create" | "upsert" => {
            let body = body.cloned().ok_or_else(|| CosmosError::bad_request("The batch operation needs a resourceBody."))?;
            let p = documents::prepare(body, c, Some(pkv))?;
            let key = DocKey { pk: p.pk.clone(), id: p.id.clone() };
            if let Some(existing) = c.docs.get(&key) {
                if kind == "create" {
                    return Err(CosmosError::conflict());
                }
                conditions(op, existing)?;
            }
            let (v, created, _) = documents::store(cx, loc, c, p, undo)?;
            Ok(result_ok(if created { 201 } else { 200 }, Some(v)))
        }
        "replace" => {
            let body = body.cloned().ok_or_else(|| CosmosError::bad_request("The batch operation needs a resourceBody."))?;
            let key = find_key(&id)?;
            let existing =
                c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
            conditions(op, existing)?;
            let p = documents::prepare(body, c, Some(pkv))?;
            if Some(&p.id) != id.as_ref() {
                return Err(CosmosError::bad_request("The document id in the body does not match the operation id."));
            }
            let (v, _, _) = documents::store(cx, loc, c, p, undo)?;
            Ok(result_ok(200, Some(v)))
        }
        "delete" => {
            let key = find_key(&id)?;
            let existing =
                c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
            conditions(op, existing)?;
            c.delete(&key, undo, cx.now);
            Ok(result_ok(204, None))
        }
        "read" => {
            let key = find_key(&id)?;
            let existing =
                c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
            conditions(op, existing)?;
            Ok(result_ok(200, Some((*existing.value).clone())))
        }
        "patch" => {
            let key = find_key(&id)?;
            let body = body.cloned().ok_or_else(|| CosmosError::bad_request("The batch operation needs a resourceBody."))?;
            if let Some(existing) = c.docs.get(&key) {
                conditions(op, existing)?;
            }
            let v = patch::patch_doc(cx, loc, c, &key, &body, false, undo)?;
            Ok(result_ok(200, Some(v)))
        }
        other => Err(CosmosError::bad_request(format!("Unsupported batch operation type '{other}'."))),
    }
}

fn failure(e: &CosmosError) -> Value {
    let mut r = json!({ "statusCode": e.status, "requestCharge": 1.0 });
    if let Some(s) = e.substatus {
        r["subStatusCode"] = json!(s);
    }
    r
}

fn skipped() -> Value {
    json!({ "statusCode": 424, "requestCharge": 0.0 })
}

fn total_charge(results: &[Value]) -> f64 {
    results.iter().filter_map(|r| r.get("requestCharge").and_then(Value::as_f64)).sum()
}

/// The answer: a JSON array of results, or HybridRow records when the request was HybridRow (the .NET SDK).
fn answer(cx: &Cx, status: u16, results: Vec<Value>) -> Out {
    if cx.is_hybrid_batch() {
        Out::raw(status, crate::hybridrow::encode_results(&results), "application/octet-stream")
    } else {
        Out::json(status, Value::Array(results))
    }
}

pub fn handle(cx: &Cx, loc: &Loc, c: &mut Container) -> R<Out> {
    c.purge_expired(cx.now);
    let body = cx.body_json()?;
    let ops = body.as_array().ok_or_else(|| CosmosError::bad_request("The batch request body must be an array of operations."))?;
    if ops.is_empty() {
        return Err(CosmosError::bad_request("Batch request has no operations."));
    }
    if ops.len() > MAX_OPS {
        return Err(CosmosError::bad_request("Batch request has more operations than what is supported."));
    }
    if cx.header("x-ms-cosmos-batch-atomic").is_some() && !cx.flag("x-ms-cosmos-batch-atomic") {
        bulk(cx, loc, c, ops)
    } else {
        transaction(cx, loc, c, ops)
    }
}

/// Non-atomic: every operation names its own partition and stands alone.
fn bulk(cx: &Cx, loc: &Loc, c: &mut Container, ops: &[Value]) -> R<Out> {
    let header_pk = documents::header_pk(cx, c)?;
    // Absent header: continue on error (the JavaScript SDK always sends it; .NET's default is the same).
    let continue_on_error =
        cx.header("x-ms-cosmos-batch-continue-on-error").map(|_| cx.flag("x-ms-cosmos-batch-continue-on-error")).unwrap_or(true);
    let mut results: Vec<Value> = Vec::with_capacity(ops.len());
    let mut stopped = false;
    for op in ops {
        if stopped {
            results.push(skipped());
            continue;
        }
        let one = documents::op_pk(c, op).and_then(|own| {
            own.or_else(|| header_pk.clone())
                .ok_or_else(|| CosmosError::bad_request("PartitionKey value must be supplied for this operation."))
        });
        let mut undo = c.begin();
        let res = one.and_then(|pkv| run_op(cx, loc, c, &pkv, op, &mut undo));
        match res {
            Ok(r) => results.push(r),
            Err(e) => {
                c.rollback(undo);
                results.push(failure(&e));
                stopped = !continue_on_error;
            }
        }
    }
    let status = if results.iter().all(|r| r["statusCode"].as_u64().map(|s| s < 400).unwrap_or(false)) { 200 } else { 207 };
    let charge = total_charge(&results);
    Ok(answer(cx, status, results).charge(charge).lsn(c.lsn))
}

/// Atomic: one logical partition, all operations or none.
fn transaction(cx: &Cx, loc: &Loc, c: &mut Container, ops: &[Value]) -> R<Out> {
    let pkv = documents::require_pk(cx, c)?;
    let mut undo = c.begin();
    let mut results: Vec<Value> = Vec::with_capacity(ops.len());
    let mut failed: Option<usize> = None;
    for (i, op) in ops.iter().enumerate() {
        match run_op(cx, loc, c, &pkv, op, &mut undo) {
            Ok(r) => results.push(r),
            Err(e) => {
                results.push(failure(&e));
                failed = Some(i);
                break;
            }
        }
    }
    let n = ops.len();
    if let Some(i) = failed {
        c.rollback(undo);
        let mut all = Vec::with_capacity(n);
        for (j, r) in results.into_iter().enumerate() {
            if j == i {
                all.push(r);
            } else {
                all.push(skipped());
            }
        }
        for _ in (i + 1)..n {
            all.push(skipped());
        }
        return Ok(answer(cx, 207, all).charge(1.0 + n as f64).lsn(c.lsn));
    }
    Ok(answer(cx, 200, results).charge(5.71 * n as f64).lsn(c.lsn))
}
