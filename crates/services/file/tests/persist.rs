//! File shares across restarts (`--data-dir`): the whole tree with its content, leases, snapshots, deleted shares.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_file::FileService;
use common::*;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-file-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> F {
    let srv = TestServer::start_persistent(vec![Arc::new(FileService::new())], d).await;
    let base = srv.url("file.core", ACCOUNT);
    F { srv, base }
}

async fn try_open(d: &Path) -> anyhow::Result<F> {
    let srv = TestServer::try_start_persistent(vec![Arc::new(FileService::new())], d, |b| b).await?;
    let base = srv.url("file.core", ACCOUNT);
    Ok(F { srv, base })
}

fn norm(text: &str, f: &F) -> String {
    text.replace(&format!(":{}", f.srv.port()), ":PORT")
}

#[tokio::test]
async fn shares_trees_content_leases_snapshots_and_deleted_shares_survive() {
    let d = dir("rich");
    let f = open(&d).await;
    f.send("PUT", "/work?restype=share", &[("x-ms-meta-owner", "alice"), ("x-ms-share-quota", "9"), ("x-ms-access-tier", "Cool")], "")
        .await;
    f.dir("work/d1").await;
    f.dir("work/d1/sub").await;
    f.send(
        "PUT",
        "/work/d1/f.txt",
        &[
            ("x-ms-type", "file"),
            ("x-ms-content-length", "3000"),
            ("x-ms-content-type", "text/x"),
            ("x-ms-meta-m", "1"),
            ("x-ms-file-attributes", "ReadOnly|Archive"),
            ("x-ms-file-creation-time", "2020-01-02T03:04:05.0000000Z"),
        ],
        "",
    )
    .await;
    f.write("work/d1/f.txt", 0, "hello").await;
    f.write("work/d1/f.txt", 2000, "tail").await;
    let l = "11111111-1111-1111-1111-111111111111";
    f.send(
        "PUT",
        "/work/d1/f.txt?comp=lease",
        &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", l)],
        "",
    )
    .await;
    f.send(
        "PUT",
        "/work?restype=share&comp=lease",
        &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", l)],
        "",
    )
    .await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Permission>rl</Permission><Expiry>2099-01-01T00:00:00Z</Expiry></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    f.send("PUT", "/work?restype=share&comp=acl", &[("x-ms-lease-id", l)], acl).await;
    let snap = f
        .send("PUT", "/work?restype=share&comp=snapshot", &[("x-ms-meta-why", "t")], "")
        .await
        .header("x-ms-snapshot")
        .unwrap()
        .to_string();
    f.write_with_lease("work/d1/f.txt", 0, "HELLO", l).await;
    let key = f
        .send("PUT", "/work?restype=share&comp=filepermission", &[], "{\"permission\": \"O:S-1-5-21-1G:S-1-5-21-2D:(A;;FA;;;SY)\"}")
        .await
        .header("x-ms-file-permission-key")
        .unwrap()
        .to_string();
    let props = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET</AllowedMethods><AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>60</MaxAgeInSeconds></CorsRule></Cors></StorageServiceProperties>";
    f.send("PUT", "/?restype=service&comp=properties", &[], props).await;
    f.share("gone").await;
    f.send("DELETE", "/gone?restype=share", &[], "").await;

    let before_props = f.send("GET", "/work?restype=share", &[], "").await;
    let before_list = norm(
        &f.send("GET", "/work/d1?restype=directory&comp=list&include=Timestamps,ETag,Attributes,PermissionKey", &[], "").await.text(),
        &f,
    );
    let before_head = f.send("HEAD", "/work/d1/f.txt", &[], "").await;
    let before_shares = norm(&f.send("GET", "/?comp=list&include=metadata,snapshots,deleted", &[], "").await.text(), &f);
    let before_ranges = f.send("GET", "/work/d1/f.txt?comp=rangelist", &[], "").await.text();
    f.srv.shutdown().await.unwrap();

    let f = open(&d).await;
    let after = f.send("GET", "/work?restype=share", &[], "").await;
    for h in [
        "etag",
        "last-modified",
        "x-ms-share-quota",
        "x-ms-access-tier",
        "x-ms-meta-owner",
        "x-ms-lease-state",
        "x-ms-lease-duration",
        "x-ms-access-tier-change-time",
    ] {
        assert_eq!(after.header(h), before_props.header(h), "{h}");
    }
    assert_eq!(
        norm(
            &f.send("GET", "/work/d1?restype=directory&comp=list&include=Timestamps,ETag,Attributes,PermissionKey", &[], "").await.text(),
            &f
        ),
        before_list
    );
    let head = f.send("HEAD", "/work/d1/f.txt", &[], "").await;
    for h in [
        "etag",
        "content-length",
        "content-type",
        "x-ms-meta-m",
        "x-ms-file-attributes",
        "x-ms-file-creation-time",
        "x-ms-file-id",
        "x-ms-file-parent-id",
        "x-ms-lease-state",
        "x-ms-file-permission-key",
    ] {
        assert_eq!(head.header(h), before_head.header(h), "{h}");
    }
    assert_eq!(norm(&f.send("GET", "/?comp=list&include=metadata,snapshots,deleted", &[], "").await.text(), &f), before_shares);
    assert_eq!(f.send("GET", "/work/d1/f.txt?comp=rangelist", &[], "").await.text(), before_ranges);
    // the content, the snapshot's older content, the leases that still hold, the policies, the permissions, the CORS rules
    let text = f.read("work/d1/f.txt").await;
    assert!(text.starts_with("HELLO") && &text[2000..2004] == "tail" && text.len() == 3000);
    assert_eq!(f.send("GET", &format!("/work/d1/f.txt?sharesnapshot={snap}"), &[("x-ms-range", "bytes=0-4")], "").await.text(), "hello");
    assert_eq!(f.send("GET", &format!("/work?restype=share&sharesnapshot={snap}"), &[], "").await.header("x-ms-meta-why"), Some("t"));
    assert_eq!(failed(&f.send("PUT", "/work/d1/f.txt?comp=metadata", &[], "").await), (412, "LeaseIdMissing".into()));
    assert_eq!(failed(&f.send("DELETE", "/work?restype=share", &[], "").await), (412, "LeaseIdMissing".into()));
    assert!(f.send("GET", "/work?restype=share&comp=acl", &[("x-ms-lease-id", l)], "").await.text().contains("<Id>p1</Id>"));
    assert_eq!(f.send("GET", "/work?restype=share&comp=filepermission", &[("x-ms-file-permission-key", &key)], "").await.status, 200);
    assert!(f.send("GET", "/?restype=service&comp=properties", &[], "").await.text().contains("https://example.com"));
    // a deleted share can still be restored
    let t = f.send("GET", "/?comp=list&include=deleted", &[], "").await.text();
    let version = tag(&t, "Version").unwrap();
    assert_eq!(
        f.send(
            "PUT",
            "/gone?restype=share&comp=undelete",
            &[("x-ms-deleted-share-name", "gone"), ("x-ms-deleted-share-version", &version)],
            ""
        )
        .await
        .status,
        201
    );
    f.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_fixed_lease_runs_out_on_schedule_across_a_restart() {
    let d = dir("lease");
    let f = open(&d).await;
    f.share("work").await;
    f.send("PUT", "/work?restype=share&comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "15")], "").await;
    f.srv.shutdown().await.unwrap();
    let f = open(&d).await;
    assert_eq!(f.send("GET", "/work?restype=share", &[], "").await.header("x-ms-lease-duration"), Some("fixed"));
    f.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_reset_is_saved_and_unchanged_state_is_not_written_again() {
    let d = dir("reset");
    let svc = Arc::new(FileService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    let f = F { base: srv.url("file.core", ACCOUNT), srv };
    f.share("work").await;
    let state_dir = d.join("file");
    svc.save(&state_dir).unwrap();
    let file = state_dir.join("file.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    let g = svc.persist_generation();
    std::thread::sleep(std::time::Duration::from_millis(30));
    svc.save(&state_dir).unwrap();
    assert_eq!(
        (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()),
        first,
        "nothing written the second time"
    );
    // reads and refused requests are not changes
    f.send("GET", "/?comp=list", &[], "").await;
    f.send("GET", "/work?restype=share", &[], "").await;
    f.send("GET", "/work/nope", &[], "").await;
    f.send("PUT", "/work?restype=share", &[], "").await;
    f.send("PUT", "/work/f?comp=metadata", &[], "").await;
    assert_eq!(svc.persist_generation(), g);
    f.dir("work/d").await;
    assert_ne!(svc.persist_generation(), g);
    f.srv.reset();
    f.srv.save_now().await;
    drop(f);
    let f = open(&d).await;
    assert!(f.send("GET", "/?comp=list", &[], "").await.text().contains("<Shares />"));
    f.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = dir("damage");
    let f = open(&d).await;
    f.share("work").await;
    f.file("work/f", 600).await;
    f.write("work/f", 0, "some text").await;
    f.srv.shutdown().await.unwrap();
    let file = d.join("file").join("file.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(f) => {
                f.srv.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("file.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    std::fs::write(&file, persist::encode("blob", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds blob state, expected file"));
    std::fs::write(&file, persist::encode("file", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, &good).unwrap();
    let f = try_open(&d).await.expect("the good file opens");
    assert!(f.read("work/f").await.starts_with("some text"));
    f.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
