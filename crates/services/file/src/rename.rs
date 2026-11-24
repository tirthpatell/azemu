//! Rename of a file or a directory, and the handle operations (nobody has a share mounted, so there are no handles).

use azemu_core::resp::Resp;
use azemu_core::time::rfc1123;
use azemu_core::Response;

use crate::dirs::{check_names, writable};
use crate::errors;
use crate::leasing::{self, Kind};
use crate::meta;
use crate::smb;
use crate::state::{attr, Account, Node};
use crate::tree;
use crate::xml;
use crate::Cx;

fn flag(c: &Cx, name: &str) -> Result<bool, Response> {
    match c.req.header(name).map(str::trim) {
        None => Ok(false),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
        Some(v) => Err(errors::invalid_header(&c.req.ctx, name, v)),
    }
}

/// `x-ms-file-rename-source`: the URL of the source. Azure refuses anything else (checked in November 2026: a bare path is
/// `InvalidHeaderValue` for `x-ms-copy-source`, and a missing header an `InternalError`; azemu names the rename header in both).
fn source(c: &Cx, share: &str) -> Result<Vec<String>, Response> {
    let ctx = &c.req.ctx;
    let v = c.req.header("x-ms-file-rename-source").ok_or_else(|| errors::missing_header(ctx, "x-ms-file-rename-source"))?;
    let p = if v.contains("://") { crate::copy::parse_url(v) } else { None };
    let Some(p) = p else { return Err(errors::invalid_header(ctx, "x-ms-file-rename-source", v)) };
    if p.share != share || !p.account.eq_ignore_ascii_case(&c.req.account) {
        return Err(errors::err(
            ctx,
            400,
            "RenameAcrossAccountsAndSharesNotSupported",
            "REST Rename does not support renaming a file between different accounts or shares.",
        ));
    }
    Ok(p.path)
}

/// Rename a file or a directory: `restype` is ignored, what the source is decides (Azure did the same in November 2026).
pub fn rename(acct: &mut Account, c: &Cx, share_name: &str, path: &[String]) -> Response {
    let req = c.req;
    let ctx = &req.ctx;
    if let Err(e) = writable(c).and_then(|_| check_names(c, path)) {
        return e;
    }
    let src_path = match source(c, share_name) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = check_names(c, &src_path) {
        return e;
    }
    let (replace, ignore_readonly) = match (flag(c, "x-ms-file-rename-replace-if-exists"), flag(c, "x-ms-file-rename-ignore-readonly")) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(a), Ok(b)) => (a, b),
    };
    let Account { shares, permissions, .. } = acct;
    let Some(share) = shares.get_mut(share_name) else { return errors::share_not_found(ctx) };

    // the source
    let Some((src_last, src_parents)) = src_path.split_last() else { return errors::resource_not_found(ctx) };
    let Some(src_parent) = tree::walk(&share.root, src_parents) else { return errors::parent_not_found(ctx) };
    let Some(src_node) = src_parent.children.get(&src_last.to_lowercase()) else { return errors::resource_not_found(ctx) };
    let directory = src_node.is_dir();
    let (inp, metadata) = match (smb::read(req, directory, true), meta::read(req)) {
        (Err(e), _) | (_, Err(e)) => return e,
        (Ok(i), Ok(m)) => (i, m),
    };
    if let Node::File(f) = src_node {
        if let Err(e) = f.lease.check_write(c.now, req.header("x-ms-source-lease-id")).map_err(|f| leasing::op_error(req, Kind::File, f)) {
            return e;
        }
    }
    // the destination
    let Some((name, parents)) = path.split_last() else { return errors::resource_already_exists(ctx) };
    // names are one name when their lower-case forms are, as the keys of a directory's children are made (not only ASCII letters:
    // `Ä` and `ä` are the same name, and a directory moved into `ä/x` is moved into itself)
    let same_path = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_lowercase() == y.to_lowercase());
    if directory && path.len() > src_path.len() && same_path(&path[..src_path.len()], &src_path) {
        return errors::conflict(ctx, "RenameCycle", "Attempt to move directory to its subdirectory is not supported.");
    }
    let Some(dst_parent) = tree::walk(&share.root, parents) else { return errors::parent_not_found(ctx) };
    if let Some(existing) = dst_parent.children.get(&name.to_lowercase()) {
        if !same_path(&src_path, path) {
            if !replace {
                return errors::resource_already_exists(ctx);
            }
            if existing.is_dir() {
                return errors::err(
                    ctx,
                    400,
                    "RenameDirectoryOperationNotSupported",
                    "REST Rename does not support overwriting a directory.",
                );
            }
            if let Node::File(f) = existing {
                if f.smb.attributes & attr::READ_ONLY != 0 && !ignore_readonly {
                    return errors::conflict(
                        ctx,
                        "ReadOnlyAttribute",
                        "The specified resource is read-only and cannot be modified at this time.",
                    );
                }
                if let Err(e) =
                    f.lease.check_write(c.now, req.header("x-ms-destination-lease-id")).map_err(|f| leasing::op_error(req, Kind::File, f))
                {
                    return e;
                }
            }
        }
    }

    // do it
    let Some(src_parent) = tree::walk_mut(&mut share.root, src_parents) else { return errors::parent_not_found(ctx) };
    let Some(mut node) = src_parent.children.remove(&src_last.to_lowercase()) else { return errors::resource_not_found(ctx) };
    if tree::walk_mut(&mut share.root, parents).is_none() {
        // the destination's directory is not there any more (it was inside what was taken out): put the node back where it was
        if let Some(p) = tree::walk_mut(&mut share.root, src_parents) {
            p.children.insert(src_last.to_lowercase(), node);
        }
        return errors::parent_not_found(ctx);
    }
    if let Some(p) = tree::walk_mut(&mut share.root, src_parents) {
        p.touch(c.now);
    }
    let Some(dst_parent) = tree::walk_mut(&mut share.root, parents) else {
        if let Some(p) = tree::walk_mut(&mut share.root, src_parents) {
            p.children.insert(src_last.to_lowercase(), node);
        }
        return errors::parent_not_found(ctx);
    };
    let parent_id = dst_parent.id;
    let result = match &mut node {
        Node::File(f) => {
            f.name = name.clone();
            f.parent_id = parent_id;
            if !metadata.is_empty() {
                f.metadata = metadata;
            }
            if let Some(t) = req.header("x-ms-content-type") {
                f.content_type = t.to_string();
            }
            let r = smb::set(permissions, req, &mut f.smb, &inp, c.now);
            f.stamp(c.now);
            r.map(|_| (f.etag.clone(), f.last_modified, f.smb.clone(), f.id, f.parent_id))
        }
        Node::Dir(d) => {
            d.name = name.clone();
            d.parent_id = parent_id;
            if !metadata.is_empty() {
                d.metadata = metadata;
            }
            let r = smb::set(permissions, req, &mut d.smb, &inp, c.now);
            d.stamp(c.now);
            r.map(|_| (d.etag.clone(), d.last_modified, d.smb.clone(), d.id, d.parent_id))
        }
    };
    match result {
        Ok((etag, modified, smb_props, id, pid)) => {
            dst_parent.touch(c.now);
            dst_parent.children.insert(name.to_lowercase(), node);
            let r = Resp::new(200u16)
                .header("ETag", etag)
                .header("Last-Modified", rfc1123(modified))
                .header("x-ms-request-server-encrypted", "true");
            smb::headers(r, &smb_props, id, pid).build()
        }
        Err(e) => {
            // put it back where it was
            if let Some(p) = tree::walk_mut(&mut share.root, src_parents) {
                p.children.insert(src_last.to_lowercase(), node);
            }
            e
        }
    }
}

/// List Handles: a share nobody has mounted has none.
pub fn list_handles(acct: &Account, c: &Cx, share_name: &str, path: &[String]) -> Response {
    let req = c.req;
    let share = match crate::shares::find(acct, req, share_name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let snap = match crate::shares::snapshot_param(req) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let root = match crate::dirs::root_of(share, &snap, c) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if let Err(m) = exists(root, path) {
        return m.response(&req.ctx);
    }
    let mut body = String::from("<EnumerationResults>");
    if let Some(n) = req.query.get("maxresults") {
        if let Ok(v) = n.parse::<u32>() {
            body.push_str(&format!("<MaxResults>{v}</MaxResults>"));
        }
    }
    body.push_str("<Entries /><NextMarker /></EnumerationResults>");
    Resp::new(200u16).xml(xml::document(&body)).build()
}

pub fn force_close_handles(acct: &Account, c: &Cx, share_name: &str, path: &[String]) -> Response {
    let req = c.req;
    if req.header("x-ms-handle-id").is_none() {
        return errors::missing_header(&req.ctx, "x-ms-handle-id");
    }
    let share = match crate::shares::find(acct, req, share_name) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if let Err(m) = exists(&share.root, path) {
        return m.response(&req.ctx);
    }
    Resp::new(200u16).header("x-ms-number-of-handles-closed", 0).header("x-ms-number-of-handles-failed", 0).build()
}

fn exists(root: &crate::state::Dir, path: &[String]) -> Result<(), tree::Miss> {
    if path.is_empty() {
        return Ok(());
    }
    tree::node_at(root, path).map(|_| ())
}
