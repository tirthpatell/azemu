//! Entity payloads and the write operations on a table's entities.
//!
//! Every write works on a `&mut Table` and returns a `Mutation` that can be
//! undone, which is how entity group transactions stay atomic.

use crate::errors::{condition_failed, entity_exists, invalid_input, needs_value, resource_not_found, ApiError, Res};
use crate::json::{self, J};
use crate::state::{key, Entity, Key, Table};
use crate::values::{decode_value, valid_property_name, Ticks, Value};

pub const MAX_CUSTOM_PROPERTIES: usize = 252;
const MAX_STRING_UNITS: usize = 32 * 1024;
const MAX_BINARY: usize = 64 * 1024;
const MAX_ENTITY_BYTES: usize = 1024 * 1024;
const MAX_KEY_UNITS: usize = 512;
const MAX_NAME_LEN: usize = 255;

/// A decoded request body.
#[derive(Debug, Clone, Default)]
pub struct Payload {
    pub pk: Option<String>,
    pub rk: Option<String>,
    pub props: Vec<(String, Value)>,
}

/// Keys may not contain `/ \ # ?` or control characters and are at most 1 KiB
/// of UTF-16 (512 characters).
pub fn validate_key(k: &str) -> Res<()> {
    if k.encode_utf16().count() > MAX_KEY_UNITS {
        return Err(invalid_input());
    }
    if k.chars().any(|c| matches!(c, '/' | '\\' | '#' | '?') || ('\u{0}'..='\u{1f}').contains(&c) || ('\u{7f}'..='\u{9f}').contains(&c)) {
        return Err(invalid_input());
    }
    Ok(())
}

/// Decode an entity body. `Timestamp` and `odata.*` members are ignored, null
/// properties are dropped. Keys are required only when `require_keys` is set.
pub fn decode(body: &[u8], require_keys: bool) -> Res<Payload> {
    let text = std::str::from_utf8(body).map_err(|_| invalid_input())?;
    let J::Obj(members) = json::parse(text).map_err(|_| invalid_input())? else {
        return Err(invalid_input());
    };
    let annotation = |name: &str| -> Option<&str> {
        members.iter().find(|(k, _)| k.strip_suffix("@odata.type") == Some(name)).and_then(|(_, v)| v.as_str())
    };
    let mut out = Payload::default();
    for (name, v) in &members {
        if name.ends_with("@odata.type") || name.starts_with("odata.") || name == "Timestamp" {
            continue;
        }
        if name == "PartitionKey" || name == "RowKey" {
            let J::Str(s) = v else {
                return Err(invalid_input());
            };
            // An insert refuses keys annotated with another type (Edm.DateTime, ...);
            // updates take their keys from the URL and ignore the annotation.
            if require_keys && annotation(name).is_some_and(|a| a != "Edm.String") {
                return Err(invalid_input());
            }
            if name == "PartitionKey" {
                out.pk = Some(s.clone());
            } else {
                out.rk = Some(s.clone());
            }
            continue;
        }
        if !valid_property_name(name) {
            return Err(ApiError::new(400, "PropertyNameInvalid", "The property name is invalid."));
        }
        if name.chars().count() > MAX_NAME_LEN {
            return Err(ApiError::new(400, "PropertyNameTooLong", "The property name exceeds the maximum allowed length (255)."));
        }
        if let Some(val) = decode_value(v, annotation(name))? {
            check_size(&val)?;
            out.props.push((name.clone(), val));
        }
    }
    check_count(out.props.len())?;
    if require_keys && (out.pk.is_none() || out.rk.is_none()) {
        return Err(needs_value());
    }
    if let Some(p) = &out.pk {
        validate_key(p)?;
    }
    if let Some(r) = &out.rk {
        validate_key(r)?;
    }
    Ok(out)
}

fn check_count(n: usize) -> Res<()> {
    if n > MAX_CUSTOM_PROPERTIES {
        return Err(ApiError::new(
            400,
            "TooManyProperties",
            "The entity contains more properties than allowed. Each entity can include up to 252 properties to store data. Each entity also has 3 system properties.",
        ));
    }
    Ok(())
}

fn check_size(v: &Value) -> Res<()> {
    let too_big = match v {
        Value::Str(s) => s.encode_utf16().count() > MAX_STRING_UNITS,
        Value::Binary(b) => b.len() > MAX_BINARY,
        _ => false,
    };
    if too_big {
        return Err(ApiError::new(
            400,
            "PropertyValueTooLarge",
            "The property value exceeds the maximum allowed size (64KB). If the property value is a string, it is UTF-16 encoded and the maximum number of characters should be 32K or less.",
        ));
    }
    Ok(())
}

fn check_entity(e: &Entity) -> Res<()> {
    check_count(e.props.len())?;
    if e.size() > MAX_ENTITY_BYTES {
        return Err(ApiError::new(400, "EntityTooLarge", "The entity is larger than the maximum allowed size (1MB)."));
    }
    Ok(())
}

/// The `If-Match` of an update or delete.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// No header: insert-or-replace / insert-or-merge.
    Any,
    /// `If-Match: *`
    Star,
    Tag(String),
}

impl Cond {
    pub fn from_header(h: Option<&str>) -> Cond {
        match h.map(str::trim) {
            None | Some("") => Cond::Any,
            Some("*") => Cond::Star,
            Some(t) => Cond::Tag(t.to_string()),
        }
    }

    fn matches(&self, e: &Entity) -> bool {
        match self {
            Cond::Any | Cond::Star => true,
            Cond::Tag(t) => strip_weak(t) == strip_weak(&e.etag()),
        }
    }
}

fn strip_weak(t: &str) -> &str {
    t.trim().strip_prefix("W/").unwrap_or(t.trim())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Replace,
    Merge,
}

/// What a write did, enough to undo it and to build its response.
#[derive(Debug)]
pub struct Mutation {
    pub key: Key,
    pub previous: Option<Entity>,
    /// The entity as stored afterwards; `None` after a delete.
    pub stored: Option<Entity>,
}

impl Table {
    pub fn undo(&mut self, m: Mutation) {
        match m.previous {
            Some(e) => {
                self.entities.insert(m.key, e);
            }
            None => {
                self.entities.remove(&m.key);
            }
        }
    }

    pub fn insert_entity(&mut self, p: Payload, now: Ticks) -> Res<Mutation> {
        let (pk, rk) = (p.pk.ok_or_else(needs_value)?, p.rk.ok_or_else(needs_value)?);
        let k = key(&pk, &rk);
        if self.entities.contains_key(&k) {
            return Err(entity_exists());
        }
        let e = Entity { pk, rk, props: p.props, ts: now };
        check_entity(&e)?;
        self.entities.insert(k.clone(), e.clone());
        Ok(Mutation { key: k, previous: None, stored: Some(e) })
    }

    /// Replace or merge the entity at `pk`/`rk`. Without `If-Match` a missing
    /// entity is created (insert-or-replace / insert-or-merge).
    pub fn update_entity(&mut self, pk: &str, rk: &str, p: Payload, mode: Mode, cond: &Cond, now: Ticks) -> Res<Mutation> {
        if p.pk.as_deref().is_some_and(|v| v != pk) || p.rk.as_deref().is_some_and(|v| v != rk) {
            return Err(invalid_input());
        }
        let k = key(pk, rk);
        let previous = self.entities.get(&k).cloned();
        let e = match &previous {
            None => {
                if *cond != Cond::Any {
                    return Err(resource_not_found());
                }
                Entity { pk: pk.to_string(), rk: rk.to_string(), props: p.props, ts: now }
            }
            Some(old) => {
                if !cond.matches(old) {
                    return Err(condition_failed());
                }
                let mut props = if mode == Mode::Merge { old.props.clone() } else { Vec::new() };
                for (name, v) in p.props {
                    match props.iter_mut().find(|(n, _)| *n == name) {
                        Some(slot) => slot.1 = v,
                        None => props.push((name, v)),
                    }
                }
                // a new etag even when nothing changed
                Entity { pk: pk.to_string(), rk: rk.to_string(), props, ts: now.max(old.ts + 1) }
            }
        };
        check_entity(&e)?;
        self.entities.insert(k.clone(), e.clone());
        Ok(Mutation { key: k, previous, stored: Some(e) })
    }

    pub fn delete_entity(&mut self, pk: &str, rk: &str, cond: &Cond) -> Res<Mutation> {
        let k = key(pk, rk);
        let Some(old) = self.entities.get(&k) else {
            return Err(resource_not_found());
        };
        if !cond.matches(old) {
            return Err(condition_failed());
        }
        let previous = self.entities.remove(&k);
        Ok(Mutation { key: k, previous, stored: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(json: &str) -> Payload {
        decode(json.as_bytes(), true).unwrap()
    }

    fn err_code(json: &str) -> String {
        decode(json.as_bytes(), true).unwrap_err().code
    }

    #[test]
    fn decode_basic_and_order() {
        let p = payload(r#"{"PartitionKey":"p","RowKey":"r","z":1,"a":"x","n":null,"Timestamp":"2020-01-01T00:00:00Z","odata.type":"t"}"#);
        assert_eq!(p.pk.as_deref(), Some("p"));
        let names: Vec<_> = p.props.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["z", "a"]);
    }

    #[test]
    fn decode_errors() {
        assert_eq!(err_code("[]"), "InvalidInput");
        assert_eq!(err_code("not json"), "InvalidInput");
        assert_eq!(err_code(r#"{"RowKey":"r"}"#), "PropertiesNeedValue");
        assert_eq!(err_code(r#"{"PartitionKey":"p"}"#), "PropertiesNeedValue");
        assert_eq!(err_code(r#"{"PartitionKey":"p","RowKey":1}"#), "InvalidInput");
        assert_eq!(
            err_code(r#"{"PartitionKey@odata.type":"Edm.DateTime","PartitionKey":"2022-04-01T09:30:45.000000Z","RowKey":"r"}"#),
            "InvalidInput"
        );
        assert_eq!(
            err_code(r#"{"PartitionKey":"p","RowKey@odata.type":"Edm.Guid","RowKey":"c5218988-f7ab-4515-ab42-66aa10e5f1a2"}"#),
            "InvalidInput"
        );
        assert_eq!(err_code(r#"{"PartitionKey":"p","RowKey":"r","123":1}"#), "PropertyNameInvalid");
        assert_eq!(err_code(&format!(r#"{{"PartitionKey":"p","RowKey":"r","{}":1}}"#, "a".repeat(256))), "PropertyNameTooLong");
        assert_eq!(err_code(r#"{"PartitionKey":"a/b","RowKey":"r"}"#), "InvalidInput");
        assert_eq!(err_code(r#"{"PartitionKey":"a\tb","RowKey":"r"}"#), "InvalidInput");
        assert_eq!(err_code(&format!(r#"{{"PartitionKey":"{}","RowKey":"r"}}"#, "k".repeat(513))), "InvalidInput");
        assert_eq!(
            err_code(&format!(r#"{{"PartitionKey":"p","RowKey":"r","s":"{}"}}"#, "x".repeat(32 * 1024 + 1))),
            "PropertyValueTooLarge"
        );
        let many: String = (0..253).map(|i| format!(r#","k{i}":1"#)).collect();
        assert_eq!(err_code(&format!(r#"{{"PartitionKey":"p","RowKey":"r"{many}}}"#)), "TooManyProperties");
        let ok: String = (0..252).map(|i| format!(r#","k{i}":1"#)).collect();
        assert!(decode(format!(r#"{{"PartitionKey":"p","RowKey":"r"{ok}}}"#).as_bytes(), true).is_ok());
    }

    #[test]
    fn key_limits() {
        assert!(validate_key(&"k".repeat(512)).is_ok());
        assert!(validate_key("").is_ok());
        assert!(validate_key("héllo 你好 🙂").is_ok());
        assert!(validate_key("a#b").is_err());
        assert!(validate_key("a?b").is_err());
        assert!(validate_key("a\\b").is_err());
    }

    fn table() -> Table {
        Table::default()
    }

    #[test]
    fn insert_update_delete_cycle() {
        let mut t = table();
        t.insert_entity(payload(r#"{"PartitionKey":"p","RowKey":"r","a":1,"b":2}"#), 100).unwrap();
        assert_eq!(t.insert_entity(payload(r#"{"PartitionKey":"p","RowKey":"r"}"#), 101).unwrap_err().code, "EntityAlreadyExists");

        let up = decode(br#"{"b":20,"c":30}"#, false).unwrap();
        let m = t.update_entity("p", "r", up, Mode::Merge, &Cond::Star, 100).unwrap();
        let e = m.stored.unwrap();
        assert_eq!(e.ts, 101, "etag moves on even when the clock did not");
        assert_eq!(e.props.len(), 3);

        let up = decode(br#"{"b":5}"#, false).unwrap();
        let e = t.update_entity("p", "r", up, Mode::Replace, &Cond::Any, 200).unwrap().stored.unwrap();
        assert_eq!(e.props, vec![("b".to_string(), Value::Int32(5))]);

        assert_eq!(t.delete_entity("p", "nope", &Cond::Star).unwrap_err().code, "ResourceNotFound");
        assert_eq!(
            t.delete_entity("p", "r", &Cond::Tag("W/\"datetime'2000-01-01T00%3A00%3A00Z'\"".into())).unwrap_err().code,
            "UpdateConditionNotSatisfied"
        );
        let tag = t.entities.values().next().unwrap().etag();
        t.delete_entity("p", "r", &Cond::Tag(tag)).unwrap();
        assert!(t.entities.is_empty());
    }

    #[test]
    fn conditional_update_of_missing_entity_is_404_and_upsert_creates() {
        let mut t = table();
        let up = || decode(br#"{"a":1}"#, false).unwrap();
        let stale = Cond::Tag("W/\"datetime'2012-06-15T22%3A51%3A44.9662825Z'\"".into());
        assert_eq!(t.update_entity("p", "r", up(), Mode::Merge, &stale, 1).unwrap_err().code, "ResourceNotFound");
        assert_eq!(t.update_entity("p", "r", up(), Mode::Merge, &Cond::Star, 1).unwrap_err().code, "ResourceNotFound");
        t.update_entity("p", "r", up(), Mode::Merge, &Cond::Any, 1).unwrap();
        assert_eq!(t.entities.len(), 1);
    }

    #[test]
    fn body_keys_must_match_the_url() {
        let mut t = table();
        let up = decode(br#"{"PartitionKey":"other","RowKey":"r"}"#, false).unwrap();
        assert_eq!(t.update_entity("p", "r", up, Mode::Merge, &Cond::Any, 1).unwrap_err().code, "InvalidInput");
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut t = table();
        t.insert_entity(payload(r#"{"PartitionKey":"p","RowKey":"r","a":1}"#), 1).unwrap();
        let before = t.entities.clone();
        let up = decode(br#"{"a":2}"#, false).unwrap();
        let m = t.update_entity("p", "r", up, Mode::Replace, &Cond::Any, 5).unwrap();
        t.undo(m);
        assert_eq!(t.entities, before);
        let m = t.delete_entity("p", "r", &Cond::Star).unwrap();
        t.undo(m);
        assert_eq!(t.entities, before);
        let m = t.insert_entity(payload(r#"{"PartitionKey":"p","RowKey":"s"}"#), 1).unwrap();
        t.undo(m);
        assert_eq!(t.entities, before);
    }
}
