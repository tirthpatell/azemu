//! Container operations: create, delete, properties, metadata, ACL, restore.

use azemu_core::etag;
use azemu_core::resp::Resp;
use azemu_core::time::{self, parse_iso8601};

use crate::cond::{self, Kind};
use crate::cx::Cx;
use crate::errors::{container_not_found, err, ApiError, ApiResult};
use crate::lease::{self, Scope};
use crate::state::{Container, SignedId};
use crate::util::{hdr, parse_metadata, validate_container_name};
use crate::xml::{self, Out};

fn public_access_header(req: &azemu_core::Request) -> Result<Option<String>, ApiError> {
    match hdr(req, "x-ms-blob-public-access") {
        None => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("blob") => Ok(Some("blob".into())),
        Some(v) if v.eq_ignore_ascii_case("container") => Ok(Some("container".into())),
        Some(v) => Err(ApiError::invalid_header("x-ms-blob-public-access", v)),
    }
}

pub fn create(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_container_name(name)?;
    let metadata = parse_metadata(req)?;
    let access = public_access_header(req)?;
    let scope = hdr(req, "x-ms-default-encryption-scope").map(str::to_string);
    let deny = hdr(req, "x-ms-deny-encryption-scope-override").map(|v| v.eq_ignore_ascii_case("true"));
    let acct = cx.account();
    if access.is_some() && !acct.public_access_allowed {
        return Err(crate::auth::public_access_not_permitted());
    }
    if acct.containers.contains_key(name) {
        return Err(err("ContainerAlreadyExists"));
    }
    let mut c = Container::new(name, now, etag::new_etag());
    c.metadata = metadata;
    c.public_access = access;
    if let Some(s) = scope {
        c.default_scope = s;
    }
    if let Some(d) = deny {
        c.deny_scope_override = d;
    }
    let r = Resp::new(201u16).header("ETag", &c.etag).header("Last-Modified", time::rfc1123(c.last_modified)).build();
    acct.containers.insert(name.to_string(), c);
    Ok(r)
}

/// Soft-deleted containers kept per account.
const MAX_DELETED_CONTAINERS: usize = 256;

pub fn delete(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    if name == crate::analytics::LOGS && cx.account().logs_pinned {
        // Microsoft Learn ("Azure Storage Analytics logging"): the container cannot be deleted once Storage Analytics has
        // been enabled, though its contents can be. The page gives no error code; this one is a guess.
        return Err(err("InvalidOperation")
            .msg("The $logs container cannot be deleted once Storage Analytics logging has been enabled. Its blobs can be deleted."));
    }
    let c = cx.container(name)?;
    lease::check_write(&c.lease, hdr(req, "x-ms-lease-id"), Scope::Container)?;
    cond::check_container(req, &c.etag, c.last_modified, Kind::Write)?;
    protected_blobs(c, now)?;
    let acct = cx.account();
    let soft = acct.props.soft_delete_days().is_some();
    let Some(mut c) = acct.containers.remove(name) else { return Err(container_not_found()) };
    if soft {
        c.deleted = Some((now, format!("{:016X}", now.unix_timestamp_nanos() / 100)));
        c.lease = Default::default();
        acct.deleted_containers.retain(|d| d.name != name);
        acct.deleted_containers.push(c);
        // Nothing ever purged these: drop the ones past the retention period, and the oldest
        // beyond a fixed number.
        let days = acct.props.soft_delete_days().unwrap_or(7) as i64;
        acct.deleted_containers.retain(|d| d.deleted.as_ref().map(|(t, _)| *t + time::Duration::days(days) > now).unwrap_or(true));
        if acct.deleted_containers.len() > MAX_DELETED_CONTAINERS {
            let extra = acct.deleted_containers.len() - MAX_DELETED_CONTAINERS;
            acct.deleted_containers.drain(..extra);
        }
    }
    Ok(Resp::new(202u16).build())
}

/// A container cannot be deleted while a blob in it (the blob, a snapshot or a version) is under a legal hold or under an
/// immutability policy that has not run out: `409 ContainerHasLegalHold`, `409 ContainerImmutabilityPolicyLocked` for a
/// locked policy. Azure's pages on immutable storage say the delete fails when a protected blob is there; they name the
/// two codes for container-level policies, which this emulator does not have (it has the policies of single blobs and
/// versions). For an unlocked policy the codes do not fit, so the answer is the blob's own code,
/// `409 BlobImmutableDueToPolicy` (a guess, see `docs/services/blob.md`). A blob that is already deleted does not count.
/// The Data Lake file system delete and the account deletion of ARM go through the same check.
fn protected_blobs(c: &Container, now: time::OffsetDateTime) -> Result<(), ApiError> {
    let (mut hold, mut locked, mut unlocked) = (false, false, false);
    for e in c.blobs.values() {
        let live = e.current.iter().filter(|b| b.deleted.is_none()).chain(e.snapshots.values()).chain(e.versions.iter());
        for b in live {
            hold |= b.legal_hold;
            if let Some(i) = b.immutability.as_ref().filter(|i| i.until > now) {
                if i.mode == "locked" {
                    locked = true;
                } else {
                    unlocked = true;
                }
            }
        }
    }
    if hold {
        Err(err("ContainerHasLegalHold"))
    } else if locked {
        Err(err("ContainerImmutabilityPolicyLocked"))
    } else if unlocked {
        Err(err("BlobImmutableDueToPolicy").msg("The container cannot be deleted: a blob in it is protected by an immutability policy."))
    } else {
        Ok(())
    }
}

/// Headers shared by Get Container Properties and Get Container Metadata.
fn container_headers(mut r: Resp, c: &Container) -> Resp {
    r = r
        .header("ETag", &c.etag)
        .header("Last-Modified", time::rfc1123(c.last_modified))
        .header("x-ms-lease-status", c.lease.status_str())
        .header("x-ms-lease-state", c.lease.state_str())
        .header("x-ms-has-immutability-policy", "false")
        .header("x-ms-has-legal-hold", "false")
        .header("x-ms-default-encryption-scope", &c.default_scope)
        .header("x-ms-deny-encryption-scope-override", c.deny_scope_override.to_string())
        .header("x-ms-immutable-storage-with-versioning-enabled", "false");
    if let Some(d) = c.lease.duration_str() {
        r = r.header("x-ms-lease-duration", d);
    }
    if let Some(p) = &c.public_access {
        r = r.header("x-ms-blob-public-access", p);
    }
    for (k, v) in &c.metadata {
        r = r.header(format!("x-ms-meta-{k}"), v);
    }
    r
}

pub fn get_properties(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let c = cx.container(name)?;
    lease::check_read(&c.lease, hdr(req, "x-ms-lease-id"), Scope::Container)?;
    Ok(container_headers(Resp::new(200u16), c).build())
}

pub fn set_metadata(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let md = parse_metadata(req)?;
    if name == crate::analytics::LOGS && cx.account().logs_pinned {
        // Microsoft Learn ("Azure Storage Analytics logging"): the container cannot be deleted once Storage Analytics has
        // been enabled, though its contents can be. The page gives no error code; this one is a guess.
        return Err(err("InvalidOperation")
            .msg("The $logs container cannot be deleted once Storage Analytics logging has been enabled. Its blobs can be deleted."));
    }
    let c = cx.container(name)?;
    lease::check_write(&c.lease, hdr(req, "x-ms-lease-id"), Scope::Container)?;
    cond::check_container(req, &c.etag, c.last_modified, Kind::Write)?;
    c.metadata = md;
    c.touch(now);
    Ok(Resp::new(200u16).header("ETag", &c.etag).header("Last-Modified", time::rfc1123(c.last_modified)).build())
}

fn norm_time(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|v| parse_iso8601(v).map(time::iso8601_7).unwrap_or_else(|| v.clone()))
}

pub fn get_acl(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let c = cx.container(name)?;
    lease::check_read(&c.lease, hdr(req, "x-ms-lease-id"), Scope::Container)?;
    let mut o = Out::new();
    o.open("SignedIdentifiers");
    for p in &c.policies {
        o.open("SignedIdentifier").el("Id", &p.id).open("AccessPolicy");
        if let Some(s) = norm_time(&p.start) {
            o.el("Start", &s);
        }
        if let Some(s) = norm_time(&p.expiry) {
            o.el("Expiry", &s);
        }
        if let Some(s) = &p.permission {
            o.el("Permission", s);
        }
        o.close("AccessPolicy").close("SignedIdentifier");
    }
    o.close("SignedIdentifiers");
    let mut r = Resp::new(200u16).header("ETag", &c.etag).header("Last-Modified", time::rfc1123(c.last_modified)).xml(o.finish());
    if let Some(p) = &c.public_access {
        r = r.header("x-ms-blob-public-access", p);
    }
    Ok(r.build())
}

pub fn set_acl(cx: &mut Cx, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let access = public_access_header(req)?;
    let mut policies: Vec<SignedId> = vec![];
    if !req.body.is_empty() {
        let text = std::str::from_utf8(&req.body).map_err(|_| ApiError::invalid_xml("The body is not valid UTF-8."))?;
        let root = xml::parse(text).map_err(|e| ApiError::invalid_xml(&e))?;
        if root.name != "SignedIdentifiers" {
            return Err(ApiError::invalid_xml("The root element must be SignedIdentifiers."));
        }
        for si in root.children_named("SignedIdentifier") {
            let id = si.child_text("Id").ok_or_else(|| err("MissingRequiredXmlNode").detail("XmlNodeName", "Id"))?;
            if id.is_empty() || id.len() > 64 {
                return Err(ApiError::invalid_xml("The signed identifier id must be between 1 and 64 characters."));
            }
            if policies.iter().any(|p| p.id == id) {
                return Err(ApiError::invalid_xml("Duplicate signed identifier id."));
            }
            let ap = si.child("AccessPolicy");
            policies.push(SignedId {
                id: id.to_string(),
                start: ap.and_then(|a| a.child_text("Start")).map(str::to_string),
                expiry: ap.and_then(|a| a.child_text("Expiry")).map(str::to_string),
                permission: ap.and_then(|a| a.child_text("Permission")).map(str::to_string),
            });
        }
        if policies.len() > 5 {
            return Err(ApiError::invalid_xml("A maximum of 5 access policies may be set."));
        }
    }
    if name == crate::analytics::LOGS && cx.account().logs_pinned {
        // Microsoft Learn ("Azure Storage Analytics logging"): the container cannot be deleted once Storage Analytics has
        // been enabled, though its contents can be. The page gives no error code; this one is a guess.
        return Err(err("InvalidOperation")
            .msg("The $logs container cannot be deleted once Storage Analytics logging has been enabled. Its blobs can be deleted."));
    }
    let public_access_allowed = cx.account().public_access_allowed;
    let c = cx.container(name)?;
    if access.is_some() && !public_access_allowed {
        return Err(crate::auth::public_access_not_permitted());
    }
    lease::check_write(&c.lease, hdr(req, "x-ms-lease-id"), Scope::Container)?;
    cond::check_container(req, &c.etag, c.last_modified, Kind::Write)?;
    c.policies = policies;
    c.public_access = access;
    c.touch(now);
    Ok(Resp::new(200u16).header("ETag", &c.etag).header("Last-Modified", time::rfc1123(c.last_modified)).build())
}

pub fn undelete(cx: &mut Cx, _name: &str) -> ApiResult {
    let req = cx.req;
    let want = hdr(req, "x-ms-deleted-container-name").ok_or_else(|| ApiError::missing_header("x-ms-deleted-container-name"))?;
    let version = hdr(req, "x-ms-deleted-container-version");
    let acct = cx.account();
    let pos = acct
        .deleted_containers
        .iter()
        .position(|c| c.name == want && version.map(|v| c.deleted.as_ref().map(|d| d.1 == v).unwrap_or(false)).unwrap_or(true))
        .ok_or_else(container_not_found)?;
    if acct.containers.contains_key(want) {
        return Err(err("ContainerAlreadyExists"));
    }
    let mut c = acct.deleted_containers.remove(pos);
    c.deleted = None;
    acct.containers.insert(c.name.clone(), c);
    Ok(Resp::new(201u16).build())
}
