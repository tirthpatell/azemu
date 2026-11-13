//! Microsoft.ServiceBus: namespaces, queues, topics, subscriptions, rules and
//! authorization rules. Entities are created in the Service Bus emulator's own state,
//! so they work over AMQP at once.

use azemu_core::time::OffsetDateTime;
use azemu_core::{async_trait, Response};
use azemu_servicebus::config::try_apply_properties;
use azemu_servicebus::duration;
use azemu_servicebus::model::Settings;
use azemu_servicebus::rules::{CorrelationFilter, Rule, RuleFilter};
use azemu_servicebus::state::{key as sb_key, Namespace};
use azemu_servicebus_sql::{Params, Value as SqlValue};
use serde_json::{json, Value};

use super::sas;
use super::stored_child;
use crate::backends::Backends;
use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::provider::{ActionTarget, Provider, Statuses};
use crate::state::Stored;
use crate::util;

pub struct ServiceBus {
    pub b: Backends,
}

const TYPES: &[&str] = &[
    "namespaces",
    "namespaces/queues",
    "namespaces/topics",
    "namespaces/topics/subscriptions",
    "namespaces/topics/subscriptions/rules",
    "namespaces/authorizationRules",
    "namespaces/queues/authorizationRules",
    "namespaces/topics/authorizationRules",
];

const PASSTHROUGH: &[&str] = &["namespaces/networkRuleSets"];

const MAX_TTL: &str = "P10675199DT2H48M5.4775807S";

fn valid_namespace_name(n: &str) -> bool {
    let b = n.as_bytes();
    (6..=50).contains(&b.len())
        && b[0].is_ascii_alphabetic()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn ms_of(t: OffsetDateTime) -> i64 {
    (t.unix_timestamp_nanos() / 1_000_000) as i64
}

fn bad(msg: String) -> ArmError {
    ArmError::bad_request("BadRequest", msg)
}

/// Settings from ARM properties on top of the defaults of the entity kind.
fn settings_from(kind: &str, props: &Value, created_ms: i64, now: i64) -> ArmResult<Settings> {
    let mut s = match kind {
        "queues" => Settings::queue_default(now),
        "topics" => Settings::topic_default(now),
        _ => Settings::subscription_default(now),
    };
    // A duration that is not ISO 8601 is a 400 that names the property and the value. The code is a guess: the
    // resource-manager specs and the recordings under `refs/` hold no example of this error, and `InvalidRequestContent`
    // is the code ARM gives when it cannot read the body it was sent.
    try_apply_properties(&mut s, props).map_err(|e| ArmError::bad_request("InvalidRequestContent", e.message))?;
    if let Some(v) = props.get("autoDeleteOnIdle").and_then(Value::as_str).and_then(duration::parse) {
        s.auto_delete_on_idle_ms = if v >= duration::TIMESPAN_MAX_MS { None } else { Some(v) };
    }
    if let Some(b) = props.get("deadLetteringOnFilterEvaluationExceptions").and_then(Value::as_bool) {
        s.dead_letter_on_filter_exception = b;
    }
    if let Some(b) = props.get("enableExpress").and_then(Value::as_bool) {
        s.enable_express = b;
    }
    if s.max_delivery_count == 0 {
        return Err(bad("The value for MaxDeliveryCount must be at least 1.".into()));
    }
    if s.lock_duration_ms > 300_000 {
        return Err(bad("The value for LockDuration cannot be more than 5 minutes.".into()));
    }
    s.created_ms = created_ms;
    s.updated_ms = now;
    Ok(s)
}

fn count_details(active: usize, dead: usize, scheduled: usize, transfer: usize, transfer_dead: usize) -> Value {
    json!({
        "activeMessageCount": active,
        "deadLetterMessageCount": dead,
        "scheduledMessageCount": scheduled,
        "transferMessageCount": transfer,
        "transferDeadLetterMessageCount": transfer_dead,
    })
}

fn titlecase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

impl ServiceBus {
    fn ns_name<'a>(&self, res: &'a Stored) -> &'a str {
        res.name_at(0)
    }

    fn with_ns<R>(&self, ns: &str, f: impl FnOnce(&mut Namespace) -> R) -> Option<R> {
        let sb = self.b.servicebus.as_ref()?;
        let mut st = sb.state.lock();
        Some(f(st.ns(ns)))
    }

    fn entity_defaults(&self, kind: &str, res: &mut Stored, now_iso: &str) {
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "status", json!("Active"));
        util::set_default(p, "createdAt", json!(now_iso));
        util::set_default(p, "updatedAt", json!(now_iso));
        util::set_default(p, "accessedAt", json!("0001-01-01T00:00:00"));
        util::set_default(p, "sizeInBytes", json!(0));
        util::set_default(p, "messageCount", json!(0));
        util::set_default(p, "countDetails", count_details(0, 0, 0, 0, 0));
        util::set_default(p, "enableBatchedOperations", json!(true));
        util::set_default(p, "defaultMessageTimeToLive", json!(MAX_TTL));
        util::set_default(p, "autoDeleteOnIdle", json!(MAX_TTL));
        // A partitioned queue or topic is 16 times as big: with `enablePartitioning: true` and no size, Azure answers 16384 (recording
        // `test_cli_mgmt_servicebus_queue.pyTestMgmtServiceBustest_queue.json`, entry 7)
        let default_size: i64 = if p.get("enablePartitioning").and_then(|v| v.as_bool()) == Some(true) { 16 * 1024 } else { 1024 };
        match kind {
            "queues" => {
                util::set_default(p, "lockDuration", json!("PT1M"));
                util::set_default(p, "maxSizeInMegabytes", json!(default_size));
                util::set_default(p, "maxMessageSizeInKilobytes", json!(256));
                util::set_default(p, "requiresDuplicateDetection", json!(false));
                util::set_default(p, "requiresSession", json!(false));
                util::set_default(p, "deadLetteringOnMessageExpiration", json!(false));
                util::set_default(p, "duplicateDetectionHistoryTimeWindow", json!("PT10M"));
                util::set_default(p, "maxDeliveryCount", json!(10));
                util::set_default(p, "enablePartitioning", json!(false));
                util::set_default(p, "enableExpress", json!(false));
            }
            "topics" => {
                util::set_default(p, "maxSizeInMegabytes", json!(default_size));
                util::set_default(p, "maxMessageSizeInKilobytes", json!(256));
                util::set_default(p, "requiresDuplicateDetection", json!(false));
                util::set_default(p, "duplicateDetectionHistoryTimeWindow", json!("PT10M"));
                util::set_default(p, "enablePartitioning", json!(false));
                util::set_default(p, "enableExpress", json!(false));
                util::set_default(p, "supportOrdering", json!(true));
                util::set_default(p, "subscriptionCount", json!(0));
            }
            _ => {
                util::set_default(p, "lockDuration", json!("PT1M"));
                util::set_default(p, "requiresSession", json!(false));
                util::set_default(p, "deadLetteringOnMessageExpiration", json!(false));
                util::set_default(p, "deadLetteringOnFilterEvaluationExceptions", json!(true));
                util::set_default(p, "maxDeliveryCount", json!(10));
            }
        }
    }

    fn put_namespace(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) {
        let now = cx.now_iso();
        let sku_name = res.body.get("sku").and_then(|s| s.get("name")).and_then(Value::as_str).unwrap_or("Standard").to_string();
        let mut sku = json!({"name": sku_name, "tier": sku_name});
        if let Some(c) = res.body.get("sku").and_then(|s| s.get("capacity")) {
            sku["capacity"] = c.clone();
        }
        res.body["sku"] = sku;
        let sub = res.sub.clone();
        let name = res.name().to_string();
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "status", json!("Active"));
        if !existed {
            p.insert("createdAt".into(), json!(now));
        }
        p.insert("updatedAt".into(), json!(now));
        p.insert("metricId".into(), json!(format!("{sub}:{}", name.to_ascii_lowercase())));
        util::set_default(p, "zoneRedundant", json!(false));
        util::set_default(p, "disableLocalAuth", json!(false));
        util::set_default(p, "minimumTlsVersion", json!("1.2"));
        util::set_default(p, "publicNetworkAccess", json!("Enabled"));
        util::set_default(p, "premiumMessagingPartitions", json!(0));
        util::set_default(p, "privateEndpointConnections", json!([]));
        self.with_ns(&name, |_| ());
        if !existed {
            children.push(sas::root_rule(res));
            children.push(stored_child(
                res,
                "networkRuleSets",
                "default",
                json!({"properties": {"publicNetworkAccess": "Enabled", "defaultAction": "Allow", "trustedServiceAccessEnabled": false, "virtualNetworkRules": [], "ipRules": []}}),
            ));
        }
    }

    fn put_entity(&self, kind: &str, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        let now_iso = cx.now_iso();
        let now = ms_of(cx.now);
        let ns = self.ns_name(res).to_string();
        let name = res.name().to_string();
        let props = res.body.get("properties").cloned().unwrap_or(json!({}));
        let created = if existed { ms_of(res.created) } else { now };
        let settings = settings_from(kind, &props, created, now)?;
        self.entity_defaults(kind, res, &now_iso);
        if kind == "subscriptions" {
            // Azure shows the client affine settings only for a client affine subscription (the azurerm provider sends
            // them always, and a stored empty block shows up as a change in the next plan).
            let affine = res.body["properties"]["isClientAffine"].as_bool().unwrap_or(false);
            if !affine {
                util::props_mut(&mut res.body).remove("clientAffineProperties");
            }
        }
        if existed {
            util::props_mut(&mut res.body).insert("updatedAt".into(), json!(now_iso));
        }
        let topic = if kind == "subscriptions" { Some(res.name_at(1).to_string()) } else { None };
        let result: Option<Result<(), String>> = self.with_ns(&ns, |n| match kind {
            "queues" => {
                if let Some(q) = n.queue_mut(&name) {
                    q.core.settings = settings;
                    Ok(())
                } else {
                    n.create_queue(&name, settings).map_err(|e| e.description)
                }
            }
            "topics" => {
                if let Some(t) = n.topic_mut(&name) {
                    t.settings = settings;
                    Ok(())
                } else {
                    n.create_topic(&name, settings).map_err(|e| e.description)
                }
            }
            _ => {
                let topic = topic.clone().unwrap_or_default();
                match n.topic_mut(&topic) {
                    None => Err(format!("topic {topic} does not exist")),
                    Some(t) => {
                        if let Some(s) = t.subs.get_mut(&sb_key(&name)) {
                            s.core.settings = settings;
                            Ok(())
                        } else {
                            n.create_subscription(&topic, &name, settings, vec![Rule::default_rule(now)]).map_err(|e| e.description)
                        }
                    }
                }
            }
        });
        if let Some(Err(e)) = result {
            return Err(bad(e));
        }
        if kind == "subscriptions" && !existed {
            children.push(stored_child(
                res,
                "rules",
                "$Default",
                json!({"properties": {
                    "action": {},
                    "filterType": "SqlFilter",
                    "sqlFilter": {"sqlExpression": "1=1", "compatibilityLevel": 20, "requiresPreprocessing": true}
                }}),
            ));
            // The subscription count of the topic.
        }
        Ok(())
    }

    fn put_rule(&self, res: &mut Stored, cx: &Cx<'_>) -> ArmResult<()> {
        let ns = res.name_at(0).to_string();
        let topic = res.name_at(1).to_string();
        let sub = res.name_at(2).to_string();
        let name = res.name().to_string();
        let props = res.body.get("properties").cloned().unwrap_or(json!({}));
        let now = ms_of(cx.now);
        let filter_type = props.get("filterType").and_then(Value::as_str).unwrap_or("SqlFilter").to_string();
        let mut rule = match filter_type.as_str() {
            "CorrelationFilter" => {
                let c = props.get("correlationFilter").cloned().unwrap_or(json!({}));
                let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
                let mut cf = CorrelationFilter {
                    correlation_id: s("correlationId"),
                    message_id: s("messageId"),
                    to: s("to"),
                    reply_to: s("replyTo"),
                    label: s("label"),
                    session_id: s("sessionId"),
                    reply_to_session_id: s("replyToSessionId"),
                    content_type: s("contentType"),
                    properties: Default::default(),
                };
                if let Some(o) = c.get("properties").and_then(Value::as_object) {
                    for (k, v) in o {
                        if let Some(sv) = v.as_str() {
                            cf.properties.insert(k.clone(), SqlValue::Str(sv.to_string()));
                        }
                    }
                }
                Rule { name: name.clone(), filter: RuleFilter::Correlation(cf), action: None, created_ms: now }
            }
            "SqlFilter" => {
                let expr = props.get("sqlFilter").and_then(|f| f.get("sqlExpression")).and_then(Value::as_str).unwrap_or("1=1");
                Rule::sql_filter(&name, expr, Params::new(), now).map_err(|e| bad(format!("The SQL filter is not valid: {e}")))?
            }
            other => return Err(bad(format!("filterType '{other}' is not valid; use SqlFilter or CorrelationFilter."))),
        };
        if let Some(expr) = props.get("action").and_then(|a| a.get("sqlExpression")).and_then(Value::as_str) {
            rule = rule.with_action(expr, Params::new()).map_err(|e| bad(format!("The SQL action is not valid: {e}")))?;
        }
        let p = util::props_mut(&mut res.body);
        util::set_default(p, "action", json!({}));
        util::set_default(p, "filterType", json!(filter_type));
        if filter_type == "SqlFilter" {
            let f = p.entry("sqlFilter").or_insert_with(|| json!({}));
            if let Some(o) = f.as_object_mut() {
                util::set_default(o, "sqlExpression", json!("1=1"));
                util::set_default(o, "compatibilityLevel", json!(20));
                util::set_default(o, "requiresPreprocessing", json!(true));
            }
        }
        let r = self.with_ns(&ns, |n| match n.topic_mut(&topic).and_then(|t| t.subs.get_mut(&sb_key(&sub))) {
            None => Err("subscription does not exist".to_string()),
            Some(s) => {
                s.rules.retain(|r| !r.name.eq_ignore_ascii_case(&name));
                s.rules.push(rule);
                Ok(())
            }
        });
        if let Some(Err(e)) = r {
            return Err(bad(e));
        }
        Ok(())
    }

    /// Fill in live numbers from the data plane.
    fn live(&self, res: &Stored, v: &mut Value) {
        let Some(sb) = &self.b.servicebus else { return };
        let ns = res.name_at(0);
        let st = sb.state.lock();
        let Some(n) = st.namespaces.get(&sb_key(ns)) else { return };
        let (counts, size) = match res.type_path().as_str() {
            "namespaces/queues" => match n.queue(res.name()) {
                Some(q) => (Some(q.core.counts()), q.core.size_bytes()),
                None => return,
            },
            "namespaces/topics/subscriptions" => match n.topic(res.name_at(1)).and_then(|t| t.subs.get(&sb_key(res.name()))) {
                Some(s) => (Some(s.core.counts()), s.core.size_bytes()),
                None => return,
            },
            "namespaces/topics" => match n.topic(res.name()) {
                Some(t) => {
                    let p = util::props_mut(v);
                    p.insert("sizeInBytes".into(), json!(t.size_bytes()));
                    p.insert("subscriptionCount".into(), json!(t.subs.len()));
                    return;
                }
                None => return,
            },
            _ => return,
        };
        if let Some(c) = counts {
            let p = util::props_mut(v);
            p.insert("messageCount".into(), json!(c.active + c.dead_letter + c.scheduled + c.transfer + c.transfer_dead_letter));
            p.insert("sizeInBytes".into(), json!(size));
            p.insert("countDetails".into(), count_details(c.active, c.dead_letter, c.scheduled, c.transfer, c.transfer_dead_letter));
        }
    }
}

#[async_trait]
impl Provider for ServiceBus {
    fn namespace(&self) -> &'static str {
        "Microsoft.ServiceBus"
    }

    fn types(&self) -> &'static [&'static str] {
        TYPES
    }

    fn passthrough(&self) -> &'static [&'static str] {
        PASSTHROUGH
    }

    fn statuses(&self, _type_path: &str) -> Statuses {
        // 200 for a create too, namespaces included (the first PUT of a namespace answers 200 in the recordings of
        // azure-mgmt-servicebus, api-version 2021-11-01)
        Statuses { create: 200, update: 200, delete: 200, delete_missing: 204 }
    }

    fn validate_name(&self, type_path: &str, name: &str) -> ArmResult<()> {
        if type_path == "namespaces" && !valid_namespace_name(name) {
            return Err(bad(format!(
                "The specified service namespace is invalid. The name '{name}' must be 6 to 50 characters, start with a letter, end with a letter or number and use only letters, numbers and hyphens."
            )));
        }
        Ok(())
    }

    async fn put(&self, cx: &Cx<'_>, res: &mut Stored, existed: bool, children: &mut Vec<Stored>) -> ArmResult<()> {
        match res.type_path().as_str() {
            "namespaces" => {
                self.put_namespace(cx, res, existed, children);
                Ok(())
            }
            "namespaces/queues" => self.put_entity("queues", cx, res, existed, children),
            "namespaces/topics" => self.put_entity("topics", cx, res, existed, children),
            "namespaces/topics/subscriptions" => self.put_entity("subscriptions", cx, res, existed, children),
            "namespaces/topics/subscriptions/rules" => self.put_rule(res, cx),
            "namespaces/authorizationRules" | "namespaces/queues/authorizationRules" | "namespaces/topics/authorizationRules" => {
                sas::prepare(res)
            }
            _ => Ok(()),
        }
    }

    async fn delete(&self, _cx: &Cx<'_>, res: &Stored, _force: bool) -> ArmResult<()> {
        let ns = res.name_at(0).to_string();
        let name = res.name().to_string();
        match res.type_path().as_str() {
            "namespaces" => {
                if let Some(sb) = &self.b.servicebus {
                    let mut st = sb.state.lock();
                    if let Some(mut n) = st.namespaces.remove(&sb_key(&ns)) {
                        for q in n.queues.values_mut() {
                            q.core.wake_all();
                        }
                        for t in n.topics.values_mut() {
                            for s in t.subs.values_mut() {
                                s.core.wake_all();
                            }
                        }
                    }
                }
            }
            "namespaces/queues" => {
                self.with_ns(&ns, |n| {
                    if let Some(mut q) = n.queues.remove(&sb_key(&name)) {
                        q.core.wake_all();
                    }
                });
            }
            "namespaces/topics" => {
                self.with_ns(&ns, |n| {
                    if let Some(mut t) = n.topics.remove(&sb_key(&name)) {
                        for s in t.subs.values_mut() {
                            s.core.wake_all();
                        }
                    }
                });
            }
            "namespaces/topics/subscriptions" => {
                let topic = res.name_at(1).to_string();
                self.with_ns(&ns, |n| {
                    if let Some(t) = n.topic_mut(&topic) {
                        if let Some(mut s) = t.subs.remove(&sb_key(&name)) {
                            s.core.wake_all();
                        }
                    }
                });
            }
            "namespaces/topics/subscriptions/rules" => {
                let topic = res.name_at(1).to_string();
                let sub = res.name_at(2).to_string();
                self.with_ns(&ns, |n| {
                    if let Some(s) = n.topic_mut(&topic).and_then(|t| t.subs.get_mut(&sb_key(&sub))) {
                        s.rules.retain(|r| !r.name.eq_ignore_ascii_case(&name));
                    }
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn render(&self, cx: &Cx<'_>, res: &Stored) -> Value {
        let mut v = res.body.clone();
        let tp = res.type_path();
        // Azure writes `Microsoft.ServiceBus/Namespaces` for a namespace and everything in lower case below it
        // (`Microsoft.ServiceBus/namespaces/queues/authorizationrules`; recordings of azure-mgmt-servicebus, api-version 2021-11-01)
        v["type"] = json!(if res.types.len() == 1 {
            format!("{}/{}", res.ns, titlecase(&res.types[0]))
        } else {
            format!("{}/{}", res.ns, res.types.iter().map(|t| t.to_ascii_lowercase()).collect::<Vec<_>>().join("/"))
        });
        if tp == "namespaces" {
            // and its location as the display name (`East US`), where other providers write `eastus`
            if let Some(d) = v.get("location").and_then(Value::as_str).and_then(crate::catalog::display_name) {
                v["location"] = json!(d);
            }
            let n = res.name();
            util::props_mut(&mut v).insert("serviceBusEndpoint".into(), json!(format!("{}/", cx.data_url("https", n, "servicebus"))));
        } else {
            self.live(res, &mut v);
        }
        v
    }

    async fn action(
        &self,
        cx: &Cx<'_>,
        target: ActionTarget<'_>,
        type_path: &str,
        action: &str,
        body: &Value,
    ) -> ArmResult<Option<Response>> {
        let a = action.to_ascii_lowercase();
        match target {
            ActionTarget::Provider if a == "checknameavailability" => {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("");
                let v = if !valid_namespace_name(name) {
                    util::name_availability(false, "InvalidName", "The specified service namespace is invalid.")
                } else if cx
                    .inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.ServiceBus" && r.types.len() == 1 && r.name().eq_ignore_ascii_case(name))
                {
                    util::name_availability(false, "NameInUse", "The specified service namespace is already in use.")
                } else {
                    util::name_availability(true, "", "")
                };
                Ok(Some(util::json_ok(200, &v)))
            }
            ActionTarget::Resource(rule) if type_path.ends_with("authorizationRules") => {
                let ns = rule.name_at(0);
                let entity = match rule.types.len() {
                    3 => Some(rule.name_at(1)),
                    _ => None,
                };
                match a.as_str() {
                    "listkeys" => Ok(Some(sas::list_keys(cx, rule, "servicebus", ns, entity))),
                    "regeneratekeys" => Ok(Some(sas::regenerate(cx, rule, body, "servicebus", ns, entity)?)),
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}
