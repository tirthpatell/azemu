//! Soft-deleted paths: List Blobs `showonly=deleted` and Undelete.

use azemu_core::resp::Resp;
use azemu_core::time::{self, Duration};
use percent_encoding::percent_decode_str;

use crate::cx::Cx;
use crate::dfs::errors::dfs_error;
use crate::dfs::model::*;
use crate::errors::{ApiError, ApiResult};
use crate::list::{max_results, remaining_days, service_endpoint};
use crate::util::{b64, b64_decode, hdr};
use crate::xml::Out;

fn days(cx: &mut Cx) -> u32 {
    cx.account().props.soft_delete_days().unwrap_or(7)
}

/// List Blobs with `showonly=deleted` on a hierarchical container.
pub fn list_deleted(cx: &mut Cx, container: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let prefix = req.query.get("prefix").unwrap_or("").to_string();
    let max = max_results(req)?;
    let marker = req.query.get("marker").unwrap_or("").to_string();
    let after = if marker.is_empty() {
        None
    } else {
        let raw = b64_decode(&marker).ok_or_else(|| ApiError::invalid_query("marker", &marker))?;
        Some(String::from_utf8_lossy(&raw).to_string())
    };
    let keep = days(cx);
    let endpoint = service_endpoint(req);
    let c = cx.container(container)?;
    let mut items: Vec<&DeletedPath> = c
        .deleted_paths
        .iter()
        .filter(|d| d.name.starts_with(&prefix) && d.deleted_at + Duration::days(keep as i64) > now)
        .filter(|d| after.as_deref().map(|a| d.name.as_str() > a).unwrap_or(true))
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name).then(a.deletion_id.cmp(&b.deletion_id)));
    let next = if items.len() > max { Some(items[max - 1].name.clone()) } else { None };
    items.truncate(max);

    let mut o = Out::new();
    o.open_attrs("EnumerationResults", &[("ServiceEndpoint", &endpoint), ("ContainerName", container)]);
    if !prefix.is_empty() {
        o.el("Prefix", &prefix);
    }
    if !marker.is_empty() {
        o.el("Marker", &marker);
    }
    if req.query.has("maxresults") {
        o.el("MaxResults", &max.to_string());
    }
    o.open("Blobs");
    for d in items {
        let b = &d.blob;
        let expires = d.deleted_at + Duration::days(keep as i64);
        o.open("Blob").el("Name", &d.name).el("DeletionId", &d.deletion_id).el("Deleted", "true").open("Properties");
        o.el("Creation-Time", &time::rfc1123(b.creation_time))
            .el("Last-Modified", &time::rfc1123(b.last_modified))
            .el("Expiry-Time", &time::rfc1123(expires))
            .el("Etag", crate::util::unquote_etag(&b.etag))
            .el("ResourceType", if is_dir(b) { "directory" } else { "file" })
            .el("Content-Length", &b.size.to_string())
            .el("Content-Type", &b.props.content_type)
            .el("Content-Encoding", b.props.content_encoding.as_deref().unwrap_or(""))
            .el("Content-Language", b.props.content_language.as_deref().unwrap_or(""))
            .el("Content-CRC64", "")
            .el("Content-MD5", "")
            .el("Cache-Control", b.props.cache_control.as_deref().unwrap_or(""))
            .el("Content-Disposition", b.props.content_disposition.as_deref().unwrap_or(""))
            .el("BlobType", "BlockBlob")
            .el("AccessTier", "Hot")
            .el("AccessTierInferred", "true")
            .el("ServerEncrypted", "true")
            .el("DeletedTime", &time::rfc1123(d.deleted_at))
            .el("RemainingRetentionDays", &remaining_days(d.deleted_at, keep, now).to_string());
        o.close("Properties").empty("OrMetadata").close("Blob");
    }
    o.close("Blobs");
    o.el("NextMarker", &next.as_deref().map(|n| b64(n.as_bytes())).unwrap_or_default());
    o.close("EnumerationResults");
    Ok(Resp::new(200u16).xml(o.finish()).build())
}

/// `PUT ?comp=undelete` with `x-ms-undelete-source: {path}?deletionid={id}`.
pub fn undelete(cx: &mut Cx, container: &str, name: &str) -> ApiResult {
    let now = cx.now;
    let source = hdr(cx.req, "x-ms-undelete-source").ok_or_else(|| ApiError::missing_header("x-ms-undelete-source"))?;
    let id = source
        .split_once("deletionid=")
        .map(|(_, i)| i.split('&').next().unwrap_or(i).to_string())
        .ok_or_else(|| ApiError::invalid_header("x-ms-undelete-source", source))?;
    let src_path = norm(&percent_decode_str(source.split('?').next().unwrap_or("")).decode_utf8_lossy());
    let target = if name.is_empty() { src_path.clone() } else { name.to_string() };
    let keep = days(cx);
    let c = cx.container(container)?;
    let pos = c
        .deleted_paths
        .iter()
        .position(|d| {
            d.deletion_id == id && (src_path.is_empty() || d.name == src_path) && d.deleted_at + Duration::days(keep as i64) > now
        })
        .ok_or_else(|| dfs_error("PathNotFound"))?;
    // What comes back is put under the name it was deleted with (`d.name`) and the names of everything that was below it, so
    // those are the names that must be free. (The name in the URL used to be the only one checked: a request that named another
    // path restored over a live file of the original name.)
    let d = &c.deleted_paths[pos];
    if live(c, &target).is_some() || live(c, &d.name).is_some() || d.children.iter().any(|(n, _)| live(c, n).is_some()) {
        return Err(dfs_error("PathAlreadyExists"));
    }
    let restore = d.name.clone();
    ensure_parents(c, &restore, now)?;
    let d = c.deleted_paths.remove(pos);
    let (creation, kind) = (d.blob.creation_time, if is_dir(&d.blob) { "directory" } else { "file" });
    c.blobs.entry(d.name.clone()).or_default().current = Some(d.blob);
    for (n, b) in d.children {
        c.blobs.entry(n).or_default().current = Some(b);
    }
    Ok(Resp::new(200u16).header("x-ms-creation-time", time::rfc1123(creation)).header("x-ms-resource-type", kind).build())
}
