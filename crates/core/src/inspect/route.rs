//! `GET /_azemu/inspect/...`: the route, the root node and the node of the emulator itself.

use std::sync::Arc;

use http::{header, Method, StatusCode};
use percent_encoding::percent_decode_str;
use serde_json::{json, Map, Value};

use super::{InspectError, InspectRequest, Node};
use crate::resp::Resp;
use crate::{json_error, Query, Response, Service};

pub(crate) const PREFIX: &str = "/_azemu/inspect";

/// Is this (collapsed) admin path one of the inspector's?
pub(crate) fn is_inspect_path(path: &str) -> bool {
    path == PREFIX || path.strip_prefix(PREFIX).is_some_and(|rest| rest.starts_with('/'))
}

/// What the `_emulator` nodes show, taken from the emulator's own admin answers. Built only when the request needs it.
pub(crate) struct EmulatorView {
    pub health: Value,
    pub config: Value,
    pub unsupported: Value,
    pub faults: Value,
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn error(e: InspectError) -> Response {
    let (status, code, message) = e.status_code();
    no_store(json_error::response(status, code, &message))
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, http::HeaderValue::from_static("no-store"));
    r.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, http::HeaderValue::from_static("nosniff"));
    r
}

fn answer(result: Result<Node, InspectError>) -> Response {
    match result {
        Ok(node) => no_store(Resp::new(StatusCode::OK).json(&node).build()),
        Err(e) => error(e),
    }
}

/// Answer a request for `path` (an admin path with the slashes collapsed, under `/_azemu/inspect`).
pub(crate) fn handle(
    services: &[Arc<dyn Service>],
    method: &Method,
    path: &str,
    query: &str,
    emulator: impl FnOnce() -> EmulatorView,
) -> Response {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Resp::new(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .json(&json!({"error": {"code": "MethodNotAllowed", "message": "The inspector only reads: GET and HEAD. To change state use the services, or POST /_azemu/reset."}}))
            .build();
    }
    let rest = path.strip_prefix(PREFIX).unwrap_or("");
    let segments: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(decode).collect();
    let query = Query::parse(query);
    let Some(first) = segments.first() else {
        return answer(root(services, query));
    };
    if first == "_emulator" {
        return answer(emulator_node(&segments[1..], query, emulator()));
    }
    let Some(svc) = services.iter().find(|s| s.name() == first.as_str()) else {
        let names: Vec<&str> = services.iter().map(|s| s.name()).collect();
        return error(InspectError::not_found(format!("No service named '{first}' is mounted. Mounted: {}.", names.join(", "))));
    };
    let req = match InspectRequest::new(svc.name(), segments[1..].to_vec(), query) {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    // a service that panics while it looks must not take the connection with it
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| svc.inspect(&req))) {
        Ok(r) => answer(r.map(|node| req.limit_answer(node))),
        Err(_) => json_error::response(StatusCode::INTERNAL_SERVER_ERROR, "InspectorFailed", "The inspector of this service failed."),
    }
}

/// `/_azemu/inspect`: the mounted services with the number of accounts each has.
fn root(services: &[Arc<dyn Service>], query: Query) -> Result<Node, InspectError> {
    let mut node = Node::new("/", "root", "azemu").prop("version", crate::VERSION).prop("commit", crate::GIT_COMMIT);
    node = node.child("_emulator", "emulator", None, format!("/{}", "_emulator"));
    let mut without: Vec<&str> = Vec::new();
    for s in services {
        let req = InspectRequest::new(s.name(), Vec::new(), query.clone())?;
        // asking for one row is enough to learn the count of accounts and whether there is an inspector at all
        let req = InspectRequest { limit: 1, ..req };
        match s.inspect(&req) {
            Ok(n) => {
                let count = n.properties.get("accounts").and_then(Value::as_u64);
                node = node.child(s.name(), "service", count, format!("/{}", s.name()));
            }
            Err(InspectError::NoInspector) => without.push(s.name()),
            Err(_) => node = node.child(s.name(), "service", None, format!("/{}", s.name())),
        }
    }
    Ok(node.prop("servicesWithoutInspector", without))
}

fn emulator_node(rest: &[String], query: Query, v: EmulatorView) -> Result<Node, InspectError> {
    let req = InspectRequest::new("_emulator", rest.to_vec(), query)?;
    let object = |v: &Value| v.as_object().cloned().unwrap_or_default();
    let missing = || InspectError::not_found(format!("There is no node '/_emulator/{}'.", rest.join("/")));
    match rest.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            let mut n = Node::new("/_emulator", "emulator", "The emulator")
                .prop("version", &v.health["version"])
                .prop("commit", &v.health["commit"])
                .prop("services", &v.health["services"])
                .prop("strictAuth", &v.config["strictAuth"])
                .prop("faultRules", &v.config["faultRules"])
                .prop("crossSiteCheck", &v.config["crossSiteCheck"]);
            if let Some(p) = v.health.get("persistence") {
                n = n.prop("persistence", p);
            }
            let faults = v.faults["rules"].as_array().map_or(0, Vec::len) as u64;
            let ops = v.unsupported["notImplemented"].as_array().map_or(0, Vec::len) as u64;
            Ok(n.child("health", "health", None, "/_emulator/health".into())
                .child("config", "config", None, "/_emulator/config".into())
                .child("unsupported", "unsupported", Some(ops), "/_emulator/unsupported".into())
                .child("faults", "faults", Some(faults), "/_emulator/faults".into()))
        }
        ["health"] => Ok(with_properties(Node::new("/_emulator/health", "health", "Health"), object(&v.health))),
        ["config"] => {
            // the settings are the rows; everything else is a property
            let mut props = object(&v.config);
            let settings = props.remove("settings").and_then(|s| s.as_array().cloned()).unwrap_or_default();
            let (range, next) = req.page_range(settings.len());
            Ok(with_properties(Node::new("/_emulator/config", "config", "Configuration"), props).with_items(settings[range].to_vec(), next))
        }
        ["unsupported"] => {
            let rows = v.unsupported["notImplemented"].as_array().cloned().unwrap_or_default();
            let (range, next) = req.page_range(rows.len());
            Ok(Node::new("/_emulator/unsupported", "unsupported", "Operations asked for and not built")
                .prop("storedOnly", &v.unsupported["storedOnly"])
                .with_items(rows[range].to_vec(), next))
        }
        ["faults"] => {
            let rows = v.faults["rules"].as_array().cloned().unwrap_or_default();
            let (range, next) = req.page_range(rows.len());
            Ok(Node::new("/_emulator/faults", "faults", "Fault rules").prop("rules", rows.len()).with_items(rows[range].to_vec(), next))
        }
        _ => Err(missing()),
    }
}

fn with_properties(mut n: Node, props: Map<String, Value>) -> Node {
    n.properties = props;
    n
}
