//! Strict authorisation of the AMQP side and the administration API (`docs/strict-auth.md`). Each test says which row of what Azure answered
//! in November 2026 it states.
mod common;
use std::sync::Arc;
use std::time::Duration;

use azemu_amqp::message::{Body, Message, Properties};
use azemu_amqp::types::Value;
use azemu_core::auth::sas::{make_token, Rights, SasRule, DEV_KEY, ROOT_RULE};
use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use common::*;

const SENDER: &str = "69a216fc-b8fb-44d8-bc22-1f3c2cd27a39";
const RECEIVER: &str = "4f6d3b9b-027b-4f4c-9142-0e5a2a2247e0";
const OWNER: &str = "090c5cfd-751d-490a-894a-3ce6f1109419";
const NS_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.ServiceBus/namespaces/default";
const AUD: &str = "https://servicebus.azure.net";
const HOST: &str = "sb://default.servicebus.windows.net";

fn unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn assign(auth: &Auth, n: u8, role: &str, principal: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: format!("11111111-1111-1111-1111-1111111111{n:02}"),
            scope: scope.into(),
            role_id: role.into(),
            principal_id: principal.into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
}

/// A strict server with the queues `q` and `other`, the topic `t` and the rules: `sendonly`, `listenonly` on the namespace, `qsend` on `q`.
async fn strict_env() -> (Env, Auth) {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.ServiceBus/namespaces", "default", NS_ID);
    let shared = azemu_servicebus::Shared::new(false, None);
    shared.set_auth(auth.clone());
    let rights = |r: &[&str]| Rights::parse(r.iter().copied()).unwrap();
    shared.sas.declare("default", None, SasRule::declared("default", None, "sendonly", rights(&["Send"]), Some("send-key".into()), None));
    shared.sas.declare(
        "default",
        None,
        SasRule::declared("default", None, "listenonly", rights(&["Listen"]), Some("listen-key".into()), None),
    );
    shared.sas.declare(
        "default",
        Some("q"),
        SasRule::declared("default", Some("q"), "qsend", rights(&["Send"]), Some("q-key".into()), None),
    );
    let e = env_with(shared).await;
    e.queue("q", |_| {});
    e.queue("other", |_| {});
    e.topic("t");
    e.sub("t", "s", vec![azemu_servicebus::rules::Rule::default_rule(0)], |_| {});
    (e, auth)
}

async fn cbs(c: &mut azemu_amqp::client::TestClient) -> (u32, u32) {
    let resp = receiver_rd(c, "$cbs", 20).await;
    let req = sender(c, "$cbs").await;
    (resp, req)
}

/// `put-token`: the status, the condition and the description of the answer.
async fn put(
    c: &mut azemu_amqp::client::TestClient,
    links: (u32, u32),
    ty: &str,
    name: &str,
    token: &str,
) -> (i64, Option<String>, String) {
    let m = Message {
        properties: Some(Properties { message_id: Some(Value::Uuid([7; 16])), reply_to: Some("$cbs".into()), ..Default::default() }),
        application_properties: Some(vec![
            (Value::str("operation"), Value::str("put-token")),
            (Value::str("type"), Value::str(ty)),
            (Value::str("name"), Value::str(name)),
        ]),
        body: Body::Value(Value::str(token)),
        ..Default::default()
    };
    send(c, links.1, &m).await;
    let r = c.recv(links.0).await.message();
    let cond = r.app_property("error-condition").and_then(|v| v.as_str().map(str::to_string));
    (
        r.app_property("status-code").and_then(|v| v.as_i64()).unwrap(),
        cond,
        r.app_property("status-description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    )
}

const SAS: &str = "servicebus.windows.net:sastoken";

fn sas(entity: &str, rule: &str, key: &str, se: i64) -> String {
    make_token(&format!("{HOST}/{entity}"), se, rule, key)
}

// ---- where a bad credential is refused: put-token ----

#[tokio::test]
async fn a_bad_sas_token_is_refused_at_put_token_with_azures_texts() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/q");
    let now = unix();
    // good: the root rule with the development key, for the queue and for the namespace; http instead of sb
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
    assert_eq!(
        put(&mut c, l, SAS, &name, &make_token("http://default.servicebus.windows.net/q", now + 600, ROOT_RULE, DEV_KEY)).await.0,
        202
    );
    let refused = |r: (i64, Option<String>, String), cond: &str, text: &str| {
        assert_eq!(r.0, 401, "{r:?}");
        assert_eq!(r.1.as_deref(), Some(cond), "{r:?}");
        assert!(r.2.starts_with(text), "{r:?}");
    };
    // expired an hour ago: ExpiredToken with the expiry time
    refused(
        put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, now - 3600)).await,
        "com.microsoft:auth-failed",
        "ExpiredToken: The token is expired. Expiration time: '",
    );
    // a wrong key, and the name of one rule with the key of another
    let r = put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, "not-the-key", now + 600)).await;
    refused(r.clone(), "com.microsoft:auth-failed", "InvalidSignature: The token has an invalid signature.");
    assert!(!r.2.contains("TrackingId"), "a wrong signature has no tracking text on Azure: {r:?}");
    refused(
        put(&mut c, l, SAS, &name, &sas("q", "sendonly", DEV_KEY, now + 600)).await,
        "com.microsoft:auth-failed",
        "InvalidSignature: The token has an invalid signature.",
    );
    // a key name that does not exist: the same text, with the tracking part
    let r = put(&mut c, l, SAS, &name, &sas("q", "nobody", DEV_KEY, now + 600)).await;
    refused(r.clone(), "com.microsoft:auth-failed", "InvalidSignature: The token has an invalid signature. TrackingId:");
    // signed for another entity
    refused(
        put(&mut c, l, SAS, &name, &sas("other", ROOT_RULE, DEV_KEY, now + 600)).await,
        "com.microsoft:auth-failed",
        "InvalidAudience: The authorization header contains a token with a wrong audience.",
    );
    // the rule of one queue used for another one is an invalid signature, not an audience problem
    let other = format!("{HOST}/other");
    refused(
        put(&mut c, l, SAS, &other, &sas("other", "qsend", "q-key", now + 600)).await,
        "com.microsoft:auth-failed",
        "InvalidSignature: The token has an invalid signature. TrackingId:",
    );
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", "qsend", "q-key", now + 600)).await.0, 202);
}

#[tokio::test]
async fn a_token_for_another_namespace_is_refused_and_a_huge_expiry_does_not_break_the_connection() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/q");
    let now = unix();
    // the development key is the same in every namespace: only the host of the audience says which one the token is for
    let other = make_token("sb://othernamespace.servicebus.windows.net/q", now + 600, ROOT_RULE, DEV_KEY);
    let r = put(&mut c, l, SAS, &name, &other).await;
    assert_eq!(r.0, 401, "{r:?}");
    assert!(r.2.starts_with("InvalidAudience: The authorization header contains a token with a wrong audience."), "{r:?}");
    // the end of time: answered like any other good token, the connection goes on
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, i64::MAX)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
}

#[tokio::test]
async fn a_sas_token_is_good_for_five_minutes_after_its_expiry() {
    // Azure: 290 s after the expiry accepted, 300 s refused
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/q");
    let now = unix();
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, now - 290)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", ROOT_RULE, DEV_KEY, now - 310)).await.0, 401);
}

// ---- where a missing claim is refused: the link attach ----

#[tokio::test]
async fn a_good_credential_without_the_claim_is_refused_at_the_attach() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let now = unix();
    let name = format!("{HOST}/q");
    // put-token accepts a send-only rule for the queue; the receiver attach is refused, the sender attach works
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", "sendonly", "send-key", now + 600)).await.0, 202);
    c.attach_sender("q").await.expect("a sender with Send");
    let err = c.attach_receiver("q", 1).await.unwrap_err().expect("the attach is answered and then detached with an error");
    assert_eq!(err.condition, "amqp:unauthorized-access");
    let text = err.description.unwrap();
    assert!(
        text.starts_with("Unauthorized access. 'Listen' claim(s) are required to perform this operation. Resource: 'sb://default.servicebus.windows.net/q'. TrackingId:"),
        "{text}"
    );
    // a listen-only rule: the other way round
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &name, &sas("q", "listenonly", "listen-key", now + 600)).await.0, 202);
    c.attach_receiver("q", 1).await.expect("a receiver with Listen");
    let err = c.attach_sender("q").await.unwrap_err().unwrap();
    assert!(err.description.unwrap().starts_with(
        "Unauthorized access. 'Send' claim(s) are required to perform this operation. Resource: 'sb://default.servicebus.windows.net/q'"
    ));
}

#[tokio::test]
async fn the_rule_of_a_queue_works_on_that_queue_only() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let now = unix();
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/q"), &sas("q", "qsend", "q-key", now + 600)).await.0, 202);
    c.attach_sender("q").await.expect("send to its own queue");
    c.attach_sender("q/$management").await.expect("its management node");
    let err = c.attach_receiver("q", 1).await.unwrap_err().unwrap();
    assert!(err.description.unwrap().contains("'Listen' claim(s)"));
    // the token was put for q: another queue is not covered by it
    let err = c.attach_sender("other").await.unwrap_err().unwrap();
    assert!(err.description.unwrap().contains("Resource: 'sb://default.servicebus.windows.net/other'"));
}

#[tokio::test]
async fn a_namespace_token_covers_every_entity_it_was_put_for_and_a_topic_covers_its_subscriptions() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let now = unix();
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/t"), &sas("t", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
    c.attach_sender("t").await.unwrap();
    c.attach_receiver("t/Subscriptions/s", 1).await.unwrap();
    c.attach_receiver("t/Subscriptions/s/$DeadLetterQueue", 1).await.unwrap();
    assert!(c.attach_receiver("q", 1).await.is_err(), "the token is for t");
}

#[tokio::test]
async fn a_connection_with_no_credential_attaches_nothing() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let err = c.attach_sender("q").await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:unauthorized-access");
}

// ---- Entra tokens ----

#[tokio::test]
async fn an_entra_principal_needs_a_role_and_the_refusal_is_at_the_attach() {
    let (e, auth) = strict_env().await;
    assign(&auth, 1, SENDER, "sender", NS_ID);
    assign(&auth, 2, RECEIVER, "receiver", NS_ID);
    assign(&auth, 3, OWNER, "owner", NS_ID);
    assign(&auth, 4, SENDER, "scoped", &format!("{NS_ID}/queues/q"));
    let name = format!("{HOST}/q");
    let token = |who: &str| TestAuthority::token(who, AUD);
    for (who, send, listen) in
        [("sender", true, false), ("receiver", false, true), ("owner", true, true), ("norole", false, false), ("scoped", true, false)]
    {
        let mut c = e.client().await;
        let l = cbs(&mut c).await;
        // put-token is accepted for every principal, with a role or without
        assert_eq!(put(&mut c, l, "jwt", &name, &token(who)).await.0, 202, "{who}");
        assert_eq!(c.attach_sender("q").await.is_ok(), send, "{who} send");
        let r = c.attach_receiver("q", 1).await;
        assert_eq!(r.is_ok(), listen, "{who} listen");
        if let Err(Some(err)) = r {
            assert_eq!(err.condition, "amqp:unauthorized-access");
            assert!(err.description.unwrap().contains("'Listen' claim(s)"));
        }
    }
    // a role on one queue is no role on the other
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, "jwt", &format!("{HOST}/other"), &token("scoped")).await.0, 202);
    let err = c.attach_sender("other").await.unwrap_err().unwrap();
    assert!(err.description.unwrap().contains("'Send' claim(s)"));
}

#[tokio::test]
async fn an_entra_token_that_is_wrong_is_refused_at_put_token() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/q");
    let good = TestAuthority::token("sender", AUD);
    let r = put(&mut c, l, "jwt", &name, &TestAuthority::token("sender", "https://vault.azure.net")).await;
    assert_eq!((r.0, r.1.as_deref()), (401, Some("com.microsoft:auth-failed")));
    assert!(r.2.starts_with("InvalidAudience: Invalid authorization token audience. TrackingId:"), "{r:?}");
    let r = put(&mut c, l, "jwt", &name, &format!("{good}x")).await;
    assert_eq!((r.0, r.1.as_deref()), (401, Some("com.microsoft:auth-failed")));
    assert!(r.2.starts_with("InvalidSignature: Invalid authorization token signature. TrackingId:"), "{r:?}");
    // a string that is no token at all: `amqp:unauthorized-access` (the condition of a garbage token on Azure)
    let r = put(&mut c, l, "jwt", &name, "garbage").await;
    assert_eq!((r.0, r.1.as_deref()), (401, Some("amqp:unauthorized-access")));
    assert!(r.2.starts_with("Malformed authorization token."), "{r:?}");
}

#[tokio::test]
async fn the_links_of_a_credential_that_ran_out_are_closed_unless_it_was_renewed() {
    let (e, auth) = strict_env().await;
    assign(&auth, 1, OWNER, "owner", NS_ID);
    let name = format!("{HOST}/q");
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let short = TestAuthority::token_with("owner", AUD, unix() - 10, unix() + 2);
    assert_eq!(put(&mut c, l, "jwt", &name, &short).await.0, 202);
    let h = c.attach_sender("q").await.unwrap();
    // renewed for another audience that covers it: stays
    let mut c2 = e.client().await;
    let l2 = cbs(&mut c2).await;
    assert_eq!(put(&mut c2, l2, "jwt", &name, &short).await.0, 202);
    let h2 = c2.attach_sender("q").await.unwrap();
    assert_eq!(put(&mut c2, l2, "jwt", &name, &TestAuthority::token("owner", AUD)).await.0, 202);
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let err = c.wait_detached(h).await.expect("the link is detached with an error");
    assert_eq!(err.condition, "amqp:unauthorized-access");
    assert!(c2.detach(h2).await.is_none(), "a renewed credential keeps the link");
}

// ---- management requests ----

#[tokio::test]
async fn a_management_request_needs_the_claim_of_its_operation() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let now = unix();
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/q"), &sas("q", "listenonly", "listen-key", now + 600)).await.0, 202);
    let resp = receiver_rd(&mut c, "q/$management", 10).await;
    let req = sender(&mut c, "q/$management").await;
    let schedule = Message {
        properties: Some(Properties {
            message_id: Some(Value::Uuid([8; 16])),
            reply_to: Some("q/$management".into()),
            ..Default::default()
        }),
        application_properties: Some(vec![(Value::str("operation"), Value::str("com.microsoft:schedule-message"))]),
        body: Body::Value(Value::Map(vec![(Value::str("messages"), Value::List(vec![]))])),
        ..Default::default()
    };
    send(&mut c, req, &schedule).await;
    let r = c.recv(resp).await.message();
    assert_eq!(r.app_property("statusCode").and_then(|v| v.as_i64()), Some(401));
    assert!(r.app_property("statusDescription").and_then(|v| v.as_str()).unwrap().contains("'Send' claim(s) are required"));
    let peek = Message {
        properties: Some(Properties {
            message_id: Some(Value::Uuid([9; 16])),
            reply_to: Some("q/$management".into()),
            ..Default::default()
        }),
        application_properties: Some(vec![(Value::str("operation"), Value::str("com.microsoft:peek-message"))]),
        body: Body::Value(Value::Map(vec![
            (Value::str("from-sequence-number"), Value::Long(0)),
            (Value::str("message-count"), Value::Int(1)),
        ])),
        ..Default::default()
    };
    send(&mut c, req, &peek).await;
    let r = c.recv(resp).await.message();
    assert_eq!(r.app_property("statusCode").and_then(|v| v.as_i64()), Some(204), "peek with Listen is not refused");
}

// ---- the mode off ----

#[tokio::test]
async fn with_the_mode_off_every_credential_is_still_accepted() {
    let e = env_with(azemu_servicebus::Shared::new(false, None)).await;
    e.queue("q", |_| {});
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/q"), "SharedAccessSignature sr=x&sig=y&se=1&skn=k").await.0, 202);
    assert_eq!(put(&mut c, l, "jwt", &format!("{HOST}/q"), "anything").await.0, 202);
    c.attach_sender("q").await.unwrap();
    c.attach_receiver("q", 1).await.unwrap();
}

// ---- the administration API ----

mod admin {
    use super::*;

    async fn call(e: &Env, method: &str, path: &str, auth: Option<&str>) -> azemu_core::testing::TestResponse {
        let url = format!("{}{path}", e.srv.url("servicebus", "default"));
        let mut h: Vec<(&str, &str)> = vec![];
        if let Some(a) = auth {
            h.push(("Authorization", a));
        }
        e.srv.send(method, &url, &h, String::new()).await
    }

    fn detail(r: &azemu_core::testing::TestResponse) -> String {
        let t = r.text();
        t.split("<Detail>").nth(1).and_then(|d| d.split("</Detail>").next()).unwrap_or("").to_string()
    }

    fn sas_header(entity: &str, rule: &str, key: &str, se: i64) -> String {
        make_token(&format!("https://default.servicebus.windows.net/{entity}"), se, rule, key)
    }

    #[tokio::test]
    async fn a_sas_token_needs_manage_for_an_entity_and_any_rule_may_read_the_namespace_information() {
        let (e, _auth) = strict_env().await;
        let now = unix();
        let root = sas_header("q", ROOT_RULE, DEV_KEY, now + 600);
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&root)).await;
        assert_eq!(r.status, 200, "{}", r.text());
        // a rule without Manage: reading an entity and listing are refused with the claims named
        let send = sas_header("q", "sendonly", "send-key", now + 600);
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&send)).await;
        assert_eq!(r.status, 401);
        assert!(r.header("content-type").unwrap().starts_with("application/xml"));
        assert!(detail(&r).starts_with("Manage,EntityRead claims required for this operation. TrackingId:"), "{}", detail(&r));
        let r =
            call(&e, "GET", "/$Resources/Queues?api-version=2021-05", Some(&sas_header("", "listenonly", "listen-key", now + 600))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("Manage,EntityRead claims required for this operation."));
        // writing
        let r = call(&e, "PUT", "/new?api-version=2021-05", Some(&sas_header("new", "sendonly", "send-key", now + 600))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("Authorization failed for specified action: Manage,EntityWrite."), "{}", detail(&r));
        // deleting a queue that is not there: 404, the answer of a caller that may read
        let r = call(&e, "DELETE", "/missing?api-version=2021-05", Some(&sas_header("missing", "sendonly", "send-key", now + 600))).await;
        assert_eq!(r.status, 404);
        assert!(detail(&r).starts_with("No service is hosted at the specified address."));
        // the namespace information is open to any rule
        let r = call(&e, "GET", "/$namespaceinfo?api-version=2021-05", Some(&sas_header("", "sendonly", "send-key", now + 600))).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    #[tokio::test]
    async fn a_token_for_another_namespace_is_a_wrong_audience_and_a_huge_expiry_is_no_panic() {
        let (e, _auth) = strict_env().await;
        let now = unix();
        let other = make_token("https://othernamespace.servicebus.windows.net/q", now + 600, ROOT_RULE, DEV_KEY);
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&other)).await;
        assert_eq!(r.status, 401, "{}", r.text());
        assert!(
            detail(&r).starts_with("InvalidAudience: The authorization header contains a token with a wrong audience."),
            "{}",
            detail(&r)
        );
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&sas_header("q", ROOT_RULE, DEV_KEY, i64::MAX))).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    #[tokio::test]
    async fn a_queue_rule_is_no_rule_for_another_queue_or_for_a_list() {
        let (e, _auth) = strict_env().await;
        let now = unix();
        let r = call(&e, "GET", "/other?api-version=2021-05", Some(&sas_header("other", "qsend", "q-key", now + 600))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("InvalidSignature: The token has an invalid signature."), "{}", detail(&r));
        let r = call(&e, "GET", "/$Resources/Queues?api-version=2021-05", Some(&sas_header("", "qsend", "q-key", now + 600))).await;
        assert!(detail(&r).starts_with("InvalidSignature: The token has an invalid signature."));
        // its own queue, read: the rule has no Manage
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&sas_header("q", "qsend", "q-key", now + 600))).await;
        assert!(detail(&r).starts_with("Manage,EntityRead claims required for this operation."), "{}", detail(&r));
    }

    #[tokio::test]
    async fn a_token_that_is_wrong_is_401_with_its_word() {
        let (e, _auth) = strict_env().await;
        let now = unix();
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&sas_header("q", ROOT_RULE, DEV_KEY, now - 3600))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("ExpiredToken: The token is expired. Expiration time: '"), "{}", detail(&r));
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&sas_header("other", ROOT_RULE, DEV_KEY, now + 600))).await;
        assert!(
            detail(&r).starts_with("InvalidAudience: The authorization header contains a token with a wrong audience."),
            "{}",
            detail(&r)
        );
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&sas_header("q", ROOT_RULE, "wrong", now + 600))).await;
        assert!(detail(&r).starts_with("InvalidSignature: The token has an invalid signature."));
        let r = call(&e, "GET", "/q?api-version=2021-05", None).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("claim is empty or token is invalid."), "{}", detail(&r));
        // a SharedAccessSignature header that cannot be read is a 500 with nothing but the tracking text
        let r = call(&e, "GET", "/q?api-version=2021-05", Some("SharedAccessSignature garbage")).await;
        assert_eq!(r.status, 500);
        assert!(detail(&r).starts_with("TrackingId:"), "{}", detail(&r));
    }

    #[tokio::test]
    async fn an_entra_principal_reads_with_the_actions_of_its_role_and_never_writes() {
        let (e, auth) = strict_env().await;
        assign(&auth, 1, RECEIVER, "receiver", NS_ID);
        assign(&auth, 2, SENDER, "scoped", &format!("{NS_ID}/queues/q"));
        assign(&auth, 3, OWNER, "owner", NS_ID);
        let bearer = |who: &str| format!("Bearer {}", TestAuthority::token(who, AUD));
        // a Data Receiver gets and lists (the `actions` of the role say `queues/read`), but cannot make a queue
        assert_eq!(call(&e, "GET", "/q?api-version=2021-05", Some(&bearer("receiver"))).await.status, 200);
        assert_eq!(call(&e, "GET", "/$Resources/Queues?api-version=2021-05", Some(&bearer("receiver"))).await.status, 200);
        assert_eq!(call(&e, "GET", "/$namespaceinfo?api-version=2021-05", Some(&bearer("receiver"))).await.status, 200);
        let r = call(&e, "PUT", "/new?api-version=2021-05", Some(&bearer("receiver"))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("Authorization failed for specified action: Manage,EntityWrite. TrackingId:"), "{}", detail(&r));
        // a Data Sender on one queue gets that queue, and nothing else
        assert_eq!(call(&e, "GET", "/q?api-version=2021-05", Some(&bearer("scoped"))).await.status, 200);
        let r = call(&e, "GET", "/$Resources/Queues?api-version=2021-05", Some(&bearer("scoped"))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("Authorization failed for specified action: Manage,EntityRead,Send,Listen."), "{}", detail(&r));
        assert_eq!(call(&e, "GET", "/$namespaceinfo?api-version=2021-05", Some(&bearer("scoped"))).await.status, 401);
        // no role
        let r = call(&e, "GET", "/q?api-version=2021-05", Some(&bearer("norole"))).await;
        assert_eq!(r.status, 401);
        assert!(detail(&r).starts_with("Authorization failed for specified action: Manage,EntityRead,Send,Listen."));
        let r = call(&e, "DELETE", "/missing?api-version=2021-05", Some(&bearer("norole"))).await;
        assert_eq!((r.status, detail(&r).starts_with("Generic: .")), (401, true), "{}", detail(&r));
        assert_eq!(call(&e, "DELETE", "/missing?api-version=2021-05", Some(&bearer("receiver"))).await.status, 404);
        // the owner does everything
        assert_eq!(call(&e, "GET", "/q?api-version=2021-05", Some(&bearer("owner"))).await.status, 200);
        // wrong audience and a changed signature
        let wrong = format!("Bearer {}", TestAuthority::token("owner", "https://vault.azure.net"));
        assert!(detail(&call(&e, "GET", "/q?api-version=2021-05", Some(&wrong)).await)
            .starts_with("InvalidAudience: Invalid authorization token audience."));
        let bad = format!("{}x", bearer("owner"));
        assert!(detail(&call(&e, "GET", "/q?api-version=2021-05", Some(&bad)).await)
            .starts_with("InvalidSignature: Invalid authorization token signature."));
        assert!(detail(&call(&e, "GET", "/q?api-version=2021-05", Some("Bearer garbage")).await).starts_with("MalformedToken: ."));
    }
}

// ---- declaring rules ----

#[tokio::test]
async fn rules_come_from_the_config_file_and_the_admin_api() {
    let dir = tempdir();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        r#"{"UserConfig":{"Namespaces":[{"Name":"emulator","AuthorizationRules":[{"Name":"listener","Rights":["Listen"],"PrimaryKey":"lk"}],
            "Queues":[{"Name":"orders","AuthorizationRules":[{"Name":"orders-send","Rights":["Send"],"PrimaryKey":"ok"}]}]}]}}"#,
    )
    .unwrap();
    let shared = azemu_servicebus::Shared::new(false, Some(path.clone()));
    shared.load_config();
    let auth = Auth::strict();
    shared.set_auth(auth);
    // `default` is the name the single namespace of the file answers to
    let rules = shared.sas.candidates("emulator", &azemu_core::auth::sas::chain_of("orders"));
    let names: Vec<_> = rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"listener") && names.contains(&"orders-send") && names.contains(&ROOT_RULE), "{names:?}");
    // a mistake in the file names the entity
    std::fs::write(
        &path,
        r#"{"UserConfig":{"Namespaces":[{"Name":"n","Queues":[{"Name":"q1","AuthorizationRules":[{"Name":"r","Rights":["Fly"]}]}]}]}}"#,
    )
    .unwrap();
    let cfg = azemu_servicebus::config::load(&path).unwrap();
    let err = azemu_servicebus::config::declared_rules(&cfg).unwrap_err();
    assert!(err.contains("queue 'q1'") && err.contains("Fly"), "{err}");
    // a reset puts back what the file declares and takes away what the admin API added
    shared.sas.declare("emulator", None, SasRule::declared("emulator", None, "temp", Rights::ALL, None, None));
    std::fs::write(
        &path,
        r#"{"UserConfig":{"Namespaces":[{"Name":"emulator","AuthorizationRules":[{"Name":"listener","Rights":["Listen"],"PrimaryKey":"lk"}]}]}}"#,
    )
    .unwrap();
    shared.reset();
    let after: Vec<_> = shared.sas.at("emulator", None).into_iter().map(|r| r.name).collect();
    assert!(after.contains(&"listener".to_string()) && !after.contains(&"temp".to_string()), "{after:?}");
}

#[tokio::test]
async fn the_admin_api_declares_a_rule_that_a_token_then_uses() {
    let (e, _auth) = strict_env().await;
    let base = format!("{}/_azemu/servicebus/default/authorization-rules", e.srv.gateway_url());
    let r = e
        .srv
        .send(
            "PUT",
            &format!("{base}/agent"),
            &[("content-type", "application/json")],
            r#"{"rights":["Send","Listen"],"primaryKey":"agent-key"}"#,
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = e
        .srv
        .send(
            "PUT",
            &format!("{base}/agent"),
            &[("content-type", "application/json")],
            r#"{"rights":["Send","Listen"],"primaryKey":"agent-key"}"#,
        )
        .await;
    assert_eq!(r.status, 200);
    let list = e.srv.send("GET", &base, &[], String::new()).await.json();
    assert!(list["rules"].as_array().unwrap().iter().any(|r| r["name"] == "agent" && r["primaryKey"] == "agent-key"), "{list}");
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/q"), &sas("q", "agent", "agent-key", unix() + 600)).await.0, 202);
    c.attach_sender("q").await.unwrap();
    c.attach_receiver("q", 1).await.unwrap();
    let r = e.srv.send("DELETE", &format!("{base}/agent"), &[], String::new()).await;
    assert_eq!(r.status, 204);
    let r = e.srv.send("PUT", &format!("{base}/bad"), &[("content-type", "application/json")], r#"{"rights":["Fly"]}"#).await;
    assert_eq!(r.status, 400);
}

fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-sb-strict-{}", uuid_like()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn uuid_like() -> String {
    format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}
