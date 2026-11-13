//! Queue-level operations: list, create, delete, metadata and access policies.

use std::collections::BTreeMap;

use azemu_core::resp::Resp;
use azemu_core::storage;
use azemu_core::time::{iso8601_7, parse_iso8601};
use azemu_core::{Request, Response};

use crate::errors;
use crate::state::{Queue, SignedId, State};
use crate::xml::{self, Node};

const MAX_LIST_RESULTS: usize = 5000;
const MAX_METADATA_BYTES: usize = 8 * 1024;
const MAX_SIGNED_IDENTIFIERS: usize = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum NameError {
    Characters,
    Length,
}

impl NameError {
    pub fn response(&self, ctx: &azemu_core::RequestCtx) -> Response {
        match self {
            NameError::Characters => errors::invalid_resource_name(ctx),
            NameError::Length => errors::name_out_of_range(ctx),
        }
    }
}

/// Queue names: 3 to 63 characters, lower-case letters, digits and single hyphens,
/// starting and ending with a letter or digit.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    let ok_chars = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !ok_chars {
        return Err(NameError::Characters);
    }
    if !(3..=63).contains(&name.len()) {
        return Err(NameError::Length);
    }
    Ok(())
}

/// `x-ms-meta-*` headers as (name, value). Names must be C# identifiers.
fn read_metadata(req: &Request) -> Result<Vec<(String, String)>, Response> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut size = 0;
    for (name, value) in req.headers.iter() {
        let Some(meta) = name.as_str().strip_prefix("x-ms-meta-") else { continue };
        let Ok(value) = value.to_str() else {
            return Err(errors::invalid_metadata(&req.ctx));
        };
        let mut chars = meta.chars();
        let valid =
            chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(errors::invalid_metadata(&req.ctx));
        }
        size += meta.len() + value.len();
        out.push((meta.to_string(), value.trim().to_string()));
    }
    if size > MAX_METADATA_BYTES {
        return Err(errors::metadata_too_large(&req.ctx));
    }
    out.sort();
    Ok(out)
}

fn same_metadata(a: &[(String, String)], b: &[(String, String)]) -> bool {
    let norm =
        |m: &[(String, String)]| -> BTreeMap<String, String> { m.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect() };
    norm(a) == norm(b)
}

pub fn create(state: &mut State, req: &Request, name: &str) -> Response {
    let metadata = match read_metadata(req) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let acct = state.accounts.entry(req.account.clone()).or_default();
    if let Some(existing) = acct.queues.get(name) {
        return if same_metadata(&existing.metadata, &metadata) {
            Resp::new(204u16).build()
        } else {
            errors::queue_already_exists(&req.ctx)
        };
    }
    acct.queues.insert(name.to_string(), Queue { metadata, ..Queue::default() });
    Resp::new(201u16).build()
}

pub fn delete(state: &mut State, req: &Request, name: &str) -> Response {
    let acct = state.accounts.entry(req.account.clone()).or_default();
    if acct.queues.remove(name).is_some() {
        Resp::new(204u16).build()
    } else {
        errors::queue_not_found(&req.ctx)
    }
}

pub fn get_metadata(state: &mut State, req: &Request, name: &str) -> Response {
    let acct = state.accounts.entry(req.account.clone()).or_default();
    let Some(q) = acct.queues.get_mut(name) else {
        return errors::queue_not_found(&req.ctx);
    };
    let count = q.approximate_count(req.ctx.time);
    let mut r = Resp::new(200u16).header("Cache-Control", "no-cache").header("x-ms-approximate-messages-count", count);
    for (k, v) in &q.metadata {
        r = r.header(format!("x-ms-meta-{k}"), v);
    }
    r.build()
}

pub fn set_metadata(state: &mut State, req: &Request, name: &str) -> Response {
    let metadata = match read_metadata(req) {
        Ok(m) => m,
        Err(r) => return r,
    };
    let acct = state.accounts.entry(req.account.clone()).or_default();
    let Some(q) = acct.queues.get_mut(name) else {
        return errors::queue_not_found(&req.ctx);
    };
    q.metadata = metadata;
    Resp::new(204u16).build()
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn list(state: &mut State, req: &Request) -> Response {
    let ctx = &req.ctx;
    let prefix = req.query.get("prefix").unwrap_or("");
    let marker = req.query.get("marker").unwrap_or("");
    let include_metadata = match req.query.get("include") {
        None | Some("") => false,
        Some(v) => {
            let parts: Vec<&str> = v.split(',').map(str::trim).collect();
            if parts.iter().any(|p| !p.eq_ignore_ascii_case("metadata")) {
                return errors::invalid_query(ctx, "include", v, "");
            }
            true
        }
    };
    let max = match req.query.get("maxresults") {
        None => None,
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 1 => Some(n),
            Ok(_) => return errors::out_of_range_query(ctx, "maxresults", v, 1, MAX_LIST_RESULTS as i64),
            Err(_) => return errors::invalid_query(ctx, "maxresults", v, ""),
        },
    };
    let limit = max.map_or(MAX_LIST_RESULTS, |n| (n as usize).min(MAX_LIST_RESULTS));
    // A marker is "/{account}/{queue}": the first queue of the next page.
    let start = marker.rsplit('/').next().unwrap_or("");

    let empty = BTreeMap::new();
    let queues = state.accounts.get(&req.account).map_or(&empty, |a| &a.queues);
    let mut matching = queues.iter().filter(|(n, _)| n.starts_with(prefix) && n.as_str() >= start);
    let page: Vec<(&String, &Queue)> = matching.by_ref().take(limit).collect();
    let next = matching.next().map(|(n, _)| n.clone());

    let mut body = format!("<EnumerationResults ServiceEndpoint=\"{}/\">", storage::xml_escape(&ctx.endpoint.base_url()));
    if !prefix.is_empty() {
        body.push_str(&xml::element("Prefix", prefix));
    }
    if !marker.is_empty() {
        body.push_str(&xml::element("Marker", marker));
    }
    if let Some(n) = max {
        body.push_str(&format!("<MaxResults>{n}</MaxResults>"));
    }
    if page.is_empty() {
        body.push_str("<Queues />");
    } else {
        body.push_str("<Queues>");
        for (name, q) in page {
            body.push_str("<Queue>");
            body.push_str(&xml::element("Name", name));
            if include_metadata {
                if q.metadata.is_empty() {
                    body.push_str("<Metadata />");
                } else {
                    body.push_str("<Metadata>");
                    for (k, v) in &q.metadata {
                        body.push_str(&xml::element(k, v));
                    }
                    body.push_str("</Metadata>");
                }
            }
            body.push_str("</Queue>");
        }
        body.push_str("</Queues>");
    }
    match next {
        Some(n) => body.push_str(&xml::element("NextMarker", &format!("/{}/{}", req.account, n))),
        None => body.push_str("<NextMarker />"),
    }
    body.push_str("</EnumerationResults>");
    Resp::new(200u16).xml(xml::document(&body)).header("Cache-Control", "no-cache").build()
}

// ---------------------------------------------------------------------------
// access policies
// ---------------------------------------------------------------------------

pub fn get_acl(state: &mut State, req: &Request, name: &str) -> Response {
    let acct = state.accounts.entry(req.account.clone()).or_default();
    let Some(q) = acct.queues.get(name) else {
        return errors::queue_not_found(&req.ctx);
    };
    let body = if q.acl.is_empty() {
        "<SignedIdentifiers />".to_string()
    } else {
        let mut s = String::from("<SignedIdentifiers>");
        for id in &q.acl {
            s.push_str("<SignedIdentifier>");
            s.push_str(&xml::element("Id", &id.id));
            if id.has_policy {
                s.push_str("<AccessPolicy>");
                for (tag, value) in [("Start", &id.start), ("Expiry", &id.expiry), ("Permission", &id.permission)] {
                    if let Some(v) = value {
                        s.push_str(&xml::element(tag, v));
                    }
                }
                s.push_str("</AccessPolicy>");
            }
            s.push_str("</SignedIdentifier>");
        }
        s.push_str("</SignedIdentifiers>");
        s
    };
    Resp::new(200u16).xml(xml::document(&body)).header("Cache-Control", "no-cache").build()
}

/// Times are stored the way Azure returns them: always with seven fraction digits.
fn normalize_time(v: &str) -> Option<String> {
    parse_iso8601(v.trim()).map(iso8601_7)
}

fn parse_identifier(n: &Node) -> Result<SignedId, ()> {
    let id = n.child_text("Id").ok_or(())?.to_string();
    if id.is_empty() || id.len() > 64 {
        return Err(());
    }
    let mut out = SignedId { id, ..SignedId::default() };
    if let Some(p) = n.child("AccessPolicy") {
        out.has_policy = true;
        if let Some(s) = p.child_text("Start").filter(|s| !s.trim().is_empty()) {
            out.start = Some(normalize_time(s).ok_or(())?);
        }
        if let Some(s) = p.child_text("Expiry").filter(|s| !s.trim().is_empty()) {
            out.expiry = Some(normalize_time(s).ok_or(())?);
        }
        if let Some(perm) = p.child_text("Permission") {
            let perm = perm.trim();
            if !perm.chars().all(|c| "raup".contains(c)) {
                return Err(());
            }
            out.permission = Some(perm.to_string());
        }
    }
    Ok(out)
}

pub fn set_acl(state: &mut State, req: &Request, name: &str) -> Response {
    let ctx = &req.ctx;
    // An empty body clears the policies.
    let ids: Vec<SignedId> = if req.body.iter().all(u8::is_ascii_whitespace) {
        vec![]
    } else {
        let Ok(body) = std::str::from_utf8(&req.body) else {
            return errors::invalid_xml_no_pos(ctx, "Error parsing Xml content");
        };
        let root = match xml::parse(body) {
            Ok(r) => r,
            Err(e) => return errors::invalid_xml(ctx, e.pos, &e.reason),
        };
        if root.name != "SignedIdentifiers" {
            return errors::invalid_xml(ctx, root.end, "Error parsing Xml content");
        }
        let nodes: Vec<&Node> = root.children_named("SignedIdentifier").collect();
        if nodes.len() > MAX_SIGNED_IDENTIFIERS {
            return errors::invalid_xml_no_pos(ctx, "");
        }
        let mut ids: Vec<SignedId> = Vec::new();
        for n in nodes {
            match parse_identifier(n) {
                Ok(id) if !ids.iter().any(|x| x.id == id.id) => ids.push(id),
                _ => return errors::invalid_xml(ctx, n.end, "Error parsing Xml content"),
            }
        }
        ids
    };
    let acct = state.accounts.entry(req.account.clone()).or_default();
    let Some(q) = acct.queues.get_mut(name) else {
        return errors::queue_not_found(ctx);
    };
    q.acl = ids;
    Resp::new(204u16).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["abc", "a-b", "a1b2", &"a".repeat(63), "123"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["UpperCase", "my--queue", "-abc", "abc-", "a_b", "啊齄丂狛", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        for bad in ["ab", "", &"a".repeat(64)] {
            assert_eq!(validate_name(bad), Err(NameError::Length), "{bad}");
        }
    }
}
