//! Query Blob Contents over real sockets: the answer is streamed, a client that goes away or stops reading
//! frees its place, and abandoned queries do not keep the places.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_blob::{BlobService, QueryLimits};
use azemu_core::testing::TestServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

const AUTH: &str = "SharedKey acct:c2ln";

fn xml(expr: &str, output_json: bool) -> String {
    let out = if output_json { "<OutputSerialization><Format><Type>json</Type></Format></OutputSerialization>" } else { "" };
    format!("<QueryRequest><QueryType>SQL</QueryType><Expression>{}</Expression>{out}</QueryRequest>", expr.replace('<', "&lt;"))
}

async fn start(limits: QueryLimits, idle: Option<Duration>) -> TestServer {
    TestServer::start_configured(vec![Arc::new(BlobService::with_query_limits(limits))], vec![], move |b| match idle {
        Some(d) => b.body_idle_timeout(d),
        None => b,
    })
    .await
}

async fn put(srv: &TestServer, name: &str, data: Vec<u8>) {
    let r = srv
        .send("PUT", &format!("{}/cnt/{name}", srv.url("blob", "acct")), &[("Authorization", AUTH), ("x-ms-blob-type", "BlockBlob")], data)
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
}

async fn make_container(srv: &TestServer) {
    let r = srv.send("PUT", &format!("{}/cnt?restype=container", srv.url("blob", "acct")), &[("Authorization", AUTH)], "").await;
    assert_eq!(r.status, 201);
}

fn request_bytes(srv: &TestServer, name: &str, body: &str) -> Vec<u8> {
    format!(
        "POST /cnt/{name}?comp=query HTTP/1.1\r\nHost: acct.blob.localhost:{}\r\nAuthorization: {AUTH}\r\nx-ms-version: 2023-11-03\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        srv.port(),
        body.len()
    )
    .into_bytes()
}

/// A connection with a small receive buffer, so a client that does not read stops the server's writes soon.
async fn small_buffer_connection(srv: &TestServer) -> TcpStream {
    let s = TcpSocket::new_v4().unwrap();
    s.set_recv_buffer_size(4096).unwrap();
    s.connect(([127, 0, 0, 1], srv.port()).into()).await.unwrap()
}

async fn harmless(srv: &TestServer) -> (u16, Duration) {
    let t = Instant::now();
    let r = srv
        .send(
            "POST",
            &format!("{}/cnt/small.csv?comp=query", srv.url("blob", "acct")),
            &[("Authorization", AUTH)],
            xml("SELECT * FROM BlobStorage", false),
        )
        .await;
    (r.status, t.elapsed())
}

#[tokio::test]
async fn a_client_that_disconnects_frees_the_only_query_place_at_once() {
    let limits = QueryLimits { max_concurrent: 1, queue_wait: Duration::from_secs(4), ..QueryLimits::default() };
    let srv = start(limits, None).await;
    make_container(&srv).await;
    put(&srv, "big.csv", [vec![b'a'; 4_000_000], b"\n".to_vec()].concat()).await;
    put(&srv, "small.csv", b"1,2\n".to_vec()).await;
    // 300 select items, each a LIKE with `_` that costs a pass over the 4 MB record: minutes of work in all
    let items: Vec<String> = (0..300).map(|i| format!("_1 LIKE '%{}b%' AS x{i}", "a_".repeat(50))).collect();
    let body = xml(&format!("SELECT {} FROM BlobStorage", items.join(", ")), false);
    let mut c = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    c.write_all(&request_bytes(&srv, "big.csv", &body)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(c);
    let (status, took) = harmless(&srv).await;
    assert_eq!(status, 200, "the place was not freed (waited {took:?})");
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[tokio::test]
async fn the_time_limit_holds_inside_one_record_over_http() {
    let limits = QueryLimits { time_budget: Duration::from_millis(300), ..QueryLimits::default() };
    let srv = start(limits, None).await;
    make_container(&srv).await;
    put(&srv, "big.csv", [vec![b'a'; 4_000_000], b"\n".to_vec()].concat()).await;
    let items: Vec<String> = (0..300).map(|i| format!("_1 LIKE '%{}b%' AS x{i}", "a_".repeat(50))).collect();
    let t = Instant::now();
    let r = srv
        .send(
            "POST",
            &format!("{}/cnt/big.csv?comp=query", srv.url("blob", "acct")),
            &[("Authorization", AUTH)],
            xml(&format!("SELECT {} FROM BlobStorage", items.join(", ")), false),
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("QueryTimeout") || r.body.windows(12).any(|w| w == b"QueryTimeout"), "no QueryTimeout in the answer");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
}

#[tokio::test]
async fn clients_that_never_read_lose_their_places_after_the_idle_time() {
    let limits = QueryLimits { max_concurrent: 2, queue_wait: Duration::from_secs(8), ..QueryLimits::default() };
    let srv = start(limits, Some(Duration::from_millis(800))).await;
    make_container(&srv).await;
    let line = "abcdefghij,klmnopqrst\n";
    put(&srv, "rows.csv", line.repeat(150_000).into_bytes()).await; // 3.3 MB, the answer of the query below is about 17 MB
    put(&srv, "small.csv", b"1,2\n".to_vec()).await;
    let body = xml("SELECT _1, _2, _1, _2, _1, _2 FROM BlobStorage", true);
    // two clients send the query and never read: they hold both places
    let mut silent = vec![];
    for _ in 0..2 {
        let mut c = small_buffer_connection(&srv).await;
        c.write_all(&request_bytes(&srv, "rows.csv", &body)).await.unwrap();
        silent.push(c);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // a third query has to wait for a place: it is answered once the silent ones were dropped (idle time 0.8 s), not refused
    let (status, took) = harmless(&srv).await;
    assert_eq!(status, 200, "waited {took:?}");
    assert!(took > Duration::from_millis(200) && took < Duration::from_secs(6), "{took:?}");
    drop(silent);
}

#[tokio::test]
async fn a_big_answer_arrives_complete_and_chunked() {
    let srv = start(QueryLimits::default(), None).await;
    make_container(&srv).await;
    let line = "abcdefghij,klmnopqrst\n";
    put(&srv, "rows.csv", line.repeat(50_000).into_bytes()).await;
    let mut c = TcpStream::connect(("127.0.0.1", srv.port())).await.unwrap();
    c.write_all(&request_bytes(&srv, "rows.csv", &xml("SELECT _1, _2, _1, _2, _1, _2 FROM BlobStorage", true))).await.unwrap();
    let mut all = Vec::new();
    c.read_to_end(&mut all).await.ok();
    let head_end = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&all[..head_end]).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("transfer-encoding: chunked"), "{head}");
    assert!(!head.contains("content-length"), "{head}");
    assert!(all.len() > 5_000_000, "{} bytes", all.len());
    assert!(all.ends_with(b"0\r\n\r\n"), "the chunked body ends properly");
}

#[tokio::test]
async fn a_query_sees_the_blob_as_it_was_when_it_started() {
    // "not reached": the scan owns shared handles to the data it started with, so a blob that is overwritten, deleted
    // and made again while the answer is still being read does not change the answer.
    let srv = start(QueryLimits::default(), None).await;
    make_container(&srv).await;
    put(&srv, "rows.csv", "OLDVALUE-a,klmnopqrst\n".repeat(150_000).into_bytes()).await;
    let body = xml("SELECT _1, _2, _1, _2, _1, _2 FROM BlobStorage", true);
    let mut c = small_buffer_connection(&srv).await;
    let req = request_bytes(&srv, "rows.csv", &body);
    c.write_all(&req).await.unwrap();
    let mut first = vec![0u8; 100];
    c.read_exact(&mut first).await.unwrap();
    // the scan is now waiting for this client; change the blob under it
    put(&srv, "rows.csv", "NEWVALUE-b,klmnopqrst\n".repeat(10).into_bytes()).await;
    let r = srv.send("DELETE", &format!("{}/cnt/rows.csv", srv.url("blob", "acct")), &[("Authorization", AUTH)], "").await;
    assert_eq!(r.status, 202);
    put(&srv, "rows.csv", "NEWVALUE-c,klmnopqrst\n".repeat(10).into_bytes()).await;
    let mut all = first;
    c.read_to_end(&mut all).await.unwrap();
    let count = |needle: &[u8]| all.windows(needle.len()).filter(|w| *w == needle).count();
    assert!(count(b"OLDVALUE-a") > 100_000, "the old version is answered");
    assert_eq!(count(b"NEWVALUE"), 0, "nothing of the new content");
    assert!(all.ends_with(b"0\r\n\r\n"));
}
