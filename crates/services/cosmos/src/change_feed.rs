//! Change feed.
//!
//! Latest version mode (`A-IM: Incremental Feed`): documents in the order of their last
//! modification. The continuation is the `etag` response header, `"<lsn>"`.
//!
//! All versions and deletes mode (`A-IM: Full-Fidelity Feed`): every create, replace and delete,
//! including TTL expiries, in the order they happened. See `read_all_versions`.

use crate::pk::PkComp;
use crate::state::{ChangeKind, ChangeLog};
use azemu_core::time;
use serde_json::{json, Value};

use crate::errors::{CosmosError, R};
use crate::out::{Cx, Out};
use crate::select::Selector;
use crate::state::Container;

const DEFAULT_PAGE: usize = 1000;

fn parse_lsn(s: &str) -> Option<u64> {
    s.trim().trim_matches('"').parse().ok()
}

/// A continuation that is ahead of the container: it came from another container (or from before a restart),
/// and answering "nothing new" would move the client's position backwards.
fn ahead_of_container(from: u64, c: &Container) -> CosmosError {
    CosmosError::bad_request(format!(
        "The change feed continuation is ahead of this container (position {from}, the container is at {}): it does not belong to this container, or the emulator was restarted.",
        c.lsn
    ))
}

pub fn read(cx: &Cx, c: &mut Container) -> R<Out> {
    let sel = Selector::from_headers(cx, c)?;
    let mut from: u64 = 0;
    let mut since: Option<i64> = None;
    match cx.header("if-none-match") {
        Some("*") => from = c.lsn,
        Some(t) => {
            from = parse_lsn(t).ok_or_else(|| CosmosError::bad_request("The change feed continuation is not valid."))?;
            if from > c.lsn {
                return Err(ahead_of_container(from, c));
            }
        }
        None => {
            if let Some(s) = cx.header("if-modified-since") {
                // a date that cannot be read is an error, not "from the beginning"
                since = Some(
                    time::parse_rfc1123(s)
                        .map(|t| t.unix_timestamp())
                        .ok_or_else(|| CosmosError::bad_request("The If-Modified-Since header is not a valid date."))?,
                );
            }
        }
    }
    let limit = cx.max_item_count().unwrap_or(DEFAULT_PAGE);
    let mut picked: Vec<(u64, Value)> = Vec::new();
    let mut more = false;
    let mut bytes = 0usize;
    for (lsn, key) in c.changes.range(from.saturating_add(1)..) {
        let Some(d) = c.docs.get(key) else { continue };
        if !sel.matches(key, d) {
            continue;
        }
        if let Some(s) = since {
            if d.ts < s {
                continue;
            }
        }
        // a page is at most 4 MB (and at least one document), whatever x-ms-max-item-count says
        let size = crate::paging::approx_size(&d.value);
        if picked.len() >= limit || (!picked.is_empty() && bytes + size > MAX_PAGE_BYTES) {
            more = true;
            break;
        }
        bytes += size;
        let mut v = (*d.value).clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("_lsn".into(), json!(lsn));
        }
        picked.push((*lsn, v));
    }
    let next = if more { picked.last().map(|(l, _)| *l).unwrap_or(c.lsn) } else { c.lsn };
    let etag = format!("\"{next}\"");
    if picked.is_empty() {
        return Ok(Out::feed_not_modified(etag).lsn(c.lsn).charge(1.0));
    }
    let docs: Vec<Value> = picked.into_iter().map(|(_, v)| v).collect();
    let n = docs.len();
    Ok(Out::json(200, crate::query::feed_body(&c.meta.rid, "Documents", docs))
        .header("etag", etag)
        .header("x-ms-item-count", n)
        .charge(2.0 + n as f64 * 0.05)
        .lsn(c.lsn))
}

/// Largest page (Azure: 4 MB per response).
const MAX_PAGE_BYTES: usize = crate::paging::MAX_PAGE_BYTES;

/// `{"pk": value}` for the path names of the container (`/a/b` gives `{"a":{"b":v}}`); undefined parts are left out.
fn partition_key_object(c: &Container, key: &[PkComp]) -> Value {
    let mut root = serde_json::Map::new();
    for (path, comp) in c.pk.paths.iter().zip(key) {
        if matches!(comp, PkComp::Undefined) {
            continue;
        }
        let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let mut cur = &mut root;
        for (i, seg) in segs.iter().enumerate() {
            if i + 1 == segs.len() {
                cur.insert((*seg).to_string(), comp.to_json());
            } else {
                let next = cur.entry((*seg).to_string()).or_insert_with(|| Value::Object(Default::default()));
                match next {
                    Value::Object(m) => cur = m,
                    _ => break,
                }
            }
        }
    }
    Value::Object(root)
}

/// All versions and deletes. Starts from "now" (`If-None-Match: *`) or from a continuation (the
/// `etag` of an earlier answer); the history is kept from the first read of the container.
/// Item shapes are the ones on Microsoft Learn ("Change feed modes", "Parse the response object"):
/// `{"current": item, "metadata": {"operationType", "lsn", "crts", ["previousImageLSN"]}}`; a
/// delete has no `current` and its metadata carries `id`, `partitionKey` and `timeToLiveExpired`.
/// Azure gives no previous image of the item ("There's currently no way to get the previous
/// version of items for either replace or delete operations"), so there is no `previous`.
pub fn read_all_versions(cx: &Cx, c: &mut Container) -> R<Out> {
    let sel = Selector::from_headers(cx, c)?;
    let from: u64 = match cx.header("if-none-match") {
        Some("*") => {
            c.log.get_or_insert_with(|| ChangeLog::new(c.lsn));
            c.lsn
        }
        Some(t) => {
            let n = parse_lsn(t).ok_or_else(|| CosmosError::bad_request("The change feed continuation is not valid."))?;
            match &c.log {
                Some(log) if n < log.floor => {
                    return Err(CosmosError::bad_request(
                        "The change feed continuation is older than the retained history: changes that old are no longer available.",
                    ))
                }
                Some(_) if n > c.lsn => return Err(ahead_of_container(n, c)),
                Some(_) => n,
                None => {
                    // History starts now (the emulator forgot or never had the changes before this point).
                    c.log = Some(ChangeLog::new(c.lsn));
                    c.lsn
                }
            }
        }
        None => {
            return Err(CosmosError::bad_request(
                "The all versions and deletes change feed can only be read from now (If-None-Match: *) or from a continuation.",
            ))
        }
    };
    let limit = cx.max_item_count().unwrap_or(DEFAULT_PAGE);
    let mut items: Vec<Value> = Vec::new();
    let mut last = from;
    let mut more = false;
    let mut bytes = 0usize;
    if let Some(log) = &c.log {
        let start = log.records.partition_point(|r| r.lsn <= from);
        for r in log.records.iter().skip(start) {
            if !sel.matches_parts(&r.key, &r.epk) {
                continue;
            }
            if items.len() >= limit || (!items.is_empty() && bytes + r.bytes > MAX_PAGE_BYTES) {
                more = true;
                break;
            }
            let mut meta = serde_json::Map::new();
            let op = match r.kind {
                ChangeKind::Create => "create",
                ChangeKind::Replace => "replace",
                ChangeKind::Delete => "delete",
            };
            meta.insert("operationType".into(), json!(op));
            meta.insert("lsn".into(), json!(r.lsn));
            meta.insert("crts".into(), json!(r.ts));
            if let Some(p) = r.prev_lsn {
                meta.insert("previousImageLSN".into(), json!(p));
            }
            let mut item = serde_json::Map::new();
            match (&r.current, r.kind) {
                (Some(v), _) => {
                    let mut cur = (**v).clone();
                    if let Some(o) = cur.as_object_mut() {
                        o.insert("_lsn".into(), json!(r.lsn));
                    }
                    item.insert("current".into(), cur);
                }
                (None, _) => {
                    meta.insert("timeToLiveExpired".into(), json!(r.ttl_expired));
                    meta.insert("id".into(), json!(r.key.id));
                    meta.insert("partitionKey".into(), partition_key_object(c, &r.key.pk));
                }
            }
            item.insert("metadata".into(), Value::Object(meta));
            items.push(Value::Object(item));
            bytes += r.bytes;
            last = r.lsn;
        }
    }
    let next = if more { last } else { c.lsn };
    let etag = format!("\"{next}\"");
    if items.is_empty() {
        return Ok(Out::feed_not_modified(etag).lsn(c.lsn).charge(1.0));
    }
    let n = items.len();
    Ok(Out::json(200, crate::query::feed_body(&c.meta.rid, "Documents", items))
        .header("etag", etag)
        .header("x-ms-item-count", n)
        .charge(2.0 + n as f64 * 0.05)
        .lsn(c.lsn))
}
