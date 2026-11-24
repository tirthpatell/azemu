//! Shares: create, properties, metadata, access policies, statistics, snapshots, leases, delete and restore, permissions, and the list.

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::time::{iso8601_7, parse_iso8601, rfc1123, Duration, OffsetDateTime};
use azemu_core::{Request, Response};

use crate::errors;
use crate::lease::Lease;
use crate::leasing::{self, Kind};
use crate::meta;
use crate::names;
use crate::state::{Account, DeletedShare, Share, SignedId, Snapshot, DEFAULT_QUOTA};
use crate::tree;
use crate::xml::{self, Node};
use crate::Cx;

const MAX_LIST_RESULTS: usize = 5000;
const MAX_SIGNED_IDENTIFIERS: usize = 5;
/// The tiers of a standard account (Azure refused `Premium` there in November 2026).
const TIERS: [&str; 3] = ["TransactionOptimized", "Hot", "Cool"];

/// A share or one of its snapshots, as far as its properties go.
pub struct View<'a> {
    pub etag: &'a str,
    pub last_modified: OffsetDateTime,
    pub quota: u64,
    pub tier: &'a str,
    pub tier_changed: OffsetDateTime,
    pub tier_change_count: u32,
    pub tier_transition: Option<&'a str>,
    pub metadata: &'a [(String, String)],
    pub lease: &'a Lease,
    pub protocols: &'a str,
}

impl<'a> View<'a> {
    pub fn of_share(s: &'a Share) -> View<'a> {
        View {
            etag: &s.etag,
            last_modified: s.last_modified,
            quota: s.quota,
            tier: &s.tier,
            tier_changed: s.tier_changed,
            tier_change_count: s.tier_change_count,
            tier_transition: s.tier_transition.as_deref(),
            metadata: &s.metadata,
            lease: &s.lease,
            protocols: &s.enabled_protocols,
        }
    }
    pub fn of_snapshot(s: &'a Snapshot) -> View<'a> {
        View {
            etag: &s.etag,
            last_modified: s.last_modified,
            quota: s.quota,
            tier: &s.tier,
            tier_changed: s.tier_changed,
            tier_change_count: s.tier_change_count,
            tier_transition: s.tier_transition.as_deref(),
            metadata: &s.metadata,
            lease: &s.lease,
            protocols: &s.enabled_protocols,
        }
    }
}

/// The snapshot a request names (`sharesnapshot`), as the string the share stores it under.
pub fn snapshot_param(req: &Request) -> Result<Option<String>, Response> {
    match req.query.get("sharesnapshot") {
        None => Ok(None),
        Some(v) => parse_iso8601(v)
            .map(|t| Some(iso8601_7(t)))
            .ok_or_else(|| errors::invalid_query(&req.ctx, "sharesnapshot", v, "Must be in the specific snapshot date time format.")),
    }
}

pub fn find<'a>(acct: &'a Account, req: &Request, name: &str) -> Result<&'a Share, Response> {
    acct.shares.get(name).ok_or_else(|| errors::share_not_found(&req.ctx))
}

pub fn find_mut<'a>(acct: &'a mut Account, req: &Request, name: &str) -> Result<&'a mut Share, Response> {
    acct.shares.get_mut(name).ok_or_else(|| errors::share_not_found(&req.ctx))
}

/// The share, or the snapshot of it the request names, as a `View`.
fn view<'a>(share: &'a Share, snap: &Option<String>, req: &Request) -> Result<View<'a>, Response> {
    match snap {
        None => Ok(View::of_share(share)),
        Some(t) => share.snapshots.iter().find(|s| &s.time == t).map(View::of_snapshot).ok_or_else(|| errors::share_not_found(&req.ctx)),
    }
}

fn tier_of(req: &Request) -> Result<Option<String>, Response> {
    match req.header("x-ms-access-tier") {
        None => Ok(None),
        Some(v) => TIERS
            .iter()
            .find(|t| t.eq_ignore_ascii_case(v.trim()))
            .map(|t| Some((*t).to_string()))
            .ok_or_else(|| errors::invalid_header(&req.ctx, "x-ms-access-tier", v)),
    }
}

fn quota_of(req: &Request) -> Result<Option<u64>, Response> {
    match req.header("x-ms-share-quota") {
        None => Ok(None),
        Some(v) => match v.trim().parse::<u64>() {
            Ok(q) if (1..=DEFAULT_QUOTA).contains(&q) => Ok(Some(q)),
            _ => Err(errors::invalid_header(&req.ctx, "x-ms-share-quota", v)),
        },
    }
}

pub fn create(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = names::validate_share(name) {
        return names::share_error(&req.ctx, e);
    }
    let (quota, tier, metadata) = match (quota_of(req), tier_of(req), meta::read(req)) {
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        (Ok(q), Ok(t), Ok(m)) => (q, t, m),
    };
    let protocols = match req.header("x-ms-enabled-protocols") {
        None => "SMB".to_string(),
        // NFS needs a premium account; Azure took any other word (`FTP`) and made an SMB share
        Some(p) if p.trim().eq_ignore_ascii_case("nfs") => return errors::invalid_header(&req.ctx, "x-ms-enabled-protocols", p),
        Some(_) => "SMB".to_string(),
    };
    if acct.shares.contains_key(name) {
        return errors::share_already_exists(&req.ctx);
    }
    let mut share = Share::new(name, c.now);
    share.quota = quota.unwrap_or(DEFAULT_QUOTA);
    if let Some(t) = tier {
        share.tier = t;
    }
    share.metadata = metadata;
    share.enabled_protocols = protocols;
    let r = Resp::new(201u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified));
    acct.shares.insert(name.to_string(), share);
    r.build()
}

/// Properties of a share or a snapshot (`GET` and `HEAD`).
pub fn get_properties(acct: &Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let v = match view(share, &snap, req) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_read(req, Kind::Share, v.lease, c.now) {
        return e;
    }
    let mut lease = v.lease.clone();
    lease.refresh(c.now);
    let mut r = Resp::new(200u16).header("Last-Modified", rfc1123(v.last_modified)).header("ETag", v.etag);
    r = meta::headers(r, v.metadata);
    for (k, val) in lease.headers() {
        r = r.header(k, val);
    }
    r = r
        .header("x-ms-has-immutability-policy", "false")
        .header("x-ms-has-legal-hold", "false")
        .header("x-ms-share-quota", v.quota)
        .header("x-ms-access-tier", v.tier)
        .header("x-ms-access-tier-change-time", rfc1123(v.tier_changed));
    if let Some(t) = v.tier_transition {
        r = r.header("x-ms-access-tier-transition-state", t).header("x-ms-access-tier-change-count", v.tier_change_count);
    }
    r.header("x-ms-enabled-protocols", v.protocols).build()
}

pub fn get_metadata(acct: &Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let v = match view(share, &snap, req) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_read(req, Kind::Share, v.lease, c.now) {
        return e;
    }
    meta::headers(Resp::new(200u16).header("Last-Modified", rfc1123(v.last_modified)).header("ETag", v.etag), v.metadata).build()
}

/// Operations on the base share only: a snapshot is read-only.
fn base_only(req: &Request) -> Result<(), Response> {
    match req.query.get("sharesnapshot") {
        None => Ok(()),
        Some(v) => Err(errors::invalid_query(
            &req.ctx,
            "sharesnapshot",
            v,
            "This operation is only allowed on the base share. ShareSnapshot query parameter should not be provided.",
        )),
    }
}

pub fn set_properties(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = base_only(req) {
        return e;
    }
    let (quota, tier) = match (quota_of(req), tier_of(req)) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(q), Ok(t)) => (q, t),
    };
    let share = match find_mut(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_write(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    if let Some(q) = quota {
        share.quota = q;
    }
    if let Some(t) = tier {
        if t != share.tier {
            share.tier_transition = Some(format!("pending-from-{}", lower_first(&share.tier)));
            share.tier = t;
            share.tier_changed = c.now;
            share.tier_change_count += 1;
        }
    }
    share.touch(c.now);
    Resp::new(200u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified)).build()
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn set_metadata(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = base_only(req) {
        return e;
    }
    let metadata = match meta::read(req) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let share = match find_mut(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_write(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    share.metadata = metadata;
    share.touch(c.now);
    Resp::new(200u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified)).build()
}

pub fn get_acl(acct: &Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = base_only(req) {
        return e;
    }
    let share = match find(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_read(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    let body = if share.acl.is_empty() {
        "<SignedIdentifiers />".to_string()
    } else {
        let mut s = String::from("<SignedIdentifiers>");
        for id in &share.acl {
            s.push_str("<SignedIdentifier>");
            s.push_str(&xml::element("Id", &id.id));
            if id.has_policy {
                s.push_str("<AccessPolicy>");
                for (tag, value) in [("Start", &id.start), ("Expiry", &id.expiry), ("Permission", &id.permission)] {
                    if let Some(v) = value {
                        s.push_str(&xml::element(tag, v));
                    }
                }
                s.push_str("</AccessPolicy>");
            }
            s.push_str("</SignedIdentifier>");
        }
        s.push_str("</SignedIdentifiers>");
        s
    };
    Resp::new(200u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified)).xml(xml::document(&body)).build()
}

/// Times are stored the way Azure returns them: always with seven fraction digits.
fn normalize_time(v: &str) -> Option<String> {
    parse_iso8601(v.trim()).map(iso8601_7)
}

fn parse_identifier(n: &Node) -> Result<SignedId, ()> {
    let id = n.child_text("Id").ok_or(())?.to_string();
    if id.is_empty() || id.len() > 64 {
        return Err(());
    }
    let mut out = SignedId { id, ..SignedId::default() };
    if let Some(p) = n.child("AccessPolicy") {
        out.has_policy = true;
        if let Some(s) = p.child_text("Start").filter(|s| !s.trim().is_empty()) {
            out.start = Some(normalize_time(s).ok_or(())?);
        }
        if let Some(s) = p.child_text("Expiry").filter(|s| !s.trim().is_empty()) {
            out.expiry = Some(normalize_time(s).ok_or(())?);
        }
        if let Some(perm) = p.child_text("Permission") {
            let perm = perm.trim();
            if !perm.chars().all(|c| "rcwdl".contains(c)) {
                return Err(());
            }
            out.permission = Some(perm.to_string());
        }
    }
    Ok(out)
}

pub fn set_acl(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = base_only(req) {
        return e;
    }
    // An empty body clears the policies.
    let ids: Vec<SignedId> = if req.body.iter().all(u8::is_ascii_whitespace) {
        vec![]
    } else {
        let Ok(body) = std::str::from_utf8(&req.body) else {
            return errors::invalid_xml_no_pos(ctx, "Error parsing Xml content");
        };
        let root = match xml::parse(body) {
            Ok(r) => r,
            Err(e) => return errors::invalid_xml(ctx, e.pos, &e.reason),
        };
        if root.name != "SignedIdentifiers" {
            return errors::invalid_xml(ctx, root.end, "Error parsing Xml content");
        }
        let nodes: Vec<&Node> = root.children_named("SignedIdentifier").collect();
        if nodes.len() > MAX_SIGNED_IDENTIFIERS {
            return errors::invalid_xml_no_pos(ctx, "");
        }
        let mut ids: Vec<SignedId> = Vec::new();
        for n in nodes {
            match parse_identifier(n) {
                Ok(id) if !ids.iter().any(|x| x.id == id.id) => ids.push(id),
                _ => return errors::invalid_xml(ctx, n.end, "Error parsing Xml content"),
            }
        }
        ids
    };
    let share = match find_mut(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_write(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    share.acl = ids;
    share.touch(c.now);
    Resp::new(200u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified)).build()
}

pub fn stats(acct: &Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = base_only(req) {
        return e;
    }
    let share = match find(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(e) = leasing::check_read(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    let body = format!("<ShareStats><ShareUsageBytes>{}</ShareUsageBytes></ShareStats>", tree::usage(&share.root));
    Resp::new(200u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified)).xml(xml::document(&body)).build()
}

pub fn snapshot(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let metadata = match meta::read(req) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let share = match find_mut(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if share.snapshots.len() >= 200 {
        return errors::conflict(&req.ctx, "TooManyShareSnapshots", "The share has reached the maximum number of snapshots.");
    }
    // Snapshot times have a resolution of one second; two in the same second would be one.
    let mut at = OffsetDateTime::from_unix_timestamp(c.now.unix_timestamp()).unwrap_or(c.now);
    let mut time = iso8601_7(at);
    while share.snapshots.iter().any(|s| s.time == time) {
        at += Duration::seconds(1);
        time = iso8601_7(at);
    }
    let snap = share.take_snapshot(time.clone(), metadata);
    share.snapshots.push(snap);
    Resp::new(201u16)
        .header("x-ms-snapshot", time)
        .header("ETag", &share.etag)
        .header("Last-Modified", rfc1123(share.last_modified))
        .build()
}

pub fn lease(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let action = match leasing::parse(req, Kind::Share) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find_mut(acct, req, name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match snap {
        None => {
            let (etag, modified) = (share.etag.clone(), share.last_modified);
            leasing::apply(req, Kind::Share, action, &mut share.lease, c.now, &etag, modified)
        }
        Some(t) => match share.snapshots.iter_mut().find(|s| s.time == t) {
            Some(s) => {
                let (etag, modified) = (s.etag.clone(), s.last_modified);
                leasing::apply(req, Kind::Share, action, &mut s.lease, c.now, &etag, modified)
            }
            None => errors::share_not_found(&req.ctx),
        },
    }
}

pub fn delete(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some(share) = acct.shares.get_mut(name) else { return errors::share_not_found(&req.ctx) };
    if let Some(t) = snap {
        let Some(i) = share.snapshots.iter().position(|s| s.time == t) else { return errors::share_not_found(&req.ctx) };
        if let Err(e) = leasing::check_write(req, Kind::Share, &share.snapshots[i].lease, c.now) {
            return e;
        }
        share.snapshots.remove(i);
        return Resp::new(202u16).build();
    }
    if let Err(e) = leasing::check_write(req, Kind::Share, &share.lease, c.now) {
        return e;
    }
    let mode = match req.header("x-ms-delete-snapshots").map(str::trim) {
        None => None,
        Some(m) if m.eq_ignore_ascii_case("include") => Some(false),
        Some(m) if m.eq_ignore_ascii_case("include-leased") => Some(true),
        Some(m) => return errors::invalid_header(&req.ctx, "x-ms-delete-snapshots", m),
    };
    if !share.snapshots.is_empty() {
        match mode {
            None => return errors::share_has_snapshots(&req.ctx),
            Some(false) => {
                let leased = share.snapshots.iter().any(|s| {
                    let mut l = s.lease.clone();
                    l.refresh(c.now);
                    l.locked()
                });
                if leased {
                    return errors::conflict(
                        &req.ctx,
                        "DeleteShareWhenSnapshotLeased",
                        "Unable to delete share because one or more share snapshots have active leases. Release the share snapshot leases or delete the share with the include-leased parameter for x-ms-delete-snapshots.",
                    );
                }
            }
            Some(true) => {}
        }
    }
    if let Some(mut gone) = acct.shares.remove(name) {
        if c.soft_delete_days > 0 {
            gone.lease.refresh(c.now);
            let version = format!("{:016X}", c.now.unix_timestamp_nanos().max(0) as u64);
            acct.deleted.push(DeletedShare { share: gone, deleted: c.now, version });
        }
    }
    Resp::new(202u16).build()
}

pub fn restore(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    let Some(deleted_name) = req.header("x-ms-deleted-share-name") else {
        return errors::missing_header(&req.ctx, "x-ms-deleted-share-name");
    };
    let Some(version) = req.header("x-ms-deleted-share-version") else {
        return errors::missing_header(&req.ctx, "x-ms-deleted-share-version");
    };
    if acct.shares.contains_key(name) {
        return errors::share_already_exists(&req.ctx);
    }
    let Some(i) = acct.deleted.iter().position(|d| d.share.name == deleted_name && d.version == version) else {
        return errors::share_not_found(&req.ctx);
    };
    let mut share = acct.deleted.remove(i).share;
    share.name = name.to_string();
    let r = Resp::new(201u16).header("ETag", &share.etag).header("Last-Modified", rfc1123(share.last_modified));
    acct.shares.insert(name.to_string(), share);
    r.build()
}

pub fn create_permission(acct: &mut Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = find(acct, req, name) {
        return e;
    }
    let json: Option<serde_json::Value> = serde_json::from_slice(&req.body).ok();
    let Some(permission) = json.as_ref().and_then(|j| j.get("permission")).and_then(|p| p.as_str()) else {
        return errors::err_json_document(&req.ctx);
    };
    let binary = json.as_ref().and_then(|j| j.get("format")).and_then(|f| f.as_str()).is_some_and(|f| f.eq_ignore_ascii_case("binary"));
    if !(crate::smb::valid_sddl(permission) || binary && crate::smb::valid_binary(permission)) {
        return errors::err(&req.ctx, 400, "FileInvalidPermission", "The specified file permission is not valid.");
    }
    let key = crate::smb::permission_key(permission);
    // Azure hands the descriptor back normalised; the one change seen is the empty audit section added at the end
    let stored = if binary || permission.contains("S:") { permission.to_string() } else { format!("{permission}S:NO_ACCESS_CONTROL") };
    acct.permissions.insert(key.clone(), stored);
    Resp::new(201u16).header("x-ms-file-permission-key", key).build()
}

pub fn get_permission(acct: &Account, c: &Cx, name: &str) -> Response {
    let req = c.req;
    if let Err(e) = find(acct, req, name) {
        return e;
    }
    let Some(key) = req.header("x-ms-file-permission-key") else {
        return errors::missing_header(&req.ctx, "x-ms-file-permission-key");
    };
    let well_formed =
        key.split_once('*').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|x| x.is_ascii_digit()));
    if !well_formed {
        return errors::invalid_header(&req.ctx, "x-ms-file-permission-key", key);
    }
    match acct.permissions.get(key) {
        Some(p) => Resp::new(200u16).json(&serde_json::json!({ "permission": p, "format": "sddl" })).build(),
        None if key == crate::state::DEFAULT_DIR_KEY || key == crate::state::DEFAULT_FILE_KEY => {
            Resp::new(200u16).json(&serde_json::json!({ "permission": DEFAULT_SDDL, "format": "sddl" })).build()
        }
        None => errors::err(&req.ctx, 400, "FileInvalidPermissionKey", "The specified file permission key does not exist in the share."),
    }
}

/// What the default permission keys stand for: full control for the system, administrators and the owner (a descriptor of the shape
/// Azure gives a new share's files; the account's own identifiers are not known here).
const DEFAULT_SDDL: &str = "O:S-1-5-21-0-0-0-0G:S-1-5-21-0-0-0-513D:PAI(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;S-1-5-21-0-0-0-513)";

pub fn list(acct: &Account, c: &Cx) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    let prefix = req.query.get("prefix").unwrap_or("");
    let marker = req.query.get("marker").unwrap_or("");
    let include = req.query.get("include").unwrap_or("");
    let (mut metadata, mut snapshots, mut deleted) = (false, false, false);
    for part in include.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "metadata" => metadata = true,
            "snapshots" => snapshots = true,
            "deleted" => deleted = true,
            _ => return errors::invalid_query(ctx, "include", include, "This query parameter value is invalid."),
        }
    }
    let max = match req.query.get("maxresults") {
        None => None,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 1 => Some(n),
            Ok(_) => return errors::out_of_range_query(ctx, "maxresults", v, 1, i64::from(i32::MAX)),
            Err(_) => return errors::invalid_query(ctx, "maxresults", v, ""),
        },
    };
    let limit = max.map_or(MAX_LIST_RESULTS, |n| (n as usize).min(MAX_LIST_RESULTS));
    // A marker is "/{account}/{share}": the first share of the next page.
    let start = if marker.is_empty() {
        ""
    } else {
        match marker.strip_prefix('/').and_then(|m| m.split_once('/')) {
            Some((_, s)) => s,
            None => return errors::bad_marker(ctx),
        }
    };

    struct Item<'a> {
        name: &'a str,
        snapshot: Option<&'a str>,
        view: View<'a>,
        deleted: Option<&'a DeletedShare>,
    }
    let mut items: Vec<Item<'_>> = Vec::new();
    for (name, share) in acct.shares.iter().filter(|(n, _)| n.starts_with(prefix) && n.as_str() >= start) {
        if snapshots {
            for s in &share.snapshots {
                items.push(Item { name, snapshot: Some(&s.time), view: View::of_snapshot(s), deleted: None });
            }
        }
        items.push(Item { name, snapshot: None, view: View::of_share(share), deleted: None });
    }
    if deleted {
        for d in acct.deleted.iter().filter(|d| d.share.name.starts_with(prefix) && d.share.name.as_str() >= start) {
            items.push(Item { name: &d.share.name, snapshot: None, view: View::of_share(&d.share), deleted: Some(d) });
        }
        items.sort_by(|a, b| a.name.cmp(b.name).then(a.deleted.is_some().cmp(&b.deleted.is_some())));
    }
    let next = items.get(limit).map(|i| i.name.to_string());
    items.truncate(limit);

    let mut body = format!("<EnumerationResults ServiceEndpoint=\"{}/\">", storage::xml_escape(&ctx.endpoint.base_url()));
    if !prefix.is_empty() {
        body.push_str(&xml::element("Prefix", prefix));
    }
    if !marker.is_empty() {
        body.push_str(&xml::element("Marker", marker));
    }
    if let Some(n) = max {
        body.push_str(&format!("<MaxResults>{n}</MaxResults>"));
    }
    if items.is_empty() {
        body.push_str("<Shares />");
    } else {
        body.push_str("<Shares>");
        for it in &items {
            let v = &it.view;
            body.push_str("<Share>");
            body.push_str(&xml::element("Name", it.name));
            if let Some(s) = it.snapshot {
                body.push_str(&xml::element("Snapshot", s));
            }
            if let Some(d) = it.deleted {
                body.push_str(&format!("<Deleted>true</Deleted><Version>{}</Version>", d.version));
            }
            let mut lease = v.lease.clone();
            lease.refresh(c.now);
            body.push_str(&format!(
                "<Properties><Last-Modified>{}</Last-Modified><Etag>{}</Etag><LeaseStatus>{}</LeaseStatus><LeaseState>{}</LeaseState>",
                rfc1123(v.last_modified),
                v.etag,
                if lease.locked() { "locked" } else { "unlocked" },
                lease.state_name()
            ));
            if let Some(d) = lease.duration_name() {
                body.push_str(&format!("<LeaseDuration>{d}</LeaseDuration>"));
            }
            body.push_str(&format!("<Quota>{}</Quota>", v.quota));
            if it.snapshot.is_none() {
                body.push_str(&format!(
                    "<AccessTier>{}</AccessTier><AccessTierChangeTime>{}</AccessTierChangeTime>",
                    v.tier,
                    rfc1123(v.tier_changed)
                ));
                if let Some(t) = v.tier_transition {
                    body.push_str(&format!(
                        "<AccessTierTransitionState>{t}</AccessTierTransitionState><AccessTierChangeCount>{}</AccessTierChangeCount>",
                        v.tier_change_count
                    ));
                }
            }
            body.push_str("<DefaultEncryptionScope>$account-encryption-key</DefaultEncryptionScope><DenyEncryptionScopeOverride>false</DenyEncryptionScopeOverride>");
            if let Some(d) = it.deleted {
                let left = (i64::from(c.soft_delete_days) - (c.now - d.deleted).whole_days()).max(0);
                body.push_str(&format!(
                    "<DeletedTime>{}</DeletedTime><RemainingRetentionDays>{left}</RemainingRetentionDays>",
                    rfc1123(d.deleted)
                ));
            }
            body.push_str("</Properties>");
            if metadata {
                body.push_str(&meta::element(v.metadata));
            }
            body.push_str("</Share>");
        }
        body.push_str("</Shares>");
    }
    match next {
        Some(n) => body.push_str(&xml::element("NextMarker", &format!("/{}/{}", req.account, n))),
        None => body.push_str("<NextMarker />"),
    }
    body.push_str("</EnumerationResults>");
    Resp::new(200u16).xml(xml::document(&body)).build()
}
