//! Page blobs: Put Page (update / clear), Put Page From URL, Get Page Ranges.

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time;

use crate::cond::{self, Kind};
use crate::copy::{read_source, source_range, SourceRead};
use crate::cx::{refresh_entry, snapshot_param, Cx};
use crate::errors::{blob_not_found, err, ApiError, ApiResult};
use crate::headers::hash_headers;
use crate::lease::{self, Scope};
use crate::state::{Blob, BlobKind};
use crate::store::{check_container_scope, check_cpk, check_mutable};
use crate::util::*;
use crate::xml::Out;

const PAGE: u64 = 512;
const MAX_PUT: u64 = 4 * 1024 * 1024;

/// The range of a page write: `x-ms-range`, else `Range` (the REST docs: "either the Range header or the
/// x-ms-range header is required"; `x-ms-range` wins when both are sent). Aligned, inclusive.
fn page_range(req: &azemu_core::Request) -> Result<(u64, u64), ApiError> {
    let (header, v) = match (hdr(req, "x-ms-range"), hdr(req, "range")) {
        (Some(v), _) => ("x-ms-range", v),
        (None, Some(v)) => ("Range", v),
        (None, None) => return Err(ApiError::missing_header("x-ms-range")),
    };
    match parse_range(v) {
        Some(RangeSpec::From { start, end: Some(end) }) => {
            if end >= (1 << 50) || start % PAGE != 0 || (end + 1) % PAGE != 0 {
                return Err(err("InvalidPageRange"));
            }
            Ok((start, end))
        }
        _ => Err(ApiError::invalid_header(header, v)),
    }
}

fn check_sequence(req: &azemu_core::Request, b: &Blob) -> Result<(), ApiError> {
    let fail = || err("SequenceNumberConditionNotMet");
    if let Some(v) = hdr_u64(req, "x-ms-if-sequence-number-le")? {
        if b.sequence > v {
            return Err(fail());
        }
    }
    if let Some(v) = hdr_u64(req, "x-ms-if-sequence-number-lt")? {
        if b.sequence >= v {
            return Err(fail());
        }
    }
    if let Some(v) = hdr_u64(req, "x-ms-if-sequence-number-eq")? {
        if b.sequence != v {
            return Err(fail());
        }
    }
    Ok(())
}

fn write_pages(cx: &mut Cx, container: &str, name: &str, range: (u64, u64), data: Option<Bytes>) -> ApiResult {
    let req = cx.req;
    let pre = cx.pre;
    let now = cx.now;
    let (start, end) = range;
    let c = cx.container(container)?;
    check_container_scope(c, req)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    if b.kind != BlobKind::Page {
        return Err(err("InvalidBlobType"));
    }
    check_cpk(b, req)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    check_mutable(b, now)?;
    check_sequence(req, b)?;
    if end >= b.size {
        return Err(err("InvalidPageRange"));
    }
    let mut hashes = None;
    match data {
        Some(d) => {
            hashes = Some((pre.md5(&d), pre.crc64(&d)));
            b.pages.write(start, d);
        }
        None => b.pages.clear(start, end + 1),
    }
    b.touch(now);
    let mut r = Resp::new(201u16)
        .header("ETag", &b.etag)
        .header("Last-Modified", time::rfc1123(b.last_modified))
        .header("x-ms-blob-sequence-number", b.sequence)
        .header("x-ms-request-server-encrypted", "true");
    if let Some((m, crc)) = hashes {
        r = hash_headers(r, &m, crc);
    }
    Ok(r.build())
}

pub fn put_page(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let action = hdr(req, "x-ms-page-write").ok_or_else(|| ApiError::missing_header("x-ms-page-write"))?;
    let range = page_range(req)?;
    let len = range.1 - range.0 + 1;
    match action.to_ascii_lowercase().as_str() {
        "update" => {
            if len > MAX_PUT {
                return Err(err("RequestBodyTooLarge"));
            }
            if req.body.len() as u64 != len {
                return Err(ApiError::invalid_header("Content-Length", &req.body.len().to_string()));
            }
            check_body_hashes(req, &req.body, cx.pre)?;
            write_pages(cx, container, name, range, Some(req.body.clone()))
        }
        "clear" => {
            if !req.body.is_empty() {
                return Err(ApiError::invalid_header("Content-Length", &req.body.len().to_string()));
            }
            write_pages(cx, container, name, range, None)
        }
        _ => Err(ApiError::invalid_header("x-ms-page-write", action)),
    }
}

pub fn put_page_from_url(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let url = hdr(req, "x-ms-copy-source").ok_or_else(|| ApiError::missing_header("x-ms-copy-source"))?;
    let range = page_range(req)?;
    let src_range = source_range(req)?.ok_or_else(|| ApiError::missing_header("x-ms-source-range"))?;
    let len = range.1 - range.0 + 1;
    if src_range.1.saturating_sub(src_range.0).saturating_add(1) != len {
        return Err(ApiError::invalid_header("x-ms-source-range", hdr(req, "x-ms-source-range").unwrap_or("")));
    }
    let SourceRead { data, .. } = read_source(cx, url, Some(src_range), true)?;
    if let Some(want) = request_md5(req, "x-ms-source-content-md5")? {
        if want != cx.pre.md5(&data) {
            return Err(err("Md5Mismatch"));
        }
    }
    if data.len() as u64 != len {
        return Err(err("InvalidRange"));
    }
    write_pages(cx, container, name, range, Some(data))
}

/// Compare two page blobs' content and return `(changed, cleared)` ranges inside `[from, to]`.
type Ranges = Vec<(u64, u64)>;

fn diff(cur: &Blob, prev: &Blob, from: u64, to: u64) -> (Ranges, Ranges) {
    let a = cur.pages.ranges(from, to);
    let p = prev.pages.ranges(from, to);
    let mut changed: Vec<(u64, u64)> = vec![];
    let push = |v: &mut Vec<(u64, u64)>, s: u64, e: u64| match v.last_mut() {
        Some(l) if l.1 + 1 == s => l.1 = e,
        _ => v.push((s, e)),
    };
    // Changed: written now and different from (or absent in) the previous snapshot.
    for (s, e) in &a {
        let mut off = *s;
        while off <= *e {
            let n = PAGE.min(*e - off + 1);
            let now_data = cur.pages.read(off, n);
            let was_valid = !prev.pages.ranges(off, off + n - 1).is_empty();
            if !was_valid || prev.pages.read(off, n) != now_data {
                push(&mut changed, off, off + n - 1);
            }
            off += n;
        }
    }
    // Cleared: written in the previous snapshot, not written now.
    let mut cleared: Vec<(u64, u64)> = vec![];
    for (s, e) in &p {
        let mut off = *s;
        while off <= *e {
            let n = PAGE.min(*e - off + 1);
            if cur.pages.ranges(off, off + n - 1).is_empty() {
                push(&mut cleared, off, off + n - 1);
            }
            off += n;
        }
    }
    (changed, cleared)
}

pub fn get_page_ranges(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let snapshot = snapshot_param(req)?;
    let prev = req.query.get("prevsnapshot").filter(|s| !s.is_empty()).map(str::to_string);
    let window = request_range(req, "x-ms-range")?;
    let marker: Option<u64> = req.query.get("marker").filter(|s| !s.is_empty()).and_then(|s| s.parse().ok());
    let max = crate::list::max_results(req)?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.find(snapshot.as_deref(), None).ok_or_else(blob_not_found)?;
    if b.kind != BlobKind::Page {
        return Err(err("InvalidBlobType"));
    }
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Read)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_read(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    let (from, to) = match window {
        Some(w) => match resolve_range(w, b.size) {
            Some(r) => r,
            None if b.size == 0 => (0, 0),
            None => return Err(err("InvalidRange")),
        },
        None => (0, b.size.saturating_sub(1)),
    };
    let (changed, cleared) = if let Some(p) = &prev {
        let pb = e.snapshots.get(p).ok_or_else(|| err("PreviousSnapshotNotFound"))?;
        diff(b, pb, from, to)
    } else if b.size == 0 {
        (vec![], vec![])
    } else {
        (b.pages.ranges(from, to), vec![])
    };
    // Paging.
    let mut all: Vec<(bool, u64, u64)> =
        changed.iter().map(|r| (false, r.0, r.1)).chain(cleared.iter().map(|r| (true, r.0, r.1))).collect();
    all.sort_by_key(|x| x.1);
    let all: Vec<_> = all.into_iter().filter(|x| marker.map(|m| x.1 >= m).unwrap_or(true)).collect();
    let next = all.get(max).map(|x| x.1);
    let mut o = Out::new();
    o.open("PageList");
    for (is_clear, s, en) in all.iter().take(max) {
        let tag = if *is_clear { "ClearRange" } else { "PageRange" };
        o.open(tag).el("Start", &s.to_string()).el("End", &en.to_string()).close(tag);
    }
    if req.query.has("maxresults") || req.query.has("marker") {
        o.el("NextMarker", &next.map(|n| n.to_string()).unwrap_or_default());
    }
    o.close("PageList");
    Ok(Resp::new(200u16)
        .header("ETag", &b.etag)
        .header("Last-Modified", time::rfc1123(b.last_modified))
        .header("x-ms-blob-content-length", b.size)
        .xml(o.finish())
        .build())
}
