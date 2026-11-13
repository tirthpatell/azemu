//! `$cbs` (claims-based security) and the `$management` node.
//!
//! Shapes follow the Python SDK (`_client_base.py`, `_constants.py`): a request carries the
//! application properties `operation` (`READ`), `type` (`com.microsoft:eventhub` or
//! `com.microsoft:partition`), `name`, `partition` and `security_token`; the response carries
//! `status-code` / `status-description` and the request's message id as correlation id, and an
//! amqp-value map as body.

use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::types::Value;

use crate::amqp_conn::*;
use crate::errors::EhError;
use crate::state::*;

struct Response {
    status: i32,
    description: String,
    condition: Option<&'static str>,
    body: Option<Value>,
}

impl Response {
    fn ok(body: Value) -> Response {
        Response { status: 200, description: "OK".into(), condition: None, body: Some(body) }
    }
    fn err(e: EhError) -> Response {
        Response { status: e.status, description: e.description, condition: Some(e.condition), body: None }
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::str(k), v)).collect())
}

fn text<'a>(m: &'a Message, key: &str) -> Option<&'a str> {
    match m.app_property(key)? {
        Value::Binary(b) => std::str::from_utf8(b).ok(),
        v => v.as_str(),
    }
}

impl EhConn {
    pub(crate) fn handle_request(&mut self, link: azemu_amqp::api::LinkId, session: u16, node: Node, req: Message) {
        let correlation = req.properties.as_ref().and_then(|p| p.message_id.clone());
        let resp = match node {
            Node::Cbs => self.cbs_request(&req),
            Node::Management => self.mgmt_request(&req),
        };
        tracing::debug!("eventhubs management -> {} {}", resp.status, resp.description);
        // Event Hubs uses `status-code` on both nodes (the SDK reads `status-code` for `$management`).
        let mut props =
            vec![(Value::str("status-code"), Value::Int(resp.status)), (Value::str("status-description"), Value::String(resp.description))];
        if let Some(c) = resp.condition {
            props.push((Value::str("error-condition"), Value::Symbol(c.to_string())));
        }
        let msg = Message {
            properties: Some(Properties { correlation_id: correlation, ..Default::default() }),
            application_properties: Some(props),
            body: Body::Value(resp.body.unwrap_or(Value::Null)),
            ..Default::default()
        };
        let reply_to = req.properties.as_ref().and_then(|p| p.reply_to.clone());
        self.respond(link, session, node, reply_to.as_deref(), msg);
    }

    fn mgmt_request(&mut self, req: &Message) -> Response {
        let op = text(req, "operation").unwrap_or("");
        let ty = text(req, "type").unwrap_or("");
        if !op.eq_ignore_ascii_case("READ") {
            return Response::err(EhError::not_implemented(&format!("management operation '{op}'")));
        }
        let Some(hub) = text(req, "name").map(str::to_string).filter(|n| !n.is_empty()) else {
            return Response::err(EhError::argument("The management request has no 'name' (the event hub)."));
        };
        let now = now_ms();
        let mut st = self.shared.state.lock();
        if let Err(e) = st.ensure_hub(&self.ns, &hub, self.shared.autocreate, now) {
            return Response::err(e);
        }
        let ns = st.resolve(&self.ns);
        let Some(h) = st.hub_mut(&self.ns, &hub) else { return Response::err(EhError::entity_not_found(&ns, &hub)) };
        h.trim_all(now);
        match ty {
            "com.microsoft:eventhub" => Response::ok(map(vec![
                ("name", Value::String(h.name.clone())),
                ("created_at", Value::Timestamp(h.created_ms)),
                ("partition_count", Value::Int(h.partitions.len() as i32)),
                ("partition_ids", Value::Array(h.partition_ids().into_iter().map(Value::String).collect())),
                // The JavaScript SDK reads `georeplication_factor` (`isGeoDrEnabled` is "greater than 1"). One region here.
                ("georeplication_factor", Value::Int(1)),
                ("type", Value::String("com.microsoft:eventhub".into())),
            ])),
            "com.microsoft:partition" => {
                let pid = text(req, "partition").unwrap_or("");
                let idx = match h.parse_pid(pid) {
                    Ok(i) => i,
                    Err(e) => return Response::err(e),
                };
                let p = &h.partitions[idx];
                Response::ok(map(vec![
                    ("name", Value::String(h.name.clone())),
                    ("partition", Value::String(idx.to_string())),
                    ("begin_sequence_number", Value::Long(p.begin_seq_reported())),
                    ("last_enqueued_sequence_number", Value::Long(p.last_seq())),
                    // The Rust SDK (`azure_messaging_eventhubs`) refuses a reply without `type` and
                    // `last_enqueued_sequence_number_epoch`. Their values are not documented anywhere I found; the
                    // SDK only checks that the keys exist. 0 is a guess for the epoch.
                    ("type", Value::String("com.microsoft:partition".into())),
                    ("last_enqueued_sequence_number_epoch", Value::Long(0)),
                    ("last_enqueued_offset", Value::String(p.last_offset_string())),
                    ("last_enqueued_time_utc", Value::Timestamp(p.last_time_reported())),
                    ("is_partition_empty", Value::Bool(p.is_empty())),
                ]))
            }
            other => Response::err(EhError::not_implemented(&format!("management type '{other}'"))),
        }
    }
}

impl EhConn {
    /// `$cbs` `put-token`. Every credential is accepted (SAS tokens and JWTs alike). What Azure does
    /// check is that the entity the token is for exists: a token for a missing event hub or consumer
    /// group is answered 404, and the SDKs report that as an authentication error before they open
    /// any link. With auto-creation on, the link attach creates the entity, so the token is accepted.
    fn cbs_request(&mut self, req: &Message) -> Response {
        let op = text(req, "operation").unwrap_or("");
        if !op.eq_ignore_ascii_case("put-token") {
            return Response::err(EhError::not_implemented(&format!("$cbs operation '{op}'")));
        }
        if !self.shared.autocreate {
            if let Some((hub, group)) = text(req, "name").and_then(audience_entity) {
                let mut st = self.shared.state.lock();
                let ns = st.resolve(&self.ns);
                match st.hub_mut(&self.ns, &hub) {
                    None => return Response::err(EhError::entity_not_found(&ns, &hub)),
                    Some(h) => {
                        if let Some(cg) = group.filter(|cg| !h.has_consumer_group(cg)) {
                            return Response::err(EhError::entity_not_found(&ns, &format!("{}/ConsumerGroups/{cg}", h.name)));
                        }
                    }
                }
            }
        }
        Response { status: 202, description: "Accepted".into(), condition: None, body: None }
    }
}

/// The event hub, and the consumer group if there is one, named by a `put-token` audience:
/// `sb://host[:port]/{hub}` or `sb://host/{hub}/consumergroups/{group}[/partitions/{n}]`.
/// `None` for a namespace-level audience (no path) and for the `$`-nodes.
fn audience_entity(audience: &str) -> Option<(String, Option<String>)> {
    let path = match audience.find("://") {
        Some(i) => audience[i + 3..].split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => audience,
    };
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let hub = segs.first().filter(|h| !h.starts_with('$'))?;
    let group = segs.iter().position(|s| s.eq_ignore_ascii_case("consumergroups")).and_then(|i| segs.get(i + 1)).map(|g| g.to_string());
    Some((hub.to_string(), group))
}

#[cfg(test)]
mod tests {
    use super::audience_entity;

    #[test]
    fn audience_names_the_hub_and_the_consumer_group() {
        assert_eq!(audience_entity("sb://ns.eventhub.localhost:4577/hub1"), Some(("hub1".into(), None)));
        assert_eq!(
            audience_entity("sb://ns.eventhub.localhost/hub1/consumergroups/$Default/partitions/0"),
            Some(("hub1".into(), Some("$Default".into())))
        );
        assert_eq!(audience_entity("sb://ns.eventhub.localhost/hub1/ConsumerGroups/g1"), Some(("hub1".into(), Some("g1".into()))));
        assert_eq!(audience_entity("sb://ns.eventhub.localhost"), None);
        assert_eq!(audience_entity("sb://ns.eventhub.localhost/"), None);
        assert_eq!(audience_entity("sb://ns.eventhub.localhost/$management"), None);
    }
}
