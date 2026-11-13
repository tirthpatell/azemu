//! Path Update: Append Data, Flush Data, Set Properties.

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::Request;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::dfs::errors::dfs_error;
use crate::dfs::filesystem::open;
use crate::dfs::model::*;
use crate::dfs::path::content_headers;
use crate::dfs::util::{http_date, parse_properties};
use crate::errors::{err, ApiError, ApiResult};
use crate::lease::{self, Action, Scope};
use crate::state::{Blob, BlobKind, Seg, Time};
use crate::store::{check_cpk, check_mutable};
use crate::util::*;

fn need_file(name: &str) -> Result<(), ApiError> {
    if name.is_empty() {
        return Err(path_conflict());
    }
    Ok(())
}

fn position(req: &Request) -> Result<u64, ApiError> {
    match req.query.get("position") {
        None | Some("") => Err(err("MissingRequiredQueryParameter").detail("QueryParameterName", "position")),
        Some(v) => v.trim().parse::<u64>().map_err(|_| ApiError::invalid_query("position", v)),
    }
}

fn flag(req: &Request, name: &str) -> Result<bool, ApiError> {
    match req.query.get(name) {
        None | Some("") => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
        Some(v) => Err(ApiError::invalid_query(name, v)),
    }
}

/// `x-ms-lease-action` of Append and Flush. Returns the `x-ms-lease-renewed` value for
/// `auto-renew`.
fn lease_action(b: &mut Blob, req: &Request, now: Time) -> Result<Option<&'static str>, ApiError> {
    let Some(a) = hdr(req, "x-ms-lease-action") else { return Ok(None) };
    let lease_id = hdr(req, "x-ms-lease-id");
    match a.to_ascii_lowercase().as_str() {
        "acquire" => {
            let act = Action {
                action: "acquire",
                lease_id: None,
                proposed: hdr(req, "x-ms-proposed-lease-id"),
                duration: hdr_i64(req, "x-ms-lease-duration")?,
                break_period: None,
            };
            lease::apply(&mut b.lease, &act, now, Scope::Blob)?;
            Ok(None)
        }
        "auto-renew" => {
            let act = Action { action: "renew", lease_id, proposed: None, duration: None, break_period: None };
            lease::apply(&mut b.lease, &act, now, Scope::Blob)?;
            Ok(Some("false"))
        }
        "release" => {
            let act = Action { action: "release", lease_id, proposed: None, duration: None, break_period: None };
            lease::apply(&mut b.lease, &act, now, Scope::Blob)?;
            Ok(None)
        }
        "acquire-release" => Ok(None),
        _ => Err(ApiError::invalid_header("x-ms-lease-action", a)),
    }
}

/// The file named by the request, with its lease up to date, in an open filesystem.
fn file_of<'a>(c: &'a mut crate::state::Container, name: &str, now: Time) -> Result<&'a mut Blob, ApiError> {
    let e = c.blobs.get_mut(name).ok_or_else(|| dfs_error("PathNotFound"))?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(|| dfs_error("PathNotFound"))?;
    if is_dir(b) {
        return Err(path_conflict());
    }
    if b.kind != BlobKind::Block {
        return Err(err("InvalidBlobType"));
    }
    Ok(b)
}

pub fn append(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    need_file(name)?;
    let pos = position(req)?;
    let flush_now = flag(req, "flush")?;
    let close = flag(req, "close")?;
    let body: Bytes = req.body.clone();
    let (md5, crc) = check_body_hashes(req, &body, cx.pre)?;
    let c = open(cx, fs)?;
    let b = file_of(c, name, now)?;
    check_cpk(b, req)?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    check_mutable(b, now)?;
    let len = body.len() as u64;
    let end = pos.checked_add(len).ok_or_else(|| ApiError::invalid_query("position", &pos.to_string()))?;
    if len > 0 {
        if let Some(d) = b.dfs.as_mut() {
            d.pending.insert(pos, body);
        }
    }
    if flush_now {
        flush_data(b, end, flag(req, "retainUncommittedData")?, close, req, now)?;
        // An append that flushes takes the `Content-Type` of its request as the file's, when the file has none of its own yet: the
        // recordings read `application/json` back after such an append with that type, and `application/octet-stream` after one with
        // that type (`test_file_async.pyTestFiletest_read_file_read.json` entries 2 and 3, `..._flush_data_with_bool.json`).
        if content_headers(req).is_none() && b.props.content_type == "application/octet-stream" {
            if let Some(ct) = hdr(req, "content-type").filter(|v| !v.is_empty()) {
                b.props.content_type = ct.to_string();
            }
        }
    }
    let renewed = lease_action(b, req, now)?;
    let mut r = Resp::new(202u16).header("x-ms-request-server-encrypted", "true");
    if req.header("content-md5").is_some() {
        r = r.header("Content-MD5", b64(&md5));
    }
    if req.header("x-ms-content-crc64").is_some() {
        r = r.header("x-ms-content-crc64", crc64_b64(crc));
    }
    if flush_now {
        r = r.header("ETag", &b.etag).header("Last-Modified", http_date(b.last_modified));
    }
    if let Some(k) = &b.cpk_sha256 {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    if let Some(v) = renewed {
        r = r.header("x-ms-lease-renewed", v);
    }
    Ok(r.build())
}

/// Commit appended data up to `position` (the length the file will have). `x-ms-content-*`
/// headers set the content properties.
fn flush_data(b: &mut Blob, position: u64, retain: bool, _close: bool, req: &Request, now: Time) -> Result<(), ApiError> {
    let committed = b.size;
    let pending = b.dfs.as_mut().map(|d| std::mem::take(&mut d.pending)).unwrap_or_default();
    let restore = |b: &mut Blob, p| {
        if let Some(d) = b.dfs.as_mut() {
            d.pending = p;
        }
    };
    let bad = || dfs_error("InvalidFlushPosition");
    if position < committed {
        restore(b, pending);
        return Err(bad());
    }
    // The chunks that belong to the new content, in order, and whether they just extend the file.
    let mut cur = committed;
    let mut extends = true;
    let mut taken: Vec<(u64, Bytes)> = vec![];
    for (off, data) in pending.iter().filter(|(o, _)| **o < position) {
        let end = off.saturating_add(data.len() as u64);
        if end > position {
            restore(b, pending.clone());
            return Err(bad());
        }
        if *off > cur {
            restore(b, pending.clone());
            return Err(bad());
        }
        if *off < cur {
            extends = false;
        }
        cur = cur.max(end);
        taken.push((*off, data.clone()));
    }
    if cur < position {
        restore(b, pending);
        return Err(bad());
    }
    if extends {
        for (_, data) in &taken {
            b.segs.push(Seg { id: None, data: data.clone() });
        }
        b.size = position;
    } else {
        let mut buf = b.read_all().to_vec();
        buf.resize(position as usize, 0);
        for (off, data) in &taken {
            buf[*off as usize..*off as usize + data.len()].copy_from_slice(data);
        }
        b.set_content(Bytes::from(buf));
    }
    if let Some(d) = b.dfs.as_mut() {
        d.pending = if retain { pending.into_iter().filter(|(o, _)| *o >= position).collect() } else { Default::default() };
    }
    if let Some(p) = content_headers(req) {
        b.props = p;
    }
    b.props.content_md5 = match hdr(req, "x-ms-content-md5") {
        Some(v) => b64_decode(v),
        None => None,
    };
    b.crc64 = None;
    b.touch(now);
    Ok(())
}

pub fn flush(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    need_file(name)?;
    let pos = position(req)?;
    let retain = flag(req, "retainUncommittedData")?;
    let close = flag(req, "close")?;
    let c = open(cx, fs)?;
    let b = file_of(c, name, now)?;
    check_cpk(b, req)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    check_mutable(b, now)?;
    flush_data(b, pos, retain, close, req, now)?;
    let renewed = lease_action(b, req, now)?;
    let encrypted = if b.cpk_sha256.is_some() { "true" } else { "false" };
    let mut r = Resp::new(200u16)
        .header("ETag", &b.etag)
        .header("Last-Modified", http_date(b.last_modified))
        .header("x-ms-request-server-encrypted", encrypted);
    if let Some(k) = &b.cpk_sha256 {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    if let Some(v) = renewed {
        r = r.header("x-ms-lease-renewed", v);
    }
    Ok(r.build())
}

pub fn set_properties(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    if name.is_empty() {
        return Err(path_conflict());
    }
    let metadata = parse_properties(req)?.unwrap_or_default();
    let content = content_headers(req);
    let c = open(cx, fs)?;
    let e = c.blobs.get_mut(name).ok_or_else(|| dfs_error("PathNotFound"))?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(|| dfs_error("PathNotFound"))?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    let dir = is_dir(b);
    b.metadata = metadata;
    if dir {
        b.metadata.push((FOLDER_KEY.to_string(), "true".to_string()));
        b.metadata.sort();
    }
    if let Some(p) = content {
        let md5 = b.props.content_md5.take();
        b.props = p;
        b.props.content_md5 = md5;
    }
    b.touch(now);
    Ok(Resp::new(200u16).header("ETag", &b.etag).header("Last-Modified", http_date(b.last_modified)).build())
}
