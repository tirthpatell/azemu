//! Strict authorisation on the classic publish endpoint (`docs/strict-auth.md`): every refusal is 401, also a missing role.

use std::sync::Arc;

use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::{TestResponse, TestServer};
use azemu_eventgrid::EventGridService;

const SENDER: &str = "d5a91429-5739-47e2-a06b-3470a27159e7";
const TOPIC_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.EventGrid/topics/topic1";

async fn publish(srv: &TestServer, token: &str) -> TestResponse {
    let body = r#"[{"id":"1","subject":"s","eventType":"t","eventTime":"2026-01-01T00:00:00Z","dataVersion":"1.0","data":{}}]"#;
    srv.send(
        "POST",
        &format!("{}/api/events?api-version=2018-01-01", srv.url("eventgrid", "topic1")),
        &[("authorization", &format!("Bearer {token}")), ("content-type", "application/json")],
        body.to_string(),
    )
    .await
}

fn message(r: &TestResponse) -> String {
    r.json()["error"]["message"].as_str().unwrap_or("").split(" Report '").next().unwrap_or("").to_string()
}

#[tokio::test]
async fn a_sender_publishes_and_every_refusal_is_401() {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.EventGrid/topics", "topic1", TOPIC_ID);
    auth.registry()
        .put_assignment(Assignment {
            id: "11111111-1111-1111-1111-111111111111".into(),
            scope: TOPIC_ID.into(),
            role_id: SENDER.into(),
            principal_id: "main".into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
    let svc = EventGridService::new();
    svc.set_auth(auth.clone());
    let srv = TestServer::start(vec![Arc::new(svc)]).await;
    // the topic is made through the admin API, the way the SDK suites do
    let r = srv
        .send(
            "PUT",
            &format!("{}/_azemu/eventgrid/topics/topic1", srv.gateway_url()),
            &[("content-type", "application/json")],
            r#"{"inputSchema":"EventGridSchema"}"#,
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let ok = TestAuthority::token("main", "https://eventgrid.azure.net");
    assert_eq!(publish(&srv, &ok).await.status, 200);
    // no role
    let r = publish(&srv, &TestAuthority::token("norole", "https://eventgrid.azure.net")).await;
    assert_eq!(r.status, 401);
    assert_eq!(message(&r), format!("The principal associated with access token presented with the incoming request does not have permission to send data to {TOPIC_ID}."));
    assert!(r.header("www-authenticate").is_none());
    assert_eq!(r.json()["error"]["details"][0]["code"], "Unauthorized");
    // a token that is wrong
    for t in [TestAuthority::token("main", "https://vault.azure.net"), format!("{ok}x")] {
        let r = publish(&srv, &t).await;
        assert_eq!(r.status, 401);
        assert_eq!(message(&r), "Invalid access token. Please ensure that the token is not expired and has the right audience.");
    }
    let r = publish(&srv, "garbage").await;
    assert_eq!(message(&r), "Unable to read access token.");
}
