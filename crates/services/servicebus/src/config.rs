//! The JSON config file, in the format of Microsoft's Service Bus emulator (`Config.json`):
//! `UserConfig.Namespaces[].Queues / Topics / Subscriptions / Rules`.
//!
//! The first namespace becomes available under its own name. When the file names exactly one
//! namespace, the name `default` (what `sb://localhost` resolves to) is an alias of it, so
//! `Endpoint=sb://localhost` reaches the configured entities, as it does with Microsoft's emulator.

use std::path::Path;

use azemu_servicebus_sql::{Params, Value as SqlValue};
use serde_json::Value;

use crate::duration;
use crate::model::Settings;
use crate::rules::{CorrelationFilter, Rule, RuleFilter};
use crate::state::State;

pub struct Config {
    pub raw: Value,
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let raw: Value = serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))?;
    Ok(Config { raw })
}

pub fn parse_str(text: &str) -> Result<Config, String> {
    Ok(Config { raw: serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))? })
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}

fn list<'a>(v: &'a Value, key: &str) -> Vec<&'a Value> {
    get(v, key).and_then(|x| x.as_array()).map(|a| a.iter().collect()).unwrap_or_default()
}

fn name_of(v: &Value) -> Option<String> {
    get(v, "Name").and_then(|n| n.as_str()).map(str::to_string).filter(|s| !s.is_empty())
}

fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// A property whose value cannot be used (today: a duration that is not ISO 8601).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyError {
    /// The property as the caller wrote it (`LockDuration`, `lockDuration`).
    pub property: String,
    pub message: String,
}

impl std::fmt::Display for PropertyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PropertyError {}

/// Apply the `Properties` object of an entity onto settings. Lenient: a value that cannot be
/// used leaves the default in place. Callers that can report an error to their client use
/// [`try_apply_properties`].
pub fn apply_properties(s: &mut Settings, props: &Value) {
    let _ = apply_each(s, props, false);
}

/// Like [`apply_properties`], but a duration that is not an ISO 8601 duration (lock duration,
/// default time to live, duplicate detection window, auto-delete on idle) is an error naming
/// the property and the value, and nothing is applied after it. `s` may be partly changed
/// when this fails: apply onto a scratch copy.
pub fn try_apply_properties(s: &mut Settings, props: &Value) -> Result<(), PropertyError> {
    apply_each(s, props, true)
}

/// A duration property: `None` when the value is absent (JSON null); a bad value is an error
/// in strict mode and `None` otherwise.
fn duration_of(name: &str, v: &Value, strict: bool) -> Result<Option<i64>, PropertyError> {
    if v.is_null() {
        return Ok(None);
    }
    match v.as_str().and_then(duration::parse) {
        Some(d) => Ok(Some(d)),
        None if strict => {
            let shown = match v {
                Value::String(t) => t.clone(),
                other => other.to_string(),
            };
            Err(PropertyError {
                property: name.to_string(),
                message: format!(
                    "The value '{shown}' of the property '{name}' is not a valid ISO 8601 duration (for example 'PT1M' or 'P1DT2H')."
                ),
            })
        }
        None => Ok(None),
    }
}

fn apply_each(s: &mut Settings, props: &Value, strict: bool) -> Result<(), PropertyError> {
    let Some(obj) = props.as_object() else { return Ok(()) };
    for (k, v) in obj {
        match k.to_ascii_lowercase().as_str() {
            "deadletteringonmessageexpiration" => s.dead_letter_on_expiry = as_bool(v).unwrap_or(s.dead_letter_on_expiry),
            "defaultmessagetimetolive" => {
                if let Some(d) = duration_of(k, v, strict)? {
                    s.default_ttl_ms = if d >= duration::TIMESPAN_MAX_MS { None } else { Some(d) };
                }
            }
            "duplicatedetectionhistorytimewindow" => {
                if let Some(d) = duration_of(k, v, strict)? {
                    s.duplicate_window_ms = d;
                }
            }
            "autodeleteonidle" => {
                if let Some(d) = duration_of(k, v, strict)? {
                    s.auto_delete_on_idle_ms = if d >= duration::TIMESPAN_MAX_MS { None } else { Some(d) };
                }
            }
            "forwarddeadletteredmessagesto" => s.forward_dead_lettered_to = v.as_str().filter(|x| !x.is_empty()).map(normalize_path),
            "forwardto" => s.forward_to = v.as_str().filter(|x| !x.is_empty()).map(normalize_path),
            "lockduration" => {
                if let Some(d) = duration_of(k, v, strict)? {
                    s.lock_duration_ms = d;
                }
            }
            "maxdeliverycount" => s.max_delivery_count = as_i64(v).and_then(|x| u32::try_from(x).ok()).unwrap_or(s.max_delivery_count),
            "requiresduplicatedetection" => s.requires_duplicate_detection = as_bool(v).unwrap_or(false),
            "requiressession" => s.requires_session = as_bool(v).unwrap_or(false),
            "maxsizeinmegabytes" => s.max_size_mb = as_i64(v).unwrap_or(s.max_size_mb),
            "maxmessagesizeinkilobytes" => s.max_message_size_kb = as_i64(v).unwrap_or(s.max_message_size_kb),
            "enablebatchedoperations" => s.enable_batched_operations = as_bool(v).unwrap_or(true),
            "enablepartitioning" => s.enable_partitioning = as_bool(v).unwrap_or(false),
            "status" => {
                if let Some(x) = v.as_str() {
                    s.status = x.to_string();
                }
            }
            "usermetadata" => s.user_metadata = v.as_str().map(str::to_string),
            _ => {}
        }
    }
    Ok(())
}

/// `sb://host/queue` or `queue` to `queue`.
pub fn normalize_path(p: &str) -> String {
    let p = p.trim();
    match p.find("://") {
        Some(i) => {
            let rest = &p[i + 3..];
            rest.split_once('/').map(|(_, path)| path).unwrap_or("").trim_matches('/').to_string()
        }
        None => p.trim_matches('/').to_string(),
    }
}

fn sql_params(v: Option<&Value>) -> Params {
    let mut p = Params::new();
    if let Some(Value::Object(o)) = v {
        for (k, v) in o {
            let val = match v {
                Value::Bool(b) => SqlValue::Bool(*b),
                Value::Number(n) => n.as_i64().map(SqlValue::Long).unwrap_or_else(|| SqlValue::Double(n.as_f64().unwrap_or(0.0))),
                Value::String(s) => SqlValue::Str(s.clone()),
                _ => SqlValue::Null,
            };
            p.insert(k.trim_start_matches('@').to_string(), val);
        }
    }
    p
}

fn parse_rule(r: &Value, now: i64) -> Result<Rule, String> {
    let name = name_of(r).ok_or("rule without a name")?;
    let props = get(r, "Properties").unwrap_or(r);
    let ftype = get(props, "FilterType").and_then(|v| v.as_str()).unwrap_or("Sql").to_ascii_lowercase();
    let mut rule = match ftype.as_str() {
        "correlation" => {
            let c = get(props, "CorrelationFilter").ok_or("correlation rule without CorrelationFilter")?;
            let s = |k: &str| get(c, k).and_then(|v| v.as_str()).map(str::to_string);
            let mut cf = CorrelationFilter {
                correlation_id: s("CorrelationId"),
                message_id: s("MessageId"),
                to: s("To"),
                reply_to: s("ReplyTo"),
                label: s("Label").or_else(|| s("Subject")),
                session_id: s("SessionId"),
                reply_to_session_id: s("ReplyToSessionId"),
                content_type: s("ContentType"),
                properties: Default::default(),
            };
            if let Some(Value::Object(o)) = get(c, "Properties") {
                for (k, v) in o {
                    let val = match v {
                        Value::Bool(b) => SqlValue::Bool(*b),
                        Value::Number(n) => n.as_i64().map(SqlValue::Long).unwrap_or_else(|| SqlValue::Double(n.as_f64().unwrap_or(0.0))),
                        Value::String(s) => SqlValue::Str(s.clone()),
                        _ => continue,
                    };
                    cf.properties.insert(k.clone(), val);
                }
            }
            Rule { name: name.clone(), filter: RuleFilter::Correlation(cf), action: None, created_ms: now }
        }
        "true" => Rule { name: name.clone(), filter: RuleFilter::True, action: None, created_ms: now },
        "false" => Rule { name: name.clone(), filter: RuleFilter::False, action: None, created_ms: now },
        _ => {
            let f = get(props, "SqlFilter").ok_or("sql rule without SqlFilter")?;
            let expr = get(f, "SqlExpression").and_then(|v| v.as_str()).ok_or("SqlFilter without SqlExpression")?;
            Rule::sql_filter(&name, expr, sql_params(get(f, "Parameters")), now)?
        }
    };
    if let Some(a) = get(props, "Action").or_else(|| get(props, "SqlRuleAction")) {
        if let Some(expr) = get(a, "SqlExpression").and_then(|v| v.as_str()) {
            rule = rule.with_action(expr, sql_params(get(a, "Parameters")))?;
        }
    }
    Ok(rule)
}

/// Create everything in the config. Entities that already exist are left alone. A bad value
/// (a duration that is not ISO 8601, a rule that does not parse) is an error that names the
/// entity, and then nothing from the file is created: the file is checked on a scratch state
/// first.
pub fn apply(state: &mut State, cfg: &Config, now: i64) -> Result<(), String> {
    apply_to(&mut State::default(), cfg, now)?;
    apply_to(state, cfg, now)
}

fn props_of(s: &mut Settings, entity: &str, props: &Value) -> Result<(), String> {
    try_apply_properties(s, props).map_err(|e| format!("{entity}: {e}"))
}

fn apply_to(state: &mut State, cfg: &Config, now: i64) -> Result<(), String> {
    let user = get(&cfg.raw, "UserConfig").unwrap_or(&cfg.raw);
    let namespaces = list(user, "Namespaces");
    if namespaces.len() == 1 {
        if let Some(n) = name_of(namespaces[0]) {
            if !n.eq_ignore_ascii_case(crate::state::DEFAULT_NAMESPACE) {
                state.default_alias = Some(n.to_lowercase());
            }
        }
    }
    for n in namespaces {
        let ns_name = name_of(n).ok_or("namespace without a name")?;
        let ns = state.ns(&ns_name);
        for q in list(n, "Queues") {
            let name = name_of(q).ok_or("queue without a name")?;
            let mut s = Settings::queue_default(now);
            if let Some(p) = get(q, "Properties") {
                props_of(&mut s, &format!("queue '{name}'"), p)?;
            }
            let _ = ns.create_queue(&name, s);
        }
        for t in list(n, "Topics") {
            let name = name_of(t).ok_or("topic without a name")?;
            let mut s = Settings::topic_default(now);
            if let Some(p) = get(t, "Properties") {
                props_of(&mut s, &format!("topic '{name}'"), p)?;
            }
            let _ = ns.create_topic(&name, s);
            for sub in list(t, "Subscriptions") {
                let sname = name_of(sub).ok_or("subscription without a name")?;
                let mut ss = Settings::subscription_default(now);
                if let Some(p) = get(sub, "Properties") {
                    props_of(&mut ss, &format!("subscription '{name}/{sname}'"), p)?;
                }
                let mut rules = Vec::new();
                for r in list(sub, "Rules") {
                    rules.push(parse_rule(r, now)?);
                }
                if rules.is_empty() {
                    rules.push(Rule::default_rule(now));
                }
                let _ = ns.create_subscription(&name, &sname, ss, rules);
            }
        }
    }
    Ok(())
}
