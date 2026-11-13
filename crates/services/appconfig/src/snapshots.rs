//! Snapshots: `PUT/GET/HEAD/PATCH /snapshots/{name}`, `GET/HEAD /snapshots`,
//! `GET /operations?snapshot={name}`.
//!
//! A snapshot copies the matching settings when it is created and never changes after.
//! Creation answers 201 `provisioning` and the snapshot is `ready` at once; the
//! operation endpoint answers `Succeeded` at once.

use std::collections::BTreeMap;
use std::sync::Arc;

use azemu_core::conditions::{Access, Outcome};
use azemu_core::resp::Resp;
use azemu_core::time::{self, Duration, OffsetDateTime};
use azemu_core::{Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::auth;
use crate::errors;
use crate::filters::{parse_tags, tags_match, Filter};
use crate::keyvalues::{conditions, select_param};
use crate::lists::{filter_param, MAX_FILTER_VALUES};
use crate::paging::{decode_cursor, encode_component, encode_cursor, link_target, respond, Page, PAGE_SIZE};
use crate::state::{Setting, Store};
use crate::timefmt::{iso_7, iso_secs};

const SNAPSHOT_CT: &str = "application/vnd.microsoft.appconfig.snapshot+json; charset=utf-8";
const SNAPSHOTSET_CT: &str = "application/vnd.microsoft.appconfig.snapshotset+json; charset=utf-8";
const DEFAULT_RETENTION: i64 = 2_592_000;

const SNAPSHOT_FIELDS: &[&str] = &[
    "etag",
    "name",
    "description",
    "status",
    "filters",
    "composition_type",
    "created",
    "expires",
    "size",
    "items_count",
    "tags",
    "retention_period",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapFilter {
    pub key: String,
    pub label: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub etag: String,
    pub archived: bool,
    pub filters: Vec<SnapFilter>,
    pub composition_type: String,
    pub created: OffsetDateTime,
    pub expires: Option<OffsetDateTime>,
    pub retention_period: i64,
    pub size: u64,
    pub tags: Vec<(String, String)>,
    pub description: Option<String>,
    /// The settings as they were at creation, in listing order.
    /// Shared with the live settings: a snapshot of a setting nobody changed holds the same `Arc`, not a copy.
    pub items: Vec<Arc<Setting>>,
}

impl Snapshot {
    fn status(&self) -> &'static str {
        if self.archived {
            "archived"
        } else {
            "ready"
        }
    }

    fn json(&self, select: Option<&[&str]>, newer: bool, provisioning: bool) -> Value {
        let mut m = Map::new();
        for f in SNAPSHOT_FIELDS {
            if let Some(sel) = select {
                if !sel.contains(f) {
                    continue;
                }
            }
            let v = match *f {
                "etag" => json!(self.etag),
                "name" => json!(self.name),
                "description" => {
                    if !newer {
                        continue;
                    }
                    json!(self.description)
                }
                "status" => json!(if provisioning { "provisioning" } else { self.status() }),
                "filters" => Value::Array(self.filters.iter().map(|f| json!({"key": f.key, "label": f.label, "tags": f.tags})).collect()),
                "composition_type" => json!(self.composition_type),
                "created" => json!(if provisioning { iso_7(self.created) } else { iso_secs(self.created) }),
                "expires" => match self.expires {
                    Some(e) => json!(iso_7(e)),
                    None => continue,
                },
                "size" => json!(if provisioning { 0 } else { self.size }),
                "items_count" => json!(if provisioning { 0 } else { self.items.len() }),
                "tags" => {
                    let mut t = Map::new();
                    for (k, v) in &self.tags {
                        t.insert(k.clone(), json!(v));
                    }
                    Value::Object(t)
                }
                _ => json!(self.retention_period),
            };
            m.insert((*f).to_string(), v);
        }
        Value::Object(m)
    }
}

/// Archived snapshots disappear when their retention period is over.
pub fn purge_expired(store: &mut Store, now: OffsetDateTime) {
    store.snapshots.retain(|_, s| s.expires.map(|e| e > now).unwrap_or(true));
}

fn newer(req: &Request) -> bool {
    auth::is_newer(auth::version(req))
}

fn name_from_path(req: &Request) -> Result<String, Response> {
    let rest = req.path.strip_prefix("/snapshots/").unwrap_or("");
    let name = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .map_err(|_| errors::invalid_argument("name", "name: The name is not valid UTF-8.", None))?
        .into_owned();
    if name.is_empty() {
        return Err(errors::invalid_argument("name", "name: The snapshot name is empty.", None));
    }
    Ok(name)
}

fn snapshot_response(req: &Request, s: &Snapshot, status: u16, provisioning: bool, select: Option<&[&str]>) -> Response {
    let version = auth::version(req);
    let body = serde_json::to_vec(&s.json(select, newer(req), provisioning)).unwrap_or_default();
    Resp::new(status)
        .header("Content-Type", SNAPSHOT_CT)
        .header("ETag", format!("\"{}\"", s.etag))
        .header("Last-Modified", time::rfc1123(s.created))
        .header("Link", format!("</kv?snapshot={}&api-version={}>; rel=\"items\"", encode_component(&s.name), encode_component(version)))
        .body(body)
        .build()
}

pub fn get(store: &mut Store, req: &Request) -> Response {
    purge_expired(store, req.ctx.time);
    let name = match name_from_path(req) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let select = match select_param(req, SNAPSHOT_FIELDS) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(s) = store.snapshots.get(&name) else {
        return errors::not_found();
    };
    let res = azemu_core::conditions::Resource::Exists { etag: &s.etag, last_modified: s.created };
    match conditions(req).evaluate(res, Access::Read) {
        Outcome::Proceed => {}
        Outcome::NotModified => return errors::empty(304),
        Outcome::PreconditionFailed => return errors::precondition_failed(),
    }
    snapshot_response(req, s, 200, false, select.as_deref())
}

fn bad(detail: &str, param: &str) -> Response {
    errors::invalid_argument(param, detail, None)
}

fn parse_filters(v: Option<&Value>) -> Result<Vec<SnapFilter>, Response> {
    let Some(Value::Array(list)) = v else {
        return Err(bad("filters: At least one filter is required.", "filters"));
    };
    if list.is_empty() || list.len() > 3 {
        return Err(bad("filters: Between 1 and 3 filters are required.", "filters"));
    }
    let mut out = Vec::new();
    for f in list {
        let Value::Object(m) = f else {
            return Err(bad("filters: Each filter must be an object.", "filters"));
        };
        let Some(Value::String(key)) = m.get("key") else {
            return Err(bad("filters: Each filter needs a 'key'.", "filters"));
        };
        let label = match m.get("label") {
            None | Some(Value::Null) => None,
            Some(Value::String(l)) => Some(l.clone()),
            Some(_) => return Err(bad("filters: 'label' must be a string.", "filters")),
        };
        let mut tags = Vec::new();
        match m.get("tags") {
            None | Some(Value::Null) => {}
            Some(Value::Array(a)) => {
                for t in a {
                    match t {
                        Value::String(s) => tags.push(s.clone()),
                        _ => return Err(bad("filters: Filter tags must be strings.", "filters")),
                    }
                }
            }
            Some(_) => return Err(bad("filters: 'tags' must be an array.", "filters")),
        }
        out.push(SnapFilter { key: key.clone(), label, tags });
    }
    Ok(out)
}

/// The settings a snapshot with these filters would contain right now.
/// The settings the filters pick, in listing order, shared with the live ones. The bytes of what is picked are counted as it
/// goes, so a filter that picks more than a snapshot may hold stops at the limit instead of collecting everything first.
fn collect(store: &Store, filters: &[SnapFilter], composition: &str) -> Result<Vec<Arc<Setting>>, Response> {
    let mut picked: BTreeMap<(&str, Option<&str>), &Arc<Setting>> = BTreeMap::new();
    let mut bytes = 0usize;
    for f in filters {
        let key = Filter::parse(&f.key, false)
            .map_err(|e| errors::invalid_argument("filters", &format!("key({}): Invalid character", e.pos), Some(e.pos)))?;
        let label = match &f.label {
            None => Filter::parse("\\0", true).unwrap_or_else(|_| Filter::any()),
            Some(l) => Filter::parse(l, true)
                .map_err(|e| errors::invalid_argument("filters", &format!("label({}): Invalid character", e.pos), Some(e.pos)))?,
        };
        if key.len() > MAX_FILTER_VALUES || label.len() > MAX_FILTER_VALUES {
            return Err(bad(
                &format!("filters: at most {MAX_FILTER_VALUES} comma-separated values are allowed in a key or label filter."),
                "filters",
            ));
        }
        let tags = parse_tags(f.tags.iter().map(String::as_str)).map_err(|d| bad(&d, "filters"))?;
        for s in store.live_arcs() {
            if key.matches_str(&s.key) && label.matches(s.label.as_deref()) && tags_match(&s.tags, &tags) {
                let id = if composition == "key" { (s.key.as_str(), None) } else { (s.key.as_str(), s.label.as_deref()) };
                if let Some(old) = picked.insert(id, s) {
                    bytes -= raw_bytes(old);
                }
                bytes += raw_bytes(s);
                if bytes > store.limits.snapshot_max_bytes {
                    return Err(too_large(store));
                }
            }
        }
    }
    Ok(picked.into_values().cloned().collect())
}

fn too_large(store: &Store) -> Response {
    bad(&format!("filters: the snapshot would be larger than the limit of {} bytes.", store.limits.snapshot_max_bytes), "filters")
}

fn raw_bytes(s: &Setting) -> usize {
    s.key.len()
        + s.label.as_deref().map(str::len).unwrap_or(0)
        + s.value.as_deref().map(str::len).unwrap_or(0)
        + s.content_type.as_deref().map(str::len).unwrap_or(0)
        + s.tags.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>()
}

/// Size as the service reports it: the bytes of the items, rounded up to 1000, and never less than 1000: recorded 1000 for one small
/// setting and also for a snapshot of no item at all (`test_appconfig_client.pyTestAppConfigurationClientAADtest_list_snapshot_configuration_settings.json`,
/// entries 12 and 13: `items_count: 0` and `size: 1000`). A snapshot still being made has size 0.
fn size_of(items: &[Arc<Setting>]) -> u64 {
    let bytes: usize = items.iter().map(|s| raw_bytes(s)).sum();
    (bytes as u64).div_ceil(1000).max(1) * 1000
}

/// What a snapshot counts against the store's snapshot storage: its size and a little for each item (the pointer and the
/// entry), with at least 1000 bytes so that snapshots of nothing are not free.
fn charge(s: &Snapshot) -> usize {
    (s.size as usize).max(1000) + 16 * s.items.len()
}

fn quota_exceeded(policy: &str) -> Response {
    errors::quota_exceeded(policy)
}

pub fn put(store: &mut Store, req: &Request) -> Response {
    purge_expired(store, req.ctx.time);
    let name = match name_from_path(req) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return errors::invalid_body(&format!("The body is not valid JSON: {e}")),
    };
    let Value::Object(m) = body else {
        return errors::invalid_body("The body must be a JSON object.");
    };
    let filters = match parse_filters(m.get("filters")) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let composition = match m.get("composition_type") {
        None | Some(Value::Null) => "key".to_string(),
        Some(Value::String(c)) if c == "key" || c == "key_label" => c.clone(),
        Some(_) => return bad("composition_type: Must be 'key' or 'key_label'.", "composition_type"),
    };
    let retention = match m.get("retention_period") {
        None | Some(Value::Null) => DEFAULT_RETENTION,
        Some(v) => match v.as_i64() {
            Some(n) if (3600..=7_776_000).contains(&n) => n,
            _ => return bad("retention_period: Must be between 3600 and 7776000 seconds.", "retention_period"),
        },
    };
    let mut tags = Vec::new();
    match m.get("tags") {
        None | Some(Value::Null) => {}
        Some(Value::Object(t)) => {
            for (k, v) in t {
                match v {
                    Value::String(s) => tags.push((k.clone(), s.clone())),
                    _ => return errors::invalid_body("Tag values must be strings."),
                }
            }
        }
        Some(_) => return errors::invalid_body("'tags' must be an object."),
    }
    let description = match m.get("description") {
        Some(Value::String(d)) => Some(d.clone()),
        _ => None,
    };
    if store.snapshots.contains_key(&name) {
        return errors::snapshot_exists(&name);
    }
    if store.snapshots.len() >= store.limits.snapshots {
        return quota_exceeded("Snapshot Count");
    }
    let items = match collect(store, &filters, &composition) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let size = size_of(&items);
    if size as usize > store.limits.snapshot_max_bytes {
        return too_large(store);
    }
    let snap = Snapshot {
        name: name.clone(),
        etag: store.fresh_etag(),
        archived: false,
        filters,
        composition_type: composition,
        created: req.ctx.time,
        expires: None,
        retention_period: retention,
        size,
        tags,
        description,
        items,
    };
    let used: usize = store.snapshots.values().map(charge).sum();
    if used + charge(&snap) > store.limits.snapshot_bytes {
        return quota_exceeded("Snapshot Storage Quota");
    }
    let mut resp = snapshot_response(req, &snap, 201, true, None);
    let op = format!(
        "{}/operations?snapshot={}&api-version={}",
        req.ctx.endpoint.base_url(),
        encode_component(&name),
        encode_component(auth::version(req))
    );
    if let Ok(v) = azemu_core::http::HeaderValue::from_str(&op) {
        resp.headers_mut().insert("Operation-Location", v);
    }
    store.snapshots.insert(name, snap);
    resp
}

pub fn patch(store: &mut Store, req: &Request) -> Response {
    purge_expired(store, req.ctx.time);
    let name = match name_from_path(req) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return errors::invalid_body(&format!("The body is not valid JSON: {e}")),
    };
    let archive = match body.get("status").and_then(Value::as_str) {
        Some("archived") => true,
        Some("ready") => false,
        _ => return bad("status: Must be 'archived' or 'ready'.", "status"),
    };
    let new_etag = store.fresh_etag();
    let Some(s) = store.snapshots.get_mut(&name) else {
        return errors::not_found();
    };
    let res = azemu_core::conditions::Resource::Exists { etag: &s.etag, last_modified: s.created };
    if !matches!(conditions(req).evaluate(res, Access::Write), Outcome::Proceed) {
        return errors::precondition_failed();
    }
    if s.archived != archive {
        s.archived = archive;
        s.etag = new_etag;
        s.expires = if archive { Some(req.ctx.time + Duration::seconds(s.retention_period)) } else { None };
    }
    snapshot_response(req, s, 200, false, None)
}

pub fn list(store: &mut Store, req: &Request) -> Response {
    purge_expired(store, req.ctx.time);
    let name = match filter_param(req, "name", false, "name") {
        Ok(f) => f,
        Err(r) => return r,
    };
    let select = match select_param(req, SNAPSHOT_FIELDS) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let statuses: Vec<String> =
        req.query.get_all("status").flat_map(|s| s.split(',')).map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    let cursor = req.query.get("after").filter(|a| !a.is_empty()).and_then(decode_cursor);
    let wanted: Vec<&Snapshot> = store
        .snapshots
        .values()
        .filter(|s| name.matches_str(&s.name))
        .filter(|s| statuses.is_empty() || statuses.iter().any(|w| w == s.status()))
        .filter(|s| cursor.as_deref().map(|c| s.name.as_str() > c).unwrap_or(true))
        .take(PAGE_SIZE)
        .collect();
    let more = wanted.len() >= PAGE_SIZE;
    let page_items: Vec<&Snapshot> = wanted.into_iter().take(PAGE_SIZE).collect();
    let items = page_items.iter().map(|s| s.json(select.as_deref(), newer(req), false)).collect();
    let mut page = Page::new(SNAPSHOTSET_CT, items);
    if more {
        if let Some(last) = page_items.last() {
            page.next = Some(link_target(req, "/snapshots", &encode_cursor(&last.name)));
        }
    }
    respond(req, page, newer(req))
}

/// `GET /operations?snapshot={name}`: creation is finished as soon as it starts.
pub fn operation(store: &mut Store, req: &Request) -> Response {
    purge_expired(store, req.ctx.time);
    let Some(name) = req.query.get("snapshot").filter(|n| !n.is_empty()) else {
        return errors::invalid_argument("snapshot", "snapshot: The snapshot parameter is required.", None);
    };
    if !store.snapshots.contains_key(name) {
        return errors::not_found();
    }
    Resp::new(200u16).json(&json!({"id": name, "status": "Succeeded", "error": null})).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::NewSetting;

    #[test]
    fn a_snapshot_shares_the_settings_with_the_live_store_instead_of_copying_them() {
        let mut store = Store::default();
        let t = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let w = NewSetting {
            key: "k".into(),
            label: None,
            value: Some("x".repeat(100_000)),
            content_type: None,
            description: None,
            tags: vec![],
            locked: false,
        };
        let live = store.write(w, t).unwrap();
        let filters = vec![SnapFilter { key: "k".into(), label: None, tags: vec![] }];
        let a = collect(&store, &filters, "key").unwrap();
        let b = collect(&store, &filters, "key").unwrap();
        assert!(Arc::ptr_eq(&a[0], &live) && Arc::ptr_eq(&b[0], &live), "no copy of the 100 KB value was made");
        // 1 (store) + 1 (revision) + 1 (`live` here) + the two snapshots' items
        assert_eq!(Arc::strong_count(&live), 5);
    }
}
