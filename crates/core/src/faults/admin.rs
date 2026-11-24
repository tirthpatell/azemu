//! `/_azemu/faults`: the admin API of the fault rules.

use bytes::Bytes;
use http::{header, Method, StatusCode};

use super::{spec, Faults};
use crate::json_error;
use crate::resp::Resp;
use crate::Response;

const PREFIX: &str = "/_azemu/faults";

/// Is this (collapsed) admin path one of the fault endpoints?
pub(crate) fn is_faults_path(path: &str) -> bool {
    path == PREFIX || path.strip_prefix(PREFIX).is_some_and(|rest| rest.starts_with('/'))
}

fn bad(message: impl AsRef<str>) -> Response {
    json_error::response(StatusCode::BAD_REQUEST, "InvalidFaultRule", message.as_ref())
}

fn not_allowed(allow: &str) -> Response {
    Resp::new(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::ALLOW, allow)
        .json(&serde_json::json!({"error": {"code": "MethodNotAllowed", "message": "Method not allowed."}}))
        .build()
}

fn parse_body(body: &Bytes) -> Result<serde_json::Value, String> {
    serde_json::from_slice(body).map_err(|e| format!("The body is not valid JSON: {e}"))
}

/// Answer a request for one of the fault endpoints. `mounted` are the service names a rule may name.
pub(crate) fn handle(faults: &Faults, mounted: &[&str], method: &Method, path: &str, body: &Bytes) -> Response {
    if path == PREFIX {
        return match *method {
            Method::GET | Method::HEAD => Resp::new(StatusCode::OK).json(&faults.list_json()).build(),
            Method::POST => {
                let value = match parse_body(body) {
                    Ok(v) => v,
                    Err(e) => return bad(e),
                };
                if value.get("rules").is_some() {
                    return bad("POST adds one rule: send the rule itself. PUT /_azemu/faults replaces the list with {\"rules\": [...]}.");
                }
                match spec::parse_rule(&value, mounted) {
                    Ok(rule) => match faults.add(rule) {
                        Ok(json) => Resp::new(StatusCode::CREATED).json(&json).build(),
                        Err(e) => json_error::response(StatusCode::CONFLICT, "TooManyFaultRules", &e),
                    },
                    Err(e) => bad(e),
                }
            }
            Method::PUT => {
                let value = match parse_body(body) {
                    Ok(v) => v,
                    Err(e) => return bad(e),
                };
                let Some(obj) = value.as_object() else {
                    return bad("The body is {\"rules\": [...]}.");
                };
                if let Some(k) = obj.keys().find(|k| *k != "rules") {
                    return bad(format!("unknown field `{k}`: the body is {{\"rules\": [...]}}"));
                }
                let Some(list) = obj.get("rules") else {
                    return bad("The body is {\"rules\": [...]}; 'rules' is missing.");
                };
                match spec::parse_rules(list, mounted) {
                    Ok(rules) => {
                        faults.replace(rules);
                        Resp::new(StatusCode::OK).json(&faults.list_json()).build()
                    }
                    Err(e) => bad(e),
                }
            }
            Method::DELETE => {
                let n = faults.clear();
                Resp::new(StatusCode::OK).json(&serde_json::json!({"removed": n})).build()
            }
            _ => not_allowed("GET, HEAD, POST, PUT, DELETE"),
        };
    }
    let id = path.strip_prefix(PREFIX).and_then(|r| r.strip_prefix('/')).unwrap_or("").trim_end_matches('/');
    if *method != Method::DELETE {
        return not_allowed("DELETE");
    }
    let Ok(id) = id.parse::<u64>() else {
        return bad(format!("'{id}' is not a rule id; ids are numbers, as GET /_azemu/faults shows."));
    };
    if faults.remove(id) {
        Resp::new(StatusCode::OK).json(&serde_json::json!({})).build()
    } else {
        json_error::response(
            StatusCode::NOT_FOUND,
            "FaultRuleNotFound",
            &format!("No fault rule has the id {id}. GET /_azemu/faults lists them."),
        )
    }
}
