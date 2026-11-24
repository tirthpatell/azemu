//! What a fault rule says (the JSON of `docs/faults.md`), and the checks that make a typing mistake an error.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Services that speak a messaging protocol next to (Service Bus, Event Hubs) or instead of (Kafka) HTTP. Only these have the
/// `operation` matcher; the others are HTTP alone.
pub(crate) const MESSAGING_SERVICES: &[&str] = &["servicebus", "eventhubs", "kafka"];

/// The most rules the emulator holds at once. A test needs a handful; this keeps a bad loop from filling memory.
pub(crate) const MAX_RULES: usize = 1000;

/// The longest `path`, `query` or `account` of a rule. A rule is held, copied into every `GET /_azemu/faults` and matched against every request.
pub(crate) const MAX_MATCHER_BYTES: usize = 1024;
/// The longest `fault.code`, `fault.message`, header name and header value, and the most headers.
const MAX_CODE_BYTES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 2048;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 2048;
const MAX_HEADERS: usize = 8;

/// The longest wait a rule may ask for (`latency`, `jitterMs`, `timeout`): an hour.
const MAX_WAIT_MS: u64 = 3_600_000;

/// What a messaging (AMQP, Kafka) client is doing when a rule is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Operation {
    /// A connection is opened.
    Connect,
    /// A message is sent (AMQP: a transfer on a link to an entity; Kafka: a produce request).
    Send,
    /// A client asks for messages (AMQP: credit on a receiving link; Kafka: a fetch request).
    Receive,
    /// A request on the `$management` node of an entity, or any other request that is not one of the above.
    Management,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Connect => "connect",
            Operation::Send => "send",
            Operation::Receive => "receive",
            Operation::Management => "management",
        }
    }
}

/// One rule, as the client wrote it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RuleSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u64>,
    pub fault: FaultSpec,
}

/// What happens when a rule fires.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum FaultSpec {
    /// The answer the service gives when it throttles.
    Throttle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// An answer the rule spells out.
    Error {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<std::collections::BTreeMap<String, String>>,
    },
    /// Wait, then handle the request as usual.
    Latency {
        ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jitter_ms: Option<u64>,
    },
    /// Close the connection without an answer.
    Drop {},
    /// Read the request and never answer.
    Timeout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ms: Option<u64>,
    },
    /// Cosmos DB: throttle once the request units of the current second are used up.
    RuBudget { ru_per_second: f64 },
}

impl FaultSpec {
    pub fn kind(&self) -> &'static str {
        match self {
            FaultSpec::Throttle { .. } => "throttle",
            FaultSpec::Error { .. } => "error",
            FaultSpec::Latency { .. } => "latency",
            FaultSpec::Drop {} => "drop",
            FaultSpec::Timeout { .. } => "timeout",
            FaultSpec::RuBudget { .. } => "ruBudget",
        }
    }

    /// Kinds that need no answer in the service's own shape: the only ones a Kafka connection can be given.
    pub(crate) fn needs_no_answer(&self) -> bool {
        matches!(self, FaultSpec::Latency { .. } | FaultSpec::Drop {} | FaultSpec::Timeout { .. })
    }
}

pub(crate) fn retry_after(seconds: Option<u64>, ms: Option<u64>) -> Option<Duration> {
    match (seconds, ms) {
        (_, Some(ms)) => Some(Duration::from_millis(ms)),
        (Some(s), None) => Some(Duration::from_secs(s)),
        _ => None,
    }
}

/// Parse a list of rules, naming the rule (`rules[2]`) that is wrong. `what` is the place: `rules`.
pub(crate) fn parse_rules(value: &serde_json::Value, mounted: &[&str]) -> Result<Vec<RuleSpec>, String> {
    let list = value.as_array().ok_or_else(|| "'rules' must be a list of rules".to_string())?;
    if list.len() > MAX_RULES {
        return Err(format!("at most {MAX_RULES} rules can be held, and {} were given", list.len()));
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        out.push(parse_rule(item, mounted).map_err(|e| format!("rules[{i}]: {e}"))?);
    }
    Ok(out)
}

/// One rule from JSON: unknown or missing fields, wrong types and values that cannot work are all errors.
pub(crate) fn parse_rule(value: &serde_json::Value, mounted: &[&str]) -> Result<RuleSpec, String> {
    if !value.is_object() {
        return Err("a rule is a JSON object".to_string());
    }
    let spec: RuleSpec = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    validate(&spec, mounted)?;
    Ok(spec)
}

fn bad_text(field: &str, v: &str) -> Result<(), String> {
    if v.is_empty() {
        return Err(format!("'{field}' is empty; leave it out to match everything"));
    }
    too_long(field, v, MAX_MATCHER_BYTES)
}

fn too_long(field: &str, v: &str, max: usize) -> Result<(), String> {
    if v.len() > max {
        return Err(format!("'{field}' is {} bytes long; the emulator takes at most {max} bytes there", v.len()));
    }
    Ok(())
}

/// The checks that serde cannot make.
pub(crate) fn validate(spec: &RuleSpec, mounted: &[&str]) -> Result<(), String> {
    if let Some(s) = &spec.service {
        if !mounted.contains(&s.as_str()) {
            return Err(format!("'service' is '{s}', and no such service is mounted. Mounted: {}.", mounted.join(", ")));
        }
    }
    for (name, v) in [("account", &spec.account), ("path", &spec.path), ("query", &spec.query)] {
        if let Some(v) = v {
            bad_text(name, v)?;
        }
    }
    if let Some(m) = &spec.method {
        if m.is_empty() || !m.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(format!("'method' is '{m}', which is not an HTTP method such as GET or PUT"));
        }
    }
    if let Some(p) = spec.probability {
        if !(0.0..=1.0).contains(&p) {
            return Err(format!("'probability' is {p}; it must be between 0 and 1"));
        }
    }
    if spec.times == Some(0) {
        return Err("'times' is 0, a rule that never fires; leave it out for no limit, or remove the rule".to_string());
    }
    // Which transport the rule is for, from what it names.
    let messaging_only = spec.service.as_deref().is_some_and(|s| s == "kafka");
    let http_only = spec.service.as_deref().is_some_and(|s| !MESSAGING_SERVICES.contains(&s));
    if spec.operation.is_some() {
        if http_only {
            return Err(format!(
                "'operation' is for messaging services ({}); '{}' is HTTP only",
                MESSAGING_SERVICES.join(", "),
                spec.service.as_deref().unwrap_or("")
            ));
        }
        if spec.method.is_some() || spec.query.is_some() {
            return Err(
                "'operation' is for messaging requests and 'method' and 'query' for HTTP requests; one rule cannot have both".to_string()
            );
        }
    }
    if messaging_only && (spec.method.is_some() || spec.query.is_some()) {
        return Err("'method' and 'query' are for HTTP requests; the 'kafka' service has none".to_string());
    }
    if let Some(p) = &spec.path {
        // `/orders/*` is an HTTP path; `orders` an entity (queue, topic, event hub). `*...` may be either.
        let http_style = p.starts_with('/');
        let entity_style = !http_style && !p.starts_with('*');
        if entity_style && http_only {
            return Err(format!("'path' is '{p}': an HTTP path starts with '/' (for example '/{p}')"));
        }
        if http_style && (spec.operation.is_some() || messaging_only) {
            return Err(format!(
                "'path' is '{p}': for a messaging rule it is the name of the entity (queue, topic, event hub), not a path"
            ));
        }
    }
    match &spec.fault {
        FaultSpec::Throttle { retry_after_seconds, retry_after_ms } => {
            both_retry(*retry_after_seconds, *retry_after_ms)?;
            kafka_answer(spec, "throttle")?;
        }
        FaultSpec::Error { status, headers, retry_after_seconds, retry_after_ms, code, message } => {
            too_long("fault.code", code.as_deref().unwrap_or(""), MAX_CODE_BYTES)?;
            too_long("fault.message", message.as_deref().unwrap_or(""), MAX_MESSAGE_BYTES)?;
            if headers.as_ref().is_some_and(|h| h.len() > MAX_HEADERS) {
                return Err(format!("'fault.headers' has more than {MAX_HEADERS} headers; the emulator takes at most {MAX_HEADERS}"));
            }
            if !(400..=599).contains(status) {
                return Err(format!("'fault.status' is {status}; an error answer has a status from 400 to 599"));
            }
            both_retry(*retry_after_seconds, *retry_after_ms)?;
            kafka_answer(spec, "error")?;
            for (k, v) in headers.iter().flatten() {
                too_long("fault.headers name", k, MAX_HEADER_NAME_BYTES)?;
                too_long("fault.headers value", v, MAX_HEADER_VALUE_BYTES)?;
                if http::HeaderName::from_bytes(k.as_bytes()).is_err() {
                    return Err(format!("'fault.headers': '{k}' is not a valid header name"));
                }
                if http::HeaderValue::from_str(v).is_err() {
                    return Err(format!("'fault.headers': the value of '{k}' is not a valid header value"));
                }
                let lower = k.to_ascii_lowercase();
                if ["content-length", "transfer-encoding", "connection", "host"].contains(&lower.as_str()) {
                    return Err(format!("'fault.headers': '{k}' cannot be set by a rule"));
                }
            }
        }
        FaultSpec::Latency { ms, jitter_ms } => {
            if *ms > MAX_WAIT_MS || jitter_ms.is_some_and(|j| j > MAX_WAIT_MS) {
                return Err(format!("'fault.ms' and 'fault.jitterMs' are at most {MAX_WAIT_MS} (one hour)"));
            }
        }
        FaultSpec::Timeout { ms } => {
            if ms.is_some_and(|m| m > MAX_WAIT_MS) {
                return Err(format!("'fault.ms' is at most {MAX_WAIT_MS} (one hour)"));
            }
        }
        FaultSpec::Drop {} => {}
        FaultSpec::RuBudget { ru_per_second } => {
            if spec.service.as_deref() != Some("cosmos") {
                return Err("a 'ruBudget' fault is for Cosmos DB: set \"service\": \"cosmos\"".to_string());
            }
            if !ru_per_second.is_finite() || *ru_per_second <= 0.0 {
                return Err(format!("'fault.ruPerSecond' is {ru_per_second}; it must be above 0"));
            }
            if spec.skip.is_some() || spec.times.is_some() || spec.probability.is_some() {
                return Err("a 'ruBudget' fault limits by itself: 'skip', 'times' and 'probability' do not apply to it".to_string());
            }
        }
    }
    Ok(())
}

fn both_retry(seconds: Option<u64>, ms: Option<u64>) -> Result<(), String> {
    if seconds.is_some() && ms.is_some() {
        return Err("'fault' has both 'retryAfterSeconds' and 'retryAfterMs'; give one".to_string());
    }
    Ok(())
}

/// A rule that can only match Kafka requests cannot be a throttle or an error answer: Kafka has no answer of that kind here.
fn kafka_answer(spec: &RuleSpec, kind: &str) -> Result<(), String> {
    if spec.service.as_deref() == Some("kafka") {
        return Err(format!(
            "a '{kind}' fault cannot be given to the 'kafka' service; Kafka connections take 'latency', 'drop' and 'timeout'"
        ));
    }
    Ok(())
}

/// Read the file of `--faults`: `{"rules": [...]}`, the body of `PUT /_azemu/faults`. The message names the file and the rule.
pub(crate) fn load_file(path: &std::path::Path, mounted: &[&str]) -> Result<Vec<RuleSpec>, String> {
    let shown = path.display();
    let bytes = std::fs::read(path).map_err(|e| match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => format!("the faults file '{shown}' is a directory, not a file"),
        _ => format!("the faults file '{shown}' cannot be read: {e}"),
    })?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("the faults file '{shown}' is not valid JSON: {e}"))?;
    let obj = value.as_object().ok_or_else(|| format!("the faults file '{shown}' must be a JSON object: {{\"rules\": [...]}}"))?;
    if let Some(k) = obj.keys().find(|k| *k != "rules") {
        return Err(format!("the faults file '{shown}' has an unknown field `{k}`: the file is {{\"rules\": [...]}}"));
    }
    let list = obj.get("rules").ok_or_else(|| format!("the faults file '{shown}' has no 'rules' list: {{\"rules\": [...]}}"))?;
    parse_rules(list, mounted).map_err(|e| format!("the faults file '{shown}': {e}"))
}

/// `*` matches any run of characters (also none, also `/`); everything else matches itself.
///
/// The pattern is cut at its stars: the first piece must start the text, the last must end it, and each piece in between is looked for, left
/// to right, in what is left. Taking the first place each piece is found is enough (a later place can only leave less for the rest). Every
/// search is `str::find`, which takes time in proportion to the text and the piece, so a pattern of any shape costs about one pass over the
/// text, not a pass for every start (a rule is held for as long as the emulator runs and is asked about every request).
pub(crate) fn glob(pattern: &str, text: &str) -> bool {
    let mut pieces = pattern.split('*');
    let first = pieces.next().unwrap_or("");
    let Some(last) = pieces.next_back() else { return pattern == text };
    let Some(rest) = text.strip_prefix(first) else { return false };
    let Some(mut rest) = rest.strip_suffix(last) else { return false };
    for piece in pieces.filter(|p| !p.is_empty()) {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().into_owned()
}

/// Does the query string have every pair of `wanted` (`a=1&b=2`, or a bare `a` for a name with any value)? A value may hold `*`.
pub(crate) fn query_has(query: &str, wanted: &str) -> bool {
    let have: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    wanted.split('&').filter(|p| !p.is_empty()).all(|w| match w.split_once('=') {
        Some((k, v)) => {
            let (k, v) = (decode(k), decode(v));
            have.iter().any(|(hk, hv)| *hk == k && glob(&v, hv))
        }
        None => {
            let k = decode(w);
            have.iter().any(|(hk, _)| *hk == k)
        }
    })
}

/// A path pattern against an HTTP path, as sent and with the percent-escapes undone.
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    glob(pattern, path) || glob(pattern, &decode_path(path))
}

fn decode_path(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MOUNTED: &[&str] = &["blob", "cosmos", "servicebus", "kafka"];

    fn rule(v: serde_json::Value) -> Result<RuleSpec, String> {
        parse_rule(&v, MOUNTED)
    }

    #[test]
    fn glob_star_matches_any_run() {
        assert!(glob("/orders/*", "/orders/1"));
        assert!(glob("/orders/*", "/orders/a/b"));
        assert!(glob("/orders/*", "/orders/"));
        assert!(!glob("/orders/*", "/order"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
        assert!(glob("*.json", "/c/x.json"));
        assert!(!glob("abc", "abcd"));
        assert!(glob("a**", "a"));
    }

    #[test]
    fn query_needs_every_pair() {
        assert!(query_has("comp=block&blockid=AA", "comp=block"));
        assert!(query_has("comp=block&blockid=AA", "comp=block&blockid=A*"));
        assert!(!query_has("comp=block", "comp=blocklist"));
        assert!(!query_has("comp=block", "comp=block&x=1"));
        assert!(query_has("restype=container&comp=list", "comp"));
        assert!(query_has("a=%20b", "a= b"));
        assert!(!query_has("", "comp=block"));
    }

    #[test]
    fn a_complete_rule_parses_and_round_trips() {
        let v = json!({"service":"blob","account":"a","method":"PUT","path":"/c/*","query":"comp=block","probability":0.5,"skip":1,"times":2,
            "fault":{"kind":"throttle","retryAfterSeconds":2}});
        let r = rule(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&r).unwrap(), v);
        assert_eq!(r.fault.kind(), "throttle");
    }

    #[test]
    fn every_kind_parses() {
        for f in [
            json!({"kind":"throttle"}),
            json!({"kind":"throttle","retryAfterMs":1500}),
            json!({"kind":"error","status":500,"code":"X","message":"m","retryAfterSeconds":1,"headers":{"x-a":"b"}}),
            json!({"kind":"latency","ms":10,"jitterMs":5}),
            json!({"kind":"drop"}),
            json!({"kind":"timeout"}),
            json!({"kind":"timeout","ms":50}),
        ] {
            rule(json!({"fault": f})).unwrap_or_else(|e| panic!("{f}: {e}"));
        }
        rule(json!({"service":"cosmos","fault":{"kind":"ruBudget","ruPerSecond":400}})).unwrap();
    }

    #[test]
    fn mistakes_are_errors_that_say_which() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (json!({"fault":{"kind":"throttle"},"servce":"blob"}), "unknown field `servce`"),
            (json!({"fault":{"kind":"throttle","retryAfter":2}}), "unknown field `retryAfter`"),
            (json!({"fault":{"kind":"drop","ms":1}}), "unknown field `ms`"),
            (json!({"fault":{"kind":"thrttle"}}), "unknown variant `thrttle`"),
            (json!({}), "missing field `fault`"),
            (json!({"fault":{}}), "missing field `kind`"),
            (json!({"fault":{"kind":"error"}}), "missing field `status`"),
            (json!({"fault":{"kind":"latency"}}), "missing field `ms`"),
            (json!({"service":"nosuch","fault":{"kind":"drop"}}), "no such service is mounted"),
            (json!({"probability":1.5,"fault":{"kind":"drop"}}), "between 0 and 1"),
            (json!({"probability":-0.1,"fault":{"kind":"drop"}}), "between 0 and 1"),
            (json!({"fault":{"kind":"error","status":200}}), "400 to 599"),
            (json!({"fault":{"kind":"error","status":99}}), "400 to 599"),
            (json!({"fault":{"kind":"error","status":600}}), "400 to 599"),
            (json!({"times":0,"fault":{"kind":"drop"}}), "'times' is 0"),
            (json!({"operation":"sned","service":"servicebus","fault":{"kind":"drop"}}), "unknown variant `sned`"),
            (json!({"operation":"send","service":"blob","fault":{"kind":"drop"}}), "HTTP only"),
            (json!({"operation":"send","method":"GET","fault":{"kind":"drop"}}), "cannot have both"),
            (json!({"service":"kafka","method":"GET","fault":{"kind":"drop"}}), "kafka"),
            (json!({"service":"kafka","fault":{"kind":"throttle"}}), "cannot be given to the 'kafka' service"),
            (json!({"service":"blob","path":"orders/*","fault":{"kind":"drop"}}), "starts with '/'"),
            (json!({"service":"servicebus","operation":"send","path":"/q","fault":{"kind":"drop"}}), "name of the entity"),
            (json!({"fault":{"kind":"ruBudget","ruPerSecond":10}}), "service\": \"cosmos"),
            (json!({"service":"cosmos","fault":{"kind":"ruBudget","ruPerSecond":0}}), "above 0"),
            (json!({"service":"cosmos","times":1,"fault":{"kind":"ruBudget","ruPerSecond":10}}), "limits by itself"),
            (json!({"fault":{"kind":"throttle","retryAfterSeconds":1,"retryAfterMs":5}}), "give one"),
            (json!({"fault":{"kind":"error","status":500,"headers":{"bad name":"x"}}}), "not a valid header name"),
            (json!({"fault":{"kind":"error","status":500,"headers":{"content-length":"1"}}}), "cannot be set"),
            (json!({"fault":{"kind":"latency","ms":99999999}}), "one hour"),
            (json!({"method":"GE T","fault":{"kind":"drop"}}), "not an HTTP method"),
            (json!({"path":"","fault":{"kind":"drop"}}), "'path' is empty"),
            (json!({"probability":"high","fault":{"kind":"drop"}}), "invalid type"),
        ];
        for (v, want) in cases {
            let e = rule(v.clone()).expect_err(&v.to_string());
            assert!(e.contains(want), "{v}: wanted '{want}' in '{e}'");
        }
    }

    #[test]
    fn a_list_names_the_rule_that_is_wrong() {
        let e = parse_rules(&json!([{"fault":{"kind":"drop"}}, {"fault":{"kind":"dorp"}}]), MOUNTED).unwrap_err();
        assert!(e.starts_with("rules[1]: "), "{e}");
        assert!(parse_rules(&json!({"a":1}), MOUNTED).is_err());
        assert!(parse_rules(&json!([1]), MOUNTED).unwrap_err().contains("JSON object"));
    }

    #[test]
    fn paths_by_style() {
        // entity-style names are for messaging services, or no service at all
        rule(json!({"service":"servicebus","path":"orders","fault":{"kind":"drop"}})).unwrap();
        rule(json!({"path":"orders","fault":{"kind":"drop"}})).unwrap();
        rule(json!({"service":"blob","path":"*","fault":{"kind":"drop"}})).unwrap();
        rule(json!({"service":"servicebus","path":"/queues/*","fault":{"kind":"drop"}})).unwrap();
    }
}
