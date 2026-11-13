//! What the Blob service asks of core: abort on disconnect only for reads and queries, and the names of
//! its 501 answers.

mod common;

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::http::Method;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use common::*;

#[test]
fn only_reads_and_queries_are_dropped_when_the_client_goes() {
    let svc = BlobService::new();
    let lake = svc.datalake();
    let ask = |s: &dyn Service, m: Method, p: &str| s.abort_on_disconnect(&m, p);
    // downloads and listings
    assert!(ask(&svc, Method::GET, "/cnt/blob"));
    assert!(ask(&svc, Method::GET, "/cnt"));
    // a query is a POST on a blob; the batch requests (account, container) have one segment at most
    assert!(ask(&svc, Method::POST, "/cnt/blob"));
    assert!(ask(&svc, Method::POST, "/cnt/dir/blob"));
    assert!(!ask(&svc, Method::POST, "/cnt"));
    assert!(!ask(&svc, Method::POST, "/"));
    // everything that changes state runs to its end
    for m in [Method::PUT, Method::DELETE, Method::HEAD, Method::OPTIONS] {
        assert!(!ask(&svc, m.clone(), "/cnt/blob"), "{m}");
        assert!(!ask(&svc, m.clone(), "/cnt"), "{m}");
    }
    // the Data Lake service: reads only (a POST there is a query or a lease)
    assert!(ask(&lake, Method::GET, "/fs/path"));
    assert!(!ask(&lake, Method::POST, "/fs/path"));
    assert!(!ask(&lake, Method::PUT, "/fs/path"));
    assert!(!ask(&lake, Method::PATCH, "/fs/path"));
}

#[tokio::test]
async fn unbuilt_operations_are_named_in_the_unsupported_list() {
    let svc = Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)]).await;
    let (blob, dfs) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    let a = [AUTH];
    assert_eq!(srv.send("PUT", &format!("{blob}/cnt?restype=container"), &a, "").await.status, 201);
    assert_eq!(srv.send("PUT", &format!("{blob}/cnt/b.csv"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], "a,b\n").await.status, 201);
    // Parquet input
    let q = "<QueryRequest><QueryType>SQL</QueryType><Expression>SELECT * FROM BlobStorage</Expression><InputSerialization><Format><Type>parquet</Type></Format></InputSerialization></QueryRequest>";
    let r = srv.send("POST", &format!("{blob}/cnt/b.csv?comp=query"), &a, q).await;
    assert_eq!(r.status, 501, "{}", r.text());
    // an operation that is not built (Rename Container) and a request nothing matches
    let r = srv.send("PUT", &format!("{blob}/cnt?restype=container&comp=rename"), &[AUTH, ("x-ms-source-container-name", "cnt")], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    let r = srv.send("GET", &format!("{blob}/cnt/b.csv?comp=nothing-like-this"), &a, "").await;
    let _ = r;
    let r = srv.send("PATCH", &format!("{dfs}/fs/p?action=nothing-like-this"), &a, "").await;
    assert_eq!(r.status, 501, "{}", r.text());

    let list = srv.send("GET", &format!("{}/_azemu/unsupported", srv.gateway_url()), &[], "").await;
    assert_eq!(list.status, 200);
    let text = list.text();
    for name in ["Blob_Query (Parquet input)", "Blob_RenameContainer", "DataLake_Unknown (PATCH action=nothing-like-this)"] {
        assert!(text.contains(name), "{name} is not in {text}");
    }
}
