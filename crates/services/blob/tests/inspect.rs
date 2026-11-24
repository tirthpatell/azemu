//! The inspector of Blob Storage (`/_azemu/inspect/blob/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;

async fn insp(e: &Env, p: String) -> azemu_core::testing::TestResponse {
    e.srv.send("GET", &format!("{}/_azemu/inspect{p}", e.srv.gateway_url()), &[], "").await
}

async fn fill(e: &Env) {
    e.create_container("logs").await;
    e.create_container("docs").await;
    e.send("PUT", "/docs?restype=container&comp=metadata", &[("x-ms-meta-owner", "team")], "").await;
    for name in ["a.txt", "dir/b.txt", "dir/sub/c.txt", "dir/sub/d.txt", "name with space.txt", "caf\u{e9}.txt"] {
        let r = e.put_blob("docs", &azemu_core::inspect::seg(name), &format!("content of {name}")).await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    // a binary blob, a snapshot, tags, metadata, a lease, an uncommitted block
    let r = e
        .send(
            "PUT",
            "/docs/bin.dat",
            &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-kind", "binary"), ("Content-Type", "application/octet-stream")],
            vec![0u8, 1, 2, 255, 254],
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("PUT", "/docs/a.txt?comp=snapshot", &[], "").await.status, 201);
    let tags = "<Tags><TagSet><Tag><Key>env</Key><Value>test</Value></Tag></TagSet></Tags>";
    assert_eq!(e.send("PUT", "/docs/a.txt?comp=tags", &[], tags).await.status, 204);
    let r = e.send("PUT", "/docs/a.txt?comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("PUT", "/docs/staged?comp=block&blockid=QUFBQQ%3D%3D", &[], "pending").await.status, 201);
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let e = start_versioned().await;
    fill(&e).await;
    // overwrite one so that a version exists
    e.put_blob("docs", "a.txt", "second").await;
    for limit in [2, 50] {
        let nodes = walk(&e.srv, "/blob", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in ["service", "account", "containers", "container", "blobs", "blob", "snapshots", "versions"] {
            assert!(kinds.contains(&k), "no {k} node among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn a_blob_node_shows_properties_and_the_first_bytes() {
    let e = start().await;
    fill(&e).await;
    let get = |p: String| insp(&e, p);
    let r = get("/blob/acct/containers/docs/blobs/a.txt".into()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["kind"], "blob");
    assert_eq!(v["properties"]["size"], 16);
    assert_eq!(v["properties"]["tags"]["env"], "test");
    assert_eq!(v["properties"]["lease"]["state"], "leased");
    assert!(v["content"].is_null(), "content only when asked for");
    let r = get("/blob/acct/containers/docs/blobs/a.txt?content=1".into()).await;
    assert_eq!(r.json()["content"]["text"], "content of a.txt");
    let r = get("/blob/acct/containers/docs/blobs/bin.dat?content=1".into()).await;
    assert_eq!(r.json()["content"]["base64"], "AAEC//4=");
    assert_eq!(r.json()["properties"]["metadata"]["kind"], "binary");
    let r = get("/blob/acct/containers/docs/blobs/dir%2Fb.txt".into()).await;
    assert_eq!(r.status, 200);
    let r = get(format!("/blob/acct/containers/docs/blobs/{}", azemu_core::inspect::seg("name with space.txt"))).await;
    assert_eq!(r.status, 200);
    // the block list: the staged block that was never committed
    let r = get("/blob/acct/containers/docs/blobs/staged/blocks".into()).await;
    assert_eq!(r.json()["items"][0]["committed"], false);
}

#[tokio::test]
async fn a_long_blob_is_cut_at_64_kib_and_says_so() {
    let e = start().await;
    e.create_container("big").await;
    let body = "x".repeat(100_000);
    assert_eq!(e.put_blob("big", "long.txt", &body).await.status, 201);
    let r = e
        .srv
        .send("GET", &format!("{}/_azemu/inspect/blob/acct/containers/big/blobs/long.txt?content=1", e.srv.gateway_url()), &[], "")
        .await;
    let c = &r.json()["content"];
    assert_eq!(c["size"], 100_000);
    assert_eq!(c["shown"], 65_536);
    assert_eq!(c["truncated"], true);
    assert_eq!(c["text"].as_str().unwrap().len(), 65_536);
}

#[tokio::test]
async fn the_delimiter_view_groups_names_into_prefixes_and_pages_through_them() {
    let e = start().await;
    fill(&e).await;
    let base = format!("{}/_azemu/inspect/blob/acct/containers/docs/blobs", e.srv.gateway_url());
    let r = e.srv.send("GET", &format!("{base}?delimiter=%2F"), &[], "").await;
    let rows = r.json()["items"].as_array().unwrap().clone();
    let names: Vec<(&str, &str)> = rows.iter().map(|x| (x["name"].as_str().unwrap(), x["type"].as_str().unwrap_or("blob"))).collect();
    assert!(names.contains(&("dir/", "prefix")), "{names:?}");
    assert!(names.contains(&("a.txt", "BlockBlob")));
    assert_eq!(names.iter().filter(|(n, _)| n.starts_with("dir/")).count(), 1, "one row for the whole directory");
    // inside the directory
    let r = e.srv.send("GET", &format!("{base}?prefix=dir%2F&delimiter=%2F"), &[], "").await;
    let names: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["dir/b.txt", "dir/sub/"]);
    // one row per page still reaches every row, and none twice
    let mut all = Vec::new();
    let mut marker = String::new();
    for _ in 0..20 {
        let r = e.srv.send("GET", &format!("{base}?delimiter=%2F&limit=2{marker}"), &[], "").await;
        let v = r.json();
        all.extend(v["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()));
        match v["next"].as_str() {
            Some(m) => marker = format!("&marker={m}"),
            None => break,
        }
    }
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(all.len(), sorted.len(), "a row came twice: {all:?}");
    assert_eq!(all.len(), 6, "{all:?}");
}

#[tokio::test]
async fn looking_changes_nothing_and_a_missing_account_is_not_made() {
    let e = start().await;
    fill(&e).await;
    let list = || async { e.send("GET", "/docs?restype=container&comp=list&include=snapshots,metadata", &[], "").await.text() };
    let props = || async { e.send("HEAD", "/docs/a.txt", &[], "").await };
    let before = (list().await, props().await);
    walk(&e.srv, "", 3).await;
    walk(&e.srv, "/blob", 1).await;
    let after = (list().await, props().await);
    assert_eq!(before.0, after.0);
    for h in ["etag", "last-modified", "x-ms-lease-state", "x-ms-lease-status"] {
        assert_eq!(before.1.header(h), after.1.header(h), "{h}");
    }
    let base = e.srv.gateway_url();
    for p in ["/blob/ghost", "/blob/ghost/containers", "/blob/acct/containers/ghost", "/blob/acct/containers/docs/blobs/ghost"] {
        let r = e.srv.send("GET", &format!("{base}/_azemu/inspect{p}"), &[], "").await;
        assert_eq!(r.status, 404, "{p}");
    }
    let r = e.srv.send("GET", &format!("{base}/_azemu/inspect/blob"), &[], "").await;
    assert_eq!(r.json()["properties"]["accounts"], 1, "looking at a missing account made one");
}
