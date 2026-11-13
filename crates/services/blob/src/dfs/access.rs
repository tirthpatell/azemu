//! Set Access Control and Set Access Control Recursive.

use azemu_core::resp::Resp;
use azemu_core::serde_json::{json, Value};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::cond::{self, Kind};
use crate::cx::{refresh_entry, Cx};
use crate::dfs::acl::{self, Entry};
use crate::dfs::auth::require;
use crate::dfs::errors::{acl_error, dfs_error};
use crate::dfs::filesystem::open;
use crate::dfs::model::*;
use crate::dfs::util::{http_date, json as json_resp};
use crate::errors::{err, ApiError, ApiResult};
use crate::lease::{self, Scope};
use crate::state::Container;
use crate::util::hdr;

/// The attributes of a path (or of the root directory when `name` is empty).
fn attrs_mut<'a>(c: &'a mut Container, name: &str) -> Option<&'a mut DfsAttrs> {
    if name.is_empty() {
        return Some(root_attrs(c));
    }
    c.blobs.get_mut(name).and_then(|e| e.live_mut()).and_then(|b| b.dfs.as_deref_mut())
}

pub fn set_access_control(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let now = cx.now;
    let owner = hdr(req, "x-ms-owner").filter(|s| !s.is_empty()).map(str::to_string);
    let group = hdr(req, "x-ms-group").filter(|s| !s.is_empty()).map(str::to_string);
    let perms = match hdr(req, "x-ms-permissions") {
        Some(v) => Some(acl::parse_permissions(v).ok_or_else(|| ApiError::invalid_header("x-ms-permissions", v))?),
        None => None,
    };
    let acl_header = hdr(req, "x-ms-acl").unwrap_or("").to_string();
    let acl_given = match hdr(req, "x-ms-acl") {
        Some(v) => Some(acl::parse_acl(v).map_err(|m| acl_error(v, m))?),
        None => None,
    };
    if perms.is_some() || acl_given.is_some() {
        require(cx, "p")?;
    }
    if owner.is_some() || group.is_some() {
        require(cx, "o")?;
    }
    let c = open(cx, fs)?;
    if let Some(e) = c.blobs.get_mut(name) {
        refresh_entry(e, now);
    }
    let (etag, lm) = if name.is_empty() {
        (c.etag.clone(), c.last_modified)
    } else {
        let b = live(c, name).ok_or_else(|| dfs_error("PathNotFound"))?;
        lease::check_write(&b.lease, hdr(req, "x-ms-lease-id"), Scope::Blob)?;
        (b.etag.clone(), b.last_modified)
    };
    cond::check(req, "", Some((&etag, lm)), Kind::Write)?;
    let a = attrs_mut(c, name).ok_or_else(|| dfs_error("PathNotFound"))?;
    if let Some(given) = acl_given {
        let given: Vec<Entry> = given.into_iter().filter(|e| a.is_dir || !e.default).collect();
        let next = acl::normalize(given, &a.acl);
        acl::check_limits(&next).map_err(|m| acl_error(&acl_header, m))?;
        a.acl = std::sync::Arc::new(next);
    }
    if let Some((bits, sticky)) = perms {
        acl::set_permission_bits(std::sync::Arc::make_mut(&mut a.acl), bits);
        a.sticky = sticky;
    }
    if let Some(o) = owner {
        a.owner = o;
    }
    if let Some(g) = group {
        a.group = g;
    }
    Ok(Resp::new(200u16).header("ETag", &etag).header("Last-Modified", http_date(lm)).header("x-ms-namespace-enabled", "true").build())
}

const DEFAULT_RECORDS: usize = 2000;

pub fn set_access_control_recursive(cx: &mut Cx, fs: &str, name: &str) -> ApiResult {
    let req = cx.req;
    let mode = match req.query.get("mode") {
        None | Some("") => return Err(err("MissingRequiredQueryParameter").detail("QueryParameterName", "mode")),
        Some(m) => m.to_ascii_lowercase(),
    };
    if !["set", "modify", "remove"].contains(&mode.as_str()) {
        return Err(ApiError::invalid_query("mode", req.query.get("mode").unwrap_or("")));
    }
    let acl_text = hdr(req, "x-ms-acl").ok_or_else(|| ApiError::missing_header("x-ms-acl"))?;
    let given =
        if mode == "remove" { acl::parse_acl_remove(acl_text) } else { acl::parse_acl(acl_text) }.map_err(|m| acl_error(acl_text, m))?;
    let max = match req.query.get("maxRecords") {
        None | Some("") => DEFAULT_RECORDS,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n > 0 => (n as usize).min(DEFAULT_RECORDS),
            _ => return Err(ApiError::invalid_query("maxRecords", v)),
        },
    };
    let after: Option<String> = match req.query.get("continuation") {
        None | Some("") => None,
        Some(t) => {
            let bad = || ApiError::invalid_query("continuation", t);
            let raw = String::from_utf8(B64.decode(t).map_err(|_| bad())?).map_err(|_| bad())?;
            Some(raw.strip_prefix("t:").ok_or_else(bad)?.to_string())
        }
    };
    require(cx, "p")?;
    let c = open(cx, fs)?;
    if !name.is_empty() && live(c, name).is_none() {
        return Err(dfs_error("PathNotFound"));
    }
    let mut order: Vec<String> = vec![name.to_string()];
    if name.is_empty() || live(c, name).map(is_dir).unwrap_or(false) {
        order.extend(below(c, name));
    }
    let todo: Vec<String> = order.into_iter().filter(|n| after.as_deref().map(|a| n.as_str() > a).unwrap_or(true)).collect();
    let (mut dirs, mut files) = (0u32, 0u32);
    let mut failed: Vec<Value> = vec![];
    let batch = todo.len().min(max);
    for n in &todo[..batch] {
        let Some(a) = attrs_mut(c, n) else { continue };
        let next = match mode.as_str() {
            "set" => acl::apply_set(&a.acl, &given, a.is_dir),
            "modify" => acl::apply_modify(&a.acl, &given, a.is_dir),
            _ => acl::apply_remove(&a.acl, &given),
        };
        // a path that would end with more than 32 access or 32 default entries is left as it is and reported
        if let Err(why) = acl::check_limits(&next) {
            failed.push(json!({ "name": n, "type": if a.is_dir { "DIRECTORY" } else { "FILE" }, "errorMessage": why }));
            continue;
        }
        if *a.acl != next {
            a.acl = std::sync::Arc::new(next);
        }
        if a.is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
    }
    let body = json!({ "directoriesSuccessful": dirs, "failedEntries": failed, "failureCount": failed.len(), "filesSuccessful": files });
    let mut r = json_resp(200, &body).header("x-ms-namespace-enabled", "true");
    if todo.len() > batch {
        let last = &todo[batch - 1];
        r = r.header("x-ms-continuation", B64.encode(format!("t:{last}").as_bytes()));
    }
    Ok(r.build())
}
