//! Files: create, read, properties, metadata, delete.

use azemu_core::bytes::Bytes;
use azemu_core::resp::Resp;
use azemu_core::time::rfc1123;
use azemu_core::{Request, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use md5::{Digest, Md5};

use crate::dirs::{check_names, root_of, writable};
use crate::download::{Download, STREAM_MIN};
use crate::errors;
use crate::lease::Lease;
use crate::leasing::{self, Kind};
use crate::meta;
use crate::shares::{find, find_mut, snapshot_param};
use crate::smb;
use crate::state::{new_id, Account, File, Node, MAX_FILE_SIZE};
use crate::tree::{self, Miss};
use crate::Cx;

/// A file's lease as it is now.
fn lease_now(f: &File, c: &Cx) -> Lease {
    let mut l = f.lease.clone();
    l.refresh(c.now);
    l
}

/// `x-ms-content-length`: the size of the file, up to 4 TiB.
fn size_header(req: &Request, required: bool) -> Result<Option<u64>, Response> {
    let Some(v) = req.header("x-ms-content-length") else {
        return if required { Err(errors::missing_header(&req.ctx, "x-ms-content-length")) } else { Ok(None) };
    };
    match v.trim().parse::<i64>() {
        Ok(n) if (0..=MAX_FILE_SIZE as i64).contains(&n) => Ok(Some(n as u64)),
        Ok(_) => Err(errors::out_of_range_input(&req.ctx, "The file size exceeds the maximum permissible limit.")),
        Err(_) => Err(errors::invalid_header(&req.ctx, "x-ms-content-length", v)),
    }
}

/// The `x-ms-content-*` properties of a request: content type, encoding, language, cache control, disposition and MD5.
pub struct ContentProps {
    pub content_type: String,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_md5: Option<Vec<u8>>,
}

pub fn content_props(req: &Request) -> Result<ContentProps, Response> {
    let get = |n: &str| req.header(n).map(|v| v.trim().to_string());
    let md5 = match req.header("x-ms-content-md5") {
        None => None,
        Some(v) => match B64.decode(v.trim()) {
            Ok(b) if b.len() == 16 => Some(b),
            _ => return Err(errors::invalid_header(&req.ctx, "x-ms-content-md5", v)),
        },
    };
    Ok(ContentProps {
        content_type: get("x-ms-content-type").filter(|v| !v.is_empty()).unwrap_or_else(|| "application/octet-stream".into()),
        content_encoding: get("x-ms-content-encoding"),
        content_language: get("x-ms-content-language"),
        cache_control: get("x-ms-cache-control"),
        content_disposition: get("x-ms-content-disposition"),
        content_md5: md5,
    })
}

fn apply_content(f: &mut File, p: ContentProps) {
    f.content_type = p.content_type;
    f.content_encoding = p.content_encoding;
    f.content_language = p.content_language;
    f.cache_control = p.cache_control;
    f.content_disposition = p.content_disposition;
    f.content_md5 = p.content_md5;
}

pub fn create(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c).and_then(|_| check_names(c, path)) {
        return e;
    }
    let size = match size_header(req, true) {
        Ok(s) => s.unwrap_or(0),
        Err(e) => return e,
    };
    match req.header("x-ms-type") {
        None => return errors::missing_header(ctx, "x-ms-type"),
        Some(t) if t.trim().eq_ignore_ascii_case("file") => {}
        Some(t) => return errors::invalid_header(ctx, "x-ms-type", t),
    }
    let (inp, props, metadata) = match (smb::read(req, false, true), content_props(req), meta::read(req)) {
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        (Ok(i), Ok(p), Ok(m)) => (i, p, m),
    };
    // since 2026-04-06 Create File may carry the content of the file
    if req.body.len() as u64 > size {
        return errors::invalid_header(ctx, "Content-Length", &req.body.len().to_string());
    }
    let Account { shares, permissions, next_id, .. } = acct;
    let Some(share) = shares.get_mut(share) else { return errors::share_not_found(ctx) };
    let Some((name, parents)) = path.split_last() else { return errors::resource_type_mismatch(ctx) };
    let Some(parent) = tree::walk_mut(&mut share.root, parents) else { return errors::parent_not_found(ctx) };
    let existing_id = match parent.children.get(&name.to_lowercase()) {
        Some(Node::Dir(_)) => return errors::resource_type_mismatch(ctx),
        Some(Node::File(f)) => {
            if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
                return e;
            }
            Some(f.id)
        }
        None => None,
    };
    let smb = match smb::for_new(permissions, req, &inp, false, &parent.smb.permission_key, c.now) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut f = File {
        name: name.clone(),
        id: existing_id.unwrap_or_else(|| new_id(next_id)),
        parent_id: parent.id,
        etag: azemu_core::etag::new_etag(),
        last_modified: c.now,
        size,
        pages: Default::default(),
        content_type: String::new(),
        content_encoding: None,
        content_language: None,
        cache_control: None,
        content_disposition: None,
        content_md5: None,
        metadata,
        smb,
        lease: Lease::default(),
        copy: None,
    };
    apply_content(&mut f, props);
    if !req.body.is_empty() {
        f.write(0, &req.body);
    }
    let mut r = Resp::new(201u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-request-server-encrypted", "true");
    r = smb::headers(r, &f.smb, f.id, f.parent_id);
    parent.touch(c.now);
    parent.children.insert(name.to_lowercase(), Node::File(f));
    r.build()
}

/// The headers every read of a file's properties has.
pub fn property_headers(mut r: Resp, f: &File, c: &Cx) -> Resp {
    r = r.header("Content-Type", &f.content_type).header("Last-Modified", rfc1123(f.last_modified)).header("ETag", &f.etag);
    for (k, v) in [
        ("Content-Encoding", &f.content_encoding),
        ("Content-Language", &f.content_language),
        ("Cache-Control", &f.cache_control),
        ("Content-Disposition", &f.content_disposition),
    ] {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            r = r.header(k, v);
        }
    }
    r = meta::headers(r, &f.metadata);
    r = r.header("x-ms-server-encrypted", "true");
    for (k, v) in lease_now(f, c).headers() {
        r = r.header(k, v);
    }
    if let Some(cp) = &f.copy {
        r = r
            .header("x-ms-copy-id", &cp.id)
            .header("x-ms-copy-source", &cp.source)
            .header("x-ms-copy-status", &cp.status)
            .header("x-ms-copy-progress", &cp.progress)
            .header("x-ms-copy-completion-time", rfc1123(cp.completed));
        if let Some(d) = &cp.description {
            r = r.header("x-ms-copy-status-description", d);
        }
    }
    smb::headers(r, &f.smb, f.id, f.parent_id)
}

enum Spec {
    From { start: u64, end: Option<u64> },
}

/// `bytes=0-9` or `bytes=5-`. Azure Files has no suffix ranges.
fn parse_range(v: &str) -> Option<Spec> {
    let rest = v.trim().strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let start = a.trim().parse::<u64>().ok()?;
    let end = if b.trim().is_empty() { None } else { Some(b.trim().parse::<u64>().ok()?) };
    match end {
        Some(e) if e < start => None,
        _ => Some(Spec::From { start, end }),
    }
}

pub fn get(acct: &Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    let head = req.method == azemu_core::http::Method::HEAD;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root = match root_of(share, &snap, c) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let f = match tree::file_at(root, path) {
        Ok(f) => f,
        Err(m) => return m.response(ctx),
    };
    if head {
        let r = Resp::new(200u16).header("Content-Length", f.size).header("x-ms-type", "File");
        return sas_overrides(req, property_headers(r, f, c)).build();
    }
    let range = req.header("x-ms-range").or_else(|| req.header("range")).and_then(parse_range);
    let want_md5 = match req.header("x-ms-range-get-content-md5").map(str::trim) {
        None => false,
        Some(v) if v.eq_ignore_ascii_case("true") => true,
        Some(v) if v.eq_ignore_ascii_case("false") => false,
        Some(v) => return errors::invalid_header(ctx, "x-ms-range-get-content-md5", v),
    };
    let (start, end, partial) = match range {
        None => (0, f.size, false),
        Some(Spec::From { start, end }) => {
            if start >= f.size {
                return errors::invalid_range_read(ctx, f.size);
            }
            (start, end.map_or(f.size, |e| e.saturating_add(1).min(f.size)), true)
        }
    };
    if want_md5 && (!partial || end - start > crate::state::MAX_PUT_RANGE as u64) {
        return errors::invalid_header(ctx, "x-ms-range-get-content-md5", "true");
    }
    // Azure Files documents no cap on the length of one Get File range, and a file may be declared as large as 4 TiB while
    // holding nothing: a read of 1 MiB or more is answered in parts as the client takes them, from the stored pages and the
    // lengths of the holes, so memory does not depend on the length of the read (`download.rs`)
    let stream = !want_md5 && end - start >= STREAM_MIN;
    let data = if stream { Vec::new() } else { f.read(start, end) };
    let mut r = Resp::new(if partial { 206u16 } else { 200u16 }).header("Accept-Ranges", "bytes");
    if partial {
        r = r.header("Content-Range", format!("bytes {start}-{}/{}", end - 1, f.size));
    }
    if want_md5 {
        r = r.header("Content-MD5", B64.encode(Md5::digest(&data)));
    } else if !partial {
        if let Some(m) = &f.content_md5 {
            r = r.header("Content-MD5", B64.encode(m));
        }
    }
    if stream {
        r = r.header("Content-Length", end - start).stream(Download::new(f.pieces(start, end)));
        return sas_overrides(req, property_headers(r, f, c)).build();
    }
    sas_overrides(req, property_headers(r, f, c)).body(Bytes::from(data)).build()
}

/// A SAS can set the response headers (`rscc`, `rscd`, `rsce`, `rscl`, `rsct`).
fn sas_overrides(req: &Request, mut r: Resp) -> Resp {
    if let azemu_core::storage::auth::Auth::Sas(p) = azemu_core::storage::auth::classify(req) {
        for (name, v) in [
            ("Cache-Control", &p.rscc),
            ("Content-Disposition", &p.rscd),
            ("Content-Encoding", &p.rsce),
            ("Content-Language", &p.rscl),
            ("Content-Type", &p.rsct),
        ] {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                r = r.header(name, v);
            }
        }
    }
    r
}

pub fn get_metadata(acct: &Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root = match root_of(share, &snap, c) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match tree::file_at(root, path) {
        Ok(f) => meta::headers(Resp::new(200u16).header("Last-Modified", rfc1123(f.last_modified)).header("ETag", &f.etag), &f.metadata)
            .header("x-ms-request-server-encrypted", "true")
            .build(),
        Err(m) => m.response(&req.ctx),
    }
}

/// The file a path names, for changing it.
pub fn locate_mut<'a>(share: &'a mut crate::state::Share, path: &[String]) -> Result<&'a mut File, Miss> {
    let (last, parents) = path.split_last().ok_or(Miss::Itself)?;
    let parent = tree::walk_mut(&mut share.root, parents).ok_or(Miss::Parent)?;
    parent.file_mut(last).ok_or(Miss::Itself)
}

pub fn set_properties(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c) {
        return e;
    }
    let (inp, props, size) = match (smb::read(req, false, false), content_props(req), size_header(req, false)) {
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        (Ok(i), Ok(p), Ok(s)) => (i, p, s),
    };
    let Account { shares, permissions, .. } = acct;
    let Some(share) = shares.get_mut(share) else { return errors::share_not_found(&req.ctx) };
    let f = match locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(&req.ctx),
    };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    if let Err(e) = smb::set(permissions, req, &mut f.smb, &inp, c.now) {
        return e;
    }
    // what a request does not give is cleared, as on a blob
    apply_content(f, props);
    if let Some(s) = size {
        f.resize(s);
    }
    f.stamp(c.now);
    let r = Resp::new(200u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-request-server-encrypted", "true");
    smb::headers(r, &f.smb, f.id, f.parent_id).build()
}

pub fn set_metadata(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c) {
        return e;
    }
    let metadata = match meta::read(req) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let share = match find_mut(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let f = match locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(&req.ctx),
    };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    f.metadata = metadata;
    f.touch(c.now);
    Resp::new(200u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-request-server-encrypted", "true")
        .build()
}

pub fn delete(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c) {
        return e;
    }
    let share = match find_mut(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let Some((last, parents)) = path.split_last() else { return errors::resource_not_found(&req.ctx) };
    let Some(parent) = tree::walk_mut(&mut share.root, parents) else { return errors::parent_not_found(&req.ctx) };
    let Some(f) = parent.file(last) else { return errors::resource_not_found(&req.ctx) };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    parent.children.remove(&last.to_lowercase());
    parent.touch(c.now);
    Resp::new(202u16).header("x-ms-link-count", 0).build()
}

pub fn lease(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c) {
        return e;
    }
    let action = match leasing::parse(req, Kind::File) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let share = match find_mut(acct, req, share) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let f = match locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(&req.ctx),
    };
    let (etag, modified) = (f.etag.clone(), f.last_modified);
    leasing::apply(req, Kind::File, action, &mut f.lease, c.now, &etag, modified)
}
