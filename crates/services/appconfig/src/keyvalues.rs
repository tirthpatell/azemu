//! `GET/HEAD/PUT/DELETE /kv/{key}`.

use azemu_core::conditions::{Access, Conditions, Outcome, Resource};
use azemu_core::resp::Resp;
use azemu_core::{Request, Response};
use percent_encoding::percent_decode_str;
use serde_json::Value;

use crate::auth;
use crate::errors;
use crate::filters::parse_select;
use crate::paging::memento;
use crate::render::{kv_response, KV_CT, KV_FIELDS};
use crate::state::{NewSetting, Setting, Store};
use crate::timefmt::parse_datetime;

/// The key in the path after `/{prefix}/`, percent-decoded. Slashes inside a key are kept.
pub fn key_from_path(req: &Request, prefix: &str) -> Result<String, Response> {
    let rest = req.path.strip_prefix('/').unwrap_or(&req.path);
    let rest = rest.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')).unwrap_or("");
    let key = percent_decode_str(rest)
        .decode_utf8()
        .map_err(|_| errors::invalid_argument("key", "key: The key is not valid UTF-8.", None))?
        .into_owned();
    if key.is_empty() || key == "." || key == ".." {
        return Err(errors::invalid_argument("key", &format!("key: '{key}' is not a valid key."), None));
    }
    Ok(key)
}

/// `label` of a single-key request: absent, empty or `\0` mean the null label.
pub fn label_param(req: &Request) -> Option<String> {
    match req.query.get("label") {
        None | Some("") | Some("\0") | Some("\\0") => None,
        Some(l) => Some(l.to_string()),
    }
}

/// Only `If-Match` and `If-None-Match` apply to settings.
pub fn conditions(req: &Request) -> Conditions<'_> {
    Conditions {
        if_match: req.header("if-match"),
        if_none_match: req.header("if-none-match"),
        if_modified_since: None,
        if_unmodified_since: None,
    }
}

pub fn resource(s: Option<&Setting>) -> Resource<'_> {
    match s {
        Some(s) => Resource::Exists { etag: &s.etag, last_modified: s.last_modified },
        None => Resource::Missing,
    }
}

pub fn accept_datetime(req: &Request) -> Result<Option<azemu_core::time::OffsetDateTime>, Response> {
    match req.header("accept-datetime") {
        None => Ok(None),
        Some(v) => parse_datetime(v)
            .map(Some)
            .ok_or_else(|| errors::invalid_argument("Accept-Datetime", "Accept-Datetime: The value is not a valid date.", None)),
    }
}

pub fn select_param(req: &Request, allowed: &[&'static str]) -> Result<Option<Vec<&'static str>>, Response> {
    parse_select(req.query.get_all("$select"), allowed)
        .map_err(|f| errors::invalid_argument("$select", &format!("$select: '{f}' is not a valid field."), None))
}

pub fn get(store: &Store, req: &Request) -> Response {
    let key = match key_from_path(req, "kv") {
        Ok(k) => k,
        Err(r) => return r,
    };
    let label = label_param(req);
    let select = match select_param(req, KV_FIELDS) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let at = match accept_datetime(req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let setting = match at {
        Some(t) => store.get_at(&key, label.as_deref(), t),
        None => store.live_get(&key, label.as_deref()),
    };
    let Some(s) = setting else {
        return errors::not_found();
    };
    match conditions(req).evaluate(resource(Some(s)), Access::Read) {
        Outcome::Proceed => {}
        Outcome::NotModified => return errors::empty(304),
        Outcome::PreconditionFailed => return errors::precondition_failed(),
    }
    let mut resp = kv_response(200, s, select.as_deref(), auth::is_newer(auth::version(req)));
    if let Some(t) = at {
        if let Ok(v) = azemu_core::http::HeaderValue::from_str(&memento(t)) {
            resp.headers_mut().insert("Memento-Datetime", v);
        }
    }
    resp
}

/// What a PUT body may carry. Fields the SDK echoes back (etag, last_modified, locked,
/// label) are ignored: the label comes from the query.
struct Body {
    value: Option<String>,
    content_type: Option<String>,
    description: Option<String>,
    tags: Vec<(String, String)>,
}

fn opt_string(v: Option<&Value>, name: &str) -> Result<Option<String>, Response> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(errors::invalid_body(&format!("'{name}' must be a string or null."))),
    }
}

fn parse_body(req: &Request) -> Result<Body, Response> {
    let v: Value = if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        Value::Object(Default::default())
    } else {
        serde_json::from_slice(&req.body).map_err(|e| errors::invalid_body(&format!("The body is not valid JSON: {e}")))?
    };
    let Value::Object(m) = v else {
        return Err(errors::invalid_body("The body must be a JSON object."));
    };
    let mut tags = Vec::new();
    match m.get("tags") {
        None | Some(Value::Null) => {}
        Some(Value::Object(t)) => {
            for (k, v) in t {
                match v {
                    Value::String(s) => tags.push((k.clone(), s.clone())),
                    _ => return Err(errors::invalid_body("Tag values must be strings.")),
                }
            }
        }
        Some(_) => return Err(errors::invalid_body("'tags' must be an object.")),
    }
    Ok(Body {
        value: opt_string(m.get("value"), "value")?,
        content_type: opt_string(m.get("content_type"), "content_type")?,
        description: opt_string(m.get("description"), "description")?,
        tags,
    })
}

pub fn put(store: &mut Store, req: &Request) -> Response {
    let key = match key_from_path(req, "kv") {
        Ok(k) => k,
        Err(r) => return r,
    };
    let label = label_param(req);
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let existing = store.live_get(&key, label.as_deref());
    if !matches!(conditions(req).evaluate(resource(existing), Access::Write), Outcome::Proceed) {
        return errors::precondition_failed();
    }
    if existing.map(|e| e.locked).unwrap_or(false) {
        return errors::key_locked(&key);
    }
    let written = store.write(
        NewSetting {
            key,
            label,
            value: body.value,
            content_type: body.content_type,
            description: body.description,
            tags: body.tags,
            locked: false,
        },
        req.ctx.time,
    );
    match written {
        Ok(s) => kv_response(200, &s, None, auth::is_newer(auth::version(req))),
        Err(_) => errors::quota_exceeded("Storage Quota"),
    }
}

pub fn delete(store: &mut Store, req: &Request) -> Response {
    let key = match key_from_path(req, "kv") {
        Ok(k) => k,
        Err(r) => return r,
    };
    let label = label_param(req);
    let existing = store.live_get(&key, label.as_deref());
    if existing.is_none() && req.header("if-match").is_none() {
        return Resp::new(204u16).header("Content-Type", KV_CT).build();
    }
    if !matches!(conditions(req).evaluate(resource(existing), Access::Write), Outcome::Proceed) {
        return errors::precondition_failed();
    }
    if existing.map(|e| e.locked).unwrap_or(false) {
        return errors::key_locked(&key);
    }
    match store.delete(&key, label.as_deref(), req.ctx.time) {
        Some(s) => kv_response(200, &s, None, auth::is_newer(auth::version(req))),
        None => Resp::new(204u16).header("Content-Type", KV_CT).build(),
    }
}
