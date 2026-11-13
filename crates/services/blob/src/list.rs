//! List Containers and List Blobs (flat and hierarchy).

use std::collections::BTreeSet;

use azemu_core::resp::Resp;
use azemu_core::time::{self, Duration};

use crate::cx::{refresh_entry, Cx};
use crate::errors::{ApiError, ApiResult};
use crate::headers::md5_header;
use crate::state::{Blob, BlobEntry, BlobKind, Container, Time};
use crate::util::{b64, b64_decode, crc64_b64, unquote_etag};
use crate::xml::Out;

/// `maxresults`: default 5000. A value above 5000 is not an error: the server returns up to 5000 items
/// (Microsoft Learn, "List Blobs"). Zero or less, and text that is not a number, are 400.
pub fn max_results(req: &azemu_core::Request) -> Result<usize, ApiError> {
    match req.query.get("maxresults") {
        None => Ok(5000),
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 1 => Ok((n as usize).min(5000)),
            Ok(_) => Err(ApiError::out_of_range_query("maxresults", v)),
            Err(_) => Err(ApiError::invalid_query("maxresults", v)),
        },
    }
}

fn includes(req: &azemu_core::Request) -> BTreeSet<String> {
    req.query.get("include").unwrap_or("").split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()
}

pub fn service_endpoint(req: &azemu_core::Request) -> String {
    format!("{}/", req.ctx.endpoint.base_url())
}

// ---------------------------------------------------------------------------
// List Containers
// ---------------------------------------------------------------------------

fn container_item(o: &mut Out, c: &Container, with_md: bool, retention_days: u32, now: Time) {
    o.open("Container").el("Name", &c.name);
    if let Some((_, version)) = &c.deleted {
        o.el("Deleted", "true").el("Version", version);
    }
    o.open("Properties")
        .el("Last-Modified", &time::rfc1123(c.last_modified))
        .el("Etag", &c.etag)
        .el("LeaseStatus", c.lease.status_str())
        .el("LeaseState", c.lease.state_str());
    if let Some(d) = c.lease.duration_str() {
        o.el("LeaseDuration", d);
    }
    if let Some(p) = &c.public_access {
        o.el("PublicAccess", p);
    }
    o.el("DefaultEncryptionScope", &c.default_scope)
        .el("DenyEncryptionScopeOverride", &c.deny_scope_override.to_string())
        .el("HasImmutabilityPolicy", "false")
        .el("HasLegalHold", "false")
        .el("ImmutableStorageWithVersioningEnabled", "false");
    if let Some((at, _)) = &c.deleted {
        o.el("DeletedTime", &time::rfc1123(*at));
        o.el("RemainingRetentionDays", &remaining_days(*at, retention_days, now).to_string());
    }
    o.close("Properties");
    if with_md && !c.metadata.is_empty() {
        o.open("Metadata");
        for (k, v) in &c.metadata {
            o.el(k, v);
        }
        o.close("Metadata");
    }
    o.close("Container");
}

pub fn remaining_days(deleted_at: Time, retention: u32, now: Time) -> i64 {
    let left = deleted_at + Duration::days(retention as i64) - now;
    ((left.whole_seconds() as f64) / 86400.0).ceil().max(0.0) as i64
}

pub fn list_containers(cx: &mut Cx) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let prefix = req.query.get("prefix").unwrap_or("");
    let marker = req.query.get("marker").unwrap_or("");
    let max = max_results(req)?;
    let inc = includes(req);
    let with_md = inc.contains("metadata");
    let with_deleted = inc.contains("deleted");
    // `include=system` (version 2020-10-02 and later) adds the system containers; plain List Containers does not return `$logs`
    let with_system = inc.contains("system");
    let endpoint = service_endpoint(req);
    let acct = cx.account();
    let days = acct.props.soft_delete_days().unwrap_or(7);
    let marker_name = marker.rsplit('/').next().unwrap_or("").to_string();

    let mut all: Vec<&Container> = acct.containers.values().filter(|c| with_system || c.name != crate::analytics::LOGS).collect();
    if with_deleted {
        all.extend(acct.deleted_containers.iter());
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let mut items: Vec<&Container> =
        all.into_iter().filter(|c| c.name.starts_with(prefix) && (marker_name.is_empty() || c.name >= marker_name)).collect();
    let next = if items.len() > max { Some(items[max].name.clone()) } else { None };
    items.truncate(max);

    let mut o = Out::new();
    o.open_attrs("EnumerationResults", &[("ServiceEndpoint", &endpoint)]);
    if !prefix.is_empty() {
        o.el("Prefix", prefix);
    }
    if !marker.is_empty() {
        o.el("Marker", marker);
    }
    if req.query.has("maxresults") {
        o.el("MaxResults", &max.to_string());
    }
    o.open("Containers");
    for c in items {
        container_item(&mut o, c, with_md, days, now);
    }
    o.close("Containers");
    o.el("NextMarker", next.as_deref().unwrap_or(""));
    o.close("EnumerationResults");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

// ---------------------------------------------------------------------------
// List Blobs
// ---------------------------------------------------------------------------

/// One entry to print.
enum Item<'a> {
    Blob { name: &'a str, blob: &'a Blob, role: Role },
    Uncommitted { name: &'a str },
    Prefix(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The base blob, listed without versions.
    Base,
    /// The base blob listed with versions: `IsCurrentVersion`.
    CurrentVersion,
    Snapshot,
    PreviousVersion,
}

struct Opts {
    snapshots: bool,
    versions: bool,
    deleted: bool,
    uncommitted: bool,
    metadata: bool,
    tags: bool,
    copy: bool,
    legal_hold: bool,
    immutability: bool,
    /// `include=permissions`: owner, group, permissions and ACL of paths (hierarchical containers).
    permissions: bool,
}

/// What of an entry is visible for this listing, in output order.
fn visible<'a>(name: &'a str, e: &'a BlobEntry, o: &Opts, retention: Option<u32>, now: Time) -> Vec<Item<'a>> {
    let alive = |b: &Blob| match b.deleted {
        None => true,
        Some(t) => o.deleted && retention.map(|d| t + Duration::days(d as i64) > now).unwrap_or(false),
    };
    let mut out: Vec<Item<'a>> = vec![];
    if o.snapshots {
        for b in e.snapshots.values() {
            if alive(b) {
                out.push(Item::Blob { name, blob: b, role: Role::Snapshot });
            }
        }
    }
    if o.versions {
        for b in &e.versions {
            if alive(b) {
                out.push(Item::Blob { name, blob: b, role: Role::PreviousVersion });
            }
        }
    }
    match &e.current {
        Some(b) if alive(b) => {
            let role = if o.versions { Role::CurrentVersion } else { Role::Base };
            out.push(Item::Blob { name, blob: b, role });
        }
        None if o.uncommitted && !e.uncommitted.is_empty() => out.push(Item::Uncommitted { name }),
        _ => {}
    }
    out
}

fn write_blob(o: &mut Out, it: &Item<'_>, opts: &Opts, retention: Option<u32>, now: Time) {
    let (name, b, role) = match it {
        Item::Blob { name, blob, role } => (*name, *blob, *role),
        Item::Uncommitted { name } => {
            o.open("Blob");
            write_name(o, name);
            o.open("Properties")
                .el("Content-Length", "0")
                .el("BlobType", "BlockBlob")
                .el("LeaseStatus", "unlocked")
                .el("LeaseState", "available")
                .close("Properties")
                .close("Blob");
            return;
        }
        Item::Prefix(_) => return,
    };
    o.open("Blob");
    write_name(o, name);
    if role == Role::Snapshot {
        o.el("Snapshot", b.snapshot.as_deref().unwrap_or(""));
    }
    if b.deleted.is_some() {
        o.el("Deleted", "true");
    }
    if matches!(role, Role::CurrentVersion | Role::PreviousVersion) {
        o.opt("VersionId", b.version_id.as_deref());
        if role == Role::CurrentVersion && b.version_id.is_some() {
            o.el("IsCurrentVersion", "true");
        }
    }
    o.open("Properties")
        .el("Creation-Time", &time::rfc1123(b.creation_time))
        .el("Last-Modified", &time::rfc1123(b.last_modified))
        .el("Etag", unquote_etag(&b.etag));
    if let Some(d) = &b.dfs {
        o.el("ResourceType", d.resource_type());
        if let Some(t) = d.expiry {
            o.el("Expiry-Time", &time::rfc1123(t));
        }
        if opts.permissions {
            o.el("Owner", &d.owner).el("Group", &d.group).el("Permissions", &d.permissions()).el("Acl", &d.acl_string());
        }
    }
    o.el("Content-Length", &b.size.to_string())
        .el("Content-Type", &b.props.content_type)
        .el("Content-Encoding", b.props.content_encoding.as_deref().unwrap_or(""))
        .el("Content-Language", b.props.content_language.as_deref().unwrap_or(""))
        .el("Content-CRC64", &b.crc64.map(crc64_b64).unwrap_or_default())
        .el("Content-MD5", &md5_header(b).unwrap_or_default())
        .el("Cache-Control", b.props.cache_control.as_deref().unwrap_or(""))
        .el("Content-Disposition", b.props.content_disposition.as_deref().unwrap_or(""));
    if b.kind == BlobKind::Page {
        o.el("x-ms-blob-sequence-number", &b.sequence.to_string());
    }
    o.el("BlobType", b.kind.name());
    if let Some((t, inferred)) = b.tier_info() {
        o.el("AccessTier", &t);
        if inferred {
            o.el("AccessTierInferred", "true");
        }
    }
    if let Some(s) = &b.archive_status {
        o.el("ArchiveStatus", s);
    }
    if matches!(role, Role::Base | Role::CurrentVersion) {
        o.el("LeaseStatus", b.lease.status_str()).el("LeaseState", b.lease.state_str());
        if let Some(d) = b.lease.duration_str() {
            o.el("LeaseDuration", d);
        }
    }
    if opts.copy {
        if let Some(c) = &b.copy {
            o.el("CopyId", &c.id)
                .el("CopyStatus", &c.status)
                .el("CopySource", &c.source)
                .el("CopyProgress", &c.progress)
                .el("CopyCompletionTime", &time::rfc1123(c.completed));
        }
    }
    o.el("ServerEncrypted", "true");
    if let Some(k) = &b.cpk_sha256 {
        o.el("CustomerProvidedKeySha256", k);
    }
    if let Some(s) = &b.encryption_scope {
        o.el("EncryptionScope", s);
    }
    if let Some(t) = b.tier_changed {
        o.el("AccessTierChangeTime", &time::rfc1123(t));
    }
    if !b.tags.is_empty() {
        o.el("TagCount", &b.tags.len().to_string());
    }
    if b.sealed {
        o.el("Sealed", "true");
    }
    if let Some(t) = b.deleted {
        o.el("DeletedTime", &time::rfc1123(t));
        o.el("RemainingRetentionDays", &remaining_days(t, retention.unwrap_or(7), now).to_string());
    }
    if opts.immutability {
        if let Some(i) = &b.immutability {
            o.el("ImmutabilityPolicyUntilDate", &time::rfc1123(i.until)).el("ImmutabilityPolicyMode", &i.mode);
        }
    }
    if opts.legal_hold && b.legal_hold {
        o.el("LegalHold", "true");
    }
    o.close("Properties");
    if opts.metadata {
        if b.metadata.is_empty() {
            o.empty("Metadata");
        } else {
            o.open("Metadata");
            for (k, v) in &b.metadata {
                o.el(k, v);
            }
            o.close("Metadata");
        }
    }
    if opts.tags {
        o.open("Tags").open("TagSet");
        for (k, v) in &b.tags {
            o.open("Tag").el("Key", k).el("Value", v).close("Tag");
        }
        o.close("TagSet").close("Tags");
    }
    o.empty("OrMetadata");
    o.close("Blob");
}

fn write_name(o: &mut Out, name: &str) {
    if name.chars().any(|c| (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r')) {
        let enc = crate::util::encode_path(name);
        o.open_attrs("Name", &[("Encoded", "true")]);
        o.raw(&azemu_core::storage::xml_escape(&enc)).close("Name");
    } else {
        o.el("Name", name);
    }
}

/// Where a page of results resumes.
enum Resume {
    Start,
    /// At `name`, skipping the first `skip` items of that name.
    At {
        name: String,
        skip: usize,
    },
    /// After everything that starts with this prefix (a collapsed directory).
    AfterPrefix(String),
}

fn encode_marker(r: &Resume) -> String {
    match r {
        Resume::Start => String::new(),
        Resume::At { name, skip } => b64(format!("B\0{skip}\0{name}").as_bytes()),
        Resume::AfterPrefix(p) => b64(format!("P\0{p}").as_bytes()),
    }
}

fn decode_marker(m: &str) -> Result<Resume, ApiError> {
    if m.is_empty() {
        return Ok(Resume::Start);
    }
    let bad = || ApiError::invalid_query("marker", m);
    let raw = b64_decode(m).ok_or_else(bad)?;
    let s = String::from_utf8(raw).map_err(|_| bad())?;
    let mut parts = s.splitn(3, '\0');
    match parts.next() {
        Some("B") => {
            let skip: usize = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
            let name = parts.next().ok_or_else(bad)?.to_string();
            Ok(Resume::At { name, skip })
        }
        Some("P") => Ok(Resume::AfterPrefix(parts.next().ok_or_else(bad)?.to_string())),
        _ => Err(bad()),
    }
}

pub fn list_blobs(cx: &mut Cx, container: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let prefix = req.query.get("prefix").unwrap_or("").to_string();
    let delimiter = req.query.get("delimiter").unwrap_or("").to_string();
    let marker_raw = req.query.get("marker").unwrap_or("").to_string();
    let max = max_results(req)?;
    let start_from = req.query.get("startfrom").unwrap_or("").to_string();
    let inc = includes(req);
    let opts = Opts {
        snapshots: inc.contains("snapshots"),
        versions: inc.contains("versions"),
        deleted: inc.contains("deleted") || inc.contains("deletedwithversions"),
        uncommitted: inc.contains("uncommittedblobs"),
        metadata: inc.contains("metadata"),
        tags: inc.contains("tags"),
        copy: inc.contains("copy"),
        legal_hold: inc.contains("legalhold"),
        immutability: inc.contains("immutabilitypolicy"),
        permissions: inc.contains("permissions"),
    };
    if req.query.get("showonly").map(|v| v.eq_ignore_ascii_case("deleted")).unwrap_or(false)
        && cx.account().containers.get(container).map(|c| c.hns).unwrap_or(false)
    {
        return crate::dfs::list_deleted(cx, container);
    }
    let resume = decode_marker(&marker_raw)?;
    let endpoint = service_endpoint(req);
    let retention = cx.account().props.soft_delete_days();
    if container == crate::analytics::LOGS {
        // log blobs past the retention period or the limits go when someone looks (Azure removes them in the background)
        let config = cx.st.config;
        if crate::analytics::prune(cx.account(), now, &config) > 0 {
            cx.st.changed = true;
        }
    }
    // Bring leases up to date first (needs &mut), then list immutably.
    {
        let c = cx.container(container)?;
        for e in c.blobs.values_mut() {
            refresh_entry(e, now);
        }
    }
    let c = cx.container(container)?;

    // Where to start walking names.
    let lower: String = match &resume {
        Resume::At { name, .. } => name.clone(),
        Resume::AfterPrefix(p) => p.clone(),
        Resume::Start => prefix.clone().max(start_from.clone()),
    };
    let lower = if let Resume::At { .. } | Resume::AfterPrefix(_) = &resume { lower.max(prefix.clone()) } else { lower };

    let mut results: Vec<Item<'_>> = vec![];
    let mut next: Option<Resume> = None;
    let mut last_prefix: Option<String> = None;
    for (name, e) in c.blobs.range::<String, _>(lower.clone()..) {
        if !name.starts_with(&prefix) {
            if name.as_str() > prefix.as_str() {
                break;
            }
            continue;
        }
        if let Resume::AfterPrefix(p) = &resume {
            if name.starts_with(p.as_str()) {
                continue;
            }
        }
        let mut items = visible(name, e, &opts, retention, now);
        if items.is_empty() {
            continue;
        }
        // Collapse into a directory when a delimiter is given.
        let mut collapsed: Option<String> = None;
        if !delimiter.is_empty() {
            if let Some(pos) = name[prefix.len()..].find(&delimiter) {
                collapsed = Some(name[..prefix.len() + pos + delimiter.len()].to_string());
            }
        }
        if let Some(p) = collapsed {
            if last_prefix.as_deref() == Some(p.as_str()) {
                continue;
            }
            if results.len() >= max {
                // Resume at the first name of this directory.
                next = Some(Resume::At { name: first_name_with(c, &p).unwrap_or_else(|| name.clone()), skip: 0 });
                break;
            }
            last_prefix = Some(p.clone());
            results.push(Item::Prefix(p));
            continue;
        }
        let mut skip = 0;
        if let Resume::At { name: rn, skip: s } = &resume {
            if rn == name {
                skip = *s;
            }
        }
        let mut idx = 0;
        for it in items.drain(..) {
            if idx < skip {
                idx += 1;
                continue;
            }
            if results.len() >= max {
                next = Some(Resume::At { name: name.clone(), skip: idx });
                break;
            }
            results.push(it);
            idx += 1;
        }
        if next.is_some() {
            break;
        }
    }

    let mut o = Out::new();
    o.open_attrs("EnumerationResults", &[("ServiceEndpoint", &endpoint), ("ContainerName", container)]);
    if !prefix.is_empty() {
        o.el("Prefix", &prefix);
    }
    if !marker_raw.is_empty() {
        o.el("Marker", &marker_raw);
    }
    if req.query.has("maxresults") {
        o.el("MaxResults", &max.to_string());
    }
    if !delimiter.is_empty() {
        o.el("Delimiter", &delimiter);
    }
    o.open("Blobs");
    for it in &results {
        match it {
            Item::Prefix(p) => {
                o.open("BlobPrefix");
                write_name(&mut o, p);
                o.close("BlobPrefix");
            }
            _ => write_blob(&mut o, it, &opts, retention, now),
        }
    }
    o.close("Blobs");
    o.el("NextMarker", &next.as_ref().map(encode_marker).unwrap_or_default());
    o.close("EnumerationResults");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

fn first_name_with(c: &Container, prefix: &str) -> Option<String> {
    c.blobs.range::<String, _>(prefix.to_string()..).next().map(|(k, _)| k.clone()).filter(|k| k.starts_with(prefix))
}
