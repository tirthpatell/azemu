//! Blob operations: Put Blob, Get Blob, Get Blob Properties, Delete, Undelete, Snapshot.

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time;
use azemu_core::Request;

use crate::auth::apply_sas_overrides;
use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, snapshot_param, version_param, Cx};
use crate::errors::{blob_not_found, container_not_found, err, ApiError, ApiResult};
use crate::headers::{blob_headers, hash_headers, md5_header, write_headers};
use crate::lease::{self, Scope};
use crate::state::{join, Blob, BlobEntry, BlobKind, OutBody, Piece};
use crate::store::{
    check_container_scope, check_cpk, check_mutable, check_scope_write, install, keep_deleted, new_id, new_spec, roll_version,
};
use crate::util::*;

/// Largest slice of a page blob one request may read (the body is built in memory).
const MAX_PAGE_READ: u64 = 1 << 30;

pub fn parse_kind(req: &Request) -> Result<BlobKind, ApiError> {
    match hdr(req, "x-ms-blob-type") {
        None => Err(ApiError::missing_header("x-ms-blob-type")),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "blockblob" => Ok(BlobKind::Block),
            "appendblob" => Ok(BlobKind::Append),
            "pageblob" => Ok(BlobKind::Page),
            _ => Err(ApiError::invalid_header("x-ms-blob-type", v)),
        },
    }
}

/// Checks that apply before a blob is created or overwritten: conditions, tag
/// conditions, lease and immutability.
pub fn guard_overwrite(entry: &mut BlobEntry, req: &Request, now: azemu_core::time::OffsetDateTime) -> Result<(), ApiError> {
    refresh_entry(entry, now);
    let live = entry.live();
    let kind = if hdr(req, "x-ms-lease-id").is_some() { Kind::CreateLeased } else { Kind::Create };
    cond::check(req, "", live.map(|b| (b.etag.as_str(), b.last_modified)), kind)?;
    cond::check_tags(req, live.map(|b| &b.tags))?;
    if let Some(b) = live {
        lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
        check_mutable(b, now)?;
    }
    Ok(())
}

pub fn put_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_blob_name(name)?;
    let kind = parse_kind(req)?;
    let spec = new_spec(req, kind, true)?;
    let mut blob = spec.build(kind, now);
    let mut hashes = None;
    match kind {
        BlobKind::Block => {
            let (md5, crc) = check_body_hashes(req, &req.body, cx.pre)?;
            if let Some(given) = &blob.props.content_md5 {
                // Azure checks x-ms-blob-content-md5 against the body as well.
                if *given != md5 {
                    return Err(err("Md5Mismatch").detail("UserSpecifiedMd5", b64(given)).detail("ServerCalculatedMd5", b64(&md5)));
                }
            }
            blob.props.content_md5 = Some(md5.clone());
            blob.crc64 = Some(crc);
            blob.set_content(req.body.clone());
            hashes = Some((md5, crc));
        }
        BlobKind::Append => {}
        BlobKind::Page => {
            let size = hdr_u64(req, "x-ms-blob-content-length")?.ok_or_else(|| ApiError::missing_header("x-ms-blob-content-length"))?;
            if size % 512 != 0 || size > (8u64 << 40) {
                return Err(ApiError::invalid_header("x-ms-blob-content-length", &size.to_string()));
            }
            blob.size = size;
            blob.sequence = hdr_u64(req, "x-ms-blob-sequence-number")?.unwrap_or(0);
        }
    }
    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    check_container_scope(c, req)?;
    let entry = c.blobs.entry(name.to_string()).or_default();
    if let Err(e) = guard_overwrite(entry, req, now) {
        c.prune(name);
        return Err(e);
    }
    install(&mut ctl, entry, blob, now);
    let b = entry.current.as_ref().ok_or_else(blob_not_found)?;
    let mut r = write_headers(Resp::new(201u16), b);
    if let Some((md5, crc)) = hashes {
        r = hash_headers(r, &md5, crc);
    }
    Ok(r.build())
}

/// Get Blob (`head` false) and Get Blob Properties / Get Blob Metadata (`head` true).
pub fn get_blob(cx: &mut Cx, container: &str, name: &str, head: bool) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let authz = cx.authz.clone();
    let c = cx.container(container)?;
    let default_scope = c.default_scope.clone();
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let is_current = e.live().map(|b| b.version_id.as_deref()) == Some(version.as_deref()) && version.is_some()
        || (snapshot.is_none() && version.is_none());
    let b = e.find(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;

    check_cpk(b, req)?;
    if snapshot.is_none() && version.is_none() {
        lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    }
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Read)?;
    cond::check_tags(req, Some(&b.tags))?;
    if !head && b.tier.as_deref() == Some("Archive") {
        return Err(err("BlobArchived"));
    }

    let size = b.size;
    let range = if head { None } else { request_range(req, "x-ms-range")? };
    // A page blob can be declared far larger than memory; refuse to build such a body.
    if !head && b.kind == BlobKind::Page {
        let wanted = match range.and_then(|r| resolve_range(r, size)) {
            Some((s, e)) => e - s + 1,
            None => size,
        };
        if wanted > MAX_PAGE_READ {
            return Err(err("InvalidRange").msg("The requested range is too large for this emulator; read page blobs in smaller ranges."));
        }
    }
    let mut r;
    let body: Bytes;
    let out: Option<OutBody>;
    match range {
        None => {
            r = Resp::new(200u16);
            // The body is only collected under the lock; joined after it is released.
            let pieces = if head { vec![] } else { b.read_pieces(0, size) };
            (body, out) = split_body(pieces, false, false);
            if let Some(md5) = md5_header(b) {
                r = r.header("Content-MD5", md5);
            }
            if let Some(crc) = b.crc64 {
                r = r.header("x-ms-content-crc64", crc64_b64(crc));
            }
            if head {
                r = r.header("Content-Length", size);
            }
        }
        Some(spec) => {
            let (start, end) =
                resolve_range(spec, size).ok_or_else(|| err("InvalidRange").header("Content-Range", format!("bytes */{size}")))?;
            let len = end - start + 1;
            let pieces = b.read_pieces(start, len);
            r = Resp::new(206u16).header("Content-Range", format!("bytes {start}-{end}/{size}"));
            let want_md5 = hdr_true(req, "x-ms-range-get-content-md5");
            let want_crc = hdr_true(req, "x-ms-range-get-content-crc64");
            if (want_md5 || want_crc) && len > 4 * 1024 * 1024 {
                return Err(ApiError::invalid_header(
                    if want_md5 { "x-ms-range-get-content-md5" } else { "x-ms-range-get-content-crc64" },
                    "true",
                ));
            }
            // `Content-MD5` / `x-ms-content-crc64` of the range are worked out after the lock.
            (body, out) = split_body(pieces, want_md5, want_crc);
            if let Some(md5) = md5_header(b) {
                r = r.header("x-ms-blob-content-md5", md5);
            }
        }
    }
    let mut r = blob_headers(r, b, is_current);
    if b.encryption_scope.is_none() && default_scope != "$account-encryption-key" {
        // a blob in a container with a default encryption scope is encrypted with it
        r = r.header("x-ms-encryption-scope", &default_scope);
    }
    if range.is_some() {
        r = r.header("Content-Type", &b.props.content_type);
    }
    let r = apply_sas_overrides(r, &authz);
    cx.st.out_body = out;
    Ok(r.body(body).build())
}

/// A body that is one stored buffer (or empty) and needs no hash goes out as it is; anything
/// else is left in pieces for the service to put together after the lock is released.
fn split_body(pieces: Vec<Piece>, md5: bool, crc64: bool) -> (Bytes, Option<OutBody>) {
    let simple = pieces.is_empty() || (pieces.len() == 1 && matches!(pieces[0], Piece::Data(_)));
    if simple && !md5 && !crc64 {
        (join(pieces), None)
    } else {
        (Bytes::new(), Some(OutBody { pieces, md5, crc64 }))
    }
}

pub fn delete_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let option = match hdr(req, "x-ms-delete-snapshots") {
        None => None,
        Some(v) if v.eq_ignore_ascii_case("include") => Some("include"),
        Some(v) if v.eq_ignore_ascii_case("only") => Some("only"),
        Some(v) => return Err(ApiError::invalid_header("x-ms-delete-snapshots", v)),
    };
    let permanent = req.query.get("deletetype").map(|v| v.eq_ignore_ascii_case("permanent")).unwrap_or(false);
    let (containers, mut ctl) = cx.account().split();
    let retention = ctl.policy.retention;
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let soft = retention.is_some() && !permanent;

    if let Some(s) = snapshot {
        let b = e.snapshots.get(&s).filter(|b| b.deleted.is_none()).ok_or_else(blob_not_found)?;
        cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
        if soft {
            if let Some(sn) = e.snapshots.get_mut(&s) {
                sn.deleted = Some(now);
            }
        } else {
            e.snapshots.remove(&s);
        }
        c.prune(name);
        // recordings `test_common_blob.pyTestStorageCommonBlobtest_delete_blob_snapshot.json` entry 4 (`true`, no soft delete) and
        // `test_common_blob.pyTestStorageCommonBlobtest_soft_delete_single_blob_snapshot.json` entry 5 (`false`, soft delete on)
        return Ok(Resp::new(202u16).header("x-ms-delete-type-permanent", (!soft).to_string()).build());
    }
    if let Some(v) = version {
        if e.current.as_ref().and_then(|b| b.version_id.as_deref()) == Some(v.as_str()) && e.live().is_some() {
            let b = e.live().ok_or_else(blob_not_found)?;
            lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
            e.current = None;
        } else {
            let pos = e.versions.iter().position(|b| b.version_id.as_deref() == Some(v.as_str())).ok_or_else(blob_not_found)?;
            check_mutable(&e.versions[pos], now)?;
            e.versions.remove(pos);
        }
        c.prune(name);
        // recording `test_common_blob.pyTestStorageCommonBlobtest_blob_version_id_operations.json` entry 20: `false` (soft delete on)
        return Ok(Resp::new(202u16).header("x-ms-delete-type-permanent", (!soft).to_string()).build());
    }

    let b = e.live().ok_or_else(blob_not_found)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    check_tier_conditions(req, b)?;
    let has_snaps = e.snapshots.values().any(|s| s.deleted.is_none());
    if has_snaps && option.is_none() {
        return Err(err("SnapshotsPresent"));
    }
    check_mutable(b, now)?;

    let delete_snaps = |e: &mut BlobEntry| {
        if soft {
            for s in e.snapshots.values_mut() {
                if s.deleted.is_none() {
                    s.deleted = Some(now);
                }
            }
        } else {
            e.snapshots.clear();
        }
    };
    // `x-ms-delete-type-permanent` says whether anything of the blob can still be recovered: `false` when it was soft deleted
    // or when versioning kept its content as a version, `true` when it is gone. Recordings: `true` for a plain delete in an
    // account without versioning or soft delete (`test_blob_access_conditions.pyTestStorageBlobAccessConditionstest_delete_blob_with_if_match.json`
    // entry 3), `false` with versioning on (`test_append_blob.pyTestStorageAppendBlobtest_create_append_blob_with_immutability_policy.json`
    // entry 10) and with soft delete on (`test_arrow.pyTestStorageApacheArrowtest_arrow_list_blobs_soft_deleted.json` entry 2).
    let mut recoverable = soft;
    match option {
        Some("only") => delete_snaps(e),
        _ => {
            if option == Some("include") {
                delete_snaps(e);
            }
            if soft {
                if let Some(cur) = e.current.as_mut() {
                    cur.deleted = Some(now);
                }
            } else if let Some(cur) = e.current.take() {
                recoverable = cur.version_id.is_some();
                keep_deleted(&mut ctl, e, cur);
                e.uncommitted.clear();
            }
        }
    }
    c.prune(name);
    Ok(Resp::new(202u16).header("x-ms-delete-type-permanent", (!recoverable).to_string()).build())
}

fn check_tier_conditions(req: &Request, b: &Blob) -> Result<(), ApiError> {
    let changed = b.tier_changed.unwrap_or(b.creation_time);
    if let Some(d) = hdr_date(req, "x-ms-access-tier-if-modified-since")? {
        if changed <= d {
            return Err(err("AccessTierChangeTimeConditionNotMet"));
        }
    }
    if let Some(d) = hdr_date(req, "x-ms-access-tier-if-unmodified-since")? {
        if changed > d {
            return Err(err("AccessTierChangeTimeConditionNotMet"));
        }
    }
    Ok(())
}

pub fn undelete_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    let mut any = e.current.is_some() || !e.snapshots.is_empty();
    if let Some(b) = e.current.as_mut() {
        b.deleted = None;
    }
    for s in e.snapshots.values_mut() {
        s.deleted = None;
    }
    for v in e.versions.iter_mut() {
        v.deleted = None;
        any = true;
    }
    if !any {
        return Err(blob_not_found());
    }
    Ok(Resp::new(200u16).build())
}

pub fn snapshot_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let md = parse_metadata(req)?;
    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live().ok_or_else(blob_not_found)?;
    check_cpk(b, req)?;
    check_scope_write(b, req)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    let mut snap = b.clone();
    let id = new_id(ctl.last_ts, now);
    snap.snapshot = Some(id.clone());
    snap.lease = Default::default();
    if !md.is_empty() {
        snap.metadata = md;
    }
    let (etag, lm) = (snap.etag.clone(), snap.last_modified);
    e.snapshots.insert(id.clone(), snap);
    roll_version(&mut ctl, e, now);
    let version = e.current.as_ref().and_then(|b| b.version_id.clone());
    let mut r = Resp::new(201u16)
        .header("x-ms-snapshot", id)
        .header("ETag", etag)
        .header("Last-Modified", time::rfc1123(lm))
        .header("x-ms-request-server-encrypted", "true");
    if let Some(v) = version {
        r = r.header("x-ms-version-id", v);
    }
    Ok(r.build())
}
