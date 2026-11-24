//! The inspector of App Configuration (`docs/inspector.md`): stores, key-values with their labels, the kept revisions and the
//! snapshots with their contents. Only reads: a store that does not exist is a 404 and is not made, no revision is dropped, and no
//! archived snapshot is purged for being past its date (it is shown with `expired: true`).

use azemu_core::inspect::{rfc3339, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Value};

use crate::snapshots::Snapshot;
use crate::state::{Setting, State, Store};

/// The longest value shown in a row: 64 KiB, cut at a character.
const VALUE_LIMIT: usize = 64 * 1024;

pub(crate) fn node(state: &State, req: &InspectRequest) -> InspectResult {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let now = OffsetDateTime::now_utc();
    match segs.as_slice() {
        [] => Ok(service_node(state, req)),
        [name, rest @ ..] => {
            let store =
                state.stores.get(*name).ok_or_else(|| InspectError::not_found(format!("There is no configuration store '{name}'.")))?;
            match rest {
                [] => Ok(store_node(name, store, req)),
                ["keyvalues"] => Ok(keyvalues_node(name, store, req)),
                ["revisions"] => Ok(revisions_node(name, store, req)),
                ["snapshots"] => Ok(snapshots_node(name, store, req, now)),
                ["snapshots", snap] => {
                    let s = store.snapshots.get(*snap).ok_or_else(|| InspectError::not_found(format!("There is no snapshot '{snap}'.")))?;
                    Ok(snapshot_node(name, s, req, now))
                }
                ["snapshots", snap, "items"] => {
                    let s = store.snapshots.get(*snap).ok_or_else(|| InspectError::not_found(format!("There is no snapshot '{snap}'.")))?;
                    Ok(snapshot_items_node(name, s, req))
                }
                _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
            }
        }
    }
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let mut names: Vec<&String> = state.stores.keys().collect();
    names.sort();
    let (range, next) = req.page_range(names.len());
    let items =
        names[range].iter().map(|n| json!({"name": n, "path": req.path(&[n]), "snapshots": state.stores[*n].snapshots.len()})).collect();
    Node::new(req.path(&[]), "service", "appconfig").prop("accounts", names.len()).with_items(items, next)
}

fn store_node(name: &str, s: &Store, req: &InspectRequest) -> Node {
    let live = s.live_after(None).count() as u64;
    let revisions = s.revs_before(None).count() as u64;
    Node::new(req.path(&[name]), "store", name)
        .prop("keyValues", live)
        .prop("revisionsKept", revisions)
        .prop("snapshots", s.snapshots.len())
        .prop(
            "limits",
            json!({
                "revisionAgeSeconds": s.limits.revision_age_secs,
                "revisionsPerKey": s.limits.revisions_per_key,
                "storeBytes": s.limits.store_bytes,
                "snapshotBytes": s.limits.snapshot_bytes,
                "snapshots": s.limits.snapshots,
                "snapshotMaxBytes": s.limits.snapshot_max_bytes,
            }),
        )
        .child("keyvalues", "keyvalues", Some(live), req.path(&[name, "keyvalues"]))
        .child("revisions", "revisions", Some(revisions), req.path(&[name, "revisions"]))
        .child("snapshots", "snapshots", Some(s.snapshots.len() as u64), req.path(&[name, "snapshots"]))
}

fn cut(v: &str) -> (&str, bool) {
    if v.len() <= VALUE_LIMIT {
        return (v, false);
    }
    let mut end = VALUE_LIMIT;
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    (&v[..end], true)
}

fn setting_row(s: &Setting) -> Value {
    let (value, truncated) = s.value.as_deref().map_or((None, false), |v| {
        let (c, t) = cut(v);
        (Some(c), t)
    });
    let tags: serde_json::Map<String, Value> = s.tags.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "name": s.key,
        "label": s.label,
        "value": value,
        "valueTruncated": truncated,
        "contentType": s.content_type,
        "locked": s.locked,
        "tags": tags,
        "description": s.description,
        "etag": s.etag,
        "lastModified": rfc3339(s.last_modified),
    })
}

/// Marker of a key-value: its key, then `0` for the null label or `1` and the label (the order the store lists in).
fn ident_marker(key: &str, label: &Option<String>) -> String {
    match label {
        None => format!("{key}\0\0"),
        Some(l) => format!("{key}\0\u{1}{l}"),
    }
}

fn parse_ident(m: &str) -> Option<(String, Option<String>)> {
    let (key, rest) = m.split_once('\0')?;
    match rest.strip_prefix('\u{1}') {
        Some(l) => Some((key.to_string(), Some(l.to_string()))),
        None => Some((key.to_string(), None)),
    }
}

/// The `?label=` filter: a label, or `(null)` for the null label.
fn label_matches(req: &InspectRequest, label: &Option<String>) -> bool {
    match req.param("label") {
        None => true,
        Some("(null)") => label.is_none(),
        Some(l) => label.as_deref() == Some(l),
    }
}

fn keyvalues_node(name: &str, s: &Store, req: &InspectRequest) -> Node {
    let prefix = req.param("key").unwrap_or("");
    let after = req.marker().and_then(parse_ident);
    let rows = s
        .live_after(after.as_ref())
        .skip_while(|x| x.key.as_str() < prefix)
        .take_while(|x| x.key.starts_with(prefix))
        .filter(|x| label_matches(req, &x.label))
        .map(|x| (ident_marker(&x.key, &x.label), setting_row(x)));
    let (items, next) = req.page_keyed(rows);
    Node::new(req.path(&[name, "keyvalues"]), "keyvalues", "key-values")
        .accepts(&["key", "label"])
        .prop("note", "key matches the start of the key; label is exact, (null) is the null label.")
        .with_items(items, next)
}

fn revisions_node(name: &str, s: &Store, req: &InspectRequest) -> Node {
    let prefix = req.param("key").unwrap_or("");
    let before = req.marker().and_then(|m| m.parse::<u64>().ok());
    let rows = s.revs_before(before).filter(|r| r.setting.key.starts_with(prefix) && label_matches(req, &r.setting.label)).map(|r| {
        let mut row = setting_row(&r.setting);
        row["revision"] = json!(r.seq);
        row["replacedOrDeleted"] = json!(r.ended.map(rfc3339));
        (r.seq.to_string(), row)
    });
    // newest first: the marker is the seq of the last row, and the next page is every seq below it
    let (items, next) = req.page_keyed(rows);
    Node::new(req.path(&[name, "revisions"]), "revisions", "revisions")
        .accepts(&["key", "label"])
        .prop("note", "Newest first. A row with a date in replacedOrDeleted is no longer the current value.")
        .with_items(items, next)
}

fn snapshot_row(name: &str, s: &Snapshot, req: &InspectRequest, now: OffsetDateTime) -> Value {
    json!({
        "name": s.name,
        "path": req.path(&[name, "snapshots", &s.name]),
        "status": if s.archived { "archived" } else { "ready" },
        "items": s.items.len(),
        "size": s.size,
        "compositionType": s.composition_type,
        "created": rfc3339(s.created),
        "expires": s.expires.map(rfc3339),
        "expired": s.expires.is_some_and(|e| e <= now),
    })
}

fn snapshots_node(name: &str, s: &Store, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let (rows, next) = req.page_map(&s.snapshots);
    let items = rows.into_iter().map(|(_, sn)| snapshot_row(name, sn, req, now)).collect();
    Node::new(req.path(&[name, "snapshots"]), "snapshots", "snapshots").prop("count", s.snapshots.len()).with_items(items, next)
}

fn snapshot_node(name: &str, s: &Snapshot, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let tags: serde_json::Map<String, Value> = s.tags.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let filters: Vec<Value> = s.filters.iter().map(|f| json!({"key": f.key, "label": f.label, "tags": f.tags})).collect();
    Node::new(req.path(&[name, "snapshots", &s.name]), "snapshot", &s.name)
        .prop("status", if s.archived { "archived" } else { "ready" })
        .prop("compositionType", &s.composition_type)
        .prop("filters", filters)
        .prop("created", rfc3339(s.created))
        .prop("expires", s.expires.map(rfc3339))
        .prop("expired", s.expires.is_some_and(|e| e <= now))
        .prop("retentionPeriodSeconds", s.retention_period)
        .prop("size", s.size)
        .prop("etag", &s.etag)
        .prop("tags", tags)
        .prop("description", &s.description)
        .child("items", "keyvalues", Some(s.items.len() as u64), req.path(&[name, "snapshots", &s.name, "items"]))
}

fn snapshot_items_node(name: &str, s: &Snapshot, req: &InspectRequest) -> Node {
    let (range, next) = req.page_range(s.items.len());
    let items = s.items[range].iter().map(|x| setting_row(x)).collect();
    Node::new(req.path(&[name, "snapshots", &s.name, "items"]), "keyvalues", "key-values of the snapshot").with_items(items, next)
}
