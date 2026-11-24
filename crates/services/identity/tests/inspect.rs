//! The inspector of the identity service (`/_azemu/inspect/identity/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;

const STORAGE: &str = "https%3A%2F%2Fstorage.azure.com%2F.default";

async fn insp(srv: &azemu_core::testing::TestServer, p: &str) -> azemu_core::testing::TestResponse {
    srv.send("GET", &format!("{}/_azemu/inspect{p}", gateway(srv)), &[], "").await
}

#[tokio::test]
async fn the_signer_shows_the_key_id_and_counts_tokens_and_nothing_secret() {
    let srv = server().await;
    // the key is made in the background; the first token waits for it
    let form = format!("grant_type=client_credentials&client_id=app-1&client_secret=s3cret&scope={STORAGE}");
    let url = format!("{}/contoso/oauth2/v2.0/token", login(&srv));
    assert_eq!(post_form(&srv, &url, &form).await.status, 200);
    let before = insp(&srv, "/identity/signer").await.json();
    assert_eq!(before["properties"]["keyReady"], true);
    assert_eq!(before["properties"]["tokensSigned"], 1);
    for _ in 0..2 {
        assert_eq!(post_form(&srv, &url, &form).await.status, 200);
    }
    let after = insp(&srv, "/identity/signer").await.json();
    assert_eq!(after["properties"]["tokensSigned"], 3);
    assert_eq!(after["properties"]["keyId"], before["properties"]["keyId"]);
    // the public key is the one the service publishes; nothing private
    let text = after.to_string();
    for private in ["\"d\"", "\"p\"", "\"q\"", "\"dp\"", "\"dq\"", "\"qi\""] {
        assert!(!text.contains(private), "{private} in {text}");
    }
    let nodes = walk(&srv, "/identity", 1).await;
    assert_eq!(nodes.len(), 2);
    assert_eq!(insp(&srv, "/identity/other").await.status, 404);
}
