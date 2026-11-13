//! Rename: `PUT /{fs}/{dest}` with `x-ms-rename-source`. A file or a directory with
//! everything below it moves in one step (the state is behind one lock), across filesystems.

use azemu_core::resp::Resp;
use percent_encoding::percent_decode_str;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::dfs::errors::dfs_error;
use crate::dfs::filesystem::open;
use crate::dfs::model::*;
use crate::dfs::util::http_date;
use crate::errors::{err, ApiError, ApiResult};
use crate::lease::{self, Scope};
use crate::state::BlobEntry;
use crate::util::hdr;

/// `/{filesystem}/{path}` (possibly percent-encoded, with a SAS after `?`, or a full URL)
/// into (filesystem, normalized path).
pub(crate) fn parse_source(raw: &str) -> Result<(String, String), ApiError> {
    let no_query = raw.split('?').next().unwrap_or(raw);
    let path = match no_query.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|i| &rest[i..]).unwrap_or("/"),
        None => no_query,
    };
    let decoded = percent_decode_str(path).decode_utf8_lossy().to_string();
    let t = decoded.trim_start_matches('/');
    let (fs, rest) = t.split_once('/').unwrap_or((t, ""));
    if fs.is_empty() {
        return Err(dfs_error("InvalidSourceUri"));
    }
    Ok((fs.to_string(), norm(rest)))
}

pub fn rename(cx: &mut Cx, dst_fs: &str, dst: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    if dst.is_empty() {
        return Err(err("InvalidResourceName").msg("The root directory cannot be a rename destination."));
    }
    valid_name(dst)?;
    match req.query.get("mode") {
        None | Some("") => {}
        Some(m) if m.eq_ignore_ascii_case("legacy") || m.eq_ignore_ascii_case("posix") => {}
        Some(m) => return Err(ApiError::invalid_query("mode", m)),
    }
    let (src_fs, src) = parse_source(hdr(req, "x-ms-rename-source").unwrap_or(""))?;
    let src_lease = hdr(req, "x-ms-source-lease-id").filter(|s| !s.is_empty());

    // Both filesystems become hierarchical and are brought up to date.
    open(cx, dst_fs)?;
    if !cx.account().containers.contains_key(&src_fs) {
        return Err(dfs_error("SourcePathNotFound"));
    }
    open(cx, &src_fs)?;
    if src.is_empty() {
        return Err(dfs_error("SourcePathNotFound").status(400).msg("The root directory cannot be renamed."));
    }
    if src_fs == dst_fs && (dst == src || dst.starts_with(&format!("{src}/"))) {
        if dst == src {
            let c = cx.account().containers.get(&src_fs).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
            let b = live(c, &src).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
            return Ok(Resp::new(201u16).header("ETag", &b.etag).header("Last-Modified", http_date(b.last_modified)).build());
        }
        return Err(dfs_error("InvalidRenameSourcePath"));
    }

    let acct = cx.account();
    // Checks first, with shared borrows only.
    let (src_dir, etag, lm) = {
        let sc = acct.containers.get_mut(&src_fs).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
        if let Some(e) = sc.blobs.get_mut(&src) {
            refresh_entry(e, now);
        }
        let b = live(sc, &src).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
        cond::check(req, "x-ms-source-", Some((&b.etag, b.last_modified)), Kind::Write).map_err(|_| err("SourceConditionNotMet"))?;
        lease::check_write(&b.lease, src_lease, Scope::Blob)?;
        // The source disappears from its old name: nothing under a legal hold or an immutability policy can be moved away
        // (the Blob endpoint refuses to delete it with the same errors).
        crate::store::check_mutable(b, now)?;
        if is_dir(b) {
            for n in below(sc, &src) {
                if let Some(x) = live(sc, &n) {
                    crate::store::check_mutable(x, now)?;
                }
            }
        }
        (is_dir(b), b.etag.clone(), b.last_modified)
    };
    let dc = acct.containers.get_mut(dst_fs).ok_or_else(|| dfs_error("PathNotFound"))?;
    if let Some(e) = dc.blobs.get_mut(dst) {
        refresh_entry(e, now);
    }
    let dest = live(dc, dst).map(|b| (is_dir(b), b.etag.clone(), b.last_modified));
    // Rename is a Path - Create: `If-None-Match: *` on an existing destination is 409 PathAlreadyExists.
    cond::check(req, "", dest.as_ref().map(|(_, e, lm)| (e.as_str(), *lm)), Kind::Create)?;
    // An existing destination is replaced: its lease needs the lease id of the request (`x-ms-lease-id`, as for Path - Create),
    // and a legal hold or an immutability policy forbids the replacement.
    if let Some(b) = live(dc, dst) {
        lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
        crate::store::check_mutable(b, now)?;
    }
    if let Some((dest_dir, ..)) = &dest {
        if *dest_dir != src_dir {
            return Err(dfs_error("InvalidSourceOrDestinationResourceType"));
        }
        if *dest_dir && has_children(dc, dst) {
            return Err(dfs_error("PathAlreadyExists"));
        }
    }
    // The parent of the destination has to exist (a rename does not create directories).
    let parent = parent_of(dst);
    if !parent.is_empty() {
        match live(dc, parent) {
            Some(p) if is_dir(p) => {}
            Some(_) => return Err(path_conflict()),
            None => return Err(dfs_error("RenameDestinationParentPathNotFound")),
        }
    }

    // Move.
    let names: Vec<String> = {
        let sc = acct.containers.get(&src_fs).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
        let mut v = vec![src.clone()];
        if src_dir {
            v.extend(below(sc, &src));
        }
        v
    };
    let mut moved: Vec<(String, BlobEntry)> = vec![];
    {
        let sc = acct.containers.get_mut(&src_fs).ok_or_else(|| dfs_error("SourcePathNotFound"))?;
        for n in &names {
            if let Some(e) = take_entry(sc, n) {
                moved.push((format!("{dst}{}", &n[src.len()..]), e));
            }
        }
    }
    let dc = acct.containers.get_mut(dst_fs).ok_or_else(|| dfs_error("PathNotFound"))?;
    // The destination is replaced (an existing empty directory or a file).
    dc.blobs.remove(dst);
    for (name, e) in moved {
        dc.blobs.insert(name, e);
    }
    Ok(Resp::new(201u16).header("ETag", &etag).header("Last-Modified", http_date(lm)).build())
}
