//! The rules and what they decide. A fixed seed makes `probability` and `jitterMs` repeatable.

use std::time::Duration;

use serde_json::json;

use super::*;

const MOUNTED: &[&str] = &["blob", "cosmos", "servicebus", "eventhubs", "kafka"];

fn faults(rules: serde_json::Value) -> Faults {
    let f = Faults::with_seed(7);
    f.replace(spec::parse_rules(&rules, MOUNTED).unwrap());
    f
}

fn http<'a>(service: &'a str, account: &'a str, method: &'a str, path: &'a str, query: &'a str) -> Probe<'a> {
    Probe::Http { service, account, method, path, query }
}

fn blob_put(f: &Faults) -> Outcome {
    f.decide(&http("blob", "acct", "PUT", "/c/b", ""))
}

fn answer(o: &Outcome) -> &FaultAnswer {
    match &o.action {
        Some(Action::Answer(a)) => a,
        other => panic!("not an answer: {other:?}"),
    }
}

#[test]
fn no_rule_means_nothing_happens_and_nothing_is_locked() {
    let f = Faults::with_seed(1);
    assert!(!f.is_active());
    assert!(blob_put(&f).is_none());
    assert!(f.is_empty());
}

#[test]
fn a_rule_with_no_matcher_matches_everything() {
    let f = faults(json!([{"fault":{"kind":"throttle"}}]));
    assert!(f.is_active());
    for (s, m, p) in [("blob", "GET", "/"), ("cosmos", "POST", "/dbs/x/colls"), ("keyvault", "DELETE", "/secrets/a")] {
        let o = f.decide(&http(s, "any", m, p, "q=1"));
        assert!(answer(&o).throttle, "{s}");
        assert_eq!(o.kinds(), "throttle");
    }
}

#[test]
fn matchers_are_all_required() {
    let f =
        faults(json!([{"service":"blob","account":"Acct","method":"put","path":"/orders/*","query":"comp=block","fault":{"kind":"drop"}}]));
    let hit = |s, a, m, p, q| f.decide(&http(s, a, m, p, q)).action.is_some();
    assert!(hit("blob", "acct", "PUT", "/orders/1", "comp=block&x=1"), "service, account (any case), method (any case), path, query");
    assert!(!hit("queue", "acct", "PUT", "/orders/1", "comp=block"), "service");
    assert!(!hit("blob", "other", "PUT", "/orders/1", "comp=block"), "account");
    assert!(!hit("blob", "acct", "GET", "/orders/1", "comp=block"), "method");
    assert!(!hit("blob", "acct", "PUT", "/other/1", "comp=block"), "path");
    assert!(!hit("blob", "acct", "PUT", "/orders/1", "comp=list"), "query");
}

#[test]
fn a_path_is_matched_as_sent_and_with_escapes_undone() {
    let f = faults(json!([{"path":"/c/my file.txt","fault":{"kind":"drop"}}, {"path":"/c/a%20b","fault":{"kind":"drop"}}]));
    assert!(f.decide(&http("blob", "a", "GET", "/c/my%20file.txt", "")).action.is_some());
    assert!(f.decide(&http("blob", "a", "GET", "/c/a%20b", "")).action.is_some());
}

#[test]
fn skip_then_times_then_the_rule_stops_matching() {
    // "the third call fails, and only that one"
    let f = faults(json!([{"skip":2,"times":1,"fault":{"kind":"throttle"}}]));
    let fired: Vec<bool> = (0..6).map(|_| blob_put(&f).action.is_some()).collect();
    assert_eq!(fired, [false, false, true, false, false, false]);
    let rules = f.list_json();
    assert_eq!(rules["rules"][0]["matched"], 6);
    assert_eq!(rules["rules"][0]["fired"], 1);
}

#[test]
fn times_alone_fires_the_first_calls() {
    let f = faults(json!([{"times":2,"fault":{"kind":"error","status":500}}]));
    let fired: Vec<bool> = (0..4).map(|_| blob_put(&f).action.is_some()).collect();
    assert_eq!(fired, [true, true, false, false]);
}

#[test]
fn skip_counts_only_requests_that_match() {
    let f = faults(json!([{"method":"PUT","skip":1,"fault":{"kind":"drop"}}]));
    assert!(f.decide(&http("blob", "a", "GET", "/", "")).action.is_none());
    assert!(f.decide(&http("blob", "a", "PUT", "/", "")).action.is_none(), "the first PUT is let through");
    assert!(f.decide(&http("blob", "a", "GET", "/", "")).action.is_none());
    assert!(f.decide(&http("blob", "a", "PUT", "/", "")).action.is_some(), "the second PUT is not");
}

#[test]
fn probability_follows_the_seed_and_is_repeatable() {
    let run = |seed| {
        let f = Faults::with_seed(seed);
        f.replace(spec::parse_rules(&json!([{"probability":0.3,"fault":{"kind":"drop"}}]), MOUNTED).unwrap());
        (0..2000).filter(|_| blob_put(&f).action.is_some()).count()
    };
    let a = run(42);
    assert_eq!(a, run(42), "the same seed gives the same sequence");
    assert!((500..700).contains(&a), "about 30% of 2000, got {a}");
    assert_ne!(run(43), a, "another seed another sequence");
    // 0 never fires, and a draw that does not fire does not use up `times`
    let f = faults(json!([{"probability":0.0,"fault":{"kind":"drop"}}]));
    assert!((0..100).all(|_| blob_put(&f).action.is_none()));
    let f = faults(json!([{"probability":0.5,"times":3,"fault":{"kind":"drop"}}]));
    let n = (0..200).filter(|_| blob_put(&f).action.is_some()).count();
    assert_eq!(n, 3);
}

#[test]
fn latency_does_not_stop_later_rules_but_another_kind_does() {
    let f = faults(json!([
        {"fault":{"kind":"latency","ms":30}},
        {"fault":{"kind":"latency","ms":20}},
        {"fault":{"kind":"throttle","retryAfterSeconds":2}},
        {"fault":{"kind":"error","status":500}}
    ]));
    let o = blob_put(&f);
    assert_eq!(o.delay, Duration::from_millis(50), "the waits add up");
    assert_eq!(answer(&o).rule, 3, "the first rule that is not a latency decides");
    assert_eq!(answer(&o).retry_after, Some(Duration::from_secs(2)));
    assert_eq!(o.kinds(), "latency,throttle");
    assert_eq!(o.rules(), "1,2,3");
    // the fourth rule never saw the request
    assert_eq!(f.list_json()["rules"][3]["matched"], 0);
}

#[test]
fn latency_alone_lets_the_request_go_on() {
    let f = faults(json!([{"fault":{"kind":"latency","ms":5}}]));
    let o = blob_put(&f);
    assert!(o.action.is_none());
    assert_eq!(o.delay, Duration::from_millis(5));
    assert_eq!(o.fired, vec![(1, "latency")]);
}

#[test]
fn jitter_adds_between_zero_and_the_jitter() {
    let f = faults(json!([{"fault":{"kind":"latency","ms":100,"jitterMs":50}}]));
    let delays: Vec<u64> = (0..200).map(|_| blob_put(&f).delay.as_millis() as u64).collect();
    assert!(delays.iter().all(|d| (100..=150).contains(d)), "{delays:?}");
    assert!(delays.iter().any(|d| *d != delays[0]), "the delay varies");
}

#[test]
fn a_latency_rule_that_is_spent_lets_the_next_rule_decide() {
    let f = faults(json!([{"times":1,"fault":{"kind":"latency","ms":10}}, {"fault":{"kind":"drop"}}]));
    assert_eq!(blob_put(&f).delay, Duration::from_millis(10));
    assert!(blob_put(&f).delay.is_zero());
}

#[test]
fn error_and_throttle_carry_what_the_rule_says() {
    let f = faults(
        json!([{"fault":{"kind":"error","status":418,"code":"Teapot","message":"short and stout","retryAfterMs":1500,"headers":{"x-extra":"1"}}}]),
    );
    let o = blob_put(&f);
    let a = answer(&o);
    assert!(!a.throttle);
    assert_eq!((a.status, a.code.as_deref(), a.message.as_deref()), (Some(418), Some("Teapot"), Some("short and stout")));
    assert_eq!(a.retry_after_ms(), Some(1500));
    assert_eq!(a.retry_after_secs(), Some(2), "rounded up for a header in seconds");
    assert_eq!(a.headers, vec![("x-extra".to_string(), "1".to_string())]);
    assert_eq!(FaultAnswer::default_code(500), "InternalServerError");
}

#[test]
fn drop_and_timeout() {
    let f = faults(
        json!([{"method":"GET","fault":{"kind":"drop"}}, {"method":"PUT","fault":{"kind":"timeout","ms":40}}, {"method":"POST","fault":{"kind":"timeout"}}]),
    );
    assert_eq!(f.decide(&http("blob", "a", "GET", "/", "")).action, Some(Action::Drop));
    assert_eq!(f.decide(&http("blob", "a", "PUT", "/", "")).action, Some(Action::Timeout(Some(Duration::from_millis(40)))));
    assert_eq!(f.decide(&http("blob", "a", "POST", "/", "")).action, Some(Action::Timeout(None)));
}

#[test]
fn http_and_messaging_rules_do_not_meet() {
    let f = faults(
        json!([{"service":"servicebus","method":"GET","fault":{"kind":"drop"}}, {"service":"servicebus","operation":"send","fault":{"kind":"drop"}}]),
    );
    assert!(f.decide(&http("servicebus", "ns", "GET", "/q", "")).action.is_some(), "the method rule is for HTTP");
    assert!(f.decide(&http("servicebus", "ns", "POST", "/q", "")).action.is_none(), "the operation rule never matches HTTP");
    let o = f.messaging("servicebus", "ns", "orders", Operation::Send);
    assert_eq!(o.fired, vec![(2, "drop")], "the method rule never matches a messaging operation");
    assert!(f.messaging("servicebus", "ns", "orders", Operation::Receive).action.is_none());
}

#[test]
fn a_messaging_path_is_the_entity() {
    let f = faults(
        json!([{"service":"servicebus","path":"orders","fault":{"kind":"drop"}}, {"service":"eventhubs","path":"hub-*","operation":"send","fault":{"kind":"drop"}}]),
    );
    let sb = |e: &str| f.messaging("servicebus", "ns", e, Operation::Send).action.is_some();
    assert!(sb("orders"));
    assert!(sb("orders/Subscriptions/billing"), "the first part is the entity");
    assert!(sb("orders/$management"));
    assert!(!sb("order"));
    assert!(!sb("other"));
    assert!(f.messaging("eventhubs", "ns", "hub-1/Partitions/0", Operation::Send).action.is_some());
    assert!(f.messaging("eventhubs", "ns", "hub-1", Operation::Receive).action.is_none(), "operation");
    // a connect has no entity
    assert!(f.messaging("servicebus", "ns", "", Operation::Connect).action.is_none());
}

#[test]
fn kafka_takes_no_answer_rules() {
    let f = faults(json!([{"fault":{"kind":"throttle"}}, {"fault":{"kind":"latency","ms":3}}]));
    let o = f.messaging("kafka", "ns", "topic", Operation::Send);
    assert!(o.action.is_none(), "the throttle rule is not for Kafka");
    assert_eq!(o.delay, Duration::from_millis(3));
}

#[test]
fn ids_are_counted_up_and_a_removed_rule_leaves_the_others() {
    let f = Faults::with_seed(1);
    let spec = |v: serde_json::Value| spec::parse_rule(&v, MOUNTED).unwrap();
    let a = f.add(spec(json!({"fault":{"kind":"drop"}}))).unwrap();
    let b = f.add(spec(json!({"fault":{"kind":"drop"}}))).unwrap();
    assert_eq!((a["id"].clone(), b["id"].clone()), (json!(1), json!(2)));
    assert!(f.remove(1));
    assert!(!f.remove(1));
    assert_eq!(f.list_json()["rules"].as_array().unwrap().len(), 1);
    let c = f.add(spec(json!({"fault":{"kind":"drop"}}))).unwrap();
    assert_eq!(c["id"], 3, "an id is not used twice");
    assert_eq!(f.clear(), 2);
    assert!(!f.is_active());
}

#[test]
fn reset_removes_api_rules_and_puts_the_file_back_with_its_ids() {
    let f = Faults::with_seed(1);
    f.set_file_rules(spec::parse_rules(&json!([{"fault":{"kind":"drop"}}, {"fault":{"kind":"latency","ms":1}}]), MOUNTED).unwrap());
    f.add(spec::parse_rule(&json!({"fault":{"kind":"throttle"}}), MOUNTED).unwrap()).unwrap();
    assert_eq!(f.len(), 3);
    f.remove(1);
    f.reset();
    let ids: Vec<u64> = f.list_json()["rules"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, vec![1, 2]);
    // counters start again
    assert_eq!(f.list_json()["rules"][0]["fired"], 0);
    // with no file a reset leaves nothing, and the fast check says so
    let g = faults(json!([{"fault":{"kind":"drop"}}]));
    g.reset();
    assert!(!g.is_active());
}

#[test]
fn the_rule_cap_is_a_conflict() {
    let f = Faults::with_seed(1);
    let rule = spec::parse_rule(&json!({"fault":{"kind":"drop"}}), MOUNTED).unwrap();
    for _ in 0..spec::MAX_RULES {
        f.add(rule.clone()).unwrap();
    }
    assert!(f.add(rule).unwrap_err().contains("at most"));
}

#[test]
fn ru_budget_throttles_when_the_second_is_used_up() {
    let f = faults(json!([{"service":"cosmos","path":"/dbs/*","fault":{"kind":"ruBudget","ruPerSecond":10}}]));
    let probe = http("cosmos", "acct", "GET", "/dbs/d/colls/c/docs/1", "");
    // other paths are not counted
    assert!(f.decide(&http("cosmos", "acct", "GET", "/", "")).charge.is_empty());
    let first = f.decide(&probe);
    assert!(first.action.is_none());
    assert_eq!(first.charge, vec![1]);
    f.add_charge(&first.charge, 6.0);
    let second = f.decide(&probe);
    assert!(second.action.is_none(), "6 of 10 used");
    f.add_charge(&second.charge, 6.0);
    let third = f.decide(&probe);
    let a = answer(&third);
    assert!(a.throttle);
    let wait = a.retry_after.unwrap();
    assert!(wait >= Duration::from_millis(1) && wait <= Duration::from_secs(1), "{wait:?}: to the next second");
    assert_eq!(third.kinds(), "ruBudget");
    assert_eq!(f.list_json()["rules"][0]["fired"], 1);
}

#[test]
fn the_list_shows_ids_counts_and_the_rule_as_written() {
    let f = faults(json!([{"service":"blob","fault":{"kind":"throttle","retryAfterSeconds":2}}]));
    blob_put(&f);
    let j = f.list_json();
    assert_eq!(j["rules"][0], json!({"id":1,"service":"blob","fault":{"kind":"throttle","retryAfterSeconds":2},"matched":1,"fired":1}));
}

#[test]
fn the_generic_answer() {
    let f = faults(json!([{"fault":{"kind":"throttle","retryAfterMs":1200}}, {"method":"POST","fault":{"kind":"error","status":500}}]));
    let o = blob_put(&f);
    let r = generic_response(answer(&o));
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "2");
    let v: serde_json::Value = serde_json::from_slice(r.body()).unwrap();
    assert_eq!(v["error"]["code"], "TooManyRequests");
    let o = f.decide(&http("blob", "a", "POST", "/", ""));
    // the first rule decides; remove it to see the second
    assert!(o.action.is_some());
    f.remove(1);
    let o = f.decide(&http("blob", "a", "POST", "/", ""));
    let r = generic_response(answer(&o));
    assert_eq!(r.status(), 500);
    let v: serde_json::Value = serde_json::from_slice(r.body()).unwrap();
    assert_eq!(v["error"]["code"], "InternalServerError");
    assert!(r.headers().get("retry-after").is_none());
}

// ---- a rule is small, and matching it takes time in proportion to the request ----

#[test]
fn a_pattern_that_would_make_a_backtracking_match_slow_is_answered_at_once() {
    // one star, a long run of letters and one that is never there: the worst case for a match that retries from every start
    let pattern = format!("/*{}b", "a".repeat(1000));
    let text = format!("/{}", "a".repeat(60_000));
    let start = std::time::Instant::now();
    for _ in 0..100 {
        assert!(!spec::path_matches(&pattern, &text));
    }
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    // and a match is still a match
    assert!(spec::glob("/a*b*c", "/aXXbYYc"));
    assert!(spec::glob("*", ""));
    assert!(spec::glob("a**b", "ab"));
    assert!(!spec::glob("a*b", "a"));
    assert!(!spec::glob("ab*ba", "aba"), "the prefix and the suffix may not overlap");
    assert!(spec::glob("/x/*/y", "/x//y"));
    assert!(spec::glob("é*é", "éaé"));
    assert!(!spec::glob("é*", "e"));
}

#[test]
fn a_rule_is_refused_when_one_of_its_texts_is_too_long() {
    let long = |n: usize| "a".repeat(n);
    let bad = |v: serde_json::Value| spec::parse_rules(&json!([v]), MOUNTED).unwrap_err();
    assert!(spec::parse_rules(&json!([{"path": format!("/{}", long(1000)), "fault": {"kind": "drop"}}]), MOUNTED).is_ok());
    for field in ["path", "query", "account"] {
        let value = if field == "path" { format!("/{}", long(2000)) } else { long(2000) };
        let e = bad(json!({field: value, "fault": {"kind": "drop"}}));
        assert!(e.contains(field) && e.contains("1024"), "{e}");
    }
    let e = bad(json!({"fault": {"kind": "error", "status": 500, "code": long(300)}}));
    assert!(e.contains("fault.code") && e.contains("256"), "{e}");
    let e = bad(json!({"fault": {"kind": "error", "status": 500, "message": long(3000)}}));
    assert!(e.contains("fault.message") && e.contains("2048"), "{e}");
    let e = bad(json!({"fault": {"kind": "error", "status": 500, "headers": {"x-a": long(3000)}}}));
    assert!(e.contains("fault.headers") && e.contains("2048"), "{e}");
    let e = bad(json!({"fault": {"kind": "error", "status": 500, "headers": {long(200): "v"}}}));
    assert!(e.contains("fault.headers") && e.contains("128"), "{e}");
    let many: serde_json::Map<String, serde_json::Value> = (0..20).map(|i| (format!("x-h{i}"), json!("v"))).collect();
    let e = bad(json!({"fault": {"kind": "error", "status": 500, "headers": many}}));
    assert!(e.contains("fault.headers") && e.contains("at most 8"), "{e}");
}
