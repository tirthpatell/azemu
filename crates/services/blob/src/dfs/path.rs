//! Path operations: create, delete, read, properties, lease.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::dfs::acl;
use crate::dfs::errors::dfs_error;
use crate::dfs::expiry;
use crate::dfs::filesystem::open;
use crate::dfs::model::*;
use crate::dfs::util::{format_properties, http_date, parse_properties};
use crate::errors::{err, ApiError, ApiResult};
use crate::lease::{self, Action, Scope};
use crate::state::{Blob, DeletedPath, Props, Time};
use crate::util::{hdr, hdr_i64};

fn nonempty(v: Option<&str>) -> Option<String> {
    v.filter(|s| !s.is_empty()).map(str::to_string)
}

/// Content headers (`x-ms-content-type`, ...) into properties. `None` when the request has none.
pub fn content_headers(req: &Request) -> Option<Props> {
    let names = ["x-ms-cache-control", "x-ms-content-type", "x-ms-content-encoding", "x-ms-content-language", "x-ms-content-disposition"];
    if !names.iter().any(|n| req.header(n).is_some()) {
        return None;
    }
    Some(Props {
        content_type: nonempty(req.header("x-ms-content-type")).unwrap_or_else(|| "application/octet-stream".into()),
        content_encoding: nonempty(req.header("x-ms-content-encoding")),
        content_language: nonempty(req.header("x-ms-content-language")),
        content_disposition: nonempty(req.header("x-ms-content-disposition")),
        cache_control: nonempty(req.header("x-ms-cache-control")),
        content_md5: None,
    })
}

/// What the create request says about ownership and permissions.
pub fn child_spec(req: &Request) -> Result<ChildSpec, ApiError> {
    let mut spec =
        ChildSpec { owner: nonempty(req.header("x-ms-owner")), group: nonempty(req.header("x-ms-group")), ..ChildSpec::default() };
    if let Some(v) = req.header("x-ms-permissions") {
        spec.permissions = Some(acl::parse_permissions(v).ok_or_else(|| ApiError::invalid_header("x-ms-permissions", v))?);
    }
    if let Some(v) = req.header("x-ms-umask") {
        spec.umask = Some(acl::parse_umask(v).ok_or_else(|| ApiError::invalid_header("x-ms-umask", v))?);
    }
    if let Some(v) = req.header("x-ms-acl") {
        spec.acl = Some(acl::parse_acl(v).map_err(|m| crate::dfs::errors::acl_error(v, m))?);
    }
    Ok(spec)
}

fn add_header(r: &mut Response, k: &str, v: &str) {
    if let (Ok(n), Ok(val)) = (azemu_core::http::HeaderName::from_bytes(k.as_bytes()), azemu_core::http::HeaderValue::from_str(v)) {
        r.headers_mut().insert(n, val);
    }
}

fn need_name(p: &str) -> Result<(), ApiError> {
    if p.is_empty() {
        return Err(err("InvalidResourceName").msg("The root directory cannot be used for this operation."));
    }
    valid_name(p)
}

pub fn create(cx: &mut Cx, fs: &str, name: &str, is_dir: bool) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    need_name(name)?;
    let spec = child_spec(req)?;
    let metadata = parse_properties(req)?;
    let content = content_headers(req);
    let expiry = expiry::resolve(req, now, now)?;
    let proposed = hdr(req, "x-ms-proposed-lease-id");
    let duration = hdr_i64(req, "x-ms-lease-duration")?;
    let lease_id = hdr(req, "x-ms-lease-id");
    let cpk = hdr(req, "x-ms-encryption-key-sha256").map(str::to_string);
    let enc_ctx = hdr(req, "x-ms-encryption-context").map(str::to_string);

    let c = open(cx, fs)?;
    if let Some(e) = c.blobs.get_mut(name) {
        refresh_entry(e, now);
    }
    let existing = live(c, name).map(|b| (is_dir_blob(b), b.etag.clone(), b.last_modified, b.lease.clone()));
    if let Some((ex_dir, ..)) = &existing {
        if *ex_dir != is_dir {
            return Err(path_conflict());
        }
    }
    let kind = if lease_id.is_some() { Kind::CreateLeased } else { Kind::Create };
    cond::check(req, "", existing.as_ref().map(|(_, e, lm, _)| (e.as_str(), *lm)), kind)?;
    if let Some((_, _, _, l)) = &existing {
        lease::check_write(l, lease_id, Scope::Blob)?;
    }
    // Create over an existing path replaces it: a legal hold or an immutability policy forbids that, as on the Blob endpoint
    // (Put Blob answers 409 BlobImmutableDueToLegalHold / BlobImmutableDueToPolicy).
    if let Some(b) = live(c, name) {
        crate::store::check_mutable(b, now)?;
    }
    ensure_parents(c, name, now)?;
    let attrs = new_child_attrs(c, parent_of(name), is_dir, &spec);
    let mut blob = if is_dir { new_dir_blob(attrs, now) } else { new_file_blob(attrs, now) };
    if let Some(p) = content {
        blob.props = p;
    }
    if let Some(mut m) = metadata {
        if is_dir {
            m.push((FOLDER_KEY.to_string(), "true".to_string()));
            m.sort();
        }
        blob.metadata = m;
    }
    if let (Some(d), Some(x)) = (blob.dfs.as_mut(), expiry) {
        d.expiry = x;
    }
    if let Some(d) = blob.dfs.as_mut() {
        d.encryption_context = enc_ctx;
    }
    blob.cpk_sha256 = cpk;
    if let Some((_, _, _, l)) = existing {
        blob.lease = l;
    }
    if let Some(p) = proposed {
        lease::apply(
            &mut blob.lease,
            &Action { action: "acquire", lease_id: None, proposed: Some(p), duration, break_period: None },
            now,
            Scope::Blob,
        )?;
    }
    let mut r = Resp::new(201u16)
        .header("ETag", &blob.etag)
        .header("Last-Modified", http_date(blob.last_modified))
        .header("x-ms-request-server-encrypted", "true");
    if let Some(k) = &blob.cpk_sha256 {
        r = r.header("x-ms-encryption-key-sha256", k);
    }
    if c.default_scope != "$account-encryption-key" {
        r = r.header("x-ms-encryption-scope", &c.default_scope);
    }
    c.blobs.entry(name.to_string()).or_default().current = Some(blob);
    Ok(r.build())
}

fn is_dir_blob(b: &Blob) -> bool {
    is_dir(b)
}

pub fn delete(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    need_name(name)?;
    let recursive = req.query.get("recursive").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false);
    let retention = cx.account().props.soft_delete_days();
    let c = open(cx, fs)?;
    if let Some(e) = c.blobs.get_mut(name) {
        refresh_entry(e, now);
    }
    let b = live(c, name).ok_or_else(|| dfs_error("PathNotFound"))?;
    cond::check(req, "", Some((&b.etag, b.last_modified)), Kind::Write)?;
    lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
    let dir = is_dir(b);
    if dir && has_children(c, name) && !recursive {
        return Err(dfs_error("DirectoryNotEmpty"));
    }
    let subtree = if dir { below(c, name) } else { vec![] };
    for n in std::iter::once(&name.to_string()).chain(subtree.iter()) {
        if let Some(x) = live(c, n) {
            crate::store::check_mutable(x, now)?;
        }
    }
    let mut taken: Vec<(String, Blob)> = vec![];
    for n in subtree.iter().chain(std::iter::once(&name.to_string())) {
        if let Some(e) = c.blobs.get_mut(n) {
            if let Some(b) = e.current.take() {
                taken.push((n.clone(), b));
            }
        }
        c.prune(n);
    }
    let mut r = Resp::new(200u16);
    match retention {
        Some(_) => {
            // what was deleted longer ago than the retention is gone for good (it used to stay in memory for ever)
            if let Some(days) = retention {
                c.deleted_paths.retain(|d| d.deleted_at + azemu_core::time::Duration::days(i64::from(days)) > now);
            }
            let mut id = filetime(now);
            while c.deleted_paths.iter().any(|d| d.deletion_id == id.to_string()) {
                id += 1;
            }
            let pos = taken.iter().position(|(n, _)| n == name);
            if let Some(pos) = pos {
                let (_, blob) = taken.remove(pos);
                c.deleted_paths.push(DeletedPath {
                    deletion_id: id.to_string(),
                    name: name.to_string(),
                    deleted_at: now,
                    blob,
                    children: taken,
                });
            }
            r = r.header("x-ms-delete-type-permanent", "false").header("x-ms-deletion-id", id.to_string());
        }
        None => r = r.header("x-ms-delete-type-permanent", "true"),
    }
    Ok(r.build())
}

/// Attributes and the etag / last-modified pair of a path or of the root directory.
fn status_of(c: &mut Container, name: &str) -> Result<(DfsAttrs, String, Time), ApiError> {
    if name.is_empty() {
        let (etag, lm) = (c.etag.clone(), c.last_modified);
        return Ok((root_attrs(c).clone(), etag, lm));
    }
    let b = live(c, name).ok_or_else(|| dfs_error("PathNotFound"))?;
    let attrs = b.dfs.as_deref().cloned().unwrap_or_else(|| DfsAttrs::new(false));
    Ok((attrs, b.etag.clone(), b.last_modified))
}

use crate::state::{Container, DfsAttrs};

pub fn get_properties(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let action = req.query.get("action").map(|a| a.to_ascii_lowercase());
    let c = open(cx, fs)?;
    if let Some(e) = c.blobs.get_mut(name) {
        refresh_entry(e, now);
    }
    match action.as_deref() {
        Some("getaccesscontrol") | Some("checkaccess") => {
            let (a, etag, lm) = status_of(c, name)?;
            cond::check(req, "", Some((&etag, lm)), Kind::Read)?;
            let mut r = Resp::new(200u16).header("ETag", &etag).header("Last-Modified", http_date(lm));
            if action.as_deref() == Some("getaccesscontrol") {
                r = r
                    .header("x-ms-owner", &a.owner)
                    .header("x-ms-group", &a.group)
                    .header("x-ms-permissions", a.permissions())
                    .header("x-ms-acl", a.acl_string());
            }
            Ok(r.build())
        }
        Some("getstatus") | None => {
            if name.is_empty() {
                let (a, etag, lm) = status_of(c, name)?;
                return Ok(Resp::new(200u16)
                    .header("ETag", &etag)
                    .header("Last-Modified", http_date(lm))
                    .header("x-ms-resource-type", "directory")
                    .header("x-ms-owner", &a.owner)
                    .header("x-ms-group", &a.group)
                    .header("x-ms-permissions", a.permissions())
                    .header("x-ms-acl", a.acl_string())
                    .build());
            }
            let mut r = crate::blob::get_blob(cx, fs, name, true)?;
            if let Some(b) = cx.account().containers.get(fs).and_then(|c| live(c, name)) {
                add_header(&mut r, "x-ms-properties", &format_properties(&b.metadata));
            }
            Ok(r)
        }
        Some(_) => Err(ApiError::invalid_query("action", req.query.get("action").unwrap_or(""))),
    }
}

pub fn read(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    need_name(name)?;
    open(cx, fs)?;
    let mut r = crate::blob::get_blob(cx, fs, name, false)?;
    if let Some(b) = cx.account().containers.get(fs).and_then(|c| live(c, name)) {
        add_header(&mut r, "x-ms-properties", &format_properties(&b.metadata));
    }
    Ok(r)
}

pub fn lease(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    need_name(name)?;
    open(cx, fs)?;
    lease::lease_blob(cx, fs, name)
}
