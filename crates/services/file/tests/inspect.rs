//! The inspector of Azure Files (`/_azemu/inspect/file/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;

async fn fill(f: &F) {
    f.share("docs").await;
    f.share("media").await;
    f.dir("docs/reports").await;
    f.dir("docs/reports/2026").await;
    f.file("docs/readme.txt", 11).await;
    f.write("docs/readme.txt", 0, "hello world").await;
    f.file("docs/reports/q1.csv", 5).await;
    f.write("docs/reports/q1.csv", 0, "a,b,c").await;
    f.file("docs/reports/2026/Jan%20Report.txt", 3).await;
    f.write("docs/reports/2026/Jan%20Report.txt", 0, "abc").await;
    assert_eq!(f.send("PUT", "/docs?restype=share&comp=snapshot", &[], "").await.status, 201);
    assert_eq!(f.send("PUT", "/docs?restype=share&comp=metadata", &[("x-ms-meta-team", "files")], "").await.status, 200);
    assert_eq!(
        f.send("PUT", "/media?restype=share&comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")], "")
            .await
            .status,
        201
    );
}

async fn insp(f: &F, p: &str) -> azemu_core::testing::TestResponse {
    f.srv.send("GET", &format!("{}/_azemu/inspect{p}", f.srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let f = start().await;
    fill(&f).await;
    for limit in [1, 2, 50] {
        let nodes = walk(&f.srv, "/file", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in ["service", "account", "shares", "share", "directory", "file", "snapshots", "snapshot"] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
}

#[tokio::test]
async fn directories_files_and_content_are_shown() {
    let f = start().await;
    fill(&f).await;
    let share = insp(&f, "/file/acct1/shares/docs").await.json();
    assert_eq!(share["properties"]["metadata"]["team"], "files");
    assert_eq!(share["properties"]["quotaGiB"], 102_400);
    let root = insp(&f, "/file/acct1/shares/docs/files").await.json();
    let names: Vec<&str> = root["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["readme.txt", "reports"]);
    let file = insp(&f, "/file/acct1/shares/docs/files/readme.txt?content=1").await.json();
    assert_eq!(file["properties"]["size"], 11);
    assert_eq!(file["content"]["text"], "hello world");
    let deep = insp(&f, "/file/acct1/shares/docs/files/reports/2026/Jan%20Report.txt?content=1").await.json();
    assert_eq!(deep["content"]["text"], "abc");
    assert!(insp(&f, "/file/acct1/shares/docs/files/readme.txt").await.json().get("content").is_none());
    let media = insp(&f, "/file/acct1/shares/media").await.json();
    assert_eq!(media["properties"]["lease"]["state"], "leased");
    // the snapshot has the tree as it was
    let snaps = insp(&f, "/file/acct1/shares/docs/snapshots").await.json();
    let path = snaps["items"][0]["path"].as_str().unwrap().to_string();
    let inside = insp(&f, &format!("{path}/files/readme.txt?content=1")).await.json();
    assert_eq!(inside["content"]["text"], "hello world");
}

#[tokio::test]
async fn looking_changes_nothing_and_a_missing_account_is_not_made() {
    let f = start().await;
    fill(&f).await;
    let view = || async {
        let mut v = vec![f.send("GET", "/?comp=list&include=snapshots,metadata", &[], "").await.text()];
        for p in ["/docs/readme.txt", "/docs/reports?restype=directory", "/media?restype=share"] {
            let r = f.send("HEAD", p, &[], "").await;
            v.push(format!("{:?} {:?} {:?}", r.header("etag"), r.header("last-modified"), r.header("x-ms-lease-state")));
        }
        v
    };
    let before = view().await;
    walk(&f.srv, "", 2).await;
    assert_eq!(before, view().await);
    for p in [
        "/file/ghost",
        "/file/acct1/shares/none",
        "/file/acct1/shares/docs/files/nope.txt",
        "/file/acct1/shares/docs/files/readme.txt/inside",
        "/file/acct1/shares/docs/snapshots/zzz",
    ] {
        assert_eq!(insp(&f, p).await.status, 404, "{p}");
    }
    assert_eq!(insp(&f, "/file").await.json()["properties"]["accounts"], 1);
}
