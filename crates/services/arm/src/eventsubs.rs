//! Event Grid event subscriptions through ARM.
//!
//! Four paths lead here (see `locate`):
//!
//! * `{resource id}/providers/Microsoft.EventGrid/eventSubscriptions/{name}`, an extension resource: this is what
//!   `azurerm_eventgrid_event_subscription` and most templates use. The resource is a storage account, a key
//!   vault, an App Configuration store (events the emulator raises) or a custom topic;
//! * `Microsoft.EventGrid/topics/{t}/eventSubscriptions/{name}`, the same thing as the extension form on a topic;
//! * `Microsoft.EventGrid/systemTopics/{st}/eventSubscriptions/{name}`, a child of a system topic (the source is the
//!   system topic's `properties.source`);
//! * the lists at subscription, group and location level.
//!
//! The subscription itself lives in the Event Grid service: for a source (storage account, vault, store) through
//! `EventGridService::put_source_subscription` with the real resource id, so events carry Azure's `topic`; for a custom
//! topic through the admin API. ARM keeps its own record (the body the client sent plus defaults) in the same state map as
//! every other resource, so deleting a group or a parent resource removes it through the provider's `delete`.
//!
//! Subscriptions at subscription or resource group scope receive Azure Resource Manager events, which the emulator does
//! not raise: a PUT there answers 501.

use azemu_core::Response;
use azemu_eventgrid::{SourceRef, SubscriptionConfig};
use serde_json::{json, Map, Value};

use crate::backends::{call, Call};
use crate::catalog;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::groups::canonical_group_name_for;
use crate::ids::{resource_id, type_string};
use crate::providers::dp_error;
use crate::state::{key, Stored};
use crate::util;

const EVENTGRID: &str = "Microsoft.EventGrid";

/// Resource types whose events the emulator raises (what a system topic or an extension subscription can hang on).
const SOURCE_TYPES: &[&str] =
    &["microsoft.storage/storageaccounts", "microsoft.keyvault/vaults", "microsoft.appconfiguration/configurationstores"];

/// The `topicType` Azure uses for each source type.
pub fn topic_type_of(source_type: &str) -> Option<&'static str> {
    match source_type.to_ascii_lowercase().as_str() {
        "microsoft.storage/storageaccounts" => Some("Microsoft.Storage.StorageAccounts"),
        "microsoft.keyvault/vaults" => Some("Microsoft.KeyVault.vaults"),
        "microsoft.appconfiguration/configurationstores" => Some("Microsoft.AppConfiguration.ConfigurationStores"),
        _ => None,
    }
}

pub fn is_source_type(t: &str) -> bool {
    SOURCE_TYPES.contains(&t.to_ascii_lowercase().as_str())
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Where in the path the subscription (or the list) hangs.
enum Place {
    /// `/subscriptions/{s}[/resourceGroups/{g}]/providers/Microsoft.EventGrid/eventSubscriptions`
    Group,
    /// `.../providers/Microsoft.EventGrid/locations/{l}/eventSubscriptions`
    Regional(String),
    SystemTopic(String),
    /// An extension on a resource (or the alias on a custom topic).
    Resource {
        ns: String,
        types: Vec<String>,
        names: Vec<String>,
    },
}

struct Located {
    place: Place,
    name: Option<String>,
    action: Option<String>,
}

fn locate(ns_raw: &str, tail: &[String]) -> Option<Located> {
    let more = |from: usize| (tail.get(from).cloned(), tail.get(from + 1).cloned());
    if eq(ns_raw, EVENTGRID) {
        if tail.first().map(|s| eq(s, "eventSubscriptions")).unwrap_or(false) {
            let (name, action) = more(1);
            return Some(Located { place: Place::Group, name, action });
        }
        if tail.len() == 3 && eq(&tail[0], "locations") && eq(&tail[2], "eventSubscriptions") {
            return Some(Located { place: Place::Regional(tail[1].clone()), name: None, action: None });
        }
        if tail.len() >= 3 && eq(&tail[0], "systemTopics") && eq(&tail[2], "eventSubscriptions") {
            let (name, action) = more(3);
            return Some(Located { place: Place::SystemTopic(tail[1].clone()), name, action });
        }
        if tail.len() >= 3 && eq(&tail[0], "topics") && eq(&tail[2], "eventSubscriptions") {
            let (name, action) = more(3);
            return Some(Located {
                place: Place::Resource { ns: EVENTGRID.to_string(), types: vec![tail[0].clone()], names: vec![tail[1].clone()] },
                name,
                action,
            });
        }
    }
    // `{type}/{name}[/{type}/{name}]/providers/Microsoft.EventGrid/eventSubscriptions[/{name}[/{action}]]`
    let mut i = 2;
    while i + 2 < tail.len() {
        if eq(&tail[i], "providers") && eq(&tail[i + 1], EVENTGRID) && eq(&tail[i + 2], "eventSubscriptions") {
            let pairs = &tail[..i];
            let types: Vec<String> = pairs.iter().step_by(2).cloned().collect();
            let names: Vec<String> = pairs.iter().skip(1).step_by(2).cloned().collect();
            let ns = catalog::canonical_namespace(ns_raw).map(str::to_string).unwrap_or_else(|| ns_raw.to_string());
            let (name, action) = more(i + 3);
            return Some(Located { place: Place::Resource { ns, types, names }, name, action });
        }
        i += 2;
    }
    None
}

/// `Some` when the path is an event subscription path (or a list of them).
pub async fn try_handle(cx: &Cx<'_>, sub: &str, rg: Option<&str>, ns_raw: &str, tail: &[String]) -> Option<ArmResult<Response>> {
    let loc = locate(ns_raw, tail)?;
    Some(run(cx, sub, rg, loc).await)
}

/// Where a subscription is kept in the Event Grid service.
#[derive(Clone)]
enum Target {
    /// A source of events: the resource id events carry as `topic`.
    Source(String),
    /// A custom topic, by name.
    Topic(String),
}

struct Ctx {
    sub: String,
    rg: String,
    /// The system topic or the resource the subscription hangs on.
    parent_id: String,
    /// The id of a subscription named `n` is `{prefix}/eventSubscriptions/{n}`.
    prefix: String,
    system_topic: Option<String>,
    target: Target,
}

fn not_found_sub(ctx: &Ctx, name: &str) -> ArmError {
    ArmError::resource_not_found("Microsoft.EventGrid/eventSubscriptions", name, &ctx.rg)
}

async fn run(cx: &Cx<'_>, sub: &str, rg: Option<&str>, loc: Located) -> ArmResult<Response> {
    let method = cx.req.method.as_str();
    let gname = match rg {
        Some(r) => canonical_group_name_for(cx, sub, r)?,
        None => String::new(),
    };
    let ctx = match &loc.place {
        Place::Group | Place::Regional(_) => {
            return match (&loc.name, method) {
                (None, "GET") => Ok(list_global(cx, sub, &gname, if let Place::Regional(l) = &loc.place { Some(l.as_str()) } else { None })),
                (None, _) => Err(method_not_allowed(method)),
                (Some(_), "PUT" | "PATCH" | "POST") => Err(ArmError::not_implemented(format!(
                    "{method} {}: event subscriptions at subscription or resource group scope receive Azure Resource Manager events, which the emulator does not raise; subscribe on a storage account, a key vault, an App Configuration store, a custom topic or a system topic",
                    cx.req.decoded_path()
                ))),
                (Some(n), "GET") => Err(ArmError::resource_not_found("Microsoft.EventGrid/eventSubscriptions", n, &gname)),
                (Some(_), "DELETE") => Ok(util::empty(204)),
                (Some(_), _) => Err(method_not_allowed(method)),
            };
        }
        Place::SystemTopic(st_name) => {
            let id = resource_id(sub, &gname, EVENTGRID, &["systemTopics".to_string()], std::slice::from_ref(st_name));
            let st = cx.inner.state.lock().resources.get(&key(&id)).cloned();
            let st = st.ok_or_else(|| ArmError::resource_not_found("Microsoft.EventGrid/systemTopics", st_name, &gname))?;
            let source = st.prop_str("source").unwrap_or("").to_string();
            Ctx {
                sub: sub.to_string(),
                rg: gname.clone(),
                parent_id: st.id.clone(),
                prefix: st.id.clone(),
                system_topic: Some(st.name().to_string()),
                target: Target::Source(source_id_in_state(cx, &source).unwrap_or(source)),
            }
        }
        Place::Resource { ns, types, names } => {
            let id = resource_id(sub, &gname, ns, types, names);
            let t = type_string(ns, types);
            let found = cx.inner.state.lock().resources.get(&key(&id)).cloned();
            let target = if eq(&t, "Microsoft.EventGrid/topics") {
                let f = found.as_ref().ok_or_else(|| ArmError::resource_not_found(&t, &names.join("/"), &gname))?;
                Target::Topic(f.name().to_string())
            } else if is_source_type(&t) {
                let f = found.as_ref().ok_or_else(|| ArmError::resource_not_found(&t, &names.join("/"), &gname))?;
                Target::Source(f.id.clone())
            } else {
                return Err(ArmError::not_implemented(format!(
                    "{method} {}: the emulator raises no events for {t}; event subscriptions are built for storage accounts, key vaults, App Configuration stores, custom topics and system topics",
                    cx.req.decoded_path()
                )));
            };
            let parent_id = found.map(|f| f.id).unwrap_or(id);
            Ctx {
                sub: sub.to_string(),
                rg: gname.clone(),
                prefix: format!("{parent_id}/providers/{EVENTGRID}"),
                parent_id,
                system_topic: None,
                target,
            }
        }
    };
    match (&loc.name, method) {
        (None, "GET") => Ok(list_children(cx, &ctx)),
        (None, _) => Err(method_not_allowed(method)),
        (Some(n), _) => {
            validate_name(n)?;
            match (method, loc.action.as_deref()) {
                ("PUT", None) => put_sub(cx, &ctx, n, false).await,
                ("PATCH", None) => put_sub(cx, &ctx, n, true).await,
                ("GET", None) => get_sub(cx, &ctx, n).await,
                ("DELETE", None) => delete_sub(cx, &ctx, n).await,
                ("POST", Some(a)) if eq(a, "getFullUrl") => full_url(cx, &ctx, n),
                ("POST", Some(a)) if eq(a, "getDeliveryAttributes") => delivery_attributes(cx, &ctx, n),
                ("POST", Some(a)) => Err(ArmError::not_implemented(format!("POST {} ({a})", cx.req.decoded_path()))),
                (m, _) => Err(method_not_allowed(m)),
            }
        }
    }
}

fn method_not_allowed(m: &str) -> ArmError {
    ArmError::new(405, "MethodNotAllowed", format!("The method {m} is not allowed on this resource."))
}

fn validate_name(n: &str) -> ArmResult<()> {
    if (3..=64).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(ArmError::bad_request(
            "InvalidRequest",
            format!("The event subscription name '{n}' is not valid. Use 3 to 64 letters, numbers and hyphens."),
        ))
    }
}

/// The id of a source as ARM stores it (the spelling of the stored resource), when ARM has it.
fn source_id_in_state(cx: &Cx<'_>, id: &str) -> Option<String> {
    cx.inner.state.lock().resources.get(&key(id)).map(|s| s.id.clone())
}

fn sub_info(res: &Stored) -> Option<&Value> {
    res.extra.get("eventSub")
}

fn info_str<'a>(res: &'a Stored, field: &str) -> &'a str {
    sub_info(res).and_then(|i| i.get(field)).and_then(Value::as_str).unwrap_or("")
}

fn target_of(res: &Stored) -> Target {
    match info_str(res, "kind") {
        "topic" => Target::Topic(info_str(res, "target").to_string()),
        _ => Target::Source(info_str(res, "target").to_string()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Listing

fn all_subs(cx: &Cx<'_>) -> Vec<Stored> {
    cx.inner.state.lock().resources.values().filter(|s| sub_info(s).is_some()).cloned().collect()
}

fn list_children(cx: &Cx<'_>, ctx: &Ctx) -> Response {
    let want = ctx.parent_id.to_ascii_lowercase();
    let items: Vec<Value> =
        all_subs(cx).into_iter().filter(|s| info_str(s, "parent").to_ascii_lowercase() == want).map(|s| render(&s, "Succeeded")).collect();
    util::list(items)
}

/// Group or subscription level: the "global" subscriptions (storage accounts, vaults and stores); regional: those on custom
/// topics of that location.
fn list_global(cx: &Cx<'_>, sub: &str, gname: &str, location: Option<&str>) -> Response {
    let prefix = if gname.is_empty() {
        format!("/subscriptions/{}/", sub.to_ascii_lowercase())
    } else {
        format!("{}/", crate::state::group_key(sub, gname))
    };
    let items: Vec<Value> = all_subs(cx)
        .into_iter()
        .filter(|s| s.id.to_ascii_lowercase().starts_with(&prefix))
        .filter(|s| info_str(s, "system_topic").is_empty())
        .filter(|s| match location {
            None => info_str(s, "kind") == "source",
            Some(l) => {
                info_str(s, "kind") == "topic"
                    && topic_location(cx, s).map(|x| x.eq_ignore_ascii_case(&crate::groups::normalize_location(l))).unwrap_or(false)
            }
        })
        .map(|s| render(&s, "Succeeded"))
        .collect();
    util::list(items)
}

fn topic_location(cx: &Cx<'_>, s: &Stored) -> Option<String> {
    let parent = info_str(s, "parent").to_string();
    cx.inner.state.lock().resources.get(&key(&parent)).and_then(|t| t.body.get("location").and_then(Value::as_str).map(String::from))
}

// ---------------------------------------------------------------------------------------------------------------
// Rendering

fn render(res: &Stored, state: &str) -> Value {
    let mut v = res.body.clone();
    let p = util::props_mut(&mut v);
    p.insert("provisioningState".into(), json!(state));
    // Azure does not show the full webhook URL (it can carry a secret); `getFullUrl` does.
    if let Some(dp) = p.get_mut("destination").and_then(|d| d.get_mut("properties")).and_then(Value::as_object_mut) {
        dp.remove("endpointUrl");
    }
    v
}

/// `provisioningState` as the Event Grid service sees the subscription now (a webhook waiting for a manual validation
/// is `AwaitingManualAction` until the validation URL is called).
async fn live_state(cx: &Cx<'_>, res: &Stored) -> &'static str {
    let view = match target_of(res) {
        Target::Source(id) => match (&cx.inner.backends.eventgrid, SourceRef::from_resource_id(&id)) {
            (Some(eg), Ok(src)) => eg.get_source_subscription(&src, res.name()),
            _ => None,
        },
        Target::Topic(t) => match &cx.inner.backends.eventgrid {
            Some(_) => {
                let path = format!("/_azemu/eventgrid/topics/{t}/subscriptions/{}", res.name());
                let r = eg_call(cx, "GET", &path, None).await;
                r.filter(|r| r.status().as_u16() == 200).and_then(|r| serde_json::from_slice::<Value>(r.body()).ok())
            }
            None => None,
        },
    };
    match view.as_ref().and_then(|v| v["validation"]["status"].as_str()) {
        Some("AwaitingManualAction") => "AwaitingManualAction",
        Some("Failed") => "Failed",
        _ => "Succeeded",
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Operations

async fn get_sub(cx: &Cx<'_>, ctx: &Ctx, name: &str) -> ArmResult<Response> {
    let id = sub_id(ctx, name);
    let res = cx.inner.state.lock().resources.get(&key(&id)).cloned();
    let res = res.ok_or_else(|| not_found_sub(ctx, name))?;
    let st = live_state(cx, &res).await;
    Ok(util::json_ok(200, &render(&res, st)))
}

fn sub_id(ctx: &Ctx, name: &str) -> String {
    format!("{}/eventSubscriptions/{name}", ctx.prefix)
}

fn full_url(cx: &Cx<'_>, ctx: &Ctx, name: &str) -> ArmResult<Response> {
    let res = cx.inner.state.lock().resources.get(&key(&sub_id(ctx, name))).cloned();
    let res = res.ok_or_else(|| not_found_sub(ctx, name))?;
    let dest = &res.body["properties"]["destination"];
    if !dest["endpointType"].as_str().map(|t| eq(t, "WebHook")).unwrap_or(false) {
        return Err(ArmError::bad_request(
            "InvalidRequest",
            "The full URL is only available for event subscriptions with a WebHook destination.",
        ));
    }
    let url = dest["properties"]["endpointUrl"].as_str().unwrap_or("");
    Ok(util::json_ok(200, &json!({ "endpointUrl": url })))
}

fn delivery_attributes(cx: &Cx<'_>, ctx: &Ctx, name: &str) -> ArmResult<Response> {
    let res = cx.inner.state.lock().resources.get(&key(&sub_id(ctx, name))).cloned();
    let res = res.ok_or_else(|| not_found_sub(ctx, name))?;
    let maps = res.body["properties"]["destination"]["properties"]["deliveryAttributeMappings"].clone();
    let value = if maps.is_array() { maps } else { json!([]) };
    Ok(util::json_ok(200, &json!({ "value": value })))
}

async fn delete_sub(cx: &Cx<'_>, ctx: &Ctx, name: &str) -> ArmResult<Response> {
    let id = sub_id(ctx, name);
    let res = cx.inner.state.lock().resources.get(&key(&id)).cloned();
    let Some(res) = res else { return Ok(util::empty(204)) };
    delete_backend(cx, &res).await?;
    cx.inner.state.lock().resources.remove(&key(&id));
    Ok(util::empty(200))
}

/// Remove the subscription from the Event Grid service (also called when its group or parent is deleted).
pub async fn delete_backend(cx: &Cx<'_>, res: &Stored) -> ArmResult<()> {
    if cx.inner.backends.eventgrid.is_none() {
        return Ok(());
    }
    match target_of(res) {
        Target::Source(id) => {
            if let (Some(eg), Ok(src)) = (&cx.inner.backends.eventgrid, SourceRef::from_resource_id(&id)) {
                eg.delete_source_subscription(&src, res.name());
            }
        }
        Target::Topic(t) => {
            let path = format!("/_azemu/eventgrid/topics/{t}/subscriptions/{}", res.name());
            if let Some(r) = eg_call(cx, "DELETE", &path, None).await {
                let s = r.status().as_u16();
                if s >= 400 && s != 404 {
                    return Err(dp_error(&r));
                }
            }
        }
    }
    Ok(())
}

/// Send a request to the Event Grid admin API in this process.
async fn eg_call(cx: &Cx<'_>, method: &str, path: &str, body: Option<Value>) -> Option<Response> {
    let eg = cx.inner.backends.eventgrid_service()?;
    let suffix = cx.port_suffix();
    Some(
        call(
            &eg,
            Call {
                method,
                path,
                query: "",
                headers: vec![("Content-Type", "application/json".into())],
                body: body.map(|b| b.to_string().into_bytes()).unwrap_or_default(),
                label: "eventgrid",
                account: "",
                port_suffix: &suffix,
            },
        )
        .await,
    )
}

fn drop_nulls(v: &mut Value) {
    if let Value::Object(m) = v {
        m.retain(|_, x| !x.is_null());
        for x in m.values_mut() {
            drop_nulls(x);
        }
    } else if let Value::Array(a) = v {
        for x in a {
            drop_nulls(x);
        }
    }
}

/// Fill in what Azure fills in and check what Azure checks. Returns the properties to store.
fn complete_properties(mut props: Map<String, Value>, source_id: &str) -> ArmResult<Map<String, Value>> {
    props.remove("provisioningState");
    props.remove("topic");
    let Some(Value::Object(dest)) = props.get_mut("destination") else {
        return Err(ArmError::bad_request("InvalidRequest", "The event subscription needs a destination (properties.destination)."));
    };
    let kind = dest.get("endpointType").and_then(Value::as_str).unwrap_or("").to_string();
    match kind.to_ascii_lowercase().as_str() {
        "webhook" => {
            let dp = dest.entry("properties").or_insert_with(|| json!({}));
            let Some(dp) = dp.as_object_mut() else {
                return Err(ArmError::bad_request("InvalidRequest", "destination.properties must be an object."));
            };
            let url = dp.get("endpointUrl").and_then(Value::as_str).unwrap_or("").to_string();
            if url.is_empty() {
                return Err(ArmError::bad_request("InvalidRequest", "A WebHook destination needs properties.endpointUrl."));
            }
            util::set_default(dp, "maxEventsPerBatch", json!(1));
            util::set_default(dp, "preferredBatchSizeInKilobytes", json!(64));
            dp.insert("endpointBaseUrl".into(), json!(url.split('?').next().unwrap_or(&url)));
        }
        "storagequeue" | "servicebusqueue" | "servicebustopic" | "eventhub" => {}
        "" => return Err(ArmError::bad_request("InvalidRequest", "The destination needs an endpointType.")),
        _ => {
            return Err(ArmError::not_implemented(format!(
                "event subscription destination {kind} (built: WebHook, StorageQueue, ServiceBusQueue, ServiceBusTopic, EventHub)"
            )))
        }
    }
    if props.contains_key("deliveryWithResourceIdentity") || props.contains_key("deadLetterWithResourceIdentity") {
        return Err(ArmError::not_implemented(
            "event subscriptions with a managed identity (deliveryWithResourceIdentity, deadLetterWithResourceIdentity)",
        ));
    }
    let filter = props.entry("filter").or_insert_with(|| json!({}));
    if let Some(f) = filter.as_object_mut() {
        util::set_default(f, "subjectBeginsWith", json!(""));
        util::set_default(f, "subjectEndsWith", json!(""));
        util::set_default(f, "isSubjectCaseSensitive", json!(false));
    }
    util::set_default(&mut props, "labels", json!([]));
    util::set_default(&mut props, "eventDeliverySchema", json!("EventGridSchema"));
    let retry = props.entry("retryPolicy").or_insert_with(|| json!({}));
    if let Some(r) = retry.as_object_mut() {
        util::set_default(r, "maxDeliveryAttempts", json!(30));
        util::set_default(r, "eventTimeToLiveInMinutes", json!(1440));
    }
    props.insert("topic".into(), json!(source_id));
    Ok(props)
}

/// The body for the Event Grid service (`SubscriptionConfig`).
fn config_for_backend(props: &Map<String, Value>) -> ArmResult<SubscriptionConfig> {
    let mut filter = props.get("filter").cloned().unwrap_or_else(|| json!({}));
    if let Some(f) = filter.as_object_mut() {
        for k in ["subjectBeginsWith", "subjectEndsWith"] {
            if f.get(k).and_then(Value::as_str) == Some("") {
                f.remove(k);
            }
        }
    }
    let mut cfg = json!({
        "destination": props.get("destination").cloned().unwrap_or(Value::Null),
        "deliverySchema": props.get("eventDeliverySchema").cloned().unwrap_or(json!("EventGridSchema")),
        "filter": filter,
        "retryPolicy": props.get("retryPolicy").cloned().unwrap_or_else(|| json!({})),
    });
    // Azure sends 0-length values and nulls where nothing is set.
    drop_nulls(&mut cfg);
    serde_json::from_value(cfg).map_err(|e| ArmError::bad_request("InvalidRequest", format!("The event subscription is not valid: {e}.")))
}

fn backend_error(status: u16, message: String) -> ArmError {
    match status {
        404 => ArmError::not_found("ResourceNotFound", message),
        501 => ArmError::not_implemented(message),
        s if s >= 500 => ArmError::new(s, "InternalServerError", message),
        _ => ArmError::bad_request("InvalidRequest", message),
    }
}

fn validation_failure(url: &str, detail: &str) -> ArmError {
    ArmError::bad_request(
        "Url validation",
        format!("Webhook validation handshake failed for {url}. {detail} For troubleshooting, visit https://aka.ms/esvalidationfailure."),
    )
}

async fn put_sub(cx: &Cx<'_>, ctx: &Ctx, name: &str, patch: bool) -> ArmResult<Response> {
    // a write into the group: a group delete waits for it, and one that has started refuses it
    let _writing = cx.inner.begin_write(&ctx.sub, &ctx.rg)?;
    let body = util::body_object(&cx.req.body)?;
    let id = sub_id(ctx, name);
    let existing = cx.inner.state.lock().resources.get(&key(&id)).cloned();
    if patch && existing.is_none() {
        return Err(not_found_sub(ctx, name));
    }
    // PUT sends `{"properties": {...}}`; the update parameters of PATCH are the properties themselves (both accepted).
    let mut incoming = body.get("properties").filter(|p| p.is_object()).cloned().unwrap_or_else(|| body.clone());
    drop_nulls(&mut incoming);
    let props_in = match (&existing, patch) {
        (Some(e), true) => {
            let mut base = e.body.get("properties").cloned().unwrap_or_else(|| json!({}));
            util::deep_merge(&mut base, &incoming);
            base
        }
        _ => incoming,
    };
    let Value::Object(props_in) = props_in else {
        return Err(ArmError::bad_request("InvalidRequest", "The event subscription properties must be an object."));
    };
    let source_id = match &ctx.target {
        Target::Source(s) => s.clone(),
        Target::Topic(_) => ctx.parent_id.clone(),
    };
    let props = complete_properties(props_in, &source_id)?;
    let cfg = config_for_backend(&props)?;
    let webhook_url = props["destination"]["properties"]["endpointUrl"]
        .as_str()
        .filter(|_| eq(props["destination"]["endpointType"].as_str().unwrap_or(""), "WebHook"))
        .map(String::from);

    // Create or replace the subscription in the Event Grid service.
    if let Some(eg) = cx.inner.backends.eventgrid.clone() {
        let base = cx.req.ctx.endpoint.base_url();
        let (created, validation) = match &ctx.target {
            Target::Source(sid) => {
                let src = SourceRef::from_resource_id(sid).map_err(|m| ArmError::bad_request("InvalidRequest", m))?;
                let out = eg.put_source_subscription(&src, name, cfg, Some(&base)).await.map_err(|e| backend_error(e.status, e.message))?;
                (out.created, out.view["validation"].clone())
            }
            Target::Topic(t) => {
                let path = format!("/_azemu/eventgrid/topics/{t}/subscriptions/{name}");
                let body = serde_json::to_value(&cfg).unwrap_or(Value::Null);
                let r = eg_call(cx, "PUT", &path, Some(body))
                    .await
                    .ok_or_else(|| ArmError::new(500, "InternalServerError", "Event Grid is not available."))?;
                if r.status().as_u16() >= 400 {
                    return Err(dp_error(&r));
                }
                let view: Value = serde_json::from_slice(r.body()).unwrap_or(Value::Null);
                (r.status().as_u16() == 201, view["validation"].clone())
            }
        };
        if validation["status"].as_str() == Some("Failed") {
            if created {
                delete_backend_target(cx, &ctx.target, name).await;
            }
            let detail = validation["detail"].as_str().unwrap_or("");
            return Err(validation_failure(webhook_url.as_deref().unwrap_or(""), detail));
        }
    }

    let mut stored = Stored {
        id: id.clone(),
        sub: ctx.sub.clone(),
        rg: ctx.rg.clone(),
        ns: EVENTGRID.to_string(),
        types: if ctx.system_topic.is_some() {
            vec!["systemTopics".into(), "eventSubscriptions".into()]
        } else {
            vec!["eventSubscriptions".into()]
        },
        names: match &ctx.system_topic {
            Some(st) => vec![st.clone(), name.to_string()],
            None => vec![name.to_string()],
        },
        body: json!({
            "id": id,
            "name": name,
            "type": if ctx.system_topic.is_some() { "Microsoft.EventGrid/systemTopics/eventSubscriptions" } else { "Microsoft.EventGrid/eventSubscriptions" },
            "properties": Value::Object(props),
        }),
        extra: json!({ "eventSub": {
            "kind": if matches!(ctx.target, Target::Topic(_)) { "topic" } else { "source" },
            "target": match &ctx.target { Target::Source(s) => s.clone(), Target::Topic(t) => t.clone() },
            "parent": ctx.parent_id,
            "system_topic": ctx.system_topic.clone().unwrap_or_default(),
        }}),
        created: existing.as_ref().map(|e| e.created).unwrap_or(cx.now),
    };
    // Keep the backend and the record in step: remember the validation outcome in the state shown.
    let state = live_state(cx, &stored).await;
    stored.body["properties"]["provisioningState"] = json!(state);
    cx.inner.state.lock().resources.insert(key(&id), stored.clone());
    // The Event Grid spec lists 201 for PUT and PATCH of an event subscription, also when it already existed.
    Ok(util::json_ok(201, &render(&stored, state)))
}

async fn delete_backend_target(cx: &Cx<'_>, target: &Target, name: &str) {
    match target {
        Target::Source(id) => {
            if let (Some(eg), Ok(src)) = (&cx.inner.backends.eventgrid, SourceRef::from_resource_id(id)) {
                eg.delete_source_subscription(&src, name);
            }
        }
        Target::Topic(t) => {
            let path = format!("/_azemu/eventgrid/topics/{t}/subscriptions/{name}");
            eg_call(cx, "DELETE", &path, None).await;
        }
    }
}

/// For `put_by_type` (deployments): `Microsoft.EventGrid/systemTopics/eventSubscriptions` with names `[topic, name]`.
pub async fn put_system_topic_subscription(cx: &Cx<'_>, sub: &str, rg: &str, names: &[String], body: Value) -> ArmResult<Value> {
    let gname = canonical_group_name_for(cx, sub, rg)?;
    let st_name = names.first().cloned().unwrap_or_default();
    let id = resource_id(sub, &gname, EVENTGRID, &["systemTopics".to_string()], std::slice::from_ref(&st_name));
    let st = cx.inner.state.lock().resources.get(&key(&id)).cloned();
    let st = st.ok_or_else(|| ArmError::resource_not_found("Microsoft.EventGrid/systemTopics", &st_name, &gname))?;
    let source = st.prop_str("source").unwrap_or("").to_string();
    let ctx = Ctx {
        sub: sub.to_string(),
        rg: gname,
        parent_id: st.id.clone(),
        prefix: st.id.clone(),
        system_topic: Some(st.name().to_string()),
        target: Target::Source(source_id_in_state(cx, &source).unwrap_or(source)),
    };
    // `put_sub` reads the request body; a deployment passes its own.
    let name = names.get(1).cloned().unwrap_or_default();
    validate_name(&name)?;
    let r = put_sub_with(cx, &ctx, &name, body).await?;
    Ok(r)
}

async fn put_sub_with(cx: &Cx<'_>, ctx: &Ctx, name: &str, body: Value) -> ArmResult<Value> {
    // A deployment cannot change the request, so run the same steps on a request whose body is `body`.
    let req = request_with_body(cx.req, body.to_string().into_bytes());
    let inner_cx = Cx { req: &req, inner: cx.inner, now: cx.now, stored_only: cx.stored_only };
    let r = put_sub(&inner_cx, ctx, name, false).await?;
    Ok(serde_json::from_slice(r.body()).unwrap_or(Value::Null))
}

fn request_with_body(req: &azemu_core::Request, body: Vec<u8>) -> azemu_core::Request {
    azemu_core::Request {
        method: azemu_core::http::Method::PUT,
        path: req.path.clone(),
        query: azemu_core::Query::parse(req.query.raw()),
        headers: req.headers.clone(),
        body: body.into(),
        account: req.account.clone(),
        ctx: req.ctx.clone(),
    }
}
