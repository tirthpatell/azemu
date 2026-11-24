//! The inspector of Azure Files (`docs/inspector.md`): accounts, shares, their directories and files, share snapshots and deleted
//! shares. Only reads: no lease is moved on in the stored state (a copy is), no share is purged for being past its retention, and an
//! account that does not exist is a 404 and is not made.

use azemu_core::inspect::{body_view, rfc3339, time_value, InspectError, InspectRequest, InspectResult, Node, BODY_LIMIT};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Value};

use crate::lease::Lease;
use crate::state::{attr, Account, Dir, File, Node as Entry, Share, Smb, State};

pub(crate) fn node(state: &State, soft_delete_days: u32, req: &InspectRequest) -> InspectResult {
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let now = OffsetDateTime::now_utc();
    let [account, rest @ ..] = segs.as_slice() else {
        return Ok(service_node(state, req));
    };
    let acct = state
        .accounts
        .get(*account)
        .ok_or_else(|| InspectError::not_found(format!("There is no storage account '{account}' with file shares.")))?;
    match rest {
        [] => Ok(account_node(account, acct, soft_delete_days, req)),
        ["shares"] => Ok(shares_node(account, acct, req, now)),
        ["deleted-shares"] => Ok(deleted_node(account, acct, req)),
        ["shares", name, tail @ ..] => {
            let share =
                acct.shares.get(*name).ok_or_else(|| InspectError::not_found(format!("There is no share '{name}' in '{account}'.")))?;
            let base = req.path(&[account, "shares", name]);
            match tail {
                [] => Ok(share_node(&base, share, now)),
                ["files", path @ ..] => tree_node(&format!("{base}/files"), &share.root, path, req, now),
                ["snapshots"] => Ok(snapshots_node(&base, share, req)),
                ["snapshots", time, tail @ ..] => {
                    let snap = share
                        .snapshots
                        .iter()
                        .find(|s| s.time == *time)
                        .ok_or_else(|| InspectError::not_found(format!("There is no snapshot '{time}' of this share.")))?;
                    let sbase = format!("{base}/snapshots/{}", azemu_core::inspect::seg(time));
                    match tail {
                        [] => Ok(snapshot_node(&sbase, snap, now)),
                        ["files", path @ ..] => tree_node(&format!("{sbase}/files"), &snap.root, path, req, now),
                        _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
                    }
                }
                _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
            }
        }
        _ => Err(InspectError::not_found(format!("There is no node {}.", req.own_path()))),
    }
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let names: Vec<&String> = state.accounts.keys().collect();
    let (range, next) = req.page_range(names.len());
    let items =
        names[range].iter().map(|n| json!({"name": n, "path": req.path(&[n]), "shares": state.accounts[*n].shares.len()})).collect();
    Node::new(req.path(&[]), "service", "file").prop("accounts", names.len()).with_items(items, next)
}

fn account_node(name: &str, a: &Account, soft_delete_days: u32, req: &InspectRequest) -> Node {
    Node::new(req.path(&[name]), "account", name)
        .prop("shares", a.shares.len())
        .prop("deletedShares", a.deleted.len())
        .prop("softDeleteDays", soft_delete_days)
        .prop("storedPermissions", a.permissions.len())
        .child("shares", "shares", Some(a.shares.len() as u64), req.path(&[name, "shares"]))
        .child("deleted-shares", "shares", Some(a.deleted.len() as u64), req.path(&[name, "deleted-shares"]))
}

fn lease_view(l: &Lease, now: OffsetDateTime) -> Value {
    let mut l = l.clone();
    l.refresh(now);
    json!({"state": format!("{:?}", l.state).to_lowercase(), "id": l.id, "seconds": l.seconds, "until": l.until.map(rfc3339)})
}

fn metadata(md: &[(String, String)]) -> Value {
    Value::Object(md.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

fn shares_node(account: &str, a: &Account, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let (rows, next) = req.page_map(&a.shares);
    let items = rows
        .into_iter()
        .map(|(k, s)| {
            json!({"name": s.name, "path": req.path(&[account, "shares", k]), "quotaGiB": s.quota, "tier": s.tier, "protocols": s.enabled_protocols,
                   "lease": lease_view(&s.lease, now)["state"], "snapshots": s.snapshots.len(), "lastModified": time_value(s.last_modified)})
        })
        .collect();
    Node::new(req.path(&[account, "shares"]), "shares", "shares").prop("count", a.shares.len()).with_items(items, next)
}

fn deleted_node(account: &str, a: &Account, req: &InspectRequest) -> Node {
    let (range, next) = req.page_range(a.deleted.len());
    let items = a.deleted[range]
        .iter()
        .map(|d| json!({"name": d.share.name, "deletedAt": time_value(d.deleted), "version": d.version, "quotaGiB": d.share.quota}))
        .collect();
    Node::new(req.path(&[account, "deleted-shares"]), "shares", "deleted shares").prop("count", a.deleted.len()).with_items(items, next)
}

fn share_node(base: &str, s: &Share, now: OffsetDateTime) -> Node {
    let acl: Vec<Value> =
        s.acl.iter().map(|a| json!({"id": a.id, "start": a.start, "expiry": a.expiry, "permission": a.permission})).collect();
    Node::new(base, "share", &s.name)
        .prop("etag", &s.etag)
        .prop("lastModified", time_value(s.last_modified))
        .prop("quotaGiB", s.quota)
        .prop("tier", &s.tier)
        .prop("tierTransition", &s.tier_transition)
        .prop("enabledProtocols", &s.enabled_protocols)
        .prop("metadata", metadata(&s.metadata))
        .prop("accessPolicies", acl)
        .prop("lease", lease_view(&s.lease, now))
        .child("files", "directory", Some(s.root.children.len() as u64), format!("{base}/files"))
        .child("snapshots", "snapshots", Some(s.snapshots.len() as u64), format!("{base}/snapshots"))
}

fn snapshots_node(base: &str, s: &Share, req: &InspectRequest) -> Node {
    let (range, next) = req.page_range(s.snapshots.len());
    let items = s.snapshots[range]
        .iter()
        .map(|x| json!({"name": x.time, "path": format!("{base}/snapshots/{}", azemu_core::inspect::seg(&x.time)), "quotaGiB": x.quota, "lastModified": time_value(x.last_modified)}))
        .collect();
    Node::new(format!("{base}/snapshots"), "snapshots", "snapshots").prop("count", s.snapshots.len()).with_items(items, next)
}

fn snapshot_node(base: &str, x: &crate::state::Snapshot, now: OffsetDateTime) -> Node {
    Node::new(base, "snapshot", &x.time)
        .prop("quotaGiB", x.quota)
        .prop("tier", &x.tier)
        .prop("enabledProtocols", &x.enabled_protocols)
        .prop("metadata", metadata(&x.metadata))
        .prop("lease", lease_view(&x.lease, now))
        .prop("lastModified", time_value(x.last_modified))
        .child("files", "directory", Some(x.root.children.len() as u64), format!("{base}/files"))
}

fn smb_view(s: &Smb) -> Value {
    json!({
        "attributes": attr::render(s.attributes),
        "creationTime": time_value(s.creation),
        "lastWriteTime": time_value(s.last_write),
        "changeTime": time_value(s.change),
        "permissionKey": s.permission_key,
    })
}

/// A directory (its children are the rows) or a file (`?content=1` shows its first bytes) below `root`, by the names in `path`.
fn tree_node(files_base: &str, root: &Dir, path: &[&str], req: &InspectRequest, now: OffsetDateTime) -> InspectResult {
    let mut dir = root;
    let mut here = files_base.to_string();
    for (i, name) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        match dir.children.get(&name.to_lowercase()) {
            Some(Entry::Dir(d)) => dir = d,
            Some(Entry::File(f)) if last => {
                here = format!("{here}/{}", azemu_core::inspect::seg(name));
                return Ok(file_node(&here, f, req, now));
            }
            _ => return Err(InspectError::not_found(format!("There is no directory or file '{}'.", path[..=i].join("/")))),
        }
        here = format!("{here}/{}", azemu_core::inspect::seg(name));
    }
    Ok(dir_node(&here, dir, path.is_empty(), req))
}

fn dir_node(here: &str, d: &Dir, is_root: bool, req: &InspectRequest) -> Node {
    use std::ops::Bound::{Excluded, Unbounded};
    let start = req.marker().map_or(Unbounded, |m| Excluded(m.to_string()));
    let rows = d.children.range((start, Unbounded)).map(|(k, e)| {
        let (kind, size, etag, modified, smb) = match e {
            Entry::Dir(x) => ("directory", None, &x.etag, x.last_modified, &x.smb),
            Entry::File(x) => ("file", Some(x.size), &x.etag, x.last_modified, &x.smb),
        };
        (
            k.as_str(),
            json!({"name": e.name(), "path": format!("{here}/{}", azemu_core::inspect::seg(e.name())), "type": kind, "size": size, "attributes": attr::render(smb.attributes), "etag": etag, "lastModified": time_value(modified)}),
        )
    });
    let (items, next) = req.page_keyed(rows);
    let mut n = Node::new(here, "directory", if is_root { "(share root)" } else { &d.name })
        .prop("children", d.children.len())
        .prop("fileId", d.id)
        .prop("etag", &d.etag)
        .prop("lastModified", time_value(d.last_modified))
        .prop("metadata", metadata(&d.metadata));
    n = n.prop("smb", smb_view(&d.smb));
    n.with_items(items, next)
}

fn file_node(here: &str, f: &File, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let mut n = Node::new(here, "file", &f.name)
        .prop("size", f.size)
        .prop("fileId", f.id)
        .prop("parentId", f.parent_id)
        .prop("etag", &f.etag)
        .prop("lastModified", time_value(f.last_modified))
        .prop("contentType", &f.content_type)
        .prop("contentEncoding", &f.content_encoding)
        .prop("contentLanguage", &f.content_language)
        .prop("cacheControl", &f.cache_control)
        .prop("contentDisposition", &f.content_disposition)
        .prop("metadata", metadata(&f.metadata))
        .prop("smb", smb_view(&f.smb))
        .prop("lease", lease_view(&f.lease, now))
        .prop("writtenPages", f.pages.len())
        .prop("copy", f.copy.as_ref().map(|c| json!({"id": c.id, "source": c.source, "status": c.status, "progress": c.progress, "completed": time_value(c.completed), "description": c.description})))
        .view("Show content (first 64 KiB)", "content=1");
    if req.flag("content") {
        let len = f.size.min(BODY_LIMIT as u64 + 4);
        let mut v = body_view(&f.read(0, len), Some(&f.content_type));
        if let Value::Object(m) = &mut v {
            m.insert("size".into(), json!(f.size));
            m.insert("truncated".into(), json!(f.size > BODY_LIMIT as u64));
        }
        n = n.with_content(v);
    }
    n
}
