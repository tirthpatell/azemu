//! Generic create / read / replace / delete / list for definition-only resources: users,
//! permissions, stored procedures, triggers and user defined functions.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::errors::{check_id, CosmosError, R};
use crate::out::{Cx, Out};
use crate::query;
use crate::rid;
use crate::state::{Meta, Resource};

/// A response for a stored resource: JSON body plus `etag` header.
pub fn resource_out(status: u16, v: Value) -> Out {
    let etag = v.get("_etag").and_then(Value::as_str).unwrap_or("").to_string();
    Out::json(status, v).header("etag", etag)
}

pub type Validator = fn(&Map<String, Value>) -> R<()>;

/// Where a set of resources lives.
pub struct Scope<'a> {
    pub map: &'a mut BTreeMap<String, Resource>,
    pub parent_bytes: &'a [u8],
    pub seq: &'a mut u64,
    /// e.g. `dbs/{rid}/colls/{rid}/sprocs/`; the item link is this plus `{rid}/`.
    pub link_prefix: String,
    /// Key of the list in a feed: `StoredProcedures`, `Users`, ...
    pub list_key: &'static str,
    pub parent_rid: String,
    /// What kind of id a new resource gets (the Java SDK reads the kind from the id).
    pub kind: rid::Kind,
    pub validate: Validator,
    /// Extra `_xxx` fields, e.g. the permission token.
    pub extras: fn(&Resource) -> Vec<(&'static str, String)>,
}

pub fn no_extras(_: &Resource) -> Vec<(&'static str, String)> {
    Vec::new()
}

impl Scope<'_> {
    fn key_of(&self, key: &str) -> Option<String> {
        if self.map.contains_key(key) {
            return Some(key.to_string());
        }
        self.map.values().find(|r| rid::matches(&r.meta.rid, key)).map(|r| r.meta.id.clone())
    }

    fn render(&self, r: &Resource) -> Value {
        let ex = (self.extras)(r);
        let ex: Vec<(&str, &str)> = ex.iter().map(|(k, v)| (*k, v.as_str())).collect();
        r.meta.json(&r.props, &format!("{}{}/", self.link_prefix, r.meta.rid), &ex)
    }

    pub fn create(&mut self, cx: &Cx, upsert: bool) -> R<Out> {
        let body = cx.body_object()?;
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| CosmosError::bad_request("The input content is invalid because the required properties - 'id; ' - are missing"))?
            .to_string();
        check_id(&id)?;
        (self.validate)(&body)?;
        if let Some(existing) = self.map.get_mut(&id) {
            if !upsert {
                return Err(CosmosError::conflict());
            }
            existing.props = strip(&body);
            existing.meta.touch(cx.now);
            let v = self.render(self.map.get(&id).ok_or_else(CosmosError::not_found_default)?);
            return Ok(resource_out(200, v));
        }
        let (bytes, used) = rid::child_of(self.parent_bytes, *self.seq + 1, self.kind);
        *self.seq = used;
        let meta = Meta::new(&id, bytes, used, cx.now);
        let r = Resource { meta, props: strip(&body) };
        let v = self.render(&r);
        self.map.insert(id, r);
        Ok(resource_out(201, v))
    }

    pub fn read(&self, key: &str) -> R<Out> {
        let k = self.key_of(key).ok_or_else(CosmosError::not_found_default)?;
        let r = self.map.get(&k).ok_or_else(CosmosError::not_found_default)?;
        Ok(resource_out(200, self.render(r)))
    }

    pub fn replace(&mut self, cx: &Cx, key: &str) -> R<Out> {
        let body = cx.body_object()?;
        let k = self.key_of(key).ok_or_else(CosmosError::not_found_default)?;
        (self.validate)(&body)?;
        let new_id = body.get("id").and_then(Value::as_str).unwrap_or(&k).to_string();
        check_id(&new_id)?;
        if new_id != k && self.map.contains_key(&new_id) {
            return Err(CosmosError::conflict());
        }
        let mut r = self.map.remove(&k).ok_or_else(CosmosError::not_found_default)?;
        r.props = strip(&body);
        r.meta.id = new_id.clone();
        r.meta.touch(cx.now);
        let v = self.render(&r);
        self.map.insert(new_id, r);
        Ok(resource_out(200, v))
    }

    pub fn delete(&mut self, key: &str) -> R<Out> {
        let k = self.key_of(key).ok_or_else(CosmosError::not_found_default)?;
        self.map.remove(&k);
        Ok(Out::empty(204))
    }

    pub fn list(&self, cx: &Cx) -> R<Out> {
        let mut items: Vec<&Resource> = self.map.values().collect();
        items.sort_by_key(|r| r.meta.seq);
        query::feed(cx, &self.parent_rid, self.list_key, items.into_iter().map(|r| self.render(r)).collect())
    }

    /// Resource name for a key (used when a user is deleted or renamed).
    pub fn name_of(&self, key: &str) -> Option<String> {
        self.key_of(key)
    }
}

/// The body without the properties the service owns.
fn strip(body: &Map<String, Value>) -> Map<String, Value> {
    body.iter()
        .filter(|(k, _)| !matches!(k.as_str(), "id" | "_rid" | "_self" | "_etag" | "_ts" | "_token"))
        .map(|(k, v)| (k.clone(), canonical(k, v)))
        .collect()
}

/// The service stores enum-like strings in their canonical case ("Pre", "All", "Read").
fn canonical(key: &str, v: &Value) -> Value {
    let Some(s) = v.as_str() else { return v.clone() };
    let table: &[&str] = match key {
        "triggerType" => &["Pre", "Post"],
        "triggerOperation" => &["All", "Create", "Update", "Delete", "Replace"],
        "permissionMode" => &["Read", "All"],
        _ => return v.clone(),
    };
    table.iter().find(|t| t.eq_ignore_ascii_case(s)).map(|t| Value::String((*t).to_string())).unwrap_or_else(|| v.clone())
}

pub fn validate_none(_: &Map<String, Value>) -> R<()> {
    Ok(())
}

fn need_str(b: &Map<String, Value>, k: &str) -> R<()> {
    match b.get(k) {
        Some(Value::String(_)) => Ok(()),
        _ => Err(CosmosError::bad_request(format!("The input content is invalid because the required properties - '{k}; ' - are missing"))),
    }
}

pub fn validate_body(b: &Map<String, Value>) -> R<()> {
    need_str(b, "body")
}

pub fn validate_trigger(b: &Map<String, Value>) -> R<()> {
    need_str(b, "body")?;
    need_str(b, "triggerType")?;
    need_str(b, "triggerOperation")?;
    let t = b.get("triggerType").and_then(Value::as_str).unwrap_or("");
    if !t.eq_ignore_ascii_case("pre") && !t.eq_ignore_ascii_case("post") {
        return Err(CosmosError::bad_request("triggerType must be Pre or Post."));
    }
    let o = b.get("triggerOperation").and_then(Value::as_str).unwrap_or("");
    if !["all", "create", "update", "delete", "replace"].contains(&o.to_ascii_lowercase().as_str()) {
        return Err(CosmosError::bad_request("triggerOperation must be All, Create, Update, Delete or Replace."));
    }
    Ok(())
}

pub fn validate_permission(b: &Map<String, Value>) -> R<()> {
    need_str(b, "resource")?;
    let m = b.get("permissionMode").and_then(Value::as_str).unwrap_or("");
    if !m.eq_ignore_ascii_case("read") && !m.eq_ignore_ascii_case("all") {
        return Err(CosmosError::bad_request("permissionMode must be Read or All."));
    }
    Ok(())
}

pub fn permission_extras(r: &Resource) -> Vec<(&'static str, String)> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let sig = STANDARD.encode(format!("azemu-permission:{}:{}", r.meta.rid, r.meta.etag));
    vec![("_token", format!("type=resource&ver=1&sig={sig};{}", STANDARD.encode(&r.meta.rid_bytes)))]
}
