mod common;
// Rule management over `$management` as the JavaScript SDK (`ServiceBusRuleManager`) uses it. The
// request messages are the exact bytes `@azure/service-bus` 7.x / rhea encode for `createRule`
// (captured with `rhea-promise`'s `message.encode`): `rule-description` is a map. The answers are
// checked against what the SDK reads in `getRules` (`core/managementClient.js`) and what the .NET
// SDK's codec classes define (descriptor codes, field order).

use azemu_amqp::message::{Message, Properties};
use azemu_amqp::types::Value;
use azemu_servicebus::rules::{Rule, RuleFilter};
use common::*;

fn with_props(body_text: &str, props: Vec<(&str, Value)>, label: Option<&str>) -> Message {
    Message {
        properties: Some(Properties { subject: label.map(str::to_string), message_id: Some(Value::str("m")), ..Default::default() }),
        application_properties: Some(props.into_iter().map(|(k, v)| (Value::str(k), v)).collect()),
        ..msg(body_text)
    }
}

fn bytes(hex: &str) -> Vec<u8> {
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

// createRule('sql', { sqlExpression: "color = 'red'", sqlParameters: {} }, { sqlExpression: "SET touched = 'yes'" })
const ADD_SQL: &str = "00537045005373d00000001700000005a1056d69642d31404040a1077265706c792d31005374d10000003a00000004a1096f7065726174696f6ea116636f6d2e6d6963726f736f66743a6164642d72756c65a10a747261636b696e674964a1057469642d31005377d10000009a00000004a10972756c652d6e616d65a10373716ca11072756c652d6465736372697074696f6ed10000006f00000004a10a73716c2d66696c746572d10000001f00000002a10a65787072657373696f6ea10d636f6c6f72203d202772656427a10f73716c2d72756c652d616374696f6ed10000002500000002a10a65787072657373696f6ea11353455420746f7563686564203d202779657327";
// createRule('p', { sqlExpression: 'qty > 3' })
const ADD_SQL_ONLY: &str = "00537045005373d00000001700000005a1056d69642d31404040a1077265706c792d31005374d10000003a00000004a1096f7065726174696f6ea116636f6d2e6d6963726f736f66743a6164642d72756c65a10a747261636b696e674964a1057469642d31005377d10000005700000004a10972756c652d6e616d65a10170a11072756c652d6465736372697074696f6ed10000002e00000002a10a73716c2d66696c746572d10000001900000002a10a65787072657373696f6ea107717479203e2033";
// createRule('corr', { subject: 'order', correlationId: 'c1', applicationProperties: { kind: 'x', n: 5 } })
const ADD_CORRELATION: &str = "00537045005373d00000001700000005a1056d69642d31404040a1077265706c792d31005374d10000003a00000004a1096f7065726174696f6ea116636f6d2e6d6963726f736f66743a6164642d72756c65a10a747261636b696e674964a1057469642d31005377d1000000e100000004a10972756c652d6e616d65a104636f7272a11072756c652d6465736372697074696f6ed1000000b500000002a112636f7272656c6174696f6e2d66696c746572d10000009800000012a10e636f7272656c6174696f6e2d6964a1026331a10a6d6573736167652d696440a102746f40a1087265706c792d746f40a1056c6162656ca1056f72646572a10a73657373696f6e2d696440a1137265706c792d746f2d73657373696f6e2d696440a10c636f6e74656e742d7479706540a10a70726f70657274696573d10000001200000004a1046b696e64a10178a1016e5205";

async fn setup() -> (Env, azemu_amqp::client::TestClient, Mgmt) {
    let e = env().await;
    e.topic("t");
    e.sub("t", "s", vec![Rule::default_rule(0)], |_| {});
    let mut c = e.client().await;
    let mg = Mgmt::open(&mut c, "t/Subscriptions/s/$management").await;
    (e, c, mg)
}

fn rules_of(e: &Env) -> Vec<Rule> {
    e.state(|st| st.ns("default").topic("t").unwrap().subs["s"].rules.clone())
}

#[tokio::test]
async fn add_rule_with_the_maps_the_js_sdk_sends() {
    let (e, mut c, mut mg) = setup().await;
    for payload in [ADD_SQL, ADD_SQL_ONLY, ADD_CORRELATION] {
        let r = mg.call_raw(&mut c, &bytes(payload)).await;
        assert_eq!(r.status, 200, "{} {}", r.description, r.condition.clone().unwrap_or_default());
    }
    let rules = rules_of(&e);
    assert_eq!(rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["$Default", "sql", "p", "corr"]);
    match &rules[1].filter {
        RuleFilter::Sql { expression, .. } => assert_eq!(expression, "color = 'red'"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rules[1].action.as_ref().unwrap().expression, "SET touched = 'yes'");
    assert!(rules[2].action.is_none());
    match &rules[3].filter {
        RuleFilter::Correlation(cf) => {
            assert_eq!(cf.correlation_id.as_deref(), Some("c1"));
            assert_eq!(cf.label.as_deref(), Some("order"));
            assert_eq!(cf.message_id, None, "an unset property (null) is no condition");
            assert_eq!(cf.properties.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    // the same name again is "already exists"
    let r = mg.call_raw(&mut c, &bytes(ADD_SQL)).await;
    assert_eq!((r.status, r.condition.as_deref()), (409, Some("com.microsoft:entity-already-exists")));
}

#[tokio::test]
async fn rules_added_with_js_maps_route_messages() {
    let (_e, mut c, mut mg) = setup().await;
    let r = mg.call(&mut c, "com.microsoft:remove-rule", vec![("rule-name", Value::str("$Default"))]).await;
    assert_eq!(r.status, 200);
    let r = mg.call_raw(&mut c, &bytes(ADD_SQL)).await;
    assert_eq!(r.status, 200);
    let s = sender(&mut c, "t").await;
    send(&mut c, s, &with_props("no", vec![("color", Value::str("blue"))], None)).await;
    send(&mut c, s, &with_props("yes", vec![("color", Value::str("red"))], None)).await;
    let rcv = receiver_rd(&mut c, "t/Subscriptions/s", 5).await;
    let m = c.recv(rcv).await.message();
    assert_eq!((body(&m), app(&m, "touched")), ("yes".to_string(), Some(Value::str("yes"))));
}

fn list(v: &Value, i: usize) -> &[Value] {
    v.as_described().unwrap().value.as_list().unwrap()[i].as_described().map(|d| d.value.as_list().unwrap()).unwrap_or(&[])
}

#[tokio::test]
async fn enumerate_rules_uses_the_descriptor_codes_the_sdks_expect() {
    let (_e, mut c, mut mg) = setup().await;
    for payload in [ADD_SQL, ADD_CORRELATION] {
        assert_eq!(mg.call_raw(&mut c, &bytes(payload)).await.status, 200);
    }
    // a false filter, added through the HTTP-free path of the engine
    let r = mg.call(&mut c, "com.microsoft:enumerate-rules", vec![("top", Value::Int(2147483647)), ("skip", Value::Int(0))]).await;
    assert_eq!(r.status, 200, "{}", r.description);
    let rules = r.body.map_get("rules").unwrap().as_list().unwrap();
    assert_eq!(rules.len(), 3);
    let descs: Vec<&Value> = rules.iter().map(|x| x.map_get("rule-description").unwrap()).collect();
    for d in &descs {
        assert_eq!(d.descriptor_code(), Some(1335734829060), "rule description: core-amqp ruleDescriptionList");
        assert!(d.as_described().unwrap().value.as_list().unwrap().len() >= 3);
    }
    let code = |d: &Value, i: usize| d.as_described().unwrap().value.as_list().unwrap()[i].descriptor_code();
    // $Default: a true filter and no action
    assert_eq!(code(descs[0], 0), Some(83483426823), "trueFilterList");
    assert_eq!(code(descs[0], 1), Some(0x0000_0137_0000_0005), "empty rule action");
    // sql: filter list [expression, compatibility level], action list [expression, level]
    assert_eq!(code(descs[1], 0), Some(83483426822), "sqlFilterList");
    assert_eq!(code(descs[1], 1), Some(1335734829062), "sqlRuleActionList");
    assert_eq!(list(descs[1], 0)[0], Value::str("color = 'red'"));
    assert_eq!(list(descs[1], 0)[1], Value::Int(20));
    assert_eq!(list(descs[1], 1)[0], Value::str("SET touched = 'yes'"));
    assert_eq!(descs[1].as_described().unwrap().value.as_list().unwrap()[2], Value::str("sql"));
    // correlation: nine fields, properties is always a map
    assert_eq!(code(descs[2], 0), Some(83483426825), "correlationFilterList");
    let f = list(descs[2], 0);
    assert_eq!(f.len(), 9);
    assert_eq!(f[0], Value::str("c1"));
    assert_eq!(f[4], Value::str("order"));
    assert!(f[8].as_map().is_some());
    // the filter-less correlation filter still carries a map (the SDK throws on null)
    let r = mg.call(&mut c, "com.microsoft:remove-rule", vec![("rule-name", Value::str("corr"))]).await;
    assert_eq!(r.status, 200);
}
