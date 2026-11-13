//! Append blobs: Append Block, Append Block From URL, Seal.

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time;

use crate::cond::{self, Kind};
use crate::copy::{read_source, source_range, SourceRead};
use crate::cx::{refresh_entry, Cx};
use crate::errors::{blob_not_found, err, ApiError, ApiResult};
use crate::headers::hash_headers;
use crate::lease::{self, Scope};
use crate::state::{BlobKind, Seg};
use crate::store::{check_container_scope, check_cpk, check_mutable};
use crate::util::*;

const MAX_BLOCK: usize = 100 * 1024 * 1024;

fn do_append(cx: &mut Cx, container: &str, name: &str, data: Bytes) -> ApiResult {
    let req = cx.req;
    let pre = cx.pre;
    let now = cx.now;
    if data.is_empty() {
        return Err(ApiError::invalid_header("Content-Length", "0"));
    }
    if data.len() > MAX_BLOCK {
        return Err(err("RequestBodyTooLarge"));
    }
    let max_size = hdr_u64(req, "x-ms-blob-condition-maxsize")?;
    let append_pos = hdr_u64(req, "x-ms-blob-condition-appendpos")?;
    let c = cx.container(container)?;
    check_container_scope(c, req)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    if b.kind != BlobKind::Append {
        return Err(err("InvalidBlobType"));
    }
    check_cpk(b, req)?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    check_mutable(b, now)?;
    if b.sealed {
        return Err(err("BlobIsSealed"));
    }
    if let Some(m) = max_size {
        if b.size + data.len() as u64 > m {
            return Err(err("MaxBlobSizeConditionNotMet"));
        }
    }
    if let Some(p) = append_pos {
        if b.size != p {
            return Err(err("AppendPositionConditionNotMet"));
        }
    }
    if b.segs.len() >= 50_000 {
        return Err(err("BlockCountExceedsLimit"));
    }
    let offset = b.size;
    let (m, crc) = (pre.md5(&data), pre.crc64(&data));
    b.size += data.len() as u64;
    b.segs.push(Seg { id: None, data });
    b.crc64 = None;
    b.props.content_md5 = None;
    b.touch(now);
    let r = hash_headers(Resp::new(201u16), &m, crc)
        .header("ETag", &b.etag)
        .header("Last-Modified", time::rfc1123(b.last_modified))
        .header("x-ms-blob-append-offset", offset)
        .header("x-ms-blob-committed-block-count", b.block_count())
        .header("x-ms-request-server-encrypted", "true");
    Ok(r.build())
}

pub fn append_block(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    check_body_hashes(req, &req.body, cx.pre)?;
    do_append(cx, container, name, req.body.clone())
}

pub fn append_block_from_url(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let url = hdr(req, "x-ms-copy-source").ok_or_else(|| ApiError::missing_header("x-ms-copy-source"))?;
    let range = source_range(req)?;
    let SourceRead { data, .. } = read_source(cx, url, range, true)?;
    if let Some(want) = request_md5(req, "x-ms-source-content-md5")? {
        if want != cx.pre.md5(&data) {
            return Err(err("Md5Mismatch"));
        }
    }
    do_append(cx, container, name, data)
}

pub fn seal(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let append_pos = hdr_u64(req, "x-ms-blob-condition-appendpos")?;
    let c = cx.container(container)?;
    let e = c.blobs.get_mut(name).ok_or_else(blob_not_found)?;
    refresh_entry(e, now);
    let b = e.live_mut().ok_or_else(blob_not_found)?;
    if b.kind != BlobKind::Append {
        return Err(err("InvalidBlobType"));
    }
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    cond::check_tags(req, Some(&b.tags))?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    if let Some(p) = append_pos {
        if b.size != p {
            return Err(err("AppendPositionConditionNotMet"));
        }
    }
    b.sealed = true;
    b.touch(now);
    Ok(Resp::new(200u16)
        .header("ETag", &b.etag)
        .header("Last-Modified", time::rfc1123(b.last_modified))
        .header("x-ms-blob-sealed", "true")
        .build())
}
