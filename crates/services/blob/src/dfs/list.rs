//! List FileSystems and List Paths.

use azemu_core::serde_json::{json, Map, Value};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

use crate::cx::Cx;
use crate::dfs::filesystem::open;
use crate::dfs::model::{below, filetime, is_dir, live, norm};
use crate::dfs::util::{bare_etag, http_date, json as json_resp};
use crate::errors::{ApiError, ApiResult};
use crate::state::Blob;

const MAX_RESULTS: usize = 5000;

fn max_results(cx: &Cx) -> Result<usize, ApiError> {
    match cx.req.query.get("maxResults") {
        None | Some("") => Ok(MAX_RESULTS),
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n > 0 => Ok((n as usize).min(MAX_RESULTS)),
            Ok(_) => Err(ApiError::out_of_range_query("maxResults", v)),
            Err(_) => Err(ApiError::invalid_query("maxResults", v)),
        },
    }
}

fn decode_token(t: &str) -> Result<String, ApiError> {
    let bad = || ApiError::invalid_query("continuation", t);
    let raw = B64.decode(t).map_err(|_| bad())?;
    String::from_utf8(raw).map_err(|_| bad())
}

pub fn list_filesystems(cx: &mut Cx) -> ApiResult {
    let max = max_results(cx)?;
    let prefix = cx.req.query.get("prefix").unwrap_or("").to_string();
    let after = match cx.req.query.get("continuation") {
        None | Some("") => None,
        Some(t) => Some(decode_token(t)?),
    };
    let acct = cx.account();
    let mut items: Vec<Value> = vec![];
    let mut next: Option<String> = None;
    for (name, c) in acct.containers.iter() {
        if !name.starts_with(&prefix) || name == crate::analytics::LOGS || after.as_deref().map(|a| name.as_str() <= a).unwrap_or(false) {
            continue;
        }
        if items.len() == max {
            next = items.last().and_then(|v| v["name"].as_str()).map(str::to_string);
            break;
        }
        items.push(json!({ "name": name, "lastModified": http_date(c.last_modified), "eTag": bare_etag(&c.etag) }));
    }
    let mut r = json_resp(200, &json!({ "filesystems": items }));
    if let Some(n) = next {
        r = r.header("x-ms-continuation", B64.encode(n.as_bytes()));
    }
    Ok(r.build())
}

/// One entry of a path listing.
pub fn path_json(name: &str, b: &Blob) -> Value {
    let mut m = Map::new();
    m.insert("name".into(), json!(name));
    m.insert("contentLength".into(), json!(b.size.to_string()));
    m.insert("creationTime".into(), json!(filetime(b.creation_time).to_string()));
    m.insert("etag".into(), json!(bare_etag(&b.etag)));
    m.insert("lastModified".into(), json!(http_date(b.last_modified)));
    if let Some(d) = &b.dfs {
        m.insert("owner".into(), json!(d.owner));
        m.insert("group".into(), json!(d.group));
        m.insert("permissions".into(), json!(d.permissions()));
        m.insert("expiryTime".into(), json!(d.expiry.map(filetime).unwrap_or(0).to_string()));
        if d.is_dir {
            m.insert("isDirectory".into(), json!("true"));
        }
        if let Some(ctx) = &d.encryption_context {
            m.insert("EncryptionContext".into(), json!(ctx));
        }
    }
    if let Some(s) = &b.encryption_scope {
        m.insert("EncryptionScope".into(), json!(s));
    }
    Value::Object(m)
}

pub fn list_paths(cx: &mut Cx, fs: &str) -> ApiResult {
    let req = cx.req;
    let recursive = match req.query.get("recursive") {
        Some(v) if v.eq_ignore_ascii_case("true") => true,
        Some(v) if v.eq_ignore_ascii_case("false") => false,
        Some(v) => return Err(ApiError::invalid_query("recursive", v)),
        None => return Err(crate::errors::err("MissingRequiredQueryParameter").detail("QueryParameterName", "recursive")),
    };
    let directory = norm(req.query.get("directory").unwrap_or(""));
    let max = max_results(cx)?;
    let begin_from = req.query.get("beginFrom").unwrap_or("").to_string();
    let after = match req.query.get("continuation") {
        None | Some("") => None,
        Some(t) => Some(decode_token(t)?),
    };
    let c = open(cx, fs)?;
    if !directory.is_empty() && !live(c, &directory).map(is_dir).unwrap_or(false) {
        return Err(crate::dfs::errors::dfs_error("PathNotFound"));
    }
    let prefix_len = if directory.is_empty() { 0 } else { directory.len() + 1 };
    let mut items: Vec<Value> = vec![];
    let mut last: Option<String> = None;
    let mut more = false;
    for name in below(c, &directory) {
        if !recursive && name[prefix_len..].contains('/') {
            continue;
        }
        // `beginFrom` is relative to the directory that is listed
        if name[prefix_len..] < *begin_from.as_str() || after.as_deref().map(|a| name.as_str() <= a).unwrap_or(false) {
            continue;
        }
        if items.len() == max {
            more = true;
            break;
        }
        if let Some(b) = live(c, &name) {
            items.push(path_json(&name, b));
            last = Some(name);
        }
    }
    // A continuation token goes with a page when entries are left. Azure also sends one when
    // the page is full and its last entry is a directory it has not looked into yet (the
    // recordings end such a listing with one more, empty page).
    let last_is_dir = last.as_deref().and_then(|l| live(c, l)).map(is_dir).unwrap_or(false);
    let mut r = json_resp(200, &json!({ "paths": items }));
    if more || (recursive && items.len() == max && last_is_dir) {
        if let Some(l) = last {
            r = r.header("x-ms-continuation", B64.encode(l.as_bytes()));
        }
    }
    Ok(r.build())
}
