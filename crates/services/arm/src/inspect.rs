//! The inspector of Resource Manager (`docs/inspector.md`): subscriptions, resource groups, the resources stored in them (with the
//! flag that says nothing is emulated behind a resource), and deployments with their operations and errors. Only reads: looking
//! does not make a subscription (a request to the service's own paths does), and the private state a provider keeps with a resource
//! (key generations) is not shown.

use std::ops::Bound::{Excluded, Included, Unbounded};

use azemu_core::inspect::{rfc3339, InspectError, InspectRequest, InspectResult, Node};
use serde_json::{json, Value};

use crate::resources::{classify, Kind};
use crate::state::{group_key, key, State, Stored};
use crate::Inner;

const DEFAULT_SUBSCRIPTION: &str = crate::basics::DEFAULT_SUBSCRIPTION;

fn is_deployment(s: &Stored) -> bool {
    s.ns == "Microsoft.Resources" && s.types.len() == 1 && s.types[0] == "deployments"
}

fn subscriptions(st: &State) -> Vec<String> {
    let mut subs = vec![DEFAULT_SUBSCRIPTION.to_string()];
    let mut add = |s: &str| {
        if !subs.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            subs.push(s.to_string());
        }
    };
    for s in &st.subs {
        add(s);
    }
    for g in st.groups.values() {
        add(&g.sub);
    }
    subs.sort();
    subs
}

fn missing(req: &InspectRequest) -> InspectError {
    InspectError::not_found(format!("There is no node {}.", req.own_path()))
}

pub(crate) fn node(inner: &Inner, req: &InspectRequest) -> InspectResult {
    let st = inner.state.lock();
    let segs: Vec<&str> = req.segments.iter().map(String::as_str).collect();
    let subs = subscriptions(&st);
    let [sub, rest @ ..] = segs.as_slice() else {
        let (range, next) = req.page_range(subs.len());
        let items = subs[range]
            .iter()
            .map(|s| json!({"name": s, "path": req.path(&[s]), "resourceGroups": st.groups.values().filter(|g| g.sub.eq_ignore_ascii_case(s)).count()}))
            .collect();
        return Ok(Node::new(req.path(&[]), "service", "arm").prop("accounts", subs.len()).with_items(items, next));
    };
    if !subs.iter().any(|s| s.eq_ignore_ascii_case(sub)) {
        return Err(InspectError::not_found(format!("There is no subscription '{sub}' here.")));
    }
    match rest {
        [] => Ok(subscription_node(&st, sub, req)),
        ["resourcegroups"] => Ok(groups_node(&st, sub, req)),
        ["resourcegroups", rg, tail @ ..] => {
            let g = st
                .groups
                .get(&group_key(sub, rg))
                .ok_or_else(|| InspectError::not_found(format!("There is no resource group '{rg}' in the subscription.")))?;
            let prefix = format!("{}/", group_key(sub, rg));
            match tail {
                [] => Ok(group_node(&st, sub, rg, g, &prefix, req)),
                _ => scope_tree(inner, &st, &prefix, &[sub, "resourcegroups", rg], tail, req),
            }
        }
        _ => {
            // the resources and deployments that live at the subscription itself
            let prefix = format!("/subscriptions/{}/providers/", key(sub));
            scope_tree(inner, &st, &prefix, &[sub], rest, req)
        }
    }
}

fn subscription_node(st: &State, sub: &str, req: &InspectRequest) -> Node {
    let prefix = format!("/subscriptions/{}/providers/", key(sub));
    let (res, dep) = count_scope(st, &prefix);
    Node::new(req.own_path(), "subscription", sub)
        .prop("resourceGroups", st.groups.values().filter(|g| g.sub.eq_ignore_ascii_case(sub)).count())
        .prop("defaultSubscription", sub == DEFAULT_SUBSCRIPTION)
        .child(
            "resourcegroups",
            "resourcegroups",
            Some(st.groups.values().filter(|g| g.sub.eq_ignore_ascii_case(sub)).count() as u64),
            req.path(&[sub, "resourcegroups"]),
        )
        .child("resources", "resources", Some(res), req.path(&[sub, "resources"]))
        .child("deployments", "deployments", Some(dep), req.path(&[sub, "deployments"]))
}

/// (resources, deployments) stored under the prefix.
fn count_scope(st: &State, prefix: &str) -> (u64, u64) {
    let (mut r, mut d) = (0, 0);
    for (_, s) in st.resources.range(prefix.to_string()..).take_while(|(k, _)| k.starts_with(prefix)) {
        if is_deployment(s) {
            d += 1;
        } else {
            r += 1;
        }
    }
    (r, d)
}

fn groups_node(st: &State, sub: &str, req: &InspectRequest) -> Node {
    let rows: Vec<_> = st.groups.values().filter(|g| g.sub.eq_ignore_ascii_case(sub)).collect();
    let (range, next) = req.page_range(rows.len());
    let items = rows[range]
        .iter()
        .map(|g| {
            let (res, dep) = count_scope(st, &format!("{}/", group_key(&g.sub, &g.name)));
            json!({"name": g.name, "path": req.path(&[sub, "resourcegroups", &g.name]), "location": g.location, "resources": res, "deployments": dep, "tags": g.tags})
        })
        .collect();
    Node::new(req.path(&[sub, "resourcegroups"]), "resourcegroups", "resource groups").prop("count", rows.len()).with_items(items, next)
}

fn group_node(st: &State, sub: &str, rg: &str, g: &crate::state::Group, prefix: &str, req: &InspectRequest) -> Node {
    let (res, dep) = count_scope(st, prefix);
    Node::new(req.own_path(), "resourcegroup", &g.name)
        .prop("location", &g.location)
        .prop("tags", &g.tags)
        .prop("managedBy", &g.managed_by)
        .child("resources", "resources", Some(res), req.path(&[sub, "resourcegroups", rg, "resources"]))
        .child("deployments", "deployments", Some(dep), req.path(&[sub, "resourcegroups", rg, "deployments"]))
}

/// `resources`, `resources/{id}`, `deployments`, `deployments/{name}`, `deployments/{name}/operations` below a scope.
fn scope_tree(inner: &Inner, st: &State, prefix: &str, at: &[&str], tail: &[&str], req: &InspectRequest) -> InspectResult {
    let path = |more: &[&str]| {
        let mut p: Vec<&str> = at.to_vec();
        p.extend_from_slice(more);
        req.path(&p)
    };
    match tail {
        ["resources"] => Ok(resources_node(inner, st, prefix, &path(&["resources"]), at, req)),
        ["resources", id] => {
            let s = st
                .resources
                .get(&key(id))
                .filter(|s| key(&s.id).starts_with(prefix) && !is_deployment(s))
                .ok_or_else(|| InspectError::not_found(format!("There is no stored resource '{id}' here.")))?;
            Ok(resource_node(inner, s, &path(&["resources", id])))
        }
        ["deployments"] => Ok(deployments_node(st, prefix, &path(&["deployments"]), at, req)),
        ["deployments", name, rest @ ..] => {
            let wanted = format!(
                "{prefix}{}{}",
                if at.len() == 1 { "microsoft.resources/deployments/" } else { "providers/microsoft.resources/deployments/" },
                key(name)
            );
            let d = st
                .resources
                .get(&wanted)
                .filter(|s| is_deployment(s))
                .ok_or_else(|| InspectError::not_found(format!("There is no deployment '{name}' here.")))?;
            match rest {
                [] => Ok(deployment_node(d, &path(&["deployments", name]))),
                ["operations"] => {
                    let ops = d.extra.get("operations").and_then(Value::as_array).cloned().unwrap_or_default();
                    let (range, next) = req.page_range(ops.len());
                    let items = ops[range].iter().map(operation_row).collect();
                    Ok(Node::new(path(&["deployments", name, "operations"]), "operations", "deployment operations")
                        .prop("count", ops.len())
                        .with_items(items, next))
                }
                _ => Err(missing(req)),
            }
        }
        _ => Err(missing(req)),
    }
}

fn resource_row(inner: &Inner, s: &Stored, path: String) -> Value {
    let (kind, stored_only) = stored_only(inner, s);
    json!({
        "name": s.name(),
        "path": path,
        "type": s.type_string(),
        "location": s.body.get("location"),
        "provisioningState": s.body.pointer("/properties/provisioningState"),
        "storedOnly": stored_only,
        "support": kind,
        "created": rfc3339(s.created),
    })
}

/// Is anything emulated behind this resource? The same question the service answers with `x-azemu-stored-only`.
fn stored_only(inner: &Inner, s: &Stored) -> (&'static str, bool) {
    let provider = inner.provider(&s.ns);
    match classify(provider, &s.types).1 {
        Kind::Built => ("built", false),
        Kind::Passthrough => ("settings only", false),
        Kind::UnbuiltInKnownProvider => ("type not built", false),
        Kind::Generic => ("stored as JSON only", !inner.strict),
    }
}

fn resources_node(inner: &Inner, st: &State, prefix: &str, own: &str, at: &[&str], req: &InspectRequest) -> Node {
    let (ty, name) = (req.param("type").map(str::to_ascii_lowercase), req.param("name").map(str::to_ascii_lowercase));
    let lower = req.marker().map_or_else(|| Included(prefix.to_string()), |m| Excluded(m.to_string()));
    let rows = st
        .resources
        .range((lower, Unbounded))
        .take_while(|(k, _)| k.starts_with(prefix))
        .filter(|(_, s)| !is_deployment(s))
        .filter(|(_, s)| {
            ty.as_ref().is_none_or(|t| s.type_string().to_ascii_lowercase().contains(t))
                && name.as_ref().is_none_or(|n| s.name().to_ascii_lowercase().contains(n))
        })
        .map(|(k, s)| {
            let mut p: Vec<&str> = at.to_vec();
            p.extend_from_slice(&["resources", k]);
            (k.as_str(), resource_row(inner, s, req.path(&p)))
        });
    let (items, next) = req.page_keyed(rows);
    Node::new(own, "resources", "stored resources").accepts(&["type", "name"]).with_items(items, next)
}

fn resource_node(inner: &Inner, s: &Stored, own: &str) -> Node {
    let (support, only) = stored_only(inner, s);
    Node::new(own, "resource", s.name())
        .prop("id", &s.id)
        .prop("type", s.type_string())
        .prop("storedOnly", only)
        .prop("support", support)
        .prop("created", rfc3339(s.created))
        .prop("resource", &s.body)
}

fn deployment_state(d: &Stored) -> Value {
    d.body.pointer("/properties/provisioningState").cloned().unwrap_or(Value::Null)
}

fn deployments_node(st: &State, prefix: &str, own: &str, at: &[&str], req: &InspectRequest) -> Node {
    let dep_prefix = if at.len() == 1 {
        format!("{prefix}microsoft.resources/deployments/")
    } else {
        format!("{prefix}providers/microsoft.resources/deployments/")
    };
    let lower = req.marker().map_or_else(|| Included(dep_prefix.clone()), |m| Excluded(m.to_string()));
    let rows = st.resources.range((lower, Unbounded)).take_while(|(k, _)| k.starts_with(&dep_prefix)).filter(|(_, s)| is_deployment(s)).map(|(k, d)| {
        let mut p: Vec<&str> = at.to_vec();
        p.extend_from_slice(&["deployments", d.name()]);
        let props = d.body.get("properties").cloned().unwrap_or(Value::Null);
        (
            k.as_str(),
            json!({"name": d.name(), "path": req.path(&p), "provisioningState": deployment_state(d), "mode": props["mode"], "timestamp": props["timestamp"], "duration": props["duration"],
                   "error": props["error"], "operations": d.extra.get("operations").and_then(Value::as_array).map_or(0, Vec::len)}),
        )
    });
    let (items, next) = req.page_keyed(rows);
    Node::new(own, "deployments", "deployments").with_items(items, next)
}

fn deployment_node(d: &Stored, own: &str) -> Node {
    let ops = d.extra.get("operations").and_then(Value::as_array).map_or(0, Vec::len) as u64;
    let mut n = Node::new(own, "deployment", d.name()).prop("id", &d.id).prop("created", rfc3339(d.created));
    if let Some(Value::Object(m)) = d.body.get("properties") {
        for (k, v) in m {
            n = n.prop(k, v);
        }
    }
    n.child("operations", "operations", Some(ops), format!("{own}/operations"))
}

fn operation_row(op: &Value) -> Value {
    let p = &op["properties"];
    json!({
        "name": p["targetResource"]["resourceName"],
        "operationId": op["operationId"],
        "provisioningState": p["provisioningState"],
        "operation": p["provisioningOperation"],
        "target": p["targetResource"]["resourceType"],
        "statusCode": p["statusCode"],
        "timestamp": p["timestamp"],
        "duration": p["duration"],
        "error": p["statusMessage"]["error"],
    })
}
