//! Data Lake: ACL limits, path rules, and the overwrite protections shared with the Blob endpoint.

use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_blob::BlobService;
use azemu_core::testing::{TestResponse, TestServer};

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

struct Lake {
    srv: TestServer,
    dfs: String,
    blob: String,
}

async fn lake() -> Lake {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    let (dfs, blob) = (srv.url("dfs", "acct"), srv.url("blob", "acct"));
    let l = Lake { srv, dfs, blob };
    assert_eq!(l.d("PUT", "/lake?resource=filesystem", &[], "").await.status, 201);
    l
}

impl Lake {
    async fn d(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.dfs), &h, body).await
    }

    async fn b(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.blob), &h, body).await
    }

    async fn file(&self, path: &str, data: &str) {
        let r = self.d("PUT", &format!("/lake/{path}?resource=file"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
        if !data.is_empty() {
            assert_eq!(self.d("PATCH", &format!("/lake/{path}?action=append&position=0"), &[], data.to_string()).await.status, 202);
        }
        assert_eq!(self.d("PATCH", &format!("/lake/{path}?action=flush&position={}", data.len()), &[], "").await.status, 200);
    }
}

fn code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

fn named(n: usize, default: bool) -> String {
    (0..n).map(|i| format!("{}user:{i:032x}:rwx", if default { "default:" } else { "" })).collect::<Vec<_>>().join(",")
}

// ---- ACL limits and sharing ----

#[tokio::test]
async fn more_than_32_acl_entries_are_refused_on_every_way_in() {
    let l = lake().await;
    let base = "user::rwx,group::r-x,other::---";
    // 29 named entries and the three base ones are 32; the mask is added, so 28 are the most that fit with it
    let too_many = format!("{base},{}", named(33, false));
    let r = l.d("PUT", "/lake/d?resource=directory", &[("x-ms-acl", &too_many)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"), "{}", r.text());
    assert!(r.text().contains("32"), "{}", r.text());
    assert_eq!(l.d("PUT", "/lake/d?resource=directory", &[], "").await.status, 201);
    for mode in ["set", "modify"] {
        let r = l.d("PATCH", &format!("/lake/d?action=setAccessControlRecursive&mode={mode}"), &[("x-ms-acl", &too_many)], "").await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"), "{mode}");
    }
    let r = l.d("PATCH", "/lake/d?action=setAccessControl", &[("x-ms-acl", &too_many)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    // default entries are counted apart: 28 access and 28 default named entries are fine
    let both = format!("{base},{},{}", named(28, false), named(28, true));
    let r = l.d("PATCH", "/lake/d?action=setAccessControl", &[("x-ms-acl", &both)], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = l.d("HEAD", "/lake/d?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-acl").unwrap().split(',').count(), 28 + 28 + 3 + 1 + 3 + 1);
}

#[tokio::test]
async fn a_6000_entry_default_acl_is_refused_before_it_costs_anything() {
    let l = lake().await;
    let huge = format!("user::rwx,group::r-x,other::---,{}", named(6000, true));
    let t = Instant::now();
    let r = l.d("PUT", "/lake/d?resource=directory", &[("x-ms-acl", &huge)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    assert!(r.body.len() < 2000, "the answer does not repeat the header ({} bytes)", r.body.len());
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[tokio::test]
async fn a_full_default_acl_is_inherited_by_many_files_and_a_deep_path_quickly() {
    let l = lake().await;
    let acl = format!("user::rwx,group::r-x,other::---,default:user::rwx,default:group::r-x,default:other::r--,{}", named(25, true));
    let r = l.d("PUT", "/lake/top?resource=directory", &[("x-ms-acl", &acl)], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let t = Instant::now();
    for i in 0..300 {
        let r = l.d("PUT", &format!("/lake/top/f{i}?resource=file"), &[], "").await;
        assert_eq!(r.status, 201);
    }
    // a deep path below it (the most a path can have is 61 segments)
    let deep: String = (0..58).map(|i| format!("l{i}")).collect::<Vec<_>>().join("/");
    let r = l.d("PUT", &format!("/lake/top/{deep}/leaf?resource=file"), &[], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let r = l.d("HEAD", "/lake/top/f7?action=getAccessControl", &[], "").await;
    let acl = r.header("x-ms-acl").unwrap();
    assert!(acl.contains("user:00000000000000000000000000000003:rwx") && !acl.contains("default:"), "{acl}");
}

#[tokio::test]
async fn a_recursive_change_that_would_pass_the_limit_is_reported_per_path() {
    let l = lake().await;
    let base = "user::rwx,group::r-x,other::---";
    assert_eq!(l.d("PUT", "/lake/d?resource=directory", &[("x-ms-acl", &format!("{base},{}", named(27, false)))], "").await.status, 201);
    l.file("d/small", "x").await;
    // d has 27 named + 3 base + mask = 31 entries; 5 more would make 36. The file below has the plain 3.
    let r = l
        .d(
            "PATCH",
            "/lake/d?action=setAccessControlRecursive&mode=modify",
            &[("x-ms-acl", &format!("user:{:032x}:r--,user:{:032x}:r--,user:{:032x}:r--,user:{:032x}:r--", 900, 901, 902, 903))],
            "",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["failureCount"], 1, "{v}");
    assert_eq!(v["failedEntries"][0]["name"], "d");
    assert_eq!(v["failedEntries"][0]["type"], "DIRECTORY");
    assert_eq!(v["filesSuccessful"], 1);
    let acl = l.d("HEAD", "/lake/d/small?action=getAccessControl", &[], "").await;
    assert!(acl.header("x-ms-acl").unwrap().contains(&format!("user:{:032x}:r--", 900)), "the file that could take the change has it");
    let dir = l.d("HEAD", "/lake/d?action=getAccessControl", &[], "").await;
    assert!(!dir.header("x-ms-acl").unwrap().contains(&format!("{:032x}", 900)), "the directory is as it was");
}

// ---- Create and rename obey legal hold, immutability, leases and conditions ----

impl Lake {
    async fn hold(&self, path: &str) {
        let r = self.b("PUT", &format!("/lake/{path}?comp=legalhold"), &[("x-ms-legal-hold", "true")], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    async fn body_of(&self, path: &str) -> String {
        self.b("GET", &format!("/lake/{path}"), &[], "").await.text()
    }

    async fn rename(&self, from: &str, to: &str, extra: &[(&str, &str)]) -> TestResponse {
        let src = format!("/lake/{from}");
        let mut h = vec![("x-ms-rename-source", src.as_str())];
        h.extend_from_slice(extra);
        self.d("PUT", &format!("/lake/{to}"), &h, "").await
    }
}

#[tokio::test]
async fn create_and_rename_do_not_replace_or_move_a_file_under_legal_hold() {
    let l = lake().await;
    l.file("held", "secret").await;
    l.hold("held").await;
    // the Blob endpoint says no; so does Data Lake now
    let r = l.b("PUT", "/lake/held", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToLegalHold"));
    let r = l.d("PUT", "/lake/held?resource=file", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToLegalHold"), "{}", r.text());
    assert_eq!(l.body_of("held").await, "secret");
    // another file renamed over it
    l.file("other", "other").await;
    let r = l.rename("other", "held", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToLegalHold"), "{}", r.text());
    assert_eq!(l.body_of("held").await, "secret");
    assert_eq!(l.body_of("other").await, "other", "the source is still there");
    // the file under hold renamed away, alone or inside a directory
    let r = l.rename("held", "moved", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToLegalHold"));
    assert_eq!(l.d("PUT", "/lake/dir?resource=directory", &[], "").await.status, 201);
    l.file("dir/inner", "in").await;
    l.hold("dir/inner").await;
    let r = l.rename("dir", "dir2", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToLegalHold"));
    assert_eq!(l.b("HEAD", "/lake/dir/inner", &[], "").await.status, 200);
    // without the hold everything works again
    let r = l.b("PUT", "/lake/held?comp=legalhold", &[("x-ms-legal-hold", "false")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(l.rename("other", "held", &[]).await.status, 201);
    assert_eq!(l.body_of("held").await, "other");
}

#[tokio::test]
async fn create_and_rename_obey_an_immutability_policy() {
    let l = lake().await;
    l.file("locked", "keep").await;
    let until = "Mon, 01 Jan 2099 00:00:00 GMT";
    let r = l
        .b(
            "PUT",
            "/lake/locked?comp=immutabilityPolicies",
            &[("x-ms-immutability-policy-until-date", until), ("x-ms-immutability-policy-mode", "unlocked")],
            "",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = l.d("PUT", "/lake/locked?resource=file", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToPolicy"), "{}", r.text());
    l.file("src", "new").await;
    let r = l.rename("src", "locked", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToPolicy"));
    let r = l.rename("locked", "gone", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobImmutableDueToPolicy"));
    assert_eq!(l.body_of("locked").await, "keep");
}

#[tokio::test]
async fn rename_over_a_leased_destination_needs_its_lease_id() {
    let l = lake().await;
    l.file("dst", "old").await;
    let r = l
        .b(
            "PUT",
            "/lake/dst?comp=lease",
            &[
                ("x-ms-lease-action", "acquire"),
                ("x-ms-lease-duration", "-1"),
                ("x-ms-proposed-lease-id", "11111111-1111-1111-1111-111111111111"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    l.file("src", "new").await;
    let r = l.rename("src", "dst", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"), "{}", r.text());
    let r = l.rename("src", "dst", &[("x-ms-lease-id", "22222222-2222-2222-2222-222222222222")]).await;
    assert_eq!(r.status, 412);
    assert_eq!(l.body_of("dst").await, "old");
    // Path - Create over it needs the id as well
    assert_eq!(l.d("PUT", "/lake/dst?resource=file", &[], "").await.status, 412);
    let r = l.rename("src", "dst", &[("x-ms-lease-id", "11111111-1111-1111-1111-111111111111")]).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(l.body_of("dst").await, "new");
}

#[tokio::test]
async fn rename_checks_the_conditions_of_both_sides_and_the_lease_of_the_source() {
    let l = lake().await;
    l.file("src", "s").await;
    l.file("dst", "d").await;
    let r = l.rename("src", "dst", &[("If-Match", "\"nope\"")]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    let r = l.rename("src", "dst", &[("x-ms-source-if-match", "\"nope\"")]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "SourceConditionNotMet"));
    let r = l.rename("src", "dst", &[("If-None-Match", "*")]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"));
    let r = l
        .b(
            "PUT",
            "/lake/src?comp=lease",
            &[
                ("x-ms-lease-action", "acquire"),
                ("x-ms-lease-duration", "-1"),
                ("x-ms-proposed-lease-id", "33333333-3333-3333-3333-333333333333"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    let r = l.rename("src", "dst", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"));
    let r = l.rename("src", "dst", &[("x-ms-source-lease-id", "33333333-3333-3333-3333-333333333333")]).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

// ---- One set of path rules for both endpoints ----

#[tokio::test]
async fn a_blob_cannot_be_made_below_a_file_or_in_place_of_a_directory() {
    let l = lake().await;
    l.file("a.txt", "data").await;
    assert_eq!(l.d("PUT", "/lake/dir?resource=directory", &[], "").await.status, 201);
    for (path, body) in [("a.txt/child", "x"), ("a.txt/x/y/z", "x"), ("dir", "x")] {
        let r = l.b("PUT", &format!("/lake/{path}"), &[("x-ms-blob-type", "BlockBlob")], body).await;
        assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"), "{path}: {}", r.text());
    }
    // staging a block and committing, and copying, follow the same rule
    let r = l.b("PUT", "/lake/a.txt/c?comp=block&blockid=MDAwMDA%3D", &[], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
    let r = l.b("PUT", "/lake/a.txt/c", &[("x-ms-copy-source", &format!("{}/lake/a.txt", l.blob))], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
    assert_eq!(l.d("GET", "/lake/a.txt/child", &[], "").await.status, 404);
    let list = l.d("GET", "/lake?resource=filesystem&recursive=true", &[], "").await.text();
    assert!(!list.contains("child"), "{list}");
    // below a directory, and a new top-level name, are fine, and show up as paths
    let r = l.b("PUT", "/lake/dir/ok.txt", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = l.b("PUT", "/lake/new/deeper/ok2.txt", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!(r.status, 201);
    assert_eq!(l.d("HEAD", "/lake/new/deeper", &[], "").await.header("x-ms-resource-type"), Some("directory"));
    // a flat container (never touched through dfs) keeps the old behaviour
    assert_eq!(l.b("PUT", "/flat?restype=container", &[], "").await.status, 201);
    assert_eq!(l.b("PUT", "/flat/a", &[("x-ms-blob-type", "BlockBlob")], "x").await.status, 201);
    assert_eq!(l.b("PUT", "/flat/a/b", &[("x-ms-blob-type", "BlockBlob")], "x").await.status, 201);
}

#[tokio::test]
async fn dot_segments_control_characters_and_invalid_utf8_are_refused_on_both_endpoints() {
    let l = lake().await;
    for name in ["a/%2E%2E/b", "%2E/x", "a/%2E", "a%00b", "a%0Ab", "bad%FFname", "bad%EF%BF%BDname"] {
        let r = l.d("PUT", &format!("/lake/{name}?resource=file"), &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"), "dfs {name}: {}", r.text());
        let r = l.b("PUT", &format!("/lake/{name}"), &[("x-ms-blob-type", "BlockBlob")], "x").await;
        // the Blob endpoint accepts dot segments in a flat container, but this one is hierarchical: same rule
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"), "blob {name}: {}", r.text());
    }
    // control characters and bad UTF-8 are refused in a flat container too
    assert_eq!(l.b("PUT", "/flat?restype=container", &[], "").await.status, 201);
    for name in ["a%00b", "bad%FFname"] {
        let r = l.b("PUT", &format!("/flat/{name}"), &[("x-ms-blob-type", "BlockBlob")], "x").await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"), "{name}");
    }
    // nothing was created: two different invalid names no longer become one
    let list = l.d("GET", "/lake?resource=filesystem&recursive=true", &[], "").await.text();
    assert!(!list.contains("bad") && !list.contains("\"name\""), "{list}");
}

#[tokio::test]
async fn path_depth_and_length_limits() {
    let l = lake().await;
    let ok: String = (0..60).map(|i| format!("d{i}")).collect::<Vec<_>>().join("/") + "/f.txt"; // 61 segments
    assert_eq!(l.d("PUT", &format!("/lake/{ok}?resource=file"), &[], "").await.status, 201);
    let too_deep: String = (0..61).map(|i| format!("e{i}")).collect::<Vec<_>>().join("/") + "/f.txt"; // 62 segments
    let r = l.d("PUT", &format!("/lake/{too_deep}?resource=file"), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"));
    let r = l.b("PUT", &format!("/lake/{too_deep}"), &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"));
    let long = "x".repeat(1025);
    let r = l.d("PUT", &format!("/lake/{long}?resource=file"), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"));
    assert_eq!(l.d("PUT", &format!("/lake/{}?resource=file", "x".repeat(1024)), &[], "").await.status, 201);
}

// ---- the undelete that could overwrite ----

#[tokio::test]
async fn undelete_never_replaces_a_live_path() {
    let l = lake().await;
    let props = "<StorageServiceProperties><DeleteRetentionPolicy><Enabled>true</Enabled><Days>3</Days></DeleteRetentionPolicy></StorageServiceProperties>";
    assert_eq!(l.b("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);
    l.file("one", "old").await;
    let r = l.d("DELETE", "/lake/one", &[], "").await;
    let id = r.header("x-ms-deletion-id").unwrap().to_string();
    // a new live file with the same name, then an undelete whose URL names a different (free) path
    l.file("one", "NEW").await;
    let r = l.b("PUT", "/lake/free?comp=undelete", &[("x-ms-undelete-source", &format!("one?deletionid={id}"))], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"), "{}", r.text());
    assert_eq!(l.body_of("one").await, "NEW");
    // the same URL as the deleted name is refused as before
    let r = l.b("PUT", "/lake/one?comp=undelete", &[("x-ms-undelete-source", &format!("one?deletionid={id}"))], "").await;
    assert_eq!(r.status, 409);
    assert_eq!(l.body_of("one").await, "NEW");
    // a directory whose child was made again: nothing comes back
    assert_eq!(l.d("PUT", "/lake/dir?resource=directory", &[], "").await.status, 201);
    l.file("dir/f", "old").await;
    let r = l.d("DELETE", "/lake/dir?recursive=true", &[], "").await;
    let did = r.header("x-ms-deletion-id").unwrap().to_string();
    assert_eq!(l.d("PUT", "/lake/dir/f?resource=file", &[], "").await.status, 201, "the parent is made by the create");
    let r = l.b("PUT", "/lake/dir?comp=undelete", &[("x-ms-undelete-source", &format!("dir?deletionid={did}"))], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"), "{}", r.text());
    assert_eq!(l.body_of("dir/f").await, "");
}
