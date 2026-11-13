//! Limits on request input: XML depth and size, and (later in this file) old versions.
mod common;
use common::*;

/// A body nested 100,000 deep must be answered with `InvalidXmlDocument`, not overflow the stack
/// of the process. Every route that parses a request body gets the same limit.
#[tokio::test]
async fn deeply_nested_xml_is_refused_on_every_route() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    let n = 100_000;
    let deep = format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    let routes = [
        ("PUT", "/cnt/b?comp=blocklist"),
        ("PUT", "/cnt/b?comp=tags"),
        ("PUT", "/?restype=service&comp=properties"),
        ("PUT", "/cnt?restype=container&comp=acl"),
    ];
    for (m, path) in routes {
        let r = e.send(m, path, &[], deep.clone()).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"), "{m} {path}: {}", r.text());
    }
    // The service still answers.
    let r = e.send("GET", "/cnt/b", &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn xml_over_the_size_limit_is_refused() {
    let e = start().await;
    e.create_container("cnt").await;
    let body = format!("<BlockList><Latest>{}</Latest></BlockList>", "A".repeat(17 * 1024 * 1024));
    let r = e.send("PUT", "/cnt/b?comp=blocklist", &[], body).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"));
}

// ---------------------------------------------------------------------------
// Old versions: the byte limit and the per-account switch
// ---------------------------------------------------------------------------

use azemu_blob::BlobService;

/// `(version id, is current)` of every listed version of `name`, oldest first as listed.
async fn versions(e: &Env, container: &str, name: &str) -> Vec<(String, bool)> {
    let r = e.send("GET", &format!("/{container}?restype=container&comp=list&include=versions&prefix={name}"), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let xml = r.text();
    let mut out = vec![];
    for blob in xml.split("<Blob>").skip(1) {
        let id = tags(blob, "VersionId").into_iter().next().unwrap_or_default();
        out.push((id, blob.contains("<IsCurrentVersion>true</IsCurrentVersion>")));
    }
    out
}

async fn put(e: &Env, path: &str, body: Vec<u8>) -> azemu_core::testing::TestResponse {
    let r = e.send("PUT", path, &[("x-ms-blob-type", "BlockBlob")], body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    r
}

#[tokio::test]
async fn old_versions_stop_at_the_byte_limit_and_the_oldest_go_first() {
    let e = start_with(BlobService::with_versioning(true, 1000)).await;
    e.create_container("cnt").await;
    let mut ids = vec![];
    for i in 0..10u8 {
        let r = put(&e, "/cnt/b", vec![i; 600]).await;
        ids.push(r.header("x-ms-version-id").unwrap().to_string());
    }
    // 600 bytes each: one old version fits under 1000 bytes, two do not.
    let v = versions(&e, "cnt", "b").await;
    assert_eq!(v.len(), 2, "{v:?}");
    assert_eq!(v[0].0, ids[8], "the newest old version is the one kept");
    assert_eq!(v[1], (ids[9].clone(), true));
    // The dropped ones are gone, the kept one reads back.
    let r = e.send("GET", &format!("/cnt/b?versionid={}", ids[0]), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "BlobNotFound"));
    let r = e.send("GET", &format!("/cnt/b?versionid={}", ids[8]), &[], "").await;
    assert_eq!((r.status, r.body.as_ref()), (200, &[8u8; 600][..]));
    let r = e.send("GET", "/cnt/b", &[], "").await;
    assert_eq!(r.body.as_ref(), &[9u8; 600][..]);
}

#[tokio::test]
async fn a_buffer_shared_by_many_versions_is_counted_once() {
    let e = start_with(BlobService::with_versioning(true, 1000)).await;
    e.create_container("cnt").await;
    put(&e, "/cnt/b", vec![7; 600]).await;
    // Set Blob Metadata makes a version each time; they all share the one 600-byte buffer.
    for i in 0..10 {
        let r = e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-meta-n", &i.to_string())], "").await;
        assert_eq!(r.status, 200);
    }
    assert_eq!(versions(&e, "cnt", "b").await.len(), 11, "10 old versions and the current one");
}

#[tokio::test]
async fn a_limit_of_zero_keeps_no_old_versions() {
    let e = start_with(BlobService::with_versioning(true, 0)).await;
    e.create_container("cnt").await;
    let a = put(&e, "/cnt/b", b"one".to_vec()).await.header("x-ms-version-id").unwrap().to_string();
    let b = put(&e, "/cnt/b", b"two".to_vec()).await.header("x-ms-version-id").unwrap().to_string();
    assert_ne!(a, b, "versions still get ids");
    assert_eq!(versions(&e, "cnt", "b").await, vec![(b, true)]);
    let r = e.send("GET", &format!("/cnt/b?versionid={a}"), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "BlobNotFound"));
}

#[tokio::test]
async fn versioning_off_behaves_like_an_account_without_versioning() {
    let e = start_with(BlobService::with_versioning(false, 256 << 20)).await;
    e.create_container("cnt").await;
    let r = put(&e, "/cnt/b", b"one".to_vec()).await;
    assert_eq!(r.header("x-ms-version-id"), None);
    let r = put(&e, "/cnt/b", b"two".to_vec()).await;
    assert_eq!(r.header("x-ms-version-id"), None);
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-meta-a", "b")], "").await;
    assert_eq!((r.status, r.header("x-ms-version-id")), (200, None));
    let r = e.send("PUT", "/cnt/b?comp=snapshot", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-version-id")), (201, None));
    assert!(r.header("x-ms-snapshot").is_some());
    let r = e.send("HEAD", "/cnt/b", &[], "").await;
    assert_eq!((r.header("x-ms-version-id"), r.header("x-ms-is-current-version")), (None, None));
    // A listing with versions shows the blob once, with no version id.
    let v = versions(&e, "cnt", "b").await;
    assert_eq!(v, vec![(String::new(), false)]);
    // A version id that was never issued is not found.
    for (m, p) in [("GET", "/cnt/b"), ("HEAD", "/cnt/b"), ("GET", "/cnt/b?comp=tags"), ("DELETE", "/cnt/b")] {
        let sep = if p.contains('?') { '&' } else { '?' };
        let r = e.send(m, &format!("{p}{sep}versionid=2026-01-01T00:00:00.0000000Z"), &[], "").await;
        assert_eq!(r.status, 404, "{m} {p}");
        if m != "HEAD" {
            assert_eq!(code(&r), "BlobNotFound", "{m} {p}");
        }
    }
    // Deleting the blob keeps nothing behind.
    let r = e.send("DELETE", "/cnt/b?", &[("x-ms-delete-snapshots", "include")], "").await;
    assert_eq!(r.status, 202);
    assert!(versions(&e, "cnt", "b").await.is_empty());
}

#[tokio::test]
async fn versioning_can_be_switched_for_one_account() {
    let svc = std::sync::Arc::new(BlobService::new());
    let srv = azemu_core::testing::TestServer::start(vec![svc.clone()]).await;
    let h = [AUTH];
    let on = srv.url("blob", "on");
    let off = srv.url("blob", "off");
    // off by default, as on Azure; ARM (or a test) switches it on per account
    assert!(!svc.versioning_enabled("on") && !svc.versioning_enabled("off"));
    svc.set_versioning_enabled("on", true);
    svc.set_versioning_enabled("off", false);
    assert!(svc.versioning_enabled("on") && !svc.versioning_enabled("off"));
    for base in [&on, &off] {
        let r = srv.send("PUT", &format!("{base}/cnt?restype=container"), &h, "").await;
        assert_eq!(r.status, 201);
    }
    let blob = [AUTH, ("x-ms-blob-type", "BlockBlob")];
    let r = srv.send("PUT", &format!("{on}/cnt/b"), &blob, "x").await;
    let first = r.header("x-ms-version-id").unwrap().to_string();
    let r = srv.send("PUT", &format!("{off}/cnt/b"), &blob, "x").await;
    assert_eq!(r.header("x-ms-version-id"), None);

    // Turning it off keeps the versions that exist and they can still be read by id.
    srv.send("PUT", &format!("{on}/cnt/b"), &blob, "y").await;
    svc.set_versioning_enabled("on", false);
    let r = srv.send("PUT", &format!("{on}/cnt/b"), &blob, "z").await;
    assert_eq!(r.header("x-ms-version-id"), None);
    let r = srv.send("GET", &format!("{on}/cnt/b?versionid={first}"), &h, "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "x"));
    // Turning it on again: new writes get ids.
    svc.set_versioning_enabled("on", true);
    let r = srv.send("PUT", &format!("{on}/cnt/b"), &blob, "w").await;
    assert!(r.header("x-ms-version-id").is_some());
}

// ---------------------------------------------------------------------------
// Numbers in a request that used to overflow (a panic in a debug build, a 500 answer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn huge_numbers_in_requests_are_refused_not_a_crash() {
    let svc = BlobService::new();
    let dfs = svc.datalake();
    let srv = azemu_core::testing::TestServer::start(vec![std::sync::Arc::new(svc), std::sync::Arc::new(dfs)]).await;
    let (b, d) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    let h = [AUTH];
    let send = |m: &'static str, url: String, extra: Vec<(&'static str, String)>, body: &'static str| {
        let srv = &srv;
        async move {
            let mut hh: Vec<(&str, &str)> = h.to_vec();
            for (k, v) in &extra {
                hh.push((k, v));
            }
            srv.send(m, &url, &hh, body).await
        }
    };

    send("PUT", format!("{b}/cnt?restype=container"), vec![], "").await;
    send("PUT", format!("{b}/cnt/src"), vec![("x-ms-blob-type", "BlockBlob".into())], "hello").await;
    send("PUT", format!("{b}/cnt/p"), vec![("x-ms-blob-type", "PageBlob".into()), ("x-ms-blob-content-length", "1024".into())], "").await;
    // Put Page From URL with an open-ended source range: the length was worked out as u64::MAX + 1.
    let r = send(
        "PUT",
        format!("{b}/cnt/p?comp=page"),
        vec![
            ("x-ms-page-write", "update".into()),
            ("x-ms-range", "bytes=0-511".into()),
            ("x-ms-source-range", "bytes=0-".into()),
            ("x-ms-copy-source", format!("{b}/cnt/src")),
        ],
        "",
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());

    // Data Lake: a position at the end of the range of u64 plus a body.
    assert_eq!(send("PUT", format!("{d}/myfs?resource=filesystem"), vec![], "").await.status, 201);
    assert_eq!(send("PUT", format!("{d}/myfs/f?resource=file"), vec![], "").await.status, 201);
    let r = send("PATCH", format!("{d}/myfs/f?action=append&position=18446744073709551615"), vec![], "xx").await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = send("PATCH", format!("{d}/myfs/f?action=append&position=18446744073709551615&flush=true"), vec![], "xx").await;
    assert_eq!(r.status, 400, "{}", r.text());
    // An expiry too far away to be a date.
    let r = send(
        "PUT",
        format!("{d}/myfs/g?resource=file"),
        vec![("x-ms-expiry-option", "RelativeToNow".into()), ("x-ms-expiry-time", "9223372036854775807".into())],
        "",
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());
}
