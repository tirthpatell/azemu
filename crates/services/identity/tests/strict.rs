//! What strict authorisation needs from the identity service (`docs/strict-auth.md`): the token authority and the principal endpoint.

mod common;

use std::sync::Arc;

use azemu_core::auth::Fault;
use azemu_core::testing::TestServer;
use azemu_identity::IdentityService;

async fn token(srv: &TestServer, query: &str) -> String {
    let r = srv.send("GET", &format!("{}/_azemu/identity/token?{query}", common::gateway(srv)), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn the_authority_verifies_what_the_service_issued_and_nothing_else() {
    let svc = Arc::new(IdentityService::new());
    let srv = TestServer::start(vec![svc.clone()]).await;
    let t = token(&srv, "resource=https://storage.azure.com&client_id=11111111-1111-1111-1111-111111111111").await;
    let authority = svc.token_authority();
    let claims = authority.verify(&t).expect("a token the service issued verifies");
    assert_eq!(claims.aud, "https://storage.azure.com");
    assert_eq!(claims.appid, "11111111-1111-1111-1111-111111111111");
    assert_eq!(claims.oid, authority.object_id_of_client("11111111-1111-1111-1111-111111111111"));
    assert!(claims.exp > claims.nbf);

    // the last characters of the signature changed
    let mut bad = t.clone();
    bad.pop();
    bad.push(if t.ends_with('A') { 'B' } else { 'A' });
    assert_eq!(authority.verify(&bad), Err(Fault::BadSignature));
    // garbage, and no algorithm
    assert_eq!(authority.verify("garbage"), Err(Fault::Malformed));
    let none = common::parts(&t);
    let h = base64_url(&serde_json::json!({"alg": "none", "typ": "JWT"}));
    let c = base64_url(&none.1);
    assert_eq!(authority.verify(&format!("{h}.{c}.x")), Err(Fault::Malformed));
    // a token signed by another service instance (another key)
    let other = Arc::new(IdentityService::new());
    let srv2 = TestServer::start(vec![other.clone()]).await;
    let t2 = token(&srv2, "resource=https://storage.azure.com").await;
    assert_eq!(authority.verify(&t2), Err(Fault::BadSignature));
}

fn base64_url(v: &serde_json::Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

#[tokio::test]
async fn the_principal_endpoint_gives_the_object_id_a_token_carries() {
    let srv = common::server().await;
    let client = "22222222-2222-2222-2222-222222222222";
    let t = token(&srv, &format!("resource=https://vault.azure.net&client_id={client}")).await;
    let oid = common::claims(&t)["oid"].as_str().unwrap().to_string();
    let r = srv.send("GET", &format!("{}/_azemu/identity/principal?client_id={client}", common::gateway(&srv)), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["objectId"], oid);
    assert_eq!(r.json()["clientId"], client);

    // nothing named: the default managed identity, the same one the managed identity endpoints issue tokens for
    let r = srv.send("GET", &format!("{}/_azemu/identity/principal", common::gateway(&srv)), &[], "").await;
    let mi = r.json();
    assert_eq!(mi["named"], "defaultManagedIdentity");
    let r = srv
        .send(
            "GET",
            &format!("{}/_azemu/identity/msi/token?resource=https://vault.azure.net&api-version=2019-08-01", common::gateway(&srv)),
            &[("X-IDENTITY-HEADER", "x")],
            "",
        )
        .await;
    let t = r.json()["access_token"].as_str().unwrap().to_string();
    assert_eq!(common::claims(&t)["oid"], mi["objectId"]);

    // an object id names itself
    let r = srv
        .send(
            "GET",
            &format!("{}/_azemu/identity/principal?object_id=33333333-3333-3333-3333-333333333333", common::gateway(&srv)),
            &[],
            "",
        )
        .await;
    assert_eq!(r.json()["objectId"], "33333333-3333-3333-3333-333333333333");
    let r = srv.send("GET", &format!("{}/_azemu/identity/principal?client_id=a&object_id=b", common::gateway(&srv)), &[], "").await;
    assert_eq!(r.status, 400);
}
