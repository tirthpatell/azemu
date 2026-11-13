//! Blob property operations: Set Properties, Set Metadata, Set Tier, immutability
//! policy and legal hold.

use azemu_core::resp::Resp;
use azemu_core::time::{self, parse_rfc1123};

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, snapshot_param, version_param, Cx};
use crate::errors::{blob_not_found, container_not_found, err, ApiError, ApiResult};
use crate::headers::write_headers;
use crate::lease::{self, Scope};
use crate::state::{BlobKind, Immutability};
use crate::store::{check_cpk, check_scope_write, roll_version, valid_tier};
use crate::util::*;

pub fn set_properties(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let given = |h: &str| hdr(req, h).map(str::to_string);
    let any_content = [
        "x-ms-blob-cache-control",
        "x-ms-blob-content-type",
        "x-ms-blob-content-md5",
        "x-ms-blob-content-encoding",
        "x-ms-blob-content-language",
        "x-ms-blob-content-disposition",
    ]
    .iter()
    .any(|h| hdr(req, h).is_some());
    let new_len = hdr_u64(req, "x-ms-blob-content-length")?;
    let seq_action = hdr(req, "x-ms-sequence-number-action").map(|s| s.to_ascii_lowercase());
    let seq_value = hdr_u64(req, "x-ms-blob-sequence-number")?;
    let md5 = match hdr(req, "x-ms-blob-content-md5") {
        None => None,
        Some(v) => match b64_decode(v) {
            Some(d) => Some(d),
            None => return Err(ApiError::invalid_header("x-ms-blob-content-md5", v)),
        },
    };
    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live().ok_or_else(blob_not_found)?;
    check_cpk(b, req)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    if (new_len.is_some() || seq_action.is_some()) && b.kind != BlobKind::Page {
        return Err(err("InvalidBlobType"));
    }
    if let Some(n) = new_len {
        if n % 512 != 0 {
            return Err(ApiError::invalid_header("x-ms-blob-content-length", &n.to_string()));
        }
    }
    if let Some(a) = &seq_action {
        if !["max", "update", "increment"].contains(&a.as_str()) {
            return Err(ApiError::invalid_header("x-ms-sequence-number-action", a));
        }
        if a != "increment" && seq_value.is_none() {
            return Err(ApiError::missing_header("x-ms-blob-sequence-number"));
        }
    }
    roll_version(&mut ctl, e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    if any_content {
        b.props.content_type =
            given("x-ms-blob-content-type").filter(|s| !s.is_empty()).unwrap_or_else(|| "application/octet-stream".into());
        b.props.content_encoding = given("x-ms-blob-content-encoding").filter(|s| !s.is_empty());
        b.props.content_language = given("x-ms-blob-content-language").filter(|s| !s.is_empty());
        b.props.content_disposition = given("x-ms-blob-content-disposition").filter(|s| !s.is_empty());
        b.props.cache_control = given("x-ms-blob-cache-control").filter(|s| !s.is_empty());
        b.props.content_md5 = md5;
    }
    if let Some(n) = new_len {
        if n < b.size {
            b.pages.truncate(n);
        }
        b.size = n;
    }
    match (seq_action.as_deref(), seq_value) {
        (Some("update"), Some(v)) => b.sequence = v,
        (Some("max"), Some(v)) => b.sequence = b.sequence.max(v),
        (Some("increment"), _) => {
            b.sequence = b.sequence.checked_add(1).ok_or_else(|| err("SequenceNumberIncrementTooLarge"))?;
        }
        _ => {}
    }
    b.touch(now);
    let mut r = Resp::new(200u16).header("ETag", &b.etag).header("Last-Modified", time::rfc1123(b.last_modified));
    if b.kind == BlobKind::Page {
        r = r.header("x-ms-blob-sequence-number", b.sequence);
    }
    Ok(r.build())
}

pub fn set_metadata(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
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
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    roll_version(&mut ctl, e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    b.metadata = md;
    if b.dfs.as_ref().map(|d| d.is_dir).unwrap_or(false) {
        // a directory stays a directory
        b.metadata.push((crate::dfs::model::FOLDER_KEY.to_string(), "true".to_string()));
        b.metadata.sort();
    }
    b.touch(now);
    Ok(write_headers(Resp::new(200u16), b).build())
}

pub fn set_tier(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let tier = hdr(req, "x-ms-access-tier").ok_or_else(|| ApiError::missing_header("x-ms-access-tier"))?;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let priority = match hdr(req, "x-ms-rehydrate-priority") {
        None => None,
        Some(p) if p.eq_ignore_ascii_case("high") => Some("High".to_string()),
        Some(p) if p.eq_ignore_ascii_case("standard") => Some("Standard".to_string()),
        Some(p) => return Err(ApiError::invalid_header("x-ms-rehydrate-priority", p)),
    };
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find_mut(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;
    // Azure ignores `x-ms-if-tags` when it sets the tier of a (premium) page blob: the
    // recording of a wrong tag condition on a page blob answers 200.
    if b.kind != BlobKind::Page {
        cond::check_tags(req, Some(&b.tags))?;
    }
    if snapshot.is_none() && version.is_none() {
        lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    }
    if b.kind == BlobKind::Append {
        return Err(err("InvalidBlobType"));
    }
    let tier = valid_tier(b.kind, tier).ok_or_else(|| ApiError::invalid_header("x-ms-access-tier", tier))?;
    if b.archive_status.is_some() {
        return Err(err("BlobBeingRehydrated"));
    }
    let from_archive = b.tier.as_deref() == Some("Archive") && tier != "Archive";
    if from_archive {
        // Rehydration takes time on Azure: the blob stays archived until it is done.
        b.archive_status = Some(format!("rehydrate-pending-to-{}", tier.to_ascii_lowercase()));
        b.rehydrate_priority = Some(priority.unwrap_or_else(|| "Standard".into()));
        b.tier_changed = Some(now);
        return Ok(Resp::new(202u16).build());
    }
    b.tier = Some(tier);
    b.tier_changed = Some(now);
    Ok(Resp::new(200u16).build())
}

pub fn set_legal_hold(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let hold = match hdr(req, "x-ms-legal-hold") {
        Some(v) if v.eq_ignore_ascii_case("true") => true,
        Some(v) if v.eq_ignore_ascii_case("false") => false,
        Some(v) => return Err(ApiError::invalid_header("x-ms-legal-hold", v)),
        None => return Err(ApiError::missing_header("x-ms-legal-hold")),
    };
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find_mut(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;
    b.legal_hold = hold;
    Ok(Resp::new(200u16).header("x-ms-legal-hold", hold.to_string()).build())
}

pub fn set_immutability(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let until =
        hdr(req, "x-ms-immutability-policy-until-date").ok_or_else(|| ApiError::missing_header("x-ms-immutability-policy-until-date"))?;
    let until_t = parse_rfc1123(until).ok_or_else(|| ApiError::invalid_header("x-ms-immutability-policy-until-date", until))?;
    let mode = hdr(req, "x-ms-immutability-policy-mode").unwrap_or("unlocked").to_ascii_lowercase();
    if mode != "unlocked" && mode != "locked" {
        return Err(ApiError::invalid_header("x-ms-immutability-policy-mode", &mode));
    }
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find_mut(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;
    if let Some(old) = &b.immutability {
        if old.mode == "locked" && old.until > now && until_t < old.until {
            return Err(err("BlobImmutableDueToPolicy"));
        }
    }
    b.immutability = Some(Immutability { until: until_t, mode: mode.clone() });
    Ok(Resp::new(200u16)
        .header("x-ms-immutability-policy-until-date", time::rfc1123(until_t))
        .header("x-ms-immutability-policy-mode", mode)
        .build())
}

pub fn delete_immutability(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let version = version_param(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find_mut(snapshot.as_deref(), version.as_deref()).ok_or_else(blob_not_found)?;
    if let Some(old) = &b.immutability {
        if old.mode == "locked" && old.until > now {
            return Err(err("BlobImmutableDueToPolicy"));
        }
    }
    b.immutability = None;
    Ok(Resp::new(200u16).build())
}
