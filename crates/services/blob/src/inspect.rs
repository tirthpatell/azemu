//! The inspector of Blob Storage and Data Lake (`docs/inspector.md`): accounts, containers, blobs, their blocks, snapshots and
//! versions, and the first bytes of a blob's content. It only reads: no lease is refreshed in the stored state (the state a lease
//! would have now is worked out on a copy), no blob is touched, and an account that does not exist is a 404 and stays so.

use std::ops::Bound::{Excluded, Included, Unbounded};

use azemu_core::inspect::{body_view, marker_for, rfc3339, seg, time_value, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::time::OffsetDateTime;
use serde_json::{json, Value};

use crate::state::{Account, Blob, BlobEntry, BlobKind, Container, Lease, State};
use crate::Shared;

pub(crate) fn node(shared: &Shared, req: &InspectRequest) -> InspectResult {
    let state = shared.state.lock();
    let now = OffsetDateTime::now_utc();
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    match segs.as_slice() {
        [] => Ok(service_node(&state, req)),
        [account, rest @ ..] => {
            let acct = state
                .accounts
                .get(*account)
                .ok_or_else(|| InspectError::not_found(format!("There is no storage account '{account}' with blobs.")))?;
            match rest {
                [] => Ok(account_node(account, acct, req)),
                ["containers"] => Ok(containers_node(account, acct, req, now)),
                ["deleted-containers"] => Ok(deleted_containers_node(account, acct, req)),
                ["containers", name, tail @ ..] => {
                    let c = acct
                        .containers
                        .get(*name)
                        .ok_or_else(|| InspectError::not_found(format!("There is no container '{name}' in '{account}'.")))?;
                    container_tree(account, c, tail, req, now)
                }
                _ => Err(not_found(req)),
            }
        }
    }
}

fn not_found(req: &InspectRequest) -> InspectError {
    InspectError::not_found(format!("There is no node {}.", req.own_path()))
}

fn service_node(state: &State, req: &InspectRequest) -> Node {
    let mut names: Vec<&String> = state.accounts.keys().collect();
    names.sort();
    let (range, next) = req.page_range(names.len());
    let items = names[range]
        .iter()
        .map(|n| {
            let a = &state.accounts[*n];
            json!({"name": n, "path": req.path(&[n]), "containers": a.containers.len(), "versioning": a.versioning})
        })
        .collect();
    Node::new(req.path(&[]), "service", "blob")
        .prop("accounts", names.len())
        .prop("note", "Blob Storage and Data Lake Gen2 (the dfs endpoint) share these accounts.")
        .with_items(items, next)
}

fn account_node(name: &str, a: &Account, req: &InspectRequest) -> Node {
    let live = a.containers.len();
    Node::new(req.path(&[name]), "account", name)
        .prop("versioning", a.versioning)
        .prop("publicAccessAllowed", a.public_access_allowed)
        .prop("softDeleteDays", a.props.soft_delete_days())
        .prop("defaultServiceVersion", &a.props.default_service_version)
        .prop("staticWebsite", a.props.static_website.enabled)
        .prop("corsRules", a.props.cors.len())
        .prop("analyticsLogging", json!({"read": a.props.logging.read, "write": a.props.logging.write, "delete": a.props.logging.delete}))
        .child("containers", "containers", Some(live as u64), req.path(&[name, "containers"]))
        .child("deleted-containers", "containers", Some(a.deleted_containers.len() as u64), req.path(&[name, "deleted-containers"]))
}

fn lease_view(lease: &Lease, now: OffsetDateTime) -> Value {
    // the lease as it is now: a copy is moved on in time, the stored one is left as it is
    let mut l = lease.clone();
    l.refresh(now);
    json!({"state": l.state_str(), "status": l.status_str(), "duration": l.duration_str(), "id": l.id, "expires": l.expires.map(rfc3339)})
}

fn metadata_view(md: &[(String, String)]) -> Value {
    Value::Object(md.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
}

fn container_row(account: &str, c: &Container, req: &InspectRequest, now: OffsetDateTime, with_path: bool) -> Value {
    let mut row = json!({
        "name": c.name,
        "publicAccess": c.public_access,
        "lease": lease_view(&c.lease, now)["state"],
        "blobs": c.blobs.len(),
        "lastModified": time_value(c.last_modified),
        "hierarchical": c.hns,
    });
    if with_path {
        row["path"] = json!(req.path(&[account, "containers", &c.name]));
    }
    if let Some((at, id)) = &c.deleted {
        row["deletedAt"] = time_value(*at);
        row["versionId"] = json!(id);
    }
    row
}

fn containers_node(account: &str, a: &Account, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let (rows, next) = req.page_map(&a.containers);
    let items = rows.iter().map(|(_, c)| container_row(account, c, req, now, true)).collect();
    Node::new(req.path(&[account, "containers"]), "containers", "containers").prop("count", a.containers.len()).with_items(items, next)
}

fn deleted_containers_node(account: &str, a: &Account, req: &InspectRequest) -> Node {
    let (range, next) = req.page_range(a.deleted_containers.len());
    let items = a.deleted_containers[range].iter().map(|c| container_row(account, c, req, OffsetDateTime::now_utc(), false)).collect();
    Node::new(req.path(&[account, "deleted-containers"]), "containers", "soft-deleted containers")
        .prop("count", a.deleted_containers.len())
        .with_items(items, next)
}

fn container_tree(account: &str, c: &Container, tail: &[&str], req: &InspectRequest, now: OffsetDateTime) -> InspectResult {
    match tail {
        [] => Ok(container_node(account, c, req, now)),
        ["blobs"] => Ok(blobs_node(account, c, req)),
        ["blobs", name, rest @ ..] => {
            let entry = c
                .blobs
                .get(*name)
                .ok_or_else(|| InspectError::not_found(format!("There is no blob '{name}' in container '{}'.", c.name)))?;
            blob_tree(account, c, name, entry, rest, req, now)
        }
        _ => Err(not_found(req)),
    }
}

fn container_node(account: &str, c: &Container, req: &InspectRequest, now: OffsetDateTime) -> Node {
    let policies: Vec<Value> =
        c.policies.iter().map(|p| json!({"id": p.id, "start": p.start, "expiry": p.expiry, "permission": p.permission})).collect();
    Node::new(req.path(&[account, "containers", &c.name]), "container", &c.name)
        .prop("etag", &c.etag)
        .prop("lastModified", time_value(c.last_modified))
        .prop("publicAccess", &c.public_access)
        .prop("metadata", metadata_view(&c.metadata))
        .prop("lease", lease_view(&c.lease, now))
        .prop("storedAccessPolicies", policies)
        .prop("defaultEncryptionScope", &c.default_scope)
        .prop("denyEncryptionScopeOverride", c.deny_scope_override)
        .prop("hierarchicalNamespace", c.hns)
        .prop("blobEntries", c.blobs.len())
        .child("blobs", "blobs", Some(c.blobs.len() as u64), req.path(&[account, "containers", &c.name, "blobs"]))
}

/// The rows of the blob list: blobs and, with a delimiter, the virtual directories (`prefix` rows). `?prefix=` limits the names.
fn blobs_node(account: &str, c: &Container, req: &InspectRequest) -> Node {
    let prefix = req.param("prefix").unwrap_or("");
    let delimiter = req.param("delimiter").filter(|d| !d.is_empty());
    let base = req.path(&[account, "containers", &c.name, "blobs"]);
    // where to start: after the marker, and not before the prefix
    let start: String = match req.marker() {
        Some(m) if m.as_bytes() >= prefix.as_bytes() => m.to_string(),
        _ => prefix.to_string(),
    };
    let skip_under = req.marker().filter(|m| delimiter.is_some_and(|d| m.ends_with(d)));
    let lower =
        if req.marker().is_some_and(|m| m.as_bytes() >= prefix.as_bytes()) { Excluded(start.clone()) } else { Included(start.clone()) };
    let mut items = Vec::new();
    let mut last_key: Option<String> = None;
    let mut next = None;
    let mut last_prefix: Option<String> = None;
    for (name, entry) in c.blobs.range::<String, _>((lower, Unbounded)) {
        if !name.starts_with(prefix) {
            break;
        }
        if skip_under.is_some_and(|m| name.starts_with(m)) {
            continue;
        }
        let row_prefix = delimiter.and_then(|d| name[prefix.len()..].find(d).map(|i| name[..prefix.len() + i + d.len()].to_string()));
        if let Some(p) = &row_prefix {
            if last_prefix.as_deref() == Some(p.as_str()) {
                continue;
            }
        }
        if items.len() == req.limit() {
            next = last_key.as_deref().map(marker_for);
            break;
        }
        match row_prefix {
            Some(p) => {
                items.push(json!({"name": p, "type": "prefix", "path": format!("{base}?prefix={}&delimiter={}", seg(&p), seg(delimiter.unwrap_or("/")))}));
                last_key = Some(p.clone());
                last_prefix = Some(p);
            }
            None => {
                items.push(blob_row(&base, name, entry));
                last_key = Some(name.clone());
            }
        }
    }
    Node::new(base.clone(), "blobs", "blobs")
        .prop("blobEntries", c.blobs.len())
        .prop("prefix", prefix)
        .prop("delimiter", delimiter)
        .accepts(&["prefix", "delimiter"])
        .with_items(items, next)
}

fn kind_name(b: &Blob) -> &'static str {
    match b.kind {
        BlobKind::Block => "BlockBlob",
        BlobKind::Append => "AppendBlob",
        BlobKind::Page => "PageBlob",
    }
}

fn blob_row(base: &str, name: &str, e: &BlobEntry) -> Value {
    let path = format!("{base}/{}", seg(name));
    match &e.current {
        Some(b) => json!({
            "name": name,
            "path": path,
            "type": kind_name(b),
            "state": if b.deleted.is_some() { "soft-deleted" } else { "live" },
            "size": b.size,
            "contentType": b.props.content_type,
            "etag": b.etag,
            "lastModified": time_value(b.last_modified),
            "tier": b.tier_info().map(|(t, _)| t),
            "snapshots": e.snapshots.len(),
            "versions": e.versions.len(),
            "tags": b.tags.len(),
            "lease": b.lease.state_str(),
        }),
        None => json!({
            "name": name,
            "path": path,
            "state": "only snapshots, versions or uncommitted blocks",
            "snapshots": e.snapshots.len(),
            "versions": e.versions.len(),
            "uncommittedBlocks": e.uncommitted.len(),
        }),
    }
}

fn blob_tree(
    account: &str,
    c: &Container,
    name: &str,
    e: &BlobEntry,
    rest: &[&str],
    req: &InspectRequest,
    now: OffsetDateTime,
) -> InspectResult {
    let base = req.path(&[account, "containers", &c.name, "blobs", name]);
    match rest {
        [] => blob_node(&base, name, e, req, now),
        ["blocks"] => Ok(blocks_node(&base, e, req)),
        ["snapshots"] => Ok(snapshots_node(&base, e, req)),
        ["versions"] => Ok(versions_node(&base, e, req)),
        _ => Err(not_found(req)),
    }
}

/// The instance the request asks for: `?snapshot=<id>`, `?version=<id>`, or the current blob.
fn instance<'a>(e: &'a BlobEntry, req: &InspectRequest) -> Result<&'a Blob, InspectError> {
    if let Some(s) = req.param("snapshot") {
        return e.snapshots.get(s).ok_or_else(|| InspectError::not_found(format!("There is no snapshot '{s}' of this blob.")));
    }
    if let Some(v) = req.param("version") {
        let hit = e
            .versions
            .iter()
            .find(|b| b.version_id.as_deref() == Some(v))
            .or_else(|| e.current.as_ref().filter(|b| b.version_id.as_deref() == Some(v)));
        return hit.ok_or_else(|| InspectError::not_found(format!("There is no version '{v}' of this blob.")));
    }
    e.current.as_ref().ok_or_else(|| InspectError::not_found("This name has no current blob."))
}

fn blob_node(base: &str, name: &str, e: &BlobEntry, req: &InspectRequest, now: OffsetDateTime) -> InspectResult {
    // a name that has only snapshots, versions or staged blocks: the node says so and links to them
    if e.current.is_none() && req.param("snapshot").is_none() && req.param("version").is_none() {
        return Ok(Node::new(base, "blob", name)
            .prop("state", "no current blob")
            .prop("uncommittedBlocks", e.uncommitted.len())
            .child("blocks", "blocks", Some(e.uncommitted.len() as u64), format!("{base}/blocks"))
            .child("snapshots", "snapshots", Some(e.snapshots.len() as u64), format!("{base}/snapshots"))
            .child("versions", "versions", Some(e.versions.len() as u64), format!("{base}/versions")));
    }
    let b = instance(e, req)?;
    let mut node = Node::new(base, "blob", name)
        .accepts(&["snapshot", "version"])
        .prop("type", kind_name(b))
        .prop("state", if b.deleted.is_some() { "soft-deleted" } else { "live" })
        .prop("deletedAt", b.deleted.map(rfc3339))
        .prop("size", b.size)
        .prop("contentType", &b.props.content_type)
        .prop("contentEncoding", &b.props.content_encoding)
        .prop("contentLanguage", &b.props.content_language)
        .prop("contentDisposition", &b.props.content_disposition)
        .prop("cacheControl", &b.props.cache_control)
        .prop("contentMd5", b.props.content_md5.as_ref().map(|m| hex(m)))
        .prop("etag", &b.etag)
        .prop("creationTime", time_value(b.creation_time))
        .prop("lastModified", time_value(b.last_modified))
        .prop("snapshot", &b.snapshot)
        .prop("versionId", &b.version_id)
        .prop("tier", b.tier_info().map(|(t, inferred)| if inferred { format!("{t} (inferred)") } else { t }))
        .prop("archiveStatus", &b.archive_status)
        .prop("lease", lease_view(&b.lease, now))
        .prop("metadata", metadata_view(&b.metadata))
        .prop("tags", &b.tags)
        .prop("sequenceNumber", (b.kind == BlobKind::Page).then_some(b.sequence))
        .prop("sealed", (b.kind == BlobKind::Append).then_some(b.sealed))
        .prop("committedBlocks", (b.kind != BlobKind::Page).then(|| b.block_count()))
        .prop("legalHold", b.legal_hold)
        .prop("immutability", b.immutability.as_ref().map(|i| json!({"mode": i.mode, "until": time_value(i.until)})))
        .prop("encryptionScope", &b.encryption_scope)
        .prop("customerProvidedKeySha256", &b.cpk_sha256)
        .prop("copy", b.copy.as_ref().map(|c| json!({"id": c.id, "source": c.source, "status": c.status, "progress": c.progress, "completed": time_value(c.completed)})))
        .prop("storedBytes", b.stored_bytes());
    if let Some(d) = &b.dfs {
        node = node.prop("dataLake", json!({"isDirectory": d.is_dir, "owner": d.owner, "group": d.group, "permissions": d.permissions(), "expiry": d.expiry.map(rfc3339)}));
    }
    if req.param("snapshot").is_none() && req.param("version").is_none() {
        if b.kind != BlobKind::Page {
            node = node.child("blocks", "blocks", Some((b.block_count() + e.uncommitted.len()) as u64), format!("{base}/blocks"));
        }
        node = node.child("snapshots", "snapshots", Some(e.snapshots.len() as u64), format!("{base}/snapshots")).child(
            "versions",
            "versions",
            Some(e.versions.len() as u64),
            format!("{base}/versions"),
        );
    }
    node = node.view("Show content (first 64 KiB)", "content=1");
    if req.flag("content") {
        let len = b.size.min(azemu_core::inspect::BODY_LIMIT as u64 + 4);
        node = node.with_content(body_view(&b.read(0, len), Some(&b.props.content_type)));
        // the body view cuts at 64 KiB itself; the real size is the blob's
        if let Some(Value::Object(c)) = node.content.as_mut() {
            c.insert("size".into(), json!(b.size));
            c.insert("truncated".into(), json!(b.size > azemu_core::inspect::BODY_LIMIT as u64));
        }
    }
    Ok(node)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Committed blocks in order, then the uncommitted ones by id.
fn blocks_node(base: &str, e: &BlobEntry, req: &InspectRequest) -> Node {
    let (committed, size_of_committed): (&[crate::state::Seg], u64) = match &e.current {
        Some(b) => (b.segs.as_slice(), b.size),
        None => (&[], 0),
    };
    let total = committed.len() + e.uncommitted.len();
    let (range, next) = req.page_range(total);
    // the offset of the first block of the page: sum of the sizes before it (cheap: blocks are at most 50,000)
    let mut off: u64 = committed.iter().take(range.start.min(committed.len())).map(|s| s.data.len() as u64).sum();
    let mut items = Vec::new();
    for i in range {
        if i < committed.len() {
            let s = &committed[i];
            items.push(json!({"name": s.id.clone().unwrap_or_else(|| format!("(no id, block {i})")), "committed": true, "size": s.data.len(), "offset": off}));
            off += s.data.len() as u64;
        } else if let Some((id, data)) = e.uncommitted.iter().nth(i - committed.len()) {
            items.push(json!({"name": id, "committed": false, "size": data.len()}));
        }
    }
    Node::new(format!("{base}/blocks"), "blocks", "block list")
        .prop("committed", committed.len())
        .prop("uncommitted", e.uncommitted.len())
        .prop("committedBytes", size_of_committed)
        .with_items(items, next)
}

fn snapshots_node(base: &str, e: &BlobEntry, req: &InspectRequest) -> Node {
    let (rows, next) = req.page_map(&e.snapshots);
    let items = rows
        .iter()
        .map(|(id, b)| json!({"name": id, "path": format!("{base}?snapshot={}", seg(id)), "size": b.size, "etag": b.etag, "lastModified": time_value(b.last_modified), "state": if b.deleted.is_some() { "soft-deleted" } else { "live" }}))
        .collect();
    Node::new(format!("{base}/snapshots"), "snapshots", "snapshots").prop("count", e.snapshots.len()).with_items(items, next)
}

fn versions_node(base: &str, e: &BlobEntry, req: &InspectRequest) -> Node {
    let mut all: Vec<&Blob> = e.versions.iter().collect();
    if let Some(c) = e.current.as_ref().filter(|c| c.version_id.is_some()) {
        all.push(c);
    }
    let (range, next) = req.page_range(all.len());
    let items = all[range]
        .iter()
        .map(|b| {
            let id = b.version_id.clone().unwrap_or_default();
            json!({"name": id, "path": format!("{base}?version={}", seg(&id)), "current": e.current.as_ref().is_some_and(|c| std::ptr::eq(c, *b)), "size": b.size, "etag": b.etag, "lastModified": time_value(b.last_modified)})
        })
        .collect();
    Node::new(format!("{base}/versions"), "versions", "versions").prop("count", all.len()).with_items(items, next)
}
