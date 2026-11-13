//! Per-service request body limits through the whole stack: a body that can never be valid for the service is
//! refused with 413 from its `Content-Length`, before any of it is read (the test sends the head and no body at all).

use std::time::Duration;

use azemu_core::testing::TestServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// (host label, method, path, what Azure documents or "generous")
const LIMITED: &[(&str, &str, &str)] = &[
    ("documents", "POST", "/dbs/d/colls/c/docs"),               // Cosmos: 2 MB
    ("vault", "PUT", "/secrets/s"),                             // Key Vault
    ("azconfig", "PUT", "/kv/k"),                               // App Configuration
    ("eventgrid", "POST", "/api/events"),                       // Event Grid
    ("servicebus", "PUT", "/q"),                                // Service Bus management API
    ("eventhub", "PUT", "/h"),                                  // Event Hubs HTTP
    ("management", "PUT", "/subscriptions/s/resourcegroups/g"), // ARM
    ("login", "POST", "/t/oauth2/v2.0/token"),                  // Identity
];

/// `login` and `management` are whole host names; the others take an account in front.
fn host_of(label: &str) -> String {
    if label == "login" || label == "management" {
        format!("{label}.localhost")
    } else {
        format!("acct.{label}.localhost")
    }
}

async fn head_only(port: u16, host: &str, method: &str, path: &str, declared: u64) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Length: {declared}\r\nAuthorization: Bearer x\r\n\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    let mut out = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut out)).await.unwrap_or(Ok(0)).unwrap_or(0);
    String::from_utf8_lossy(&out[..n]).into_owned()
}

#[tokio::test]
async fn services_refuse_a_huge_declared_body_before_reading_it() {
    let stack = azemu::full_stack();
    let srv = TestServer::start_with(stack.services, stack.raw).await;
    for (label, method, path) in LIMITED {
        // 900 MB: under core's 1 GiB limit, over every service's own
        let out = head_only(srv.port(), &host_of(label), method, path, 900 * 1024 * 1024).await;
        assert!(out.starts_with("HTTP/1.1 413"), "{label}: {out}");
    }
    // a small body is not refused (whatever the service then says about it)
    for (label, method, path) in LIMITED {
        let url = format!("http://{}:{}{path}", host_of(label), srv.port());
        let r = srv.send(method, &url, &[("Authorization", "Bearer x")], vec![b'{'; 100 * 1024]).await;
        assert_ne!(r.status, 413, "{label}: {}", r.text());
    }
}

#[tokio::test]
async fn the_limits_are_the_ones_chosen() {
    let stack = azemu::full_stack();
    let srv = TestServer::start_with(stack.services, stack.raw).await;
    let send = |label: &'static str, method: &'static str, path: &'static str, n: usize| {
        let url = format!("http://{}:{}{path}", host_of(label), srv.port());
        let srv = &srv;
        async move { srv.send(method, &url, &[("Authorization", "Bearer x")], vec![b'{'; n]).await }
    };
    // Cosmos: 2 MiB exactly passes core (the service then says what it thinks); one byte more is a 413 in Cosmos' shape
    let two_mib = 2 * 1024 * 1024;
    assert_ne!(send("documents", "POST", "/dbs/d/colls/c/docs", two_mib).await.status, 413);
    let r = send("documents", "POST", "/dbs/d/colls/c/docs", two_mib + 1).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["code"], "RequestEntityTooLarge");
    assert!(r.header("x-ms-activity-id").is_some());
    // Event Grid: the classic publish API takes 1,536,000 bytes
    assert_eq!(send("eventgrid", "POST", "/api/events", 1_536_001).await.status, 413);
    assert_eq!(send("eventgrid", "POST", "/api/events", 1_536_001).await.json()["error"]["code"], "RequestEntityTooLarge");
    // the others: 1 MiB (2 MiB Event Hubs, 4.25 MiB ARM)
    for (label, method, path) in [
        ("vault", "PUT", "/secrets/s"),
        ("azconfig", "PUT", "/kv/k"),
        ("servicebus", "PUT", "/q"),
        ("login", "POST", "/t/oauth2/v2.0/token"),
    ] {
        assert_ne!(send(label, method, path, 1024 * 1024).await.status, 413, "{label} at the limit");
        assert_eq!(send(label, method, path, 1024 * 1024 + 1).await.status, 413, "{label} over the limit");
    }
    assert_ne!(send("eventhub", "PUT", "/h", 2 * 1024 * 1024).await.status, 413);
    assert_eq!(send("eventhub", "PUT", "/h", 2 * 1024 * 1024 + 1).await.status, 413);
    let arm_max = 4 * 1024 * 1024 + 256 * 1024;
    assert_ne!(send("management", "PUT", "/subscriptions/s/resourcegroups/g", arm_max).await.status, 413);
    let r = send("management", "PUT", "/subscriptions/s/resourcegroups/g", arm_max + 1).await;
    assert_eq!((r.status, r.json()["error"]["code"].as_str().unwrap_or("").to_string()), (413, "RequestEntityTooLarge".to_string()));
    // Blob, Queue and Table keep their own rules: a 3 MiB body is not refused by core
    let r = send("blob", "PUT", "/cnt/b", 3 * 1024 * 1024).await;
    assert_ne!(r.status, 413, "{}", r.text());
}
