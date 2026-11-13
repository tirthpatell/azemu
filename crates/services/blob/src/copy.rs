//! Copy Blob, Put Blob From URL, Abort Copy, and reading a source blob that is
//! addressed by URL (the URL points back at this emulator, so it is resolved from
//! state and never fetched).

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time;
use azemu_core::{Query, Request};
use percent_encoding::percent_decode_str;

use crate::blob::{guard_overwrite, parse_kind};
use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::errors::{blob_not_found, container_not_found, err, ApiError, ApiResult};
use crate::headers::{hash_headers, write_headers};
use crate::pre::INLINE_MAX;
use crate::state::{join, Blob, BlobKind, CopyInfo, Piece, SourceWait};
use crate::store::{install, new_spec};
use crate::util::*;

struct ParsedUrl {
    account: String,
    container: String,
    blob: String,
    query: Query,
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

fn parse_url(url: &str) -> Option<ParsedUrl> {
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let (authority, path_q) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (path, query) = path_q.split_once('?').unwrap_or((path_q, ""));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or(host) } else { host.split(':').next().unwrap_or(host) };
    let labels: Vec<&str> = host.split('.').collect();
    let path = path.trim_start_matches('/');
    let (account, path) = if labels.len() >= 2 && labels[1].eq_ignore_ascii_case("blob") {
        (labels[0].to_ascii_lowercase().trim_end_matches("-secondary").to_string(), path.to_string())
    } else {
        let (a, p) = path.split_once('/').unwrap_or((path, ""));
        (decode(a).to_ascii_lowercase(), p.to_string())
    };
    let (container, blob) = path.split_once('/').unwrap_or((path.as_str(), ""));
    if container.is_empty() || blob.is_empty() {
        return None;
    }
    Some(ParsedUrl { account, container: decode(container), blob: decode(blob), query: Query::parse(query) })
}

fn cannot_verify(status: u16, code: &str, msg: &str) -> ApiError {
    ApiError::new("CannotVerifyCopySource")
        .status(status)
        .msg(msg)
        .detail("CopySourceStatusCode", status.to_string())
        .detail("CopySourceErrorCode", code)
        .detail("CopySourceErrorMessage", msg)
}

/// Find the source blob of a copy: a clone of the record and the URL to remember.
///
/// `cond_kind` decides what a failed `x-ms-source-if-*` condition answers: `Kind::Read` for the
/// "from URL" operations (Put Blob / Put Block / Append Block / Put Page from URL: a failed
/// If-Modified-Since or If-None-Match is a 304, as Azure answers), `Kind::Write` for Copy Blob.
pub fn find_source(cx: &mut Cx, url: &str, cond_kind: Kind) -> Result<(Blob, String), ApiError> {
    let now = cx.now;
    let req = cx.req;
    let p = parse_url(url).ok_or_else(|| ApiError::invalid_header("x-ms-copy-source", url))?;
    let dest_account = cx.acct.clone();
    let st = &mut *cx.st;
    let Some(acct) = st.accounts.get_mut(&p.account) else {
        return Err(if p.account == dest_account {
            cannot_verify(404, "ContainerNotFound", "The specified container does not exist.")
        } else {
            cannot_verify(404, "ResourceNotFound", "The specified resource does not exist.")
        });
    };
    let source_allows_public = acct.public_access_allowed;
    let Some(c) = acct.containers.get_mut(&p.container) else {
        return Err(cannot_verify(404, "ContainerNotFound", "The specified container does not exist."));
    };
    let has_sas = p.query.has("sig") || req.header("x-ms-copy-source-authorization").is_some();
    if p.account != dest_account && !has_sas && !source_allows_public {
        return Err(cannot_verify(409, "PublicAccessNotPermitted", "Public access is not permitted on this storage account."));
    }
    if p.account != dest_account && !has_sas && c.public_access.is_none() {
        return Err(cannot_verify(
            401,
            "NoAuthenticationInformation",
            "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.",
        ));
    }
    let e = c.blobs.get_mut(&p.blob).ok_or_else(|| cannot_verify(404, "BlobNotFound", "The specified blob does not exist."))?;
    refresh_entry(e, now);
    let snapshot = p.query.get("snapshot").filter(|s| !s.is_empty());
    let version = p.query.get("versionid").filter(|s| !s.is_empty());
    let b = e.find(snapshot, version).ok_or_else(|| cannot_verify(404, "BlobNotFound", "The specified blob does not exist."))?;
    // Source conditions.
    cond::check(req, "x-ms-source-", Some((&b.etag, b.last_modified)), cond_kind)?;
    cond::check_tags_header(req, "x-ms-source-if-tags", Some(&b.tags))?;
    // Show the source URL without the SAS.
    let shown = url.split('?').next().unwrap_or(url).to_string();
    let shown = match (snapshot, version) {
        (Some(s), _) => format!("{shown}?snapshot={s}"),
        (None, Some(v)) => format!("{shown}?versionid={v}"),
        _ => shown,
    };
    Ok((b.clone(), shown))
}

/// `x-ms-source-range: bytes=a-b` as `(a, b)` inclusive.
pub fn source_range(req: &Request) -> Result<Option<(u64, u64)>, ApiError> {
    match req.header("x-ms-source-range") {
        None => Ok(None),
        Some(v) => match parse_range(v) {
            Some(RangeSpec::From { start, end: Some(e) }) => Ok(Some((start, e))),
            Some(RangeSpec::From { start, end: None }) => Ok(Some((start, u64::MAX))),
            _ => Err(ApiError::invalid_header("x-ms-source-range", v)),
        },
    }
}

pub struct SourceRead {
    pub data: Bytes,
    pub blob: Blob,
    pub url: String,
}

/// Read (a range of) the source blob. A big source is not joined here, under the lock: the
/// request is parked (`State::wait_source`) and run again once the service has joined and hashed
/// the data (`Pre::prefetch`); the second time the data is found in `cx.pre`.
pub fn read_source(cx: &mut Cx, url: &str, range: Option<(u64, u64)>, _check: bool) -> Result<SourceRead, ApiError> {
    if let Some(p) = &cx.pre.source {
        return Ok(SourceRead { data: p.data.clone(), blob: p.blob.clone(), url: p.url.clone() });
    }
    let (blob, shown) = find_source(cx, url, Kind::Read)?;
    let pieces = match range {
        None => blob.read_pieces(0, blob.size),
        Some((a, b)) => {
            if a >= blob.size && blob.size > 0 || (blob.size == 0 && a > 0) {
                return Err(err("InvalidRange"));
            }
            let end = b.min(blob.size.saturating_sub(1));
            if blob.size == 0 {
                vec![]
            } else {
                blob.read_pieces(a, end - a + 1)
            }
        }
    };
    let total: usize = pieces.iter().map(Piece::len).sum();
    if total > INLINE_MAX {
        cx.st.wait_source = Some(SourceWait { pieces, blob, url: shown });
        // Never sent: the service sees `wait_source` and runs the request again.
        return Err(err("InternalError"));
    }
    Ok(SourceRead { data: join(pieces), blob, url: shown })
}

fn copy_info(url: String, size: u64, now: time::OffsetDateTime) -> CopyInfo {
    CopyInfo {
        id: uuid::Uuid::new_v4().to_string(),
        source: url,
        status: "success".into(),
        progress: format!("{size}/{size}"),
        completed: now,
    }
}

pub fn copy_blob(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_blob_name(name)?;
    let url = hdr(req, "x-ms-copy-source").ok_or_else(|| ApiError::missing_header("x-ms-copy-source"))?;
    let md = parse_metadata(req)?;
    let tags = parse_tags_header(req)?;
    let tag_option_copy = hdr(req, "x-ms-copy-source-tag-option").map(|v| v.eq_ignore_ascii_case("copy")).unwrap_or(false);
    let tier = match hdr(req, "x-ms-access-tier") {
        None => None,
        Some(t) => Some(
            crate::store::valid_tier(BlobKind::Block, t)
                .or_else(|| crate::store::valid_tier(BlobKind::Page, t))
                .ok_or_else(|| ApiError::invalid_header("x-ms-access-tier", t))?,
        ),
    };
    let seal = hdr(req, "x-ms-seal-blob").map(|v| v.eq_ignore_ascii_case("true"));
    let (src, shown) = find_source(cx, url, Kind::Write)?;
    let spec = new_spec(req, src.kind, false)?;

    let mut nb = src.clone();
    nb.etag = azemu_core::etag::new_etag();
    nb.last_modified = now;
    nb.creation_time = now;
    nb.lease = Default::default();
    nb.snapshot = None;
    nb.deleted = None;
    nb.legal_hold = spec.legal_hold;
    nb.immutability = spec.immutability;
    nb.tier = tier.clone();
    nb.tier_changed = tier.as_ref().map(|_| now);
    nb.archive_status = None;
    nb.rehydrate_priority = None;
    nb.dfs = None;
    if !md.is_empty() {
        nb.metadata = md;
    }
    // Tags are replaced by the request's tags unless the request asks to copy them.
    if !tag_option_copy {
        nb.tags = tags;
    }
    if src.kind == BlobKind::Append {
        nb.sealed = seal.unwrap_or(src.sealed);
    }
    nb.copy = Some(copy_info(shown, src.size, now));

    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    let entry = c.blobs.entry(name.to_string()).or_default();
    if let Err(e) = guard_overwrite(entry, req, now) {
        c.prune(name);
        return Err(e);
    }
    let copy_id = nb.copy.as_ref().map(|c| c.id.clone()).unwrap_or_default();
    install(&mut ctl, entry, nb, now);
    let b = entry.current.as_ref().ok_or_else(blob_not_found)?;
    Ok(write_headers(Resp::new(202u16), b).header("x-ms-copy-id", copy_id).header("x-ms-copy-status", "success").build())
}

pub fn put_blob_from_url(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    validate_blob_name(name)?;
    let url = hdr(req, "x-ms-copy-source").ok_or_else(|| ApiError::missing_header("x-ms-copy-source"))?;
    if parse_kind(req)? != BlobKind::Block {
        return Err(ApiError::invalid_header("x-ms-blob-type", hdr(req, "x-ms-blob-type").unwrap_or("")));
    }
    let keep_props = hdr(req, "x-ms-copy-source-blob-properties").map(|v| !v.eq_ignore_ascii_case("false")).unwrap_or(true);
    let spec = new_spec(req, BlobKind::Block, false)?;
    let SourceRead { data, blob: src, url: shown } = read_source(cx, url, None, true)?;
    if let Some(want) = request_md5(req, "content-md5")? {
        if want != cx.pre.md5(&data) {
            return Err(err("Md5Mismatch"));
        }
    }
    if let Some(want) = request_md5(req, "x-ms-source-content-md5")? {
        if want != cx.pre.md5(&data) {
            return Err(err("Md5Mismatch"));
        }
    }
    let (m, crc) = (cx.pre.md5(&data), cx.pre.crc64(&data));
    let mut nb = spec.build(BlobKind::Block, now);
    if keep_props {
        // The source's properties are the start; each one the request names itself replaces the source's (recording
        // `test_block_blob.pyTestStorageBlockBlobtest_upload_blob_from_url_overwrite_properties.json`, entry 3: the source says
        // `spanish`, the request names `english` and no other property, and the copy answers `english` with the source's content type).
        let given = nb.props.clone();
        nb.props = src.props.clone();
        if hdr(req, "x-ms-blob-content-type").is_some() {
            nb.props.content_type = given.content_type;
        }
        if hdr(req, "x-ms-blob-content-encoding").is_some() {
            nb.props.content_encoding = given.content_encoding;
        }
        if hdr(req, "x-ms-blob-content-language").is_some() {
            nb.props.content_language = given.content_language;
        }
        if hdr(req, "x-ms-blob-content-disposition").is_some() {
            nb.props.content_disposition = given.content_disposition;
        }
        if hdr(req, "x-ms-blob-cache-control").is_some() {
            nb.props.cache_control = given.cache_control;
        }
    }
    nb.props.content_md5 = Some(m.clone());
    nb.crc64 = Some(crc);
    nb.set_content(data);
    nb.copy = Some(copy_info(shown, nb.size, now));
    let copy_id = nb.copy.as_ref().map(|c| c.id.clone()).unwrap_or_default();

    let (containers, mut ctl) = cx.account().split();
    let c = containers.get_mut(container).ok_or_else(container_not_found)?;
    let entry = c.blobs.entry(name.to_string()).or_default();
    if let Err(e) = guard_overwrite(entry, req, now) {
        c.prune(name);
        return Err(e);
    }
    install(&mut ctl, entry, nb, now);
    let b = entry.current.as_ref().ok_or_else(blob_not_found)?;
    Ok(hash_headers(write_headers(Resp::new(201u16), b), &m, crc)
        .header("x-ms-copy-id", copy_id)
        .header("x-ms-copy-status", "success")
        .build())
}

pub fn abort_copy(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    req.query.get("copyid").ok_or_else(|| ApiError::new("MissingRequiredQueryParameter").detail("QueryParameterName", "copyid"))?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let _ = e.live().ok_or_else(blob_not_found)?;
    // Copies finish immediately here, so there is never one to abort.
    Err(err("NoPendingCopyOperation"))
}
