//! Resetting one storage account (`POST /_azemu/reset?account=<name>`): its queues, messages and service
//! properties go; the other accounts stay; persistence sees a change.

mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_queue::QueueService;
use common::*;

const AUTH: [(&str, &str); 1] = [("Authorization", "SharedKey acct1:c2ln")];

#[tokio::test]
async fn one_account_is_emptied_and_the_other_is_not() {
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let (alpha, beta) = (srv.url("queue", "alpha"), srv.url("queue", "beta"));
    for base in [&alpha, &beta] {
        assert_eq!(srv.send("PUT", &format!("{base}/jobs"), &AUTH, "").await.status, 201);
        assert_eq!(srv.send("POST", &format!("{base}/jobs/messages"), &AUTH, msg_body("hello")).await.status, 201);
    }
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders>*</AllowedHeaders><ExposedHeaders>*</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(srv.send("PUT", &format!("{alpha}/?restype=service&comp=properties"), &AUTH, props).await.status, 202);

    let before = svc.persist_generation();
    assert!(svc.reset_account("Alpha"));
    assert_ne!(svc.persist_generation(), before, "persistence must see the reset");

    let r = srv.send("GET", &format!("{alpha}/jobs/messages?peekonly=true"), &AUTH, "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
    let r = srv.send("GET", &format!("{alpha}/?restype=service&comp=properties"), &AUTH, "").await;
    assert!(!r.text().contains("example.com"), "the service properties went too");
    let r = srv.send("GET", &format!("{beta}/jobs/messages?peekonly=true"), &AUTH, "").await;
    assert_eq!(tags(&r.text(), "MessageText"), vec!["hello"]);
    // a new queue in the emptied account works; an account that was never used is fine
    assert_eq!(srv.send("PUT", &format!("{alpha}/jobs"), &AUTH, "").await.status, 201);
    assert!(svc.reset_account("never-used"));
}

#[tokio::test]
async fn the_admin_endpoint_resets_one_account() {
    let srv = TestServer::start(vec![Arc::new(QueueService::new())]).await;
    let (alpha, beta) = (srv.url("queue", "alpha"), srv.url("queue", "beta"));
    for base in [&alpha, &beta] {
        assert_eq!(srv.send("PUT", &format!("{base}/jobs"), &AUTH, "").await.status, 201);
    }
    let r = srv.send("POST", &format!("{}/_azemu/reset?account=alpha", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(srv.send("GET", &format!("{alpha}/jobs/messages?peekonly=true"), &AUTH, "").await.status, 404);
    assert_eq!(srv.send("GET", &format!("{beta}/jobs/messages?peekonly=true"), &AUTH, "").await.status, 200);
}
