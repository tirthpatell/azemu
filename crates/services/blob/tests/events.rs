//! Storage events raised through the event sink (what Event Grid subscribes to).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use azemu_blob::{BlobService, StorageEvent, StorageEventSink};
use azemu_core::testing::{TestResponse, TestServer};
use parking_lot::Mutex;

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

#[derive(Default)]
struct Sink {
    wanted: AtomicBool,
    asked: Mutex<Vec<String>>,
    events: Mutex<Vec<StorageEvent>>,
}

impl StorageEventSink for Sink {
    fn wants(&self, account: &str) -> bool {
        self.asked.lock().push(account.to_string());
        self.wanted.load(Ordering::SeqCst)
    }
    fn raise(&self, event: StorageEvent) {
        self.events.lock().push(event);
    }
}

struct Env {
    srv: TestServer,
    sink: Arc<Sink>,
}

async fn start(wanted: bool) -> Env {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let sink = Arc::new(Sink::default());
    sink.wanted.store(wanted, Ordering::SeqCst);
    blob.set_event_sink(sink.clone());
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    Env { srv, sink }
}

impl Env {
    async fn b(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.srv.url("blob", "Acct")), &h, body.to_string()).await
    }

    async fn d(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.srv.url("dfs", "Acct")), &h, body.to_string()).await
    }

    fn take(&self) -> Vec<StorageEvent> {
        std::mem::take(&mut *self.sink.events.lock())
    }
}

#[tokio::test]
async fn put_blob_and_delete_raise_events_with_the_documented_fields() {
    let e = start(true).await;
    assert_eq!(e.b("PUT", "/cnt?restype=container", &[], "").await.status, 201);
    assert!(e.take().is_empty(), "creating a container raises nothing");
    let r = e
        .b(
            "PUT",
            "/cnt/dir/a%20b.txt",
            &[("x-ms-blob-type", "BlockBlob"), ("Content-Type", "text/plain"), ("x-ms-client-request-id", "client-1")],
            "hello",
        )
        .await;
    assert_eq!(r.status, 201);
    let evs = e.take();
    assert_eq!(evs.len(), 1);
    let ev = &evs[0];
    assert_eq!(ev.account, "acct");
    assert_eq!(ev.event_type, "Microsoft.Storage.BlobCreated");
    assert_eq!(ev.subject, "/blobServices/default/containers/cnt/blobs/dir/a b.txt");
    assert_eq!(ev.data_version, "");
    let d = &ev.data;
    assert_eq!(d["api"], "PutBlob");
    assert_eq!(d["clientRequestId"], "client-1");
    assert_eq!(d["requestId"].as_str(), r.header("x-ms-request-id"));
    assert_eq!(d["eTag"].as_str(), r.header("etag").map(|t| t.trim_matches('"')));
    assert_eq!(
        (d["contentType"].as_str(), d["contentLength"].as_u64(), d["blobType"].as_str()),
        (Some("text/plain"), Some(5), Some("BlockBlob"))
    );
    assert!(d["url"].as_str().unwrap().ends_with("/cnt/dir/a%20b.txt"));
    assert!(d["sequencer"].as_str().unwrap().len() >= 16);
    assert!(d["storageDiagnostics"]["batchId"].is_string());

    assert_eq!(e.b("DELETE", "/cnt/dir/a%20b.txt", &[], "").await.status, 202);
    let evs = e.take();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].event_type, "Microsoft.Storage.BlobDeleted");
    assert_eq!(evs[0].data["api"], "DeleteBlob");
    assert_eq!(evs[0].data["contentType"], "text/plain");
    assert!(evs[0].data.get("eTag").is_none() && evs[0].data.get("contentLength").is_none());
    // sequencers grow
    assert!(evs[0].data["sequencer"].as_str() > d["sequencer"].as_str());
}

#[tokio::test]
async fn nothing_is_built_when_nobody_listens() {
    let e = start(false).await;
    e.b("PUT", "/cnt?restype=container", &[], "").await;
    e.b("PUT", "/cnt/a", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert!(e.take().is_empty());
    assert!(e.sink.asked.lock().iter().all(|a| a == "acct"), "asked about the lower-case account");
    assert!(!e.sink.asked.lock().is_empty());
    // listening starts: the next write is seen
    e.sink.wanted.store(true, Ordering::SeqCst);
    e.b("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!(e.take().len(), 1);
}

#[tokio::test]
async fn failed_and_silent_operations_raise_nothing() {
    let e = start(true).await;
    e.b("PUT", "/cnt?restype=container", &[], "").await;
    e.b("PUT", "/cnt/a", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    e.take();
    // 409: the blob exists
    let r = e.b("PUT", "/cnt/a", &[("x-ms-blob-type", "BlockBlob"), ("If-None-Match", "*")], "y").await;
    assert_eq!(r.status, 409);
    // 404
    assert_eq!(e.b("DELETE", "/cnt/missing", &[], "").await.status, 404);
    // reads, metadata, snapshots, staged blocks
    e.b("GET", "/cnt/a", &[], "").await;
    e.b("PUT", "/cnt/a?comp=metadata", &[("x-ms-meta-k", "v")], "").await;
    e.b("PUT", "/cnt/a?comp=snapshot", &[], "").await;
    e.b("PUT", "/cnt/blk?comp=block&blockid=YWFh", &[], "zz").await;
    assert!(e.take().is_empty());
}

#[tokio::test]
async fn tier_change_and_block_list() {
    let e = start(true).await;
    e.b("PUT", "/cnt?restype=container", &[], "").await;
    e.b("PUT", "/cnt/a", &[("x-ms-blob-type", "BlockBlob")], "0123456789").await;
    e.take();
    assert_eq!(e.b("PUT", "/cnt/a?comp=tier", &[("x-ms-access-tier", "Cool")], "").await.status, 200);
    assert_eq!(e.b("PUT", "/cnt/a?comp=tier", &[("x-ms-access-tier", "Hot")], "").await.status, 200);
    let evs = e.take();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].event_type, "Microsoft.Storage.BlobTierChanged");
    assert_eq!(evs[0].data["accessTier"], "Cool");
    assert!(evs[0].data.get("previousTier").is_none());
    assert_eq!((evs[1].data["accessTier"].as_str(), evs[1].data["previousTier"].as_str()), (Some("Hot"), Some("Cool")));
    assert_eq!(evs[1].data["contentLength"], 10);

    e.b("PUT", "/cnt/blk?comp=block&blockid=YWFh", &[], "zz").await;
    let r = e.b("PUT", "/cnt/blk?comp=blocklist", &[], "<BlockList><Latest>YWFh</Latest></BlockList>").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let evs = e.take();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].data["api"], "PutBlockList");
    assert_eq!(evs[0].data["contentLength"], 2);
}

#[tokio::test]
async fn data_lake_events() {
    let e = start(true).await;
    assert_eq!(e.d("PUT", "/files?resource=filesystem", &[], "").await.status, 201);
    let r = e.d("PUT", "/files/dir?resource=directory", &[], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(e.d("PUT", "/files/dir/f.txt?resource=file", &[], "").await.status, 201);
    assert_eq!(e.d("PATCH", "/files/dir/f.txt?action=append&position=0", &[], "abc").await.status, 202);
    assert_eq!(e.d("PATCH", "/files/dir/f.txt?action=flush&position=3", &[], "").await.status, 200); // no close: silent
    assert_eq!(e.d("PATCH", "/files/dir/f.txt?action=flush&position=3&close=true", &[], "").await.status, 200);
    let evs = e.take();
    let types: Vec<(&str, &str, &str)> =
        evs.iter().map(|v| (v.event_type.as_str(), v.data["api"].as_str().unwrap(), v.data_version.as_str())).collect();
    assert_eq!(
        types,
        vec![
            ("Microsoft.Storage.DirectoryCreated", "CreateDirectory", "1"),
            ("Microsoft.Storage.BlobCreated", "CreateFile", "2"),
            ("Microsoft.Storage.BlobCreated", "FlushWithClose", "2"),
        ]
    );
    assert!(evs[1].data["url"].as_str().unwrap().contains(".dfs."));
    assert_eq!(evs[2].data["contentOffset"], 3);

    let r = e.d("PUT", "/files/dir2", &[("x-ms-rename-source", "/files/dir")], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = e.d("DELETE", "/files/dir2?recursive=true", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let evs = e.take();
    assert_eq!(evs.len(), 2);
    assert_eq!(
        (evs[0].event_type.as_str(), evs[0].data["api"].as_str().unwrap()),
        ("Microsoft.Storage.DirectoryRenamed", "RenameDirectory")
    );
    assert!(evs[0].data["sourceUrl"].as_str().unwrap().ends_with("/files/dir"));
    assert!(evs[0].data["sourceBlobUrl"].as_str().unwrap().contains(".blob."));
    assert_eq!(evs[0].subject, "/blobServices/default/containers/files/blobs/dir2");
    assert_eq!((evs[1].event_type.as_str(), evs[1].data["recursive"].as_str()), ("Microsoft.Storage.DirectoryDeleted", Some("true")));
}
