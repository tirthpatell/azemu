//! Resource providers wired to emulated services.

use azemu_core::Response;
use serde_json::{json, Map, Value};

use crate::backends::Backends;
use crate::error::ArmError;
use crate::provider::Provider;
use crate::state::Stored;

mod appconfig;
mod cosmos;
mod eventgrid;
mod eventhubs;
mod keyvault;
mod sas;
mod servicebus;
mod storage;

pub fn all(b: &Backends) -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(storage::Storage { b: b.clone() }),
        Box::new(servicebus::ServiceBus { b: b.clone() }),
        Box::new(eventhubs::EventHubs { b: b.clone() }),
        Box::new(keyvault::KeyVault { b: b.clone() }),
        Box::new(cosmos::Cosmos { b: b.clone() }),
        Box::new(appconfig::AppConfig { b: b.clone() }),
        Box::new(eventgrid::EventGrid { b: b.clone() }),
    ]
}

/// Turn a failed answer from a service into an ARM error.
pub fn dp_error(resp: &Response) -> ArmError {
    let status = resp.status().as_u16();
    let body = String::from_utf8_lossy(resp.body()).to_string();
    let code = resp
        .headers()
        .get("x-ms-error-code")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| serde_json::from_str::<Value>(&body).ok().and_then(|v| v["error"]["code"].as_str().map(String::from)))
        .or_else(|| xml_text(&body, "Code"))
        .unwrap_or_else(|| "DataPlaneError".to_string());
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .or_else(|| xml_text(&body, "Message"))
        .unwrap_or_else(|| body.chars().take(300).collect());
    ArmError::new(if status >= 400 { status } else { 500 }, &code, message)
}

/// Percent-encode a value for a query string.
pub fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Text of the first `<tag>..</tag>`.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Text of every `<tag>..</tag>`.
pub fn xml_all(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(s) = rest.find(&open) {
        let after = &rest[s + open.len()..];
        let Some(e) = after.find(&close) else { break };
        out.push(after[..e].to_string());
        rest = &after[e + close.len()..];
    }
    out
}

pub fn stored_child(parent: &Stored, ty: &str, name: &str, body: Value) -> Stored {
    let mut types = parent.types.clone();
    types.push(ty.to_string());
    let mut names = parent.names.clone();
    names.push(name.to_string());
    let id = crate::ids::resource_id(&parent.sub, &parent.rg, &parent.ns, &types, &names);
    let mut body = body;
    body["id"] = json!(id);
    body["name"] = json!(name);
    body["type"] = json!(crate::ids::type_string(&parent.ns, &types));
    Stored {
        id,
        sub: parent.sub.clone(),
        rg: parent.rg.clone(),
        ns: parent.ns.clone(),
        types,
        names,
        body,
        extra: Value::Object(Map::new()),
        created: parent.created,
    }
}

/// Generation counter kept in `extra`.
pub fn generation(res: &Stored, which: &str) -> u64 {
    res.extra.get("gen").and_then(|g| g.get(which)).and_then(Value::as_u64).unwrap_or(0)
}

pub fn bump_generation(cx: &crate::cx::Cx<'_>, id: &str, which: &str) -> u64 {
    let mut st = cx.inner.state.lock();
    let Some(r) = st.resources.get_mut(&crate::state::key(id)) else { return 0 };
    if !r.extra.is_object() {
        r.extra = Value::Object(Map::new());
    }
    let extra = r.extra.as_object_mut().expect("extra is an object");
    let gen = extra.entry("gen").or_insert_with(|| Value::Object(Map::new()));
    let n = gen.get(which).and_then(Value::as_u64).unwrap_or(0) + 1;
    gen[which] = json!(n);
    n
}
