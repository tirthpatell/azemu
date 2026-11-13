//! Tags: `.../providers/Microsoft.Resources/tags/default` on a subscription, a resource
//! group or a resource, and the subscription-wide tag name list.

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::ids::parse_scope;
use crate::state::{group_key, key};
use crate::util;

fn tags_json(scope_id: &str, tags: &Map<String, Value>) -> Value {
    json!({
        "id": format!("{scope_id}/providers/Microsoft.Resources/tags/default"),
        "name": "default",
        "type": "Microsoft.Resources/tags",
        "properties": { "tags": tags },
    })
}

/// Most tags on one resource, group or subscription, and the longest name and value (Microsoft Learn, "Tag resources, resource
/// groups, and subscriptions for logical organization": 50 tags, names up to 512 characters, values up to 256). The error text and
/// code are not documented; they are the emulator's.
pub const MAX_TAGS: usize = 50;
const MAX_TAG_NAME: usize = 512;
const MAX_TAG_VALUE: usize = 256;

pub fn check_tags(tags: &Map<String, Value>) -> ArmResult<()> {
    if tags.len() > MAX_TAGS {
        return Err(ArmError::bad_request(
            "InvalidRequestContent",
            format!("A resource can have at most {MAX_TAGS} tags; the request has {}.", tags.len()),
        ));
    }
    for (k, v) in tags {
        if k.chars().count() > MAX_TAG_NAME {
            return Err(ArmError::bad_request("InvalidRequestContent", format!("A tag name can have at most {MAX_TAG_NAME} characters.")));
        }
        if v.as_str().is_some_and(|s| s.chars().count() > MAX_TAG_VALUE) {
            return Err(ArmError::bad_request(
                "InvalidRequestContent",
                format!("The value of tag '{k}' can have at most {MAX_TAG_VALUE} characters."),
            ));
        }
    }
    Ok(())
}

fn tags_of_value(v: &Value) -> Map<String, Value> {
    v.get("properties").and_then(|p| p.get("tags")).and_then(Value::as_object).cloned().unwrap_or_default()
}

pub fn handle(cx: &Cx<'_>, scope_segs: &[String]) -> ArmResult<Response> {
    let sc = parse_scope(scope_segs);
    let Some(sub) = sc.sub.as_deref() else {
        return Err(ArmError::not_implemented("tags at tenant scope"));
    };
    let scope_id = format!("/{}", scope_segs.join("/"));
    let method = cx.req.method.as_str();
    let mut st = cx.inner.state.lock();
    // Where the tags live.
    enum Where {
        Sub,
        Group(String),
        Resource(String),
    }
    let place = if sc.rest.is_empty() {
        match &sc.rg {
            None => Where::Sub,
            Some(rg) => {
                if st.group(sub, rg).is_none() {
                    return Err(ArmError::group_not_found(rg));
                }
                Where::Group(group_key(sub, rg))
            }
        }
    } else {
        let k = key(&scope_id);
        if !st.resources.contains_key(&k) {
            return Err(ArmError::not_found("ResourceNotFound", format!("The resource '{scope_id}' was not found.")));
        }
        Where::Resource(k)
    };
    let read = |st: &crate::state::State| -> Map<String, Value> {
        match &place {
            Where::Sub => st.scope_tags.get(&key(&scope_id)).cloned().unwrap_or_default(),
            Where::Group(k) => st.groups.get(k).map(|g| g.tags.clone()).unwrap_or_default(),
            Where::Resource(k) => {
                st.resources.get(k).and_then(|r| r.body.get("tags")).and_then(Value::as_object).cloned().unwrap_or_default()
            }
        }
    };
    let write = |st: &mut crate::state::State, tags: Map<String, Value>| match &place {
        Where::Sub => {
            st.scope_tags.insert(key(&scope_id), tags);
        }
        Where::Group(k) => {
            if let Some(g) = st.groups.get_mut(k) {
                g.tags = tags;
            }
        }
        Where::Resource(k) => {
            if let Some(r) = st.resources.get_mut(k) {
                r.body["tags"] = Value::Object(tags);
            }
        }
    };
    match method {
        "GET" => Ok(util::json_ok(200, &tags_json(&scope_id, &read(&st)))),
        "PUT" => {
            let body = util::body_object(&cx.req.body)?;
            let tags = tags_of_value(&body);
            check_tags(&tags)?;
            write(&mut st, tags.clone());
            Ok(util::json_ok(200, &tags_json(&scope_id, &tags)))
        }
        "PATCH" => {
            let body = util::body_object(&cx.req.body)?;
            let patch = tags_of_value(&body);
            let op = body.get("operation").and_then(Value::as_str).unwrap_or("Merge").to_ascii_lowercase();
            let mut tags = read(&st);
            match op.as_str() {
                "replace" => tags = patch,
                "delete" => {
                    for (k, v) in &patch {
                        if tags.get(k) == Some(v) {
                            tags.remove(k);
                        }
                    }
                }
                _ => tags.extend(patch),
            }
            check_tags(&tags)?;
            write(&mut st, tags.clone());
            Ok(util::json_ok(200, &tags_json(&scope_id, &tags)))
        }
        "DELETE" => {
            write(&mut st, Map::new());
            Ok(util::empty(200))
        }
        m => Err(ArmError::new(405, "MethodNotAllowed", format!("The method {m} is not allowed on tags."))),
    }
}

/// `/subscriptions/{s}/tagNames[/{name}[/tagValues/{value}]]`
pub fn tag_names(cx: &Cx<'_>, sub: &str, rest: &[String]) -> ArmResult<Response> {
    let method = cx.req.method.as_str();
    let mut st = cx.inner.state.lock();
    let names = st.tag_names.entry(sub.to_ascii_lowercase()).or_default();
    let name_json = |n: &str, values: &[String]| {
        json!({
            "id": format!("/subscriptions/{sub}/tagNames/{n}"),
            "tagName": n,
            "count": { "type": "Total", "value": values.len() },
            "values": values.iter().map(|v| json!({"id": format!("/subscriptions/{sub}/tagNames/{n}/tagValues/{v}"), "tagValue": v, "count": {"type": "Total", "value": 0}})).collect::<Vec<_>>(),
        })
    };
    match (method, rest.len()) {
        ("GET", 1) => Ok(util::list(names.iter().map(|(n, v)| name_json(n, v)).collect())),
        ("PUT", 2) => {
            names.entry(rest[1].clone()).or_default();
            Ok(util::json_ok(201, &name_json(&rest[1], &[])))
        }
        ("DELETE", 2) => {
            names.remove(&rest[1]);
            Ok(util::empty(200))
        }
        ("PUT", 4) => {
            let v = names.entry(rest[1].clone()).or_default();
            if !v.contains(&rest[3]) {
                v.push(rest[3].clone());
            }
            Ok(util::json_ok(
                201,
                &json!({"id": format!("/subscriptions/{sub}/tagNames/{}/tagValues/{}", rest[1], rest[3]), "tagValue": rest[3], "count": {"type": "Total", "value": 0}}),
            ))
        }
        ("DELETE", 4) => {
            if let Some(v) = names.get_mut(&rest[1]) {
                v.retain(|x| x != &rest[3]);
            }
            Ok(util::empty(200))
        }
        _ => Err(ArmError::not_implemented(format!("{method} {}", cx.req.decoded_path()))),
    }
}
