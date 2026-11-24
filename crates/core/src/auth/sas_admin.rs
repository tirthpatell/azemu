//! The JSON admin API for authorization rules, the same for Service Bus (`/_azemu/servicebus`) and Event Hubs (`/_azemu/eventhubs`):
//!
//! | Request | Answer |
//! |---|---|
//! | `GET {prefix}/{ns}/authorization-rules` | the rules of the namespace and of its entities that apply in strict mode, with their keys |
//! | `PUT {prefix}/{ns}/authorization-rules/{name}` | declare (201) or replace (200); body `{"rights":["Send","Listen"],"entity":"queue","primaryKey":"..","secondaryKey":".."}`, `entity` and the keys optional |
//! | `GET`, `DELETE {prefix}/{ns}/authorization-rules/{name}[?entity=]` | read one; remove (204) |
//!
//! A rule declared here is kept in memory only: the config file of the service is what comes back after a reset or a restart.

use serde_json::{json, Value};

use super::sas::{SasRule, SasRules};
use crate::resp::Resp;
use crate::{json_error, Request, Response};

fn rule_json(ns: &str, entity: &Option<String>, r: &SasRule) -> Value {
    let mut v = r.to_json();
    v["namespace"] = json!(ns);
    if let Some(e) = entity {
        v["entity"] = json!(e);
    }
    v
}

/// `rest` is the path after the prefix, split: `["myns", "authorization-rules", "name"]`. `None`: not a request for rules.
pub fn handle(rules: &SasRules, req: &Request, rest: &[&str]) -> Option<Response> {
    let (ns, name) = match rest {
        [ns, w] if w.eq_ignore_ascii_case("authorization-rules") => (*ns, None),
        [ns, w, name] if w.eq_ignore_ascii_case("authorization-rules") => (*ns, Some(*name)),
        _ => return None,
    };
    let method = req.method.as_str();
    let entity_q = req.query.get("entity").map(str::trim).filter(|e| !e.is_empty()).map(str::to_string);
    Some(match (method, name) {
        ("GET", None) => {
            let mut list: Vec<Value> = rules.at(ns, None).iter().map(|r| rule_json(ns, &None, r)).collect();
            for (n, e, r) in rules.declared_list() {
                if n.eq_ignore_ascii_case(ns) && e.is_some() {
                    list.push(rule_json(ns, &e, &r));
                }
            }
            Resp::new(200u16).json(&json!({"rules": list})).build()
        }
        ("GET", Some(name)) => {
            let found = rules.at(ns, entity_q.as_deref()).into_iter().find(|r| r.name.eq_ignore_ascii_case(name));
            match found {
                Some(r) => Resp::new(200u16).json(&rule_json(ns, &entity_q, &r)).build(),
                None => json_error::response(404, "NotFound", &format!("the authorization rule '{name}' does not exist")),
            }
        }
        ("PUT", Some(name)) => {
            let mut body: Value = match serde_json::from_slice(&req.body) {
                Ok(v) => v,
                Err(e) => return Some(json_error::response(400, "BadRequest", &format!("the body is not JSON: {e}"))),
            };
            let entity = body.get("entity").and_then(|e| e.as_str()).map(str::to_string).or(entity_q);
            if let Some(o) = body.as_object_mut() {
                o.insert("name".into(), json!(name));
            }
            match SasRule::from_json(ns, entity.as_deref(), &body) {
                Ok(rule) => {
                    let existed = rules.declared_list().iter().any(|(n, e, r)| {
                        n.eq_ignore_ascii_case(ns)
                            && e.as_deref().map(str::to_ascii_lowercase) == entity.as_deref().map(str::to_ascii_lowercase)
                            && r.name.eq_ignore_ascii_case(name)
                    });
                    rules.declare(ns, entity.as_deref(), rule.clone());
                    Resp::new(if existed { 200u16 } else { 201u16 }).json(&rule_json(ns, &entity, &rule)).build()
                }
                Err(e) => json_error::response(400, "BadRequest", &e),
            }
        }
        ("DELETE", Some(name)) => {
            if rules.remove(ns, entity_q.as_deref(), name) {
                Resp::new(204u16).build()
            } else {
                json_error::response(
                    404,
                    "NotFound",
                    &format!("no declared authorization rule '{name}' (rules that Resource Manager made are changed there)"),
                )
            }
        }
        _ => json_error::response(405, "MethodNotAllowed", "use GET, PUT or DELETE"),
    })
}
