//! Resource groups, and the generic resource listings of a subscription or group.

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::ids::group_id;
use crate::state::{group_key, Group, Stored};
use crate::util;

pub fn normalize_location(loc: &str) -> String {
    loc.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase()
}

fn valid_group_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 90
        && !name.ends_with('.')
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '(' | ')' | '-' | '.'))
}

pub fn group_json(g: &Group) -> Value {
    let mut v = json!({
        "id": group_id(&g.sub, &g.name),
        "name": g.name,
        "type": "Microsoft.Resources/resourceGroups",
        "location": g.location,
        "properties": { "provisioningState": "Succeeded" },
    });
    if !g.tags.is_empty() {
        v["tags"] = Value::Object(g.tags.clone());
    }
    if let Some(m) = &g.managed_by {
        v["managedBy"] = json!(m);
    }
    v
}

fn tags_of(body: &Value) -> Option<Map<String, Value>> {
    match body.get("tags") {
        Some(Value::Object(m)) => Some(m.clone()),
        _ => None,
    }
}

pub async fn handle(cx: &Cx<'_>, sub: &str, rg: Option<&str>, rest: &[&str]) -> ArmResult<Response> {
    let method = cx.req.method.as_str();
    match (rg, rest) {
        (None, ["resourcegroups"]) if method == "GET" => list_groups(cx, sub),
        (None, ["resourcegroups", name]) => {
            // The name came lower-cased from the router; use the original spelling.
            let segs = cx.req.segments();
            let original = segs.get(3).cloned().unwrap_or_else(|| (*name).to_string());
            one_group(cx, sub, &original).await
        }
        (Some(rg), []) => one_group(cx, sub, rg).await,
        _ => Err(ArmError::not_implemented(format!("{method} {}", cx.req.decoded_path()))),
    }
}

async fn one_group(cx: &Cx<'_>, sub: &str, name: &str) -> ArmResult<Response> {
    match cx.req.method.as_str() {
        "PUT" => put_group(cx, sub, name),
        "GET" => {
            let st = cx.inner.state.lock();
            let g = st.group(sub, name).ok_or_else(|| ArmError::group_not_found(name))?;
            Ok(util::json_ok(200, &group_json(g)))
        }
        "HEAD" => {
            let exists = cx.inner.state.lock().group(sub, name).is_some();
            Ok(util::empty(if exists { 204 } else { 404 }))
        }
        "PATCH" => patch_group(cx, sub, name),
        "DELETE" => delete_group(cx, sub, name).await,
        m => Err(ArmError::new(405, "MethodNotAllowed", format!("The method {m} is not allowed for this resource group."))),
    }
}

fn put_group(cx: &Cx<'_>, sub: &str, name: &str) -> ArmResult<Response> {
    let body = util::body_object(&cx.req.body)?;
    let (status, v) = put_group_body(cx, sub, name, &body)?;
    Ok(util::json_ok(status, &v))
}

/// Create or update a group from a body. Also used by deployments. Returns the status and the group.
pub fn put_group_body(cx: &Cx<'_>, sub: &str, name: &str, body: &Value) -> ArmResult<(u16, Value)> {
    let body = body.clone();
    if let Some(t) = tags_of(&body) {
        crate::tags::check_tags(&t)?;
    }
    if !valid_group_name(name) {
        return Err(ArmError::bad_request(
            "InvalidResourceGroup",
            format!("The provided resource group name '{name}' has invalid characters."),
        ));
    }
    let mut st = cx.inner.state.lock();
    let key = group_key(sub, name);
    if st.deleting.contains(&key) {
        return Err(crate::writers::being_deleted(name));
    }
    if let Some(g) = st.groups.get_mut(&key) {
        if let Some(loc) = body.get("location").and_then(Value::as_str) {
            if normalize_location(loc) != g.location {
                return Err(ArmError::conflict(
                    "InvalidResourceGroupLocation",
                    format!(
                        "Invalid resource group location '{loc}'. The Resource group already exists in the location '{}' and location cannot be changed.",
                        g.location
                    ),
                ));
            }
        }
        if let Some(t) = tags_of(&body) {
            g.tags = t;
        }
        if let Some(m) = body.get("managedBy").and_then(Value::as_str) {
            g.managed_by = Some(m.to_string());
        }
        return Ok((200, group_json(g)));
    }
    let Some(loc) = body.get("location").and_then(Value::as_str).filter(|l| !l.trim().is_empty()) else {
        return Err(ArmError::bad_request("LocationRequired", "The location property is required for this definition."));
    };
    let g = Group {
        sub: sub.to_string(),
        name: name.to_string(),
        location: normalize_location(loc),
        tags: tags_of(&body).unwrap_or_default(),
        managed_by: body.get("managedBy").and_then(Value::as_str).map(String::from),
    };
    let out = group_json(&g);
    st.groups.insert(key, g);
    Ok((201, out))
}

fn patch_group(cx: &Cx<'_>, sub: &str, name: &str) -> ArmResult<Response> {
    let body = util::body_object(&cx.req.body)?;
    let mut st = cx.inner.state.lock();
    let g = st.groups.get_mut(&group_key(sub, name)).ok_or_else(|| ArmError::group_not_found(name))?;
    if let Some(t) = tags_of(&body) {
        g.tags = t;
    }
    if let Some(m) = body.get("managedBy").and_then(Value::as_str) {
        g.managed_by = Some(m.to_string());
    }
    Ok(util::json_ok(200, &group_json(g)))
}

/// Delete each resource through its provider, in the order given, and take every one that went away out of the state. The others
/// stay, and the first failure is the answer (a delete either removes everything or says why not).
pub(crate) async fn delete_members(cx: &Cx<'_>, items: &[(Stored, bool)]) -> ArmResult<()> {
    let mut first: Option<ArmError> = None;
    for (m, force) in items {
        let done = match cx.inner.provider(&m.ns) {
            Some(p) => p.delete(cx, m, *force).await,
            None => Ok(()),
        };
        match done {
            Ok(()) => {
                cx.inner.state.lock().resources.remove(&crate::state::key(&m.id));
            }
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Delete a group and everything in it, data plane included. 202 with a status URL that
/// answers 200 at once, as Azure does. The answer means the group and its members are gone: the delete first closes the group
/// to new writes and waits for the ones running (`writers.rs`), then deletes every member. If one member cannot be deleted the
/// group stays with what is left, the group is open for writes again, and the error of the first failure is the answer.
async fn delete_group(cx: &Cx<'_>, sub: &str, name: &str) -> ArmResult<Response> {
    let key = cx.inner.begin_group_delete(sub, name).await?;
    let members = cx.inner.state.lock().in_group_deepest_first(sub, name);
    let items: Vec<(Stored, bool)> = members.into_iter().map(|m| (m, true)).collect();
    if let Err(e) = delete_members(cx, &items).await {
        cx.inner.end_group_delete(&key);
        return Err(e);
    }
    {
        let mut st = cx.inner.state.lock();
        // nothing can have been written meanwhile; take what is left of the group's records
        let left: Vec<String> = st
            .resources
            .range(format!("{key}/")..)
            .take_while(|(k, _)| k.starts_with(&format!("{key}/")))
            .map(|(k, _)| k.clone())
            .collect();
        for k in left {
            st.resources.remove(&k);
        }
        st.groups.remove(&key);
        st.scope_tags.remove(&key);
        st.deleting.remove(&key);
    }
    Ok(cx.accepted())
}

fn list_groups(cx: &Cx<'_>, sub: &str) -> ArmResult<Response> {
    let filter = cx.req.query.get("$filter").unwrap_or("");
    let tag_name = filter_value(filter, "tagName");
    let tag_value = filter_value(filter, "tagValue");
    let st = cx.inner.state.lock();
    let prefix = format!("/subscriptions/{}/", sub.to_ascii_lowercase());
    let items = st
        .groups
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, g)| g)
        .filter(|g| match &tag_name {
            Some(n) => {
                g.tags.iter().any(|(k, v)| k.eq_ignore_ascii_case(n) && tag_value.as_ref().map(|tv| v.as_str() == Some(tv)).unwrap_or(true))
            }
            None => true,
        })
        .map(group_json)
        .collect();
    Ok(util::list(items))
}

/// `tagName eq 'x'` -> `x`.
fn filter_value(filter: &str, field: &str) -> Option<String> {
    let lower = filter.to_ascii_lowercase();
    let at = lower.find(&format!("{} eq '", field.to_ascii_lowercase()))?;
    let start = at + field.len() + 5;
    let rest = &filter[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn generic_json(s: &Stored) -> Value {
    let mut v = json!({"id": s.id, "name": s.name(), "type": s.type_string()});
    for k in ["location", "tags", "sku", "kind", "managedBy", "identity", "plan", "zones", "extendedLocation"] {
        if let Some(x) = s.body.get(k) {
            v[k] = x.clone();
        }
    }
    v
}

/// `GET /subscriptions/{s}/resources` and `.../resourceGroups/{rg}/resources`: top-level
/// resources only, `$filter` on resourceType, name, location, tagName and tagValue.
pub fn list_resources(cx: &Cx<'_>, sub: &str, rg: Option<&str>) -> ArmResult<Response> {
    let filter = cx.req.query.get("$filter").unwrap_or("");
    let rtype = filter_value(filter, "resourceType");
    let name = filter_value(filter, "name");
    let location = filter_value(filter, "location");
    let tag_name = filter_value(filter, "tagName");
    let tag_value = filter_value(filter, "tagValue");
    let st = cx.inner.state.lock();
    if let Some(rg) = rg {
        if st.group(sub, rg).is_none() {
            return Err(ArmError::group_not_found(rg));
        }
    }
    let prefix = match rg {
        Some(rg) => format!("{}/", group_key(sub, rg)),
        None => format!("/subscriptions/{}/", sub.to_ascii_lowercase()),
    };
    let items = st
        .resources
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, s)| s)
        .filter(|s| s.types.len() == 1 && s.ns != "Microsoft.Resources" && s.extra.get("eventSub").is_none())
        .filter(|s| rtype.as_ref().map(|t| s.type_string().eq_ignore_ascii_case(t)).unwrap_or(true))
        .filter(|s| name.as_ref().map(|n| s.name().eq_ignore_ascii_case(n)).unwrap_or(true))
        .filter(|s| {
            location
                .as_ref()
                .map(|l| s.body.get("location").and_then(Value::as_str).map(|x| x.eq_ignore_ascii_case(l)).unwrap_or(false))
                .unwrap_or(true)
        })
        .filter(|s| match &tag_name {
            Some(n) => s
                .body
                .get("tags")
                .and_then(Value::as_object)
                .map(|t| {
                    t.iter().any(|(k, v)| k.eq_ignore_ascii_case(n) && tag_value.as_ref().map(|tv| v.as_str() == Some(tv)).unwrap_or(true))
                })
                .unwrap_or(false),
            None => true,
        })
        .map(generic_json)
        .collect();
    Ok(util::list(items))
}

/// The stored spelling of a group name, or `ResourceGroupNotFound`.
pub fn canonical_group_name_for(cx: &Cx<'_>, sub: &str, rg: &str) -> ArmResult<String> {
    let st = cx.inner.state.lock();
    st.group(sub, rg).map(|g| g.name.clone()).ok_or_else(|| ArmError::group_not_found(rg))
}
