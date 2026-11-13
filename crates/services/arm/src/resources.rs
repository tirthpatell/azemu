//! The generic resource engine: PUT, GET, HEAD, PATCH, DELETE, list and POST actions
//! for anything under `.../providers/{namespace}/...`. Providers wired to emulated
//! services plug in through [`Provider`].

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::catalog;
use crate::cx::Cx;
use crate::deployments;
use crate::error::{ArmError, ArmResult};
use crate::groups::normalize_location;
use crate::ids::{parse_res, resource_id, type_string};
use crate::provider::{ActionTarget, Provider, Statuses, STATUSES_DEFAULT};
use crate::state::{group_key, key, Stored};
use crate::util;

/// How much ARM knows about a resource type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A provider builds it (and may drive a service).
    Built,
    /// A provider lists it as settings only; stored as JSON.
    Passthrough,
    /// A known provider, a type it has not built.
    UnbuiltInKnownProvider,
    /// No provider for this namespace: generic storage.
    Generic,
}

/// Canonical spelling of each type segment, and what is known about the full chain.
fn classify(provider: Option<&dyn Provider>, types: &[String]) -> (Vec<String>, Kind) {
    let Some(p) = provider else {
        return (types.to_vec(), Kind::Generic);
    };
    let mut canon = Vec::new();
    for i in 0..types.len() {
        let want = types[..=i].join("/").to_ascii_lowercase();
        let hit = p.types().iter().chain(p.passthrough().iter()).find(|t| t.to_ascii_lowercase() == want);
        match hit {
            Some(t) => canon.push(t.rsplit('/').next().unwrap_or(t).to_string()),
            None => canon.push(types[i].clone()),
        }
    }
    let full = canon.join("/").to_ascii_lowercase();
    let kind = if p.types().iter().any(|t| t.to_ascii_lowercase() == full) {
        Kind::Built
    } else if p.passthrough().iter().any(|t| t.to_ascii_lowercase() == full) {
        Kind::Passthrough
    } else {
        Kind::UnbuiltInKnownProvider
    };
    (canon, kind)
}

fn not_built(cx: &Cx<'_>, ns: &str, types: &[String]) -> ArmError {
    ArmError::not_implemented(format!(
        "{} {} ({}) is not built; the type {} is not stored because it would need a service the emulator does not have",
        cx.req.method,
        cx.req.decoded_path(),
        ns,
        type_string(ns, types),
    ))
}

pub async fn handle(cx: &Cx<'_>, sub: &str, rg: Option<&str>, ns_raw: &str, tail: &[String]) -> ArmResult<Response> {
    let method = cx.req.method.as_str();
    if let Some(r) = crate::eventsubs::try_handle(cx, sub, rg, ns_raw, tail).await {
        return r;
    }
    if ns_raw.eq_ignore_ascii_case("Microsoft.Resources") {
        match tail.first().map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("deployments") => return deployments::handle(cx, sub, rg, &tail[1..]).await,
            Some("operations") if method == "GET" => return Ok(util::list(vec![])),
            _ => {}
        }
    }
    if tail.len() == 1 && tail[0].eq_ignore_ascii_case("operations") && method == "GET" {
        return Ok(util::list(vec![]));
    }
    let ns = catalog::canonical_namespace(ns_raw).map(str::to_string).unwrap_or_else(|| ns_raw.to_string());
    let provider = cx.inner.provider(&ns);
    let path = parse_res(tail);
    let (types, kind) = classify(provider, &path.types);
    if kind == Kind::Generic && !types.is_empty() && !cx.inner.strict {
        // every answer about a type nothing emulates says so (writes are counted in `put_core`)
        cx.mark_stored_only();
    }
    match (path.trailing.as_deref(), types.is_empty()) {
        // /providers/{ns}/{type}/{name}
        (None, false) => match method {
            "PUT" | "PATCH" => {
                let body = util::body_object(&cx.req.body)?;
                let patch = method == "PATCH";
                let (stored, existed, statuses) = put_core(cx, sub, rg, &ns, &types, &path.names, kind, provider, body, patch).await?;
                let status = if existed { statuses.update } else { statuses.create };
                Ok(util::json_ok(status, &render(cx, provider, &stored)))
            }
            "GET" => get_resource(cx, sub, rg, &ns, &types, &path.names, kind, provider),
            "HEAD" => match get_resource(cx, sub, rg, &ns, &types, &path.names, kind, provider) {
                Ok(_) => Ok(util::empty(204)),
                Err(e) if e.status == 404 => Ok(util::empty(404)),
                Err(e) => Err(e),
            },
            "DELETE" => delete_resource(cx, sub, rg, &ns, &types, &path.names, provider).await,
            m => Err(ArmError::new(405, "MethodNotAllowed", format!("The method {m} is not allowed on this resource."))),
        },
        // /providers/{ns}/.../{type}   or   .../{name}/{action}
        (Some(t), _) => {
            if method == "GET"
                && !types.is_empty()
                && provider.map(|p| p.get_actions().iter().any(|a| a.eq_ignore_ascii_case(t))).unwrap_or(false)
            {
                return action(cx, sub, rg, &ns, &types, &path.names, t, kind, provider).await;
            }
            if method == "GET" {
                let mut chain = types.clone();
                let (c, listed_kind) = classify(provider, &[chain.clone(), vec![t.to_string()]].concat());
                chain = c;
                if listed_kind == Kind::Generic && !cx.inner.strict {
                    cx.mark_stored_only();
                }
                return list_resources(cx, sub, rg, &ns, &chain, &path.names, provider);
            }
            if method == "POST" {
                return action(cx, sub, rg, &ns, &types, &path.names, t, kind, provider).await;
            }
            Err(ArmError::new(405, "MethodNotAllowed", format!("The method {method} is not allowed on this resource.")))
        }
        (None, true) => Err(ArmError::not_implemented(format!("{method} {}", cx.req.decoded_path()))),
    }
}

fn rg_name(rg: Option<&str>) -> &str {
    rg.unwrap_or("")
}

fn canonical_group_name(cx: &Cx<'_>, sub: &str, rg: &str) -> ArmResult<String> {
    let st = cx.inner.state.lock();
    st.group(sub, rg).map(|g| g.name.clone()).ok_or_else(|| ArmError::group_not_found(rg))
}

fn resource_key(sub: &str, rg: &str, ns: &str, types: &[String], names: &[String]) -> String {
    key(&resource_id(sub, rg, ns, types, names))
}

/// Create, replace or patch one resource. Shared by the HTTP handler and deployments.
/// Returns the stored resource, whether it existed before, and the provider's status codes.
#[allow(clippy::too_many_arguments)]
async fn put_core(
    cx: &Cx<'_>,
    sub: &str,
    rg: Option<&str>,
    ns: &str,
    types: &[String],
    names: &[String],
    kind: Kind,
    provider: Option<&dyn Provider>,
    body: Value,
    patch: bool,
) -> ArmResult<(Stored, bool, Statuses)> {
    let rg = match rg {
        Some(r) => canonical_group_name(cx, sub, r)?,
        None => String::new(),
    };
    let rg = rg.as_str();
    // a write into the group: a group delete waits for it, and one that has started refuses it (see `writers.rs`)
    let _writing = cx.inner.begin_write(sub, rg)?;
    match kind {
        Kind::UnbuiltInKnownProvider => return Err(not_built(cx, ns, types)),
        Kind::Generic if cx.inner.strict => return Err(not_built(cx, ns, types)),
        _ => {}
    }
    let type_path = types.join("/");
    let name = names.last().cloned().unwrap_or_default();
    if name.is_empty() {
        return Err(ArmError::bad_request("InvalidResourceName", "The resource name is empty."));
    }
    if let Some(p) = provider {
        p.validate_name(&type_path, &name)?;
    }
    let id = resource_id(sub, rg, ns, types, names);
    let k = key(&id);

    let existing = cx.inner.state.lock().resources.get(&k).cloned();
    if patch && existing.is_none() {
        return Err(ArmError::resource_not_found(&type_string(ns, types), &names.join("/"), rg));
    }
    // A child needs its parent.
    if types.len() > 1 {
        let n = types.len() - 1;
        let pk = resource_key(sub, rg, ns, &types[..n], &names[..n]);
        if !cx.inner.state.lock().resources.contains_key(&pk) {
            return Err(parent_missing(&types[..n], &names[..n]));
        }
    }
    let statuses: Statuses = provider.map(|p| p.statuses(&type_path)).unwrap_or(STATUSES_DEFAULT);
    let global = provider.map(|p| p.global_name(&type_path)).unwrap_or(false);
    if global {
        let st = cx.inner.state.lock();
        let clash = st.resources.values().any(|s| {
            s.id.to_ascii_lowercase() != k
                && s.ns == ns
                && s.type_path().eq_ignore_ascii_case(&type_path)
                && s.name().eq_ignore_ascii_case(&name)
        });
        if clash {
            return Err(ArmError::conflict(
                "Conflict",
                format!("The name '{name}' of {} is already taken by another resource.", type_string(ns, types)),
            ));
        }
    }

    // Build the resource from the body (PUT replaces, PATCH merges).
    let mut res_body = match (&existing, patch) {
        (Some(e), true) => {
            let mut b = e.body.clone();
            let mut p = body.clone();
            if let (Some(tags), Some(obj)) = (p.get("tags").cloned(), b.as_object_mut()) {
                obj.insert("tags".into(), tags);
                if let Some(po) = p.as_object_mut() {
                    po.remove("tags");
                }
            }
            util::deep_merge(&mut b, &p);
            b
        }
        _ => body,
    };
    let top_level = types.len() == 1;
    if let Some(o) = res_body.as_object_mut() {
        o.remove("id");
        o.remove("name");
        o.remove("type");
        if let Some(Value::String(l)) = o.get("location").cloned() {
            o.insert("location".into(), json!(normalize_location(&l)));
        }
    }
    if top_level && !rg.is_empty() && res_body.get("location").and_then(Value::as_str).map(str::is_empty).unwrap_or(true) {
        let needs = provider.map(|p| p.requires_location(&type_path)).unwrap_or(false);
        if needs && existing.is_none() {
            return Err(ArmError::bad_request("LocationRequired", "The location property is required for this definition."));
        }
        if existing.is_none() {
            let loc = cx.inner.state.lock().group(sub, rg).map(|g| g.location.clone()).unwrap_or_default();
            res_body["location"] = json!(loc);
        } else if let Some(e) = &existing {
            if let Some(l) = e.body.get("location") {
                res_body["location"] = l.clone();
            }
        }
    }
    if let Some(Value::Null) = res_body.get("tags") {
        res_body.as_object_mut().map(|o| o.remove("tags"));
    }
    if let Some(t) = res_body.get("tags").and_then(Value::as_object) {
        crate::tags::check_tags(t)?;
    }
    let mut stored = Stored {
        id: id.clone(),
        sub: sub.to_string(),
        rg: rg.to_string(),
        ns: ns.to_string(),
        types: types.to_vec(),
        names: names.to_vec(),
        body: res_body,
        extra: existing.as_ref().map(|e| e.extra.clone()).unwrap_or(Value::Object(Map::new())),
        created: existing.as_ref().map(|e| e.created).unwrap_or(cx.now),
    };
    // Header fields of every ARM resource.
    stored.body["id"] = json!(id);
    stored.body["name"] = json!(name);
    stored.body["type"] = json!(type_string(ns, types));
    util::props_mut(&mut stored.body).insert("provisioningState".into(), json!("Succeeded"));

    let mut children = Vec::new();
    if let Some(p) = provider {
        if kind == Kind::Built {
            p.put(cx, &mut stored, existing.is_some(), &mut children).await?;
        }
    }
    // what is stored holds no port (the answer fills in the one of the request, see `util::without_port`)
    util::without_port(&mut stored.body, &cx.port_suffix());
    {
        let mut st = cx.inner.state.lock();
        st.resources.insert(k, stored.clone());
        for c in children {
            st.resources.insert_if_absent(key(&c.id), c);
        }
    }
    if kind == Kind::Generic {
        // stored as JSON and nothing else: a made-up success unless the user is told
        cx.mark_stored_only();
        let full_type = type_string(ns, types);
        if azemu_core::unsupported::note_stored_only(&full_type) {
            tracing::warn!(
                "arm: {full_type} is stored as plain JSON and nothing is emulated for it; it answers Succeeded and does nothing. \
                 Answers for it carry x-azemu-stored-only: true; GET /_azemu/unsupported lists the types. AZEMU_ARM_STRICT=1 refuses them."
            );
        }
    }
    Ok((stored, existing.is_some(), statuses))
}

/// PUT a resource named by its full type (`Microsoft.Storage/storageAccounts`) and name
/// list, as a deployment does. Returns the rendered resource.
pub async fn put_by_type(cx: &Cx<'_>, sub: &str, rg: &str, full_type: &str, names: &[String], body: Value) -> ArmResult<Value> {
    let (ns_raw, rest) = full_type
        .split_once('/')
        .ok_or_else(|| ArmError::bad_request("InvalidTemplate", format!("'{full_type}' is not a resource type.")))?;
    if full_type.eq_ignore_ascii_case("Microsoft.EventGrid/systemTopics/eventSubscriptions") {
        return crate::eventsubs::put_system_topic_subscription(cx, sub, rg, names, body).await;
    }
    let ns = catalog::canonical_namespace(ns_raw).map(str::to_string).unwrap_or_else(|| ns_raw.to_string());
    let given: Vec<String> = rest.split('/').map(String::from).collect();
    if given.len() != names.len() {
        return Err(ArmError::bad_request(
            "InvalidTemplate",
            format!("The resource type '{full_type}' needs {} name segments, found {}.", given.len(), names.len()),
        ));
    }
    let provider = cx.inner.provider(&ns);
    let (types, kind) = classify(provider, &given);
    let rg_opt = if rg.is_empty() { None } else { Some(rg) };
    let (stored, _, _) = put_core(cx, sub, rg_opt, &ns, &types, names, kind, provider, body, false).await?;
    Ok(render(cx, provider, &stored))
}

fn parent_missing(types: &[String], names: &[String]) -> ArmError {
    let mut parts = Vec::new();
    for (t, n) in types.iter().zip(names) {
        parts.push(format!("{t}/{n}"));
    }
    ArmError::not_found(
        "ParentResourceNotFound",
        format!("Can not perform requested operation on nested resource. Parent resource '{}' not found.", parts.join("/")),
    )
}

fn render(cx: &Cx<'_>, provider: Option<&dyn Provider>, s: &Stored) -> Value {
    match provider {
        Some(p) => p.render(cx, s),
        None => s.body.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn get_resource(
    cx: &Cx<'_>,
    sub: &str,
    rg: Option<&str>,
    ns: &str,
    types: &[String],
    names: &[String],
    _kind: Kind,
    provider: Option<&dyn Provider>,
) -> ArmResult<Response> {
    let st = cx.inner.state.lock();
    let rg_s = rg_name(rg);
    if !rg_s.is_empty() && st.group(sub, rg_s).is_none() {
        return Err(ArmError::group_not_found(rg_s));
    }
    let k = resource_key(sub, st.group(sub, rg_s).map(|g| g.name.as_str()).unwrap_or(""), ns, types, names);
    match st.resources.get(&k) {
        Some(s) => {
            let s = s.clone();
            drop(st);
            Ok(util::json_ok(200, &render(cx, provider, &s)))
        }
        None => {
            if types.len() > 1 {
                let n = types.len() - 1;
                let gname = st.group(sub, rg_s).map(|g| g.name.clone()).unwrap_or_default();
                let pk = resource_key(sub, &gname, ns, &types[..n], &names[..n]);
                if !st.resources.contains_key(&pk) {
                    return Err(parent_missing(&types[..n], &names[..n]));
                }
            }
            Err(ArmError::resource_not_found(&type_string(ns, types), &names.join("/"), rg_s))
        }
    }
}

async fn delete_resource(
    cx: &Cx<'_>,
    sub: &str,
    rg: Option<&str>,
    ns: &str,
    types: &[String],
    names: &[String],
    provider: Option<&dyn Provider>,
) -> ArmResult<Response> {
    let rg_s = rg_name(rg);
    let gname = if rg_s.is_empty() { String::new() } else { canonical_group_name(cx, sub, rg_s)? };
    let id = resource_id(sub, &gname, ns, types, names);
    let (existing, below) = {
        let st = cx.inner.state.lock();
        (st.resources.get(&key(&id)).cloned(), st.descendants(&id))
    };
    let statuses = provider.map(|p| p.statuses(&types.join("/"))).unwrap_or(STATUSES_DEFAULT);
    let Some(existing) = existing else {
        return Ok(util::empty(statuses.delete_missing));
    };
    // what is below goes with it (forced); the resource itself is deleted as asked. Each one that went away leaves the state; the
    // first failure is the answer and what failed stays.
    let items: Vec<(Stored, bool)> = below.into_iter().map(|c| (c, true)).chain(std::iter::once((existing, false))).collect();
    crate::groups::delete_members(cx, &items).await?;
    Ok(util::empty(statuses.delete))
}

fn list_resources(
    cx: &Cx<'_>,
    sub: &str,
    rg: Option<&str>,
    ns: &str,
    chain: &[String],
    parent_names: &[String],
    provider: Option<&dyn Provider>,
) -> ArmResult<Response> {
    let st = cx.inner.state.lock();
    let rg_s = rg_name(rg);
    let gname = match rg_s {
        "" => String::new(),
        r => st.group(sub, r).map(|g| g.name.clone()).ok_or_else(|| ArmError::group_not_found(r))?,
    };
    let want_type = chain.join("/").to_ascii_lowercase();
    let parent_prefix = if parent_names.is_empty() {
        None
    } else {
        let n = chain.len() - 1;
        let pk = resource_key(sub, &gname, ns, &chain[..n], parent_names);
        if !st.resources.contains_key(&pk) {
            return Err(parent_missing(&chain[..n], parent_names));
        }
        Some(format!("{pk}/"))
    };
    let scope_prefix =
        if gname.is_empty() { format!("/subscriptions/{}/", sub.to_ascii_lowercase()) } else { format!("{}/", group_key(sub, &gname)) };
    let items: Vec<Stored> = st
        .resources
        .iter()
        .filter(|(k, s)| {
            k.starts_with(&scope_prefix)
                && s.ns == ns
                && s.type_path().to_ascii_lowercase() == want_type
                && parent_prefix.as_ref().map(|p| k.starts_with(p.as_str())).unwrap_or(true)
        })
        .map(|(_, s)| s.clone())
        .collect();
    drop(st);
    Ok(util::list(items.iter().map(|s| render(cx, provider, s)).collect()))
}

#[allow(clippy::too_many_arguments)]
async fn action(
    cx: &Cx<'_>,
    sub: &str,
    rg: Option<&str>,
    ns: &str,
    types: &[String],
    names: &[String],
    action: &str,
    kind: Kind,
    provider: Option<&dyn Provider>,
) -> ArmResult<Response> {
    let body = util::body_object(&cx.req.body)?;
    let Some(p) = provider else {
        return Err(ArmError::not_implemented(format!("POST {} (no provider for {ns})", cx.req.decoded_path())));
    };
    if kind == Kind::UnbuiltInKnownProvider && !types.is_empty() {
        // Some actions on types ARM does not store still make sense (purging a deleted vault).
        if let Some(r) = p.action(cx, ActionTarget::Provider, &types.join("/"), action, &body).await? {
            return Ok(r);
        }
        return Err(not_built(cx, ns, types));
    }
    if types.is_empty() {
        return match p.action(cx, ActionTarget::Provider, "", action, &body).await? {
            Some(r) => Ok(r),
            None => Err(ArmError::not_implemented(format!("POST {}", cx.req.decoded_path()))),
        };
    }
    let rg_s = rg_name(rg);
    let gname = if rg_s.is_empty() { String::new() } else { canonical_group_name(cx, sub, rg_s)? };
    let k = resource_key(sub, &gname, ns, types, names);
    let target = cx.inner.state.lock().resources.get(&k).cloned();
    let Some(target) = target else {
        return Err(ArmError::resource_not_found(&type_string(ns, types), &names.join("/"), rg_s));
    };
    match p.action(cx, ActionTarget::Resource(&target), &types.join("/"), action, &body).await? {
        Some(r) => Ok(r),
        None => Err(ArmError::not_implemented(format!("POST {}", cx.req.decoded_path()))),
    }
}
