//! The admin API of the Schema Registry: schema groups are made here, as Azure makes them on the management side
//! (Resource Manager, the portal), not through the data plane.
//!
//! | Request | Result |
//! |---|---|
//! | `GET /_azemu/eventhubs/{ns}/schemagroups` | every group with its schemas |
//! | `PUT /_azemu/eventhubs/{ns}/schemagroups/{group}` | create (201) or change the mode and properties (200); body `{"schemaType":"Avro","schemaCompatibility":"Forward","groupProperties":{"k":"v"}}`, all optional (Avro, None) |
//! | `GET`, `DELETE /_azemu/eventhubs/{ns}/schemagroups/{group}` | read; delete the group with its schemas (204) |

use std::collections::BTreeMap;

use azemu_core::resp::Resp;
use azemu_core::time::{iso8601_7, OffsetDateTime};
use azemu_core::{json_error, Request, Response};
use serde_json::{json, Value};

use crate::schema_registry::{Compatibility, SchemaGroup, SchemaType};
use crate::state::Shared;

fn iso(ms: i64) -> Value {
    match OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(t) => Value::String(iso8601_7(t)),
        Err(_) => Value::Null,
    }
}

fn group_json(ns: &str, g: &SchemaGroup) -> Value {
    json!({
        "name": g.name,
        "namespace": ns,
        "schemaType": g.schema_type.name(),
        "schemaCompatibility": g.compatibility.name(),
        "groupProperties": g.properties,
        "createdAt": iso(g.created_ms),
        "updatedAt": iso(g.updated_ms),
        "etag": g.etag,
        "schemas": g.schemas.values().map(|s| json!({
            "name": s.name,
            "versions": s.versions.iter().map(|v| json!({"version": v.version, "id": v.id, "bytes": v.content.len()})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

pub fn route(shared: &Shared, req: &Request, ns: &str, rest: &[&str]) -> Response {
    let method = req.method.as_str();
    match (method, rest) {
        ("GET", []) => {
            let st = shared.state.lock();
            let nsk = st.resolve(ns);
            let groups: Vec<Value> =
                st.namespaces.get(&nsk).map(|n| n.schema_groups.values().map(|g| group_json(&nsk, g)).collect()).unwrap_or_default();
            Resp::new(200u16).json(&json!({"schemaGroups": groups})).build()
        }
        ("GET", [g]) => {
            let st = shared.state.lock();
            let nsk = st.resolve(ns);
            match st.schema_group(ns, g) {
                Some(group) => Resp::new(200u16).json(&group_json(&nsk, group)).build(),
                None => json_error::response(404, "NotFound", &format!("The schema group '{g}' was not found in namespace '{nsk}'.")),
            }
        }
        ("PUT", [g]) => put(shared, req, ns, g),
        ("DELETE", [g]) => {
            let mut st = shared.state.lock();
            let nsk = st.resolve(ns);
            if st.delete_schema_group(ns, g) {
                Resp::new(204u16).build()
            } else {
                json_error::response(404, "NotFound", &format!("The schema group '{g}' was not found in namespace '{nsk}'."))
            }
        }
        _ => json_error::response(501, "NotImplemented", &format!("eventhubs: {} {}", req.method, req.decoded_path())),
    }
}

fn bad(m: String) -> Response {
    json_error::response(400, "BadRequest", &m)
}

fn put(shared: &Shared, req: &Request, ns: &str, name: &str) -> Response {
    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return bad(format!("The body is not valid JSON: {e}")),
        }
    };
    let text = |k: &str| body.get(k).and_then(Value::as_str);
    let schema_type = match text("schemaType") {
        Some(t) => match SchemaType::parse(t) {
            Some(t) => t,
            None => return bad(format!("schemaType '{t}' is not one of Avro, Json, Custom, Protobuf, Unknown.")),
        },
        None => SchemaType::Avro,
    };
    let compatibility = match text("schemaCompatibility") {
        Some(c) => match Compatibility::parse(c) {
            Some(c) => c,
            None => return bad(format!("schemaCompatibility '{c}' is not one of None, Backward, Forward.")),
        },
        None => Compatibility::None,
    };
    let properties = match body.get("groupProperties") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => Some(
            o.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect::<BTreeMap<_, _>>(),
        ),
        Some(_) => return bad("groupProperties must be an object.".into()),
    };
    let mut st = shared.state.lock();
    let nsk = st.resolve(ns);
    match st.put_schema_group(ns, name, schema_type, compatibility, properties) {
        Ok(created) => match st.schema_group(ns, name) {
            Some(g) => Resp::new(if created { 201u16 } else { 200 }).json(&group_json(&nsk, g)).build(),
            None => bad("The schema group could not be read back.".into()),
        },
        Err(e) => bad(e.message),
    }
}
