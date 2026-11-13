//! `/dbs/{db}/colls/{coll}/docs`: documents (create, upsert, read, replace, delete, list).

use std::ops::Bound;
use std::sync::Arc;

use azemu_core::etag;
use serde_json::{json, Map, Value};

use crate::epk;
use crate::errors::{check_item_id, pk_mismatch, CosmosError, R};
use crate::out::{doc_charge, Cx, Out};
use crate::paging;
use crate::pk::{self, DocKey, PkValue};
use crate::query;
use crate::select::Selector;
use crate::state::{Container, Doc, Undo};

pub const MAX_DOC_BYTES: usize = 2 * 1024 * 1024;

/// Where the container is, for `_self` links and content paths.
pub struct Loc {
    pub db_rid: String,
}

pub fn doc_self(loc: &Loc, c: &Container, rid: &str) -> String {
    format!("dbs/{}/colls/{}/docs/{}/", loc.db_rid, c.meta.rid, rid)
}

const SYSTEM: [&str; 6] = ["_rid", "_self", "_etag", "_attachments", "_ts", "_lsn"];

/// A validated request body.
pub struct Prepared {
    pub props: Map<String, Value>,
    pub id: String,
    pub pk: PkValue,
}

/// Validate a document body: object, string id, ttl, partition key (checked against the
/// header when one is sent).
pub fn prepare(body: Value, c: &Container, header_pk: Option<&PkValue>) -> R<Prepared> {
    let Value::Object(mut m) = body else {
        return Err(CosmosError::bad_request("The document body must be a JSON object."));
    };
    let id = match m.get("id") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing")),
    };
    check_item_id(&id)?;
    crate::limits::check_map_nesting(&m)?;
    for k in SYSTEM {
        m.shift_remove(k);
    }
    if c.default_ttl.is_some() {
        if let Some(t) = m.get("ttl") {
            match t.as_i64() {
                Some(n) if n == -1 || (n > 0 && n <= 2_147_483_647) => {}
                _ => {
                    return Err(CosmosError::bad_request(
                        "The value of ttl must be either -1 (never expire) or a positive integer up to 2147483647.",
                    ))
                }
            }
        }
    }
    let doc_pk = c.pk.extract(&Value::Object(m.clone()))?;
    if let Some(h) = header_pk {
        if h != &doc_pk {
            return Err(pk_mismatch());
        }
    }
    Ok(Prepared { props: m, id, pk: doc_pk })
}

/// A partition key given as values: `[]` is the undefined partition of a single path container;
/// otherwise one value per path.
fn full_key(c: &Container, v: PkValue) -> R<PkValue> {
    if v.is_empty() && c.pk.paths.len() == 1 {
        return Ok(vec![crate::pk::PkComp::Undefined]);
    }
    if v.len() != c.pk.paths.len() {
        return Err(pk_mismatch());
    }
    Ok(v)
}

/// The partition key header as a full key, if the request has one.
pub fn header_pk(cx: &Cx, c: &Container) -> R<Option<PkValue>> {
    match cx.header("x-ms-documentdb-partitionkey") {
        None => Ok(None),
        Some(raw) => Ok(Some(full_key(c, pk::parse_header(raw)?)?)),
    }
}

/// The partition key an operation of a non-atomic batch carries in its own `partitionKey`
/// member: the SDKs write it as a string holding the JSON array (`"[\"p\"]"`); a JSON array is
/// accepted too. `None` when the operation has none.
pub fn op_pk(c: &Container, op: &Value) -> R<Option<PkValue>> {
    match op.get("partitionKey") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(raw)) => Ok(Some(full_key(c, pk::parse_header(raw)?)?)),
        Some(Value::Array(a)) => {
            let v = a.iter().map(crate::pk::PkComp::from_json).collect::<R<PkValue>>()?;
            Ok(Some(full_key(c, v)?))
        }
        Some(_) => Err(CosmosError::bad_request("The partitionKey of a batch operation must be a JSON array, as a string or an array.")),
    }
}

/// Header key for operations that cannot find the document without it.
pub fn require_pk(cx: &Cx, c: &Container) -> R<PkValue> {
    header_pk(cx, c)?.ok_or_else(|| CosmosError::bad_request("PartitionKey value must be supplied for this operation."))
}

/// The document with this `_rid` (the id as a client may have written it: `-` or `/`).
fn key_by_rid(c: &Container, id: &str) -> Option<DocKey> {
    c.by_rid.get(id).or_else(|| c.by_rid.get(&id.replace('/', "-"))).cloned()
}

/// The document a `/docs/{id}` path addresses: by id plus partition key header, or by `_rid`.
pub fn locate(cx: &Cx, c: &Container, id: &str) -> R<DocKey> {
    if let Some(pkv) = header_pk(cx, c)? {
        let key = DocKey { pk: pkv, id: id.to_string() };
        if c.docs.contains_key(&key) {
            return Ok(key);
        }
    }
    if let Some(k) = key_by_rid(c, id) {
        return Ok(k);
    }
    let pkv = require_pk(cx, c)?;
    Ok(DocKey { pk: pkv, id: id.to_string() })
}

/// Assemble the stored JSON: user properties, then the system properties.
pub fn render(loc: &Loc, c: &Container, props: &Map<String, Value>, rid: &str, etag: &str, ts: i64) -> Value {
    let mut m = props.clone();
    m.insert("_rid".into(), json!(rid));
    m.insert("_self".into(), json!(doc_self(loc, c, rid)));
    m.insert("_etag".into(), json!(etag));
    m.insert("_attachments".into(), json!("attachments/"));
    m.insert("_ts".into(), json!(ts));
    Value::Object(m)
}

/// Store a validated document. `existing` rid and replacement semantics are decided by
/// the caller. Returns the stored JSON.
pub fn store(cx: &Cx, loc: &Loc, c: &mut Container, p: Prepared, undo: &mut Undo) -> R<(Value, bool, u64)> {
    let key = DocKey { pk: p.pk.clone(), id: p.id.clone() };
    let existing = c.docs.get(&key).map(|d| d.rid.clone());
    let created = existing.is_none();
    let rid = match existing {
        Some(r) => r,
        None => c.next_doc_rid().1,
    };
    let etag = etag::new_etag();
    let value = render(loc, c, &p.props, &rid, &etag, cx.now);
    c.check_unique(&key, &value)?;
    let doc = Doc { value: Arc::new(value.clone()), rid, epk: epk::epk(&c.pk, &p.pk), lsn: 0, ts: cx.now, etag };
    c.put(key, doc, undo);
    Ok((value, created, c.lsn))
}

/// `If-Match` / `If-None-Match` against an existing document.
pub fn check_write_preconditions(cx: &Cx, existing: &Doc) -> R<()> {
    if let Some(m) = cx.header("if-match") {
        let m = m.trim();
        if m != "*" && !etag::eq(m, &existing.etag) {
            return Err(CosmosError::precondition_failed());
        }
    }
    if let Some(m) = cx.header("if-none-match") {
        let m = m.trim();
        if m == "*" || etag::eq(m, &existing.etag) {
            return Err(CosmosError::precondition_failed());
        }
    }
    Ok(())
}

fn minimal(cx: &Cx) -> bool {
    cx.header("prefer").map(|p| p.to_ascii_lowercase().contains("return=minimal")).unwrap_or(false)
}

pub fn doc_out(cx: &Cx, c: &Container, status: u16, value: Value, write: bool) -> Out {
    let tag = value.get("_etag").and_then(Value::as_str).unwrap_or("").to_string();
    // the body is serialised after the lock is released; the size sets the charge there
    Out::json(status, value).charge_by_size(write, minimal(cx) && write).header("etag", tag).lsn(c.lsn)
}

fn check_size(cx: &Cx) -> R<()> {
    if cx.req.body.len() > MAX_DOC_BYTES {
        return Err(CosmosError::too_large());
    }
    Ok(())
}

pub fn create(cx: &Cx, loc: &Loc, c: &mut Container, upsert: bool) -> R<Out> {
    check_size(cx)?;
    c.purge_expired(cx.now);
    let body = cx.body_json()?;
    let hp = header_pk(cx, c)?;
    let p = prepare(body, c, hp.as_ref())?;
    let key = DocKey { pk: p.pk.clone(), id: p.id.clone() };
    if let Some(existing) = c.docs.get(&key) {
        if !upsert {
            return Err(CosmosError::conflict());
        }
        check_write_preconditions(cx, existing)?;
    } else if let Some(m) = cx.header("if-match") {
        if upsert && m.trim() != "*" {
            return Err(CosmosError::not_found_default());
        }
    }
    let mut undo = c.begin();
    let (value, created, _) = store(cx, loc, c, p, &mut undo)?;
    Ok(doc_out(cx, c, if created { 201 } else { 200 }, value, true))
}

pub fn read(cx: &Cx, c: &mut Container, id: &str) -> R<Out> {
    c.purge_expired(cx.now);
    let key = locate(cx, c, id)?;
    let doc = c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
    if let Some(m) = cx.header("if-none-match") {
        let m = m.trim();
        if m == "*" || etag::eq(m, &doc.etag) {
            return Ok(Out::empty(304).header("etag", &doc.etag).lsn(c.lsn));
        }
    }
    if let Some(m) = cx.header("if-match") {
        let m = m.trim();
        if m != "*" && !etag::eq(m, &doc.etag) {
            return Err(CosmosError::precondition_failed());
        }
    }
    Ok(doc_out(cx, c, 200, (*doc.value).clone(), false))
}

pub fn replace(cx: &Cx, loc: &Loc, c: &mut Container, id: &str) -> R<Out> {
    check_size(cx)?;
    c.purge_expired(cx.now);
    let body = cx.body_json()?;
    let hp = header_pk(cx, c)?;
    let p = prepare(body, c, hp.as_ref())?;
    if p.id != id && key_by_rid(c, id).is_none() {
        return Err(CosmosError::bad_request("The document id in the body does not match the id in the request path."));
    }
    let mut key = DocKey { pk: p.pk.clone(), id: p.id.clone() };
    if !c.docs.contains_key(&key) {
        // addressed by _rid
        key = locate(cx, c, id)?;
    }
    let existing = c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
    check_write_preconditions(cx, existing)?;
    let mut undo = c.begin();
    let (value, _, _) = store(cx, loc, c, p, &mut undo)?;
    Ok(doc_out(cx, c, 200, value, true))
}

pub fn delete(cx: &Cx, c: &mut Container, id: &str) -> R<Out> {
    c.purge_expired(cx.now);
    let key = locate(cx, c, id)?;
    let existing = c.docs.get(&key).ok_or_else(|| CosmosError::not_found("Entity with the specified id does not exist in the system."))?;
    check_write_preconditions(cx, existing)?;
    let mut undo = c.begin();
    c.delete(&key, &mut undo, cx.now);
    Ok(Out::empty(204).charge(doc_charge(true, 1024)).lsn(c.lsn))
}

/// `GET /docs`: read all documents, or the change feed when `A-IM` asks for it.
pub fn list(cx: &Cx, c: &mut Container) -> R<Out> {
    c.purge_expired(cx.now);
    let aim = cx.header("a-im").map(str::to_ascii_lowercase).unwrap_or_default();
    if aim.contains("incremental") {
        return crate::change_feed::read(cx, c);
    }
    if aim.contains("full-fidelity") || aim.contains("full fidelity") {
        return crate::change_feed::read_all_versions(cx, c);
    }
    let sel = Selector::from_headers(cx, c)?;
    let tok = paging::decode(cx.header("x-ms-continuation"));
    // Resume right behind the last document of the previous page: the documents are in key
    // order, so no page needs to look at the pages before it.
    let start = match (&tok.after, &sel.pk) {
        (Some(k), _) => Bound::Excluded(k.clone()),
        (None, Some(p)) => Bound::Included(DocKey { pk: p.clone(), id: String::new() }),
        (None, None) => Bound::Unbounded,
    };
    let limit = cx.max_item_count().unwrap_or(usize::MAX);
    let mut iter = c
        .docs
        .range((start, Bound::Unbounded))
        .take_while(|(k, _)| match &sel.pk {
            Some(p) => k.pk.len() >= p.len() && k.pk[..p.len()] == p[..],
            None => true,
        })
        .filter(|(k, d)| sel.matches(k, d))
        .skip(if tok.after.is_none() { tok.skip } else { 0 });
    let mut items: Vec<Value> = Vec::new();
    let mut last: Option<DocKey> = None;
    let mut bytes = 0usize;
    let mut more = false;
    for (k, d) in iter.by_ref() {
        let size = paging::approx_size(&d.value);
        if items.len() >= limit || (!items.is_empty() && bytes + size > paging::MAX_PAGE_BYTES) {
            more = true;
            break;
        }
        bytes += size;
        items.push((*d.value).clone());
        last = Some(k.clone());
    }
    let next = match (more, last) {
        (true, Some(k)) => Some(paging::encode(&paging::Token { after: Some(k), ..Default::default() })),
        _ => None,
    };
    let n = items.len();
    let mut out = Out::json(200, query::feed_body(&c.meta.rid, "Documents", items))
        .header("x-ms-item-count", n)
        .charge(2.0 + n as f64 * 0.01)
        .lsn(c.lsn);
    if let Some(t) = next {
        out = out.header("x-ms-continuation", t);
    }
    Ok(out)
}

/// `POST .../operations/partitionkeydelete`: remove every document of one logical partition.
pub fn delete_partition(cx: &Cx, c: &mut Container) -> R<Out> {
    c.purge_expired(cx.now);
    let pkv = require_pk(cx, c)?;
    let keys: Vec<DocKey> = c.partition(&pkv).map(|(k, _)| k.clone()).collect();
    let mut undo = c.begin();
    for k in keys {
        c.delete(&k, &mut undo, cx.now);
    }
    Ok(Out::json(200, json!({})).lsn(c.lsn))
}
