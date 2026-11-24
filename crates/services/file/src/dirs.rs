//! Directories: create, properties, metadata, delete, and the listing of the files and directories in one. The root of a share is a
//! directory too (an empty path).

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::time::{iso8601_7, rfc1123};
use azemu_core::Response;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::errors;
use crate::meta;
use crate::names;
use crate::shares::{find, find_mut, snapshot_param};
use crate::smb;
use crate::state::{new_id, Account, Dir, Node, Share};
use crate::tree::{self, Miss};
use crate::xml;
use crate::Cx;

/// The root of the share or of the snapshot the request names, and the snapshot time if there is one.
pub fn root_of<'a>(share: &'a Share, snap: &Option<String>, c: &Cx) -> Result<&'a Dir, Response> {
    match snap {
        None => Ok(&share.root),
        Some(t) => share.snapshots.iter().find(|s| &s.time == t).map(|s| &s.root).ok_or_else(|| errors::share_not_found(&c.req.ctx)),
    }
}

/// An error for a snapshot on an operation that changes things.
pub fn writable(c: &Cx) -> Result<(), Response> {
    match c.req.query.get("sharesnapshot") {
        None => Ok(()),
        Some(v) => Err(errors::invalid_query(
            &c.req.ctx,
            "sharesnapshot",
            v,
            "This operation is only allowed on the base share. ShareSnapshot query parameter should not be provided.",
        )),
    }
}

pub fn check_names(c: &Cx, path: &[String]) -> Result<(), Response> {
    names::validate_path(path).map_err(|e| names::share_error(&c.req.ctx, e))
}

fn dir_headers(r: Resp, d: &Dir) -> Resp {
    smb::headers(r, &d.smb, d.id, d.parent_id)
}

pub fn create(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c).and_then(|_| check_names(c, path)) {
        return e;
    }
    let (inp, metadata) = match (smb::read(req, true, true), meta::read(req)) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(i), Ok(m)) => (i, m),
    };
    let Account { shares, permissions, next_id, .. } = acct;
    let Some(share) = shares.get_mut(share) else { return errors::share_not_found(&req.ctx) };
    let Some((name, parents)) = path.split_last() else { return errors::resource_already_exists(&req.ctx) };
    let Some(parent) = tree::walk_mut(&mut share.root, parents) else { return errors::parent_not_found(&req.ctx) };
    match parent.children.get(&name.to_lowercase()) {
        Some(Node::Dir(_)) => return errors::resource_already_exists(&req.ctx),
        Some(Node::File(_)) => return errors::resource_type_mismatch(&req.ctx),
        None => {}
    }
    let smb = match smb::for_new(permissions, req, &inp, true, &parent.smb.permission_key, c.now) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut dir = Dir::new(name, new_id(next_id), parent.id, c.now, &smb.permission_key);
    dir.smb = smb;
    dir.metadata = metadata;
    let r = Resp::new(201u16)
        .header("ETag", &dir.etag)
        .header("Last-Modified", rfc1123(dir.last_modified))
        .header("x-ms-request-server-encrypted", "true");
    let r = dir_headers(r, &dir);
    parent.touch(c.now);
    parent.children.insert(name.to_lowercase(), Node::Dir(dir));
    r.build()
}

/// Properties of a directory (`GET`, `HEAD`) and, with `metadata`, only its metadata.
pub fn get_properties(acct: &Account, c: &Cx, share: &str, path: &[String], metadata_only: bool) -> Response {
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
    let d = match tree::dir_at(root, path) {
        Ok(d) => d,
        Err(m) => return m.response(&req.ctx),
    };
    let r = Resp::new(200u16).header("Last-Modified", rfc1123(d.last_modified)).header("ETag", &d.etag);
    let r = meta::headers(r, &d.metadata);
    if metadata_only {
        return r.header("x-ms-request-server-encrypted", "true").build();
    }
    if path.is_empty() {
        // the root of a share: no permission of its own, ids 0
        return r
            .header("x-ms-server-encrypted", "false")
            .header("x-ms-file-change-time", iso8601_7(d.smb.change))
            .header("x-ms-file-last-write-time", iso8601_7(d.smb.last_write))
            .header("x-ms-file-creation-time", iso8601_7(d.smb.creation))
            .header("x-ms-file-id", 0)
            .header("x-ms-file-parent-id", 0)
            .header("x-ms-file-attributes", "Directory")
            .build();
    }
    dir_headers(r.header("x-ms-server-encrypted", "true"), d).build()
}

pub fn set_properties(acct: &mut Account, c: &Cx, share: &str, path: &[String]) -> Response {
    let req = c.req;
    if let Err(e) = writable(c).and_then(|_| check_names(c, path)) {
        return e;
    }
    let inp = match smb::read(req, true, false) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let Account { shares, permissions, .. } = acct;
    let Some(share) = shares.get_mut(share) else { return errors::share_not_found(&req.ctx) };
    let d = match locate_mut(&mut share.root, path) {
        Ok(d) => d,
        Err(m) => return m.response(&req.ctx),
    };
    if let Err(e) = smb::set(permissions, req, &mut d.smb, &inp, c.now) {
        return e;
    }
    d.stamp(c.now);
    let r = Resp::new(200u16)
        .header("ETag", &d.etag)
        .header("Last-Modified", rfc1123(d.last_modified))
        .header("x-ms-request-server-encrypted", "true");
    dir_headers(r, d).build()
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
    let d = match locate_mut(&mut share.root, path) {
        Ok(d) => d,
        Err(m) => return m.response(&req.ctx),
    };
    d.metadata = metadata;
    d.touch(c.now);
    Resp::new(200u16)
        .header("ETag", &d.etag)
        .header("x-ms-request-server-encrypted", "true")
        .header("Last-Modified", rfc1123(d.last_modified))
        .build()
}

/// The directory a path names, for changing it.
pub fn locate_mut<'a>(root: &'a mut Dir, path: &[String]) -> Result<&'a mut Dir, Miss> {
    let Some((last, parents)) = path.split_last() else { return Ok(root) };
    let parent = tree::walk_mut(root, parents).ok_or(Miss::Parent)?;
    parent.dir_mut(last).ok_or(Miss::Itself)
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
    let Some((last, parents)) = path.split_last() else {
        // the root of a share cannot be deleted
        return errors::err(
            &req.ctx,
            409,
            "ResourceTypeMismatch",
            "The specified resource type does not match the type of the existing resource.",
        );
    };
    let Some(parent) = tree::walk_mut(&mut share.root, parents) else { return errors::parent_not_found(&req.ctx) };
    match parent.dir(last) {
        None => return errors::resource_not_found(&req.ctx),
        Some(d) if !d.children.is_empty() => return errors::directory_not_empty(&req.ctx),
        Some(_) => {}
    }
    parent.children.remove(&last.to_lowercase());
    parent.touch(c.now);
    Resp::new(202u16).build()
}

// ---------------------------------------------------------------------------
// listing
// ---------------------------------------------------------------------------

/// A marker is `1!{length}!{base64 of the name, with - for =}`; a plain name is taken as it is.
fn decode_marker(m: &str) -> Option<String> {
    if let Some(rest) = m.strip_prefix("1!") {
        let (len, b64) = rest.split_once('!')?;
        if len.parse::<usize>().ok()? != b64.len() {
            return None;
        }
        let bytes = B64.decode(b64.replace('-', "=")).ok()?;
        return String::from_utf8(bytes).ok();
    }
    Some(m.to_string())
}

fn encode_marker(name: &str) -> String {
    let b64 = B64.encode(name.as_bytes()).replace('=', "-");
    format!("1!{}!{b64}", b64.len())
}

struct Include {
    timestamps: bool,
    etag: bool,
    attributes: bool,
    permission_key: bool,
}

pub fn list(acct: &Account, c: &Cx, share_name: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    let snap = match snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let share = match find(acct, req, share_name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root = match root_of(share, &snap, c) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let dir = match tree::dir_at(root, path) {
        Ok(d) => d,
        Err(m) => return m.response(ctx),
    };
    let prefix = req.query.get("prefix").unwrap_or("");
    let marker = req.query.get("marker").unwrap_or("");
    let mut inc = Include { timestamps: false, etag: false, attributes: false, permission_key: false };
    if let Some(list) = req.query.get("include") {
        for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            // `permission-key`, which azure-storage-file-share 12.27.1 sends, is refused by Azure (checked in November 2026)
            match part.to_ascii_lowercase().as_str() {
                "timestamps" => inc.timestamps = true,
                "etag" => inc.etag = true,
                "attributes" => inc.attributes = true,
                "permissionkey" => inc.permission_key = true,
                _ => return errors::invalid_query(ctx, "include", list, "This query parameter value is invalid."),
            }
        }
    }
    match req.header("x-ms-file-extended-info").map(str::trim) {
        None => {}
        Some(v) if v.eq_ignore_ascii_case("true") => {}
        Some(v) => return errors::invalid_header(ctx, "x-ms-file-extended-info", v),
    }
    let max = match req.query.get("maxresults") {
        None => None,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 1 => Some(n),
            Ok(_) => return errors::out_of_range_query(ctx, "maxresults", v, 1, i64::from(i32::MAX)),
            Err(_) => return errors::invalid_query(ctx, "maxresults", v, ""),
        },
    };
    let limit = max.map_or(5000, |n| (n as usize).min(5000));
    let start = if marker.is_empty() {
        None
    } else {
        match decode_marker(marker) {
            Some(m) => Some(m.to_lowercase()),
            None => return errors::bad_marker(ctx),
        }
    };

    // files first, then directories, each in name order without regard to case
    let lower_prefix = prefix.to_lowercase();
    let mut entries: Vec<(u8, &String, &Node)> = Vec::new();
    for (key, node) in dir.children.iter().filter(|(k, _)| k.starts_with(&lower_prefix)) {
        entries.push((u8::from(node.is_dir()), key, node));
    }
    entries.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    if let Some(m) = &start {
        let rank = u8::from(dir.children.get(m).is_some_and(Node::is_dir));
        entries.retain(|(r, k, _)| (*r, *k) >= (rank, m));
    }
    let next = entries.get(limit).map(|(_, _, n)| n.name().to_string());
    entries.truncate(limit);

    let mut body = format!(
        "<EnumerationResults ServiceEndpoint=\"{}/\" ShareName=\"{}\"",
        storage::xml_escape(&ctx.endpoint.base_url()),
        storage::xml_escape(share_name)
    );
    if let Some(s) = &snap {
        body.push_str(&format!(" ShareSnapshot=\"{s}\""));
    }
    body.push_str(&format!(" DirectoryPath=\"{}\">", storage::xml_escape(&path.join("/"))));
    if !prefix.is_empty() {
        body.push_str(&xml::element("Prefix", prefix));
    }
    if !marker.is_empty() {
        body.push_str(&xml::element("Marker", marker));
    }
    if let Some(n) = max {
        body.push_str(&format!("<MaxResults>{n}</MaxResults>"));
    }
    body.push_str(&format!("<DirectoryId>{}</DirectoryId>", dir.id));
    if entries.is_empty() {
        body.push_str("<Entries />");
    } else {
        body.push_str("<Entries>");
        for (_, _, node) in &entries {
            let (tag, name, id, smb, etag, modified, size) = match node {
                Node::File(f) => ("File", &f.name, f.id, &f.smb, &f.etag, f.last_modified, Some(f.size)),
                Node::Dir(d) => ("Directory", &d.name, d.id, &d.smb, &d.etag, d.last_modified, None),
            };
            body.push_str(&format!("<{tag}>{}<FileId>{id}</FileId><Properties", xml::element("Name", name)));
            let mut props = String::new();
            if let Some(s) = size {
                props.push_str(&format!("<Content-Length>{s}</Content-Length>"));
            }
            if inc.timestamps {
                props.push_str(&format!(
                    "<CreationTime>{}</CreationTime><LastAccessTime>{}</LastAccessTime><LastWriteTime>{}</LastWriteTime><ChangeTime>{}</ChangeTime><Last-Modified>{}</Last-Modified>",
                    iso8601_7(smb.creation),
                    iso8601_7(smb.creation),
                    iso8601_7(smb.last_write),
                    iso8601_7(smb.change),
                    rfc1123(modified)
                ));
            }
            if inc.etag {
                props.push_str(&format!("<Etag>{etag}</Etag>"));
            }
            if props.is_empty() {
                body.push_str(" />");
            } else {
                body.push_str(&format!(">{props}</Properties>"));
            }
            if inc.attributes {
                body.push_str(&format!("<Attributes>{}</Attributes>", crate::state::attr::render(smb.attributes)));
            }
            if inc.permission_key {
                body.push_str(&format!("<PermissionKey>{}</PermissionKey>", smb.permission_key));
            }
            body.push_str(&format!("</{tag}>"));
        }
        body.push_str("</Entries>");
    }
    match next {
        Some(n) => body.push_str(&xml::element("NextMarker", &encode_marker(&n))),
        None => body.push_str("<NextMarker />"),
    }
    body.push_str("</EnumerationResults>");
    Resp::new(200u16).xml(xml::document(&body)).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip_and_match_azure() {
        // taken from Azure: the name `c.txt` gave `1!8!Yy50eHQ-`
        assert_eq!(encode_marker("c.txt"), "1!8!Yy50eHQ-");
        assert_eq!(decode_marker("1!8!Yy50eHQ-").as_deref(), Some("c.txt"));
        assert_eq!(decode_marker("plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(decode_marker("1!9!Yy50eHQ-"), None);
    }
}
