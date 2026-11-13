//! `QueueService::enqueue_base64`: how other services (Event Grid) put messages in a queue.

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_queue::{EnqueueError, QueueService};

const H: (&str, &str) = ("Authorization", "SharedKey acct1:c2ln");

#[tokio::test]
async fn enqueued_message_is_read_back_like_any_other() {
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("queue", "Acct1");
    assert_eq!(srv.send("PUT", &format!("{base}/queue1"), &[H], "").await.status, 201);

    svc.enqueue_base64("ACCT1", "queue1", b"{\"a\":1}", None).unwrap();
    svc.enqueue_base64("acct1", "queue1", b"second", Some(3600)).unwrap();

    let r = srv.send("GET", &format!("{base}/queue1/messages?numofmessages=5"), &[H], "").await;
    assert_eq!(r.status, 200);
    let xml = r.text();
    // base64 of the JSON, as the SDK reads it with a base64 decode policy
    assert!(xml.contains("<MessageText>eyJhIjoxfQ==</MessageText>"), "{xml}");
    assert!(xml.contains("<MessageText>c2Vjb25k</MessageText>"), "{xml}");
}

#[tokio::test]
async fn ttl_and_errors() {
    let svc = Arc::new(QueueService::new());
    let srv = TestServer::start(vec![svc.clone()]).await;
    let base = srv.url("queue", "acct1");
    srv.send("PUT", &format!("{base}/queue1"), &[H], "").await;
    svc.enqueue_base64("acct1", "queue1", b"never", Some(-1)).unwrap();
    let xml = srv.send("GET", &format!("{base}/queue1/messages?peekonly=true"), &[H], "").await.text();
    assert!(xml.contains("<ExpirationTime>Fri, 31 Dec 9999 23:59:59 GMT</ExpirationTime>"), "{xml}");

    assert_eq!(
        svc.enqueue_base64("acct1", "nope", b"x", None),
        Err(EnqueueError::QueueNotFound { account: "acct1".into(), queue: "nope".into() })
    );
    assert!(matches!(svc.enqueue_base64("other", "queue1", b"x", None), Err(EnqueueError::QueueNotFound { .. })));
    assert!(matches!(svc.enqueue_base64("acct1", "queue1", &vec![0u8; 60_000], None), Err(EnqueueError::TooLarge { .. })));
    assert!(svc.enqueue_base64("acct1", "queue1", &vec![0u8; 40_000], None).is_ok());
    // 53,336 characters
}
