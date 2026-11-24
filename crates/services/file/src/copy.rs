//! Copy File, Abort Copy, and reading a source file that is addressed by URL (the URL points back at this emulator, so it is
//! resolved from state and never fetched). A source that is a blob lives in another service and is not built.

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_7, rfc1123};
use azemu_core::{Query, Request, Response};
use percent_encoding::percent_decode_str;

use crate::dirs::{check_names, writable};
use crate::errors;
use crate::leasing::{self, Kind};
use crate::meta;
use crate::names;
use crate::smb::{self, Pick};
use crate::state::{attr, new_id, Account, CopyInfo, File, Node, State};
use crate::tree;
use crate::Cx;

pub(crate) struct ParsedUrl {
    pub account: String,
    pub share: String,
    pub path: Vec<String>,
    query: Query,
    blob: bool,
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

pub(crate) fn parse_url(url: &str) -> Option<ParsedUrl> {
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
    let blob = labels.len() >= 2 && labels[1].eq_ignore_ascii_case("blob");
    let (account, path) = if labels.len() >= 2 && (labels[1].eq_ignore_ascii_case("file") || blob) {
        (labels[0].to_ascii_lowercase(), path.to_string())
    } else {
        let (a, p) = path.split_once('/').unwrap_or((path, ""));
        (decode(a).to_ascii_lowercase(), p.to_string())
    };
    let (share, file) = path.split_once('/').unwrap_or((path.as_str(), ""));
    if share.is_empty() || file.is_empty() {
        return None;
    }
    let names: Vec<String> = names::split_path(&decode(file), false);
    Some(ParsedUrl { account, share: decode(share), path: names, query: Query::parse(query), blob })
}

fn cannot_verify(ctx: &azemu_core::RequestCtx, status: u16, code: &str, msg: &str) -> Response {
    azemu_core::storage::error_with_details(
        ctx,
        status,
        "CannotVerifyCopySource",
        msg,
        &[("CopySourceStatusCode", &status.to_string()), ("CopySourceErrorCode", code), ("CopySourceErrorMessage", msg)],
    )
}

/// The source file of a copy: a clone of it (pages are shared, not copied) and the URL to remember.
fn find_source(state: &State, c: &Cx, url: &str) -> Result<(File, String), Response> {
    let ctx = &c.req.ctx;
    let Some(p) = parse_url(url) else { return Err(errors::invalid_header(ctx, "x-ms-copy-source", url)) };
    if p.blob {
        return Err(errors::not_implemented(ctx, "copy from a blob to a file"));
    }
    // A source in the account of the request that is not there is a plain 404; in another account Azure cannot look and says so.
    let same = p.account.eq_ignore_ascii_case(&c.req.account);
    let share_missing = || {
        if same {
            errors::share_not_found(ctx)
        } else {
            cannot_verify(ctx, 404, "ShareNotFound", "The specified share does not exist.")
        }
    };
    let file_missing = || {
        if same {
            errors::resource_not_found(ctx)
        } else {
            cannot_verify(ctx, 404, "ResourceNotFound", "The specified resource does not exist.")
        }
    };
    let Some(acct) = state.accounts.get(&p.account) else { return Err(share_missing()) };
    let Some(share) = acct.shares.get(&p.share) else { return Err(share_missing()) };
    let root = match p.query.get("sharesnapshot") {
        None => &share.root,
        Some(t) => {
            let t = azemu_core::time::parse_iso8601(t).map(iso8601_7).unwrap_or_default();
            match share.snapshots.iter().find(|s| s.time == t) {
                Some(s) => &s.root,
                None => return Err(share_missing()),
            }
        }
    };
    match tree::file_at(root, &p.path) {
        Ok(f) => Ok((f.clone(), url.split('?').next().unwrap_or(url).to_string())),
        Err(_) => Err(file_missing()),
    }
}

/// The bytes `range` (inclusive) of the source file of a Put Range From URL.
pub fn read_source(state: &State, c: &Cx, url: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>, Response> {
    let (f, _) = find_source(state, c, url)?;
    let (a, b) = range.unwrap_or((0, f.size.saturating_sub(1)));
    if f.size == 0 || b >= f.size {
        return Err(errors::err(&c.req.ctx, 416, "InvalidRange", "The range specified is invalid for the current size of the resource."));
    }
    Ok(f.read(a, b + 1))
}

fn flag(req: &Request, name: &str) -> Result<bool, Response> {
    match req.header(name).map(str::trim) {
        None => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
        Some(v) => Err(errors::invalid_header(&req.ctx, name, v)),
    }
}

pub fn copy_file(state: &mut State, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c).and_then(|_| check_names(c, path)) {
        return e;
    }
    let Some(url) = req.header("x-ms-copy-source") else { return errors::missing_header(ctx, "x-ms-copy-source") };
    // `x-ms-file-copy-set-archive` is no choice here: a copy always has the Archive attribute (checked against Azure)
    let (inp, metadata, ignore_readonly) = match (smb::read(req, false, true), meta::read(req), flag(req, "x-ms-file-copy-ignore-readonly"))
    {
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        (Ok(i), Ok(m), Ok(r)) => (i, m, r),
    };
    let (src, source_url) = match find_source(state, c, url) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    let Account { shares, permissions, next_id, .. } = acct;
    let Some(share) = shares.get_mut(share) else { return errors::share_not_found(ctx) };
    let Some((name, parents)) = path.split_last() else { return errors::resource_type_mismatch(ctx) };
    let Some(parent) = tree::walk_mut(&mut share.root, parents) else { return errors::parent_not_found(ctx) };
    let existing = match parent.children.get(&name.to_lowercase()) {
        Some(Node::Dir(_)) => return errors::resource_type_mismatch(ctx),
        Some(Node::File(f)) => {
            if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
                return e;
            }
            if f.smb.attributes & attr::READ_ONLY != 0 && !ignore_readonly {
                return errors::conflict(
                    ctx,
                    "ReadOnlyAttribute",
                    "The specified resource is read-only and cannot be modified at this time.",
                );
            }
            Some(f.id)
        }
        None => None,
    };
    // What a copy gets (checked against Azure in November 2026): the attributes asked for plus `Archive`, times of now, and the
    // permission of the source. Content properties and, when none are sent, the metadata come from the source.
    let mut new_smb = match smb::for_new(permissions, req, &inp, false, &src.smb.permission_key, c.now) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if inp.permission == smb::Perm::Absent {
        new_smb.permission_key = src.smb.permission_key.clone();
    }
    new_smb.attributes = if inp.attributes == Pick::Absent { attr::ARCHIVE } else { new_smb.attributes & !attr::NORMAL | attr::ARCHIVE };
    let copy_id = uuid::Uuid::new_v4().to_string();
    let mut f = src.clone();
    f.name = name.clone();
    f.id = existing.unwrap_or_else(|| new_id(next_id));
    f.parent_id = parent.id;
    f.etag = azemu_core::etag::new_etag();
    f.last_modified = c.now;
    f.smb = new_smb;
    f.smb.change = c.now;
    f.lease = Default::default();
    if !metadata.is_empty() {
        f.metadata = metadata;
    }
    f.copy = Some(CopyInfo {
        id: copy_id.clone(),
        source: source_url,
        status: "success".into(),
        completed: c.now,
        progress: format!("{0}/{0}", src.size),
        description: None,
    });
    let r = Resp::new(202u16)
        .header("ETag", &f.etag)
        .header("Last-Modified", rfc1123(f.last_modified))
        .header("x-ms-copy-id", copy_id)
        .header("x-ms-copy-status", "success");
    parent.touch(c.now);
    parent.children.insert(name.to_lowercase(), Node::File(f));
    r.build()
}

pub fn abort(state: &mut State, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c) {
        return e;
    }
    match req.header("x-ms-copy-action").map(str::trim) {
        None => return errors::missing_header(ctx, "x-ms-copy-action"),
        Some(a) if a.eq_ignore_ascii_case("abort") => {}
        Some(a) => return errors::invalid_header(ctx, "x-ms-copy-action", a),
    }
    let id = req.query.get("copyid").unwrap_or("");
    if uuid::Uuid::parse_str(id).is_err() {
        return errors::invalid_query(ctx, "copyid", id, "copyid needs to be valid Guid.");
    }
    let acct = state.accounts.entry(req.account.to_ascii_lowercase()).or_default();
    let Some(share) = acct.shares.get_mut(share) else { return errors::share_not_found(ctx) };
    let f = match crate::files::locate_mut(share, path) {
        Ok(f) => f,
        Err(m) => return m.response(ctx),
    };
    if let Err(e) = leasing::check_write(req, Kind::File, &f.lease, c.now) {
        return e;
    }
    // Copies finish at once here, so there is never one to abort (Azure also says so for a copy that is done, whatever the ID).
    let _ = f;
    errors::conflict(ctx, "NoPendingCopyOperation", "There is currently no pending copy operation.")
}
