//! Strict authorisation of the AMQP side (`docs/strict-auth.md`). Each test says which row of what Azure answered in November 2026 it states.
mod common;
use std::sync::Arc;

use azemu_amqp::client::TestClient;
use azemu_amqp::message::Properties;
use azemu_amqp::types::Value;
use azemu_core::auth::sas::{make_token, Rights, SasRule, DEV_KEY, ROOT_RULE};
use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use common::*;

const SENDER: &str = "2b629674-e913-4c01-ae53-ef4638d8f975";
const RECEIVER: &str = "a638d3c7-ab3a-418d-83e6-5f17a39d4fde";
const NS_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.EventHub/namespaces/myns";
const AUD: &str = "https://eventhubs.azure.net";
const HOST: &str = "sb://myns.servicebus.windows.net";

fn unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn assign(auth: &Auth, n: u8, role: &str, principal: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: format!("22222222-2222-2222-2222-2222222222{n:02}"),
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

async fn strict_env() -> (Env, Auth) {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.EventHub/namespaces", "myns", NS_ID);
    let shared = azemu_eventhubs::Shared::new(false, None);
    shared.set_auth(auth.clone());
    let rights = |r: &[&str]| Rights::parse(r.iter().copied()).unwrap();
    shared.sas.declare("myns", None, SasRule::declared("myns", None, "sendonly", rights(&["Send"]), Some("send-key".into()), None));
    shared.sas.declare("myns", None, SasRule::declared("myns", None, "listenonly", rights(&["Listen"]), Some("listen-key".into()), None));
    let e = env_with(shared).await;
    e.hub("h", 2);
    e.hub("other", 1);
    (e, auth)
}

async fn put(c: &mut TestClient, links: (u32, u32), ty: &str, name: &str, token: &str) -> (i64, Option<String>, String) {
    let mut m = TestClient::value_message(
        Value::str(token),
        vec![("operation", Value::str("put-token")), ("type", Value::str(ty)), ("name", Value::str(name))],
    );
    m.properties = Some(Properties { message_id: Some(Value::Uuid([9; 16])), ..Default::default() });
    send_one(c, links.1, &m).await;
    let r = c.recv(links.0).await.message();
    (
        r.app_property("status-code").and_then(|v| v.as_i64()).unwrap(),
        r.app_property("error-condition").and_then(|v| v.as_str().map(str::to_string)),
        r.app_property("status-description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    )
}

async fn cbs(c: &mut TestClient) -> (u32, u32) {
    let resp = c.attach_receiver("$cbs", 20).await.unwrap();
    let req = attach_sender(c, "$cbs").await.unwrap();
    (resp, req)
}

fn sas(entity: &str, rule: &str, key: &str, se: i64) -> String {
    make_token(&format!("{HOST}/{entity}"), se, rule, key)
}

const SAS: &str = "servicebus.windows.net:sastoken";

#[tokio::test]
async fn a_bad_sas_token_is_refused_at_put_token() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/h");
    let now = unix();
    // the namespace's token is accepted for a hub, and so is the hub's own
    assert_eq!(put(&mut c, l, SAS, &name, &sas("", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
    let r = put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, now - 3600)).await;
    assert_eq!((r.0, r.1.as_deref()), (401, Some("com.microsoft:auth-failed")));
    assert!(r.2.starts_with("ExpiredToken: The token is expired. Expiration time: '"), "{r:?}");
    let r = put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, "nope", now + 600)).await;
    assert_eq!(r.2, "InvalidSignature: The token has an invalid signature.");
    let r = put(&mut c, l, SAS, &name, &sas("h", "nobody", DEV_KEY, now + 600)).await;
    assert!(r.2.starts_with("InvalidSignature: The token has an invalid signature. TrackingId:"), "{r:?}");
    let r = put(&mut c, l, SAS, &name, &sas("other", ROOT_RULE, DEV_KEY, now + 600)).await;
    assert_eq!(r.2, "InvalidAudience: The authorization header contains a token with a wrong audience.");
    // 290 s late is accepted, 310 s refused
    assert_eq!(put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, now - 290)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, now - 310)).await.0, 401);
}

#[tokio::test]
async fn a_token_for_another_namespace_is_refused_and_a_huge_expiry_does_not_break_the_connection() {
    let (e, _auth) = strict_env().await;
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    let name = format!("{HOST}/h");
    let now = unix();
    // the development key is the same in every namespace: only the host of the audience says which one the token is for
    let other = make_token("sb://othernamespace.servicebus.windows.net/h", now + 600, ROOT_RULE, DEV_KEY);
    let r = put(&mut c, l, SAS, &name, &other).await;
    assert_eq!(r.0, 401, "{r:?}");
    assert!(r.2.starts_with("InvalidAudience: The authorization header contains a token with a wrong audience."), "{r:?}");
    assert_eq!(put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, i64::MAX)).await.0, 202);
    assert_eq!(put(&mut c, l, SAS, &name, &sas("h", ROOT_RULE, DEV_KEY, now + 600)).await.0, 202);
}

#[tokio::test]
async fn a_rule_without_the_claim_is_refused_at_the_attach() {
    let (e, _auth) = strict_env().await;
    let now = unix();
    // a send-only rule: sending works, receiving is detached with the partition path in lower case
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/h"), &sas("h", "sendonly", "send-key", now + 600)).await.0, 202);
    attach_sender(&mut c, "h").await.expect("send with Send");
    attach_sender(&mut c, "h/Partitions/1").await.expect("send to a partition with Send");
    let err = consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap_err().unwrap();
    assert_eq!(err.condition, "amqp:unauthorized-access");
    let text = err.description.unwrap();
    assert!(
        text.starts_with("Unauthorized access. 'Listen' claim(s) are required to perform this operation. Resource: 'sb://myns.servicebus.windows.net/h/consumergroups/$default/partitions/0'. TrackingId:"),
        "{text}"
    );
    // a listen-only rule: the other way
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/h"), &sas("h", "listenonly", "listen-key", now + 600)).await.0, 202);
    consume(&mut c, "h", "$Default", 0, &from_start()).await.expect("receive with Listen");
    let err = attach_sender(&mut c, "h").await.unwrap_err().unwrap();
    assert!(err.description.unwrap().starts_with("Unauthorized access. 'Send' claim(s) are required to perform this operation. Resource: 'sb://myns.servicebus.windows.net/h'. TrackingId:"));
}

#[tokio::test]
async fn management_reads_need_the_right_to_send_or_to_receive() {
    let (e, auth) = strict_env().await;
    assign(&auth, 1, RECEIVER, "reader", NS_ID);
    let now = unix();
    let hub = vec![("name", "h"), ("operation", "READ"), ("type", "com.microsoft:eventhub")];
    // a send-only rule may read the hub's properties (found on Azure)
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/h"), &sas("h", "sendonly", "send-key", now + 600)).await.0, 202);
    let links = mgmt_links(&mut c).await;
    assert_eq!(mgmt(&mut c, links, hub.clone()).await.0, 200);
    // a reader
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, "jwt", &format!("{HOST}/h"), &TestAuthority::token("reader", AUD)).await.0, 202);
    let links = mgmt_links(&mut c).await;
    assert_eq!(mgmt(&mut c, links, hub.clone()).await.0, 200);
    // no role: the link attaches and the answer to the READ is a 401 inside it
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, "jwt", &format!("{HOST}/h"), &TestAuthority::token("norole", AUD)).await.0, 202);
    let links = mgmt_links(&mut c).await;
    let mut m = TestClient::value_message(Value::Null, hub.iter().map(|(k, v)| (*k, Value::str(*v))).collect());
    m.properties = Some(Properties { message_id: Some(Value::Uuid([3; 16])), ..Default::default() });
    m.body = azemu_amqp::message::Body::Empty;
    send_one(&mut c, links.0, &m).await;
    let r = c.recv(links.1).await.message();
    assert_eq!(r.app_property("status-code").and_then(|v| v.as_i64()), Some(401));
    assert_eq!(r.app_property("error-condition").and_then(|v| v.as_str()), Some("amqp:unauthorized-access"));
    assert_eq!(r.app_property("status-description").and_then(|v| v.as_str()), Some("Attempted to perform an unauthorized operation."));
}

#[tokio::test]
async fn an_entra_principal_needs_a_role_and_the_refusal_is_at_the_attach() {
    let (e, auth) = strict_env().await;
    assign(&auth, 1, SENDER, "sender", NS_ID);
    assign(&auth, 2, RECEIVER, "reader", NS_ID);
    assign(&auth, 3, SENDER, "scoped", &format!("{NS_ID}/eventhubs/h"));
    let name = format!("{HOST}/h");
    for (who, send, listen) in [("sender", true, false), ("reader", false, true), ("norole", false, false), ("scoped", true, false)] {
        let mut c = e.client().await;
        let l = cbs(&mut c).await;
        assert_eq!(put(&mut c, l, "jwt", &name, &TestAuthority::token(who, AUD)).await.0, 202, "{who}");
        assert_eq!(attach_sender(&mut c, "h").await.is_ok(), send, "{who} send");
        assert_eq!(consume(&mut c, "h", "$Default", 0, &from_start()).await.is_ok(), listen, "{who} receive");
    }
    // a role on the hub `h` is none on `other`
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, "jwt", &format!("{HOST}/other"), &TestAuthority::token("scoped", AUD)).await.0, 202);
    assert!(attach_sender(&mut c, "other").await.is_err());
    // a token that is wrong
    let r = put(&mut c, l, "jwt", &name, &TestAuthority::token("sender", "https://vault.azure.net")).await;
    assert!(r.2.starts_with("InvalidAudience: Invalid authorization token audience. TrackingId:"), "{r:?}");
    let r = put(&mut c, l, "jwt", &name, &format!("{}x", TestAuthority::token("sender", AUD))).await;
    assert!(r.2.starts_with("InvalidSignature: Invalid authorization token signature."), "{r:?}");
}

#[tokio::test]
async fn with_the_mode_off_every_credential_is_still_accepted() {
    let e = env().await;
    e.hub("h", 1);
    let mut c = e.client().await;
    let l = cbs(&mut c).await;
    assert_eq!(put(&mut c, l, SAS, &format!("{HOST}/h"), "SharedAccessSignature sr=x&sig=y&se=1&skn=k").await.0, 202);
    attach_sender(&mut c, "h").await.unwrap();
    consume(&mut c, "h", "$Default", 0, &from_start()).await.unwrap();
}
