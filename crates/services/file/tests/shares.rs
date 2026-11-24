//! Shares: create, properties, quota and tier, metadata, access policies, statistics, snapshots, soft delete and the list.

mod common;
use common::*;

#[tokio::test]
async fn create_get_head_and_delete() {
    let f = start().await;
    let r =
        f.send("PUT", "/sh1?restype=share", &[("x-ms-meta-owner", "me"), ("x-ms-share-quota", "7"), ("x-ms-access-tier", "Hot")], "").await;
    assert_eq!(r.status, 201);
    assert!(r.header("etag").unwrap().starts_with("\"0x") && r.header("last-modified").is_some());
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share", &[], "").await), (409, "ShareAlreadyExists".into()));
    for method in ["GET", "HEAD"] {
        let r = f.send(method, "/sh1?restype=share", &[], "").await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("x-ms-share-quota"), Some("7"));
        assert_eq!(r.header("x-ms-access-tier"), Some("Hot"));
        assert_eq!(r.header("x-ms-meta-owner"), Some("me"));
        assert_eq!(r.header("x-ms-lease-state"), Some("available"));
        assert_eq!(r.header("x-ms-lease-status"), Some("unlocked"));
        assert_eq!(r.header("x-ms-enabled-protocols"), Some("SMB"));
        assert_eq!(r.header("x-ms-has-legal-hold"), Some("false"));
    }
    assert_eq!(f.send("DELETE", "/sh1?restype=share", &[], "").await.status, 202);
    assert_eq!(failed(&f.send("GET", "/sh1?restype=share", &[], "").await), (404, "ShareNotFound".into()));
    assert_eq!(failed(&f.send("DELETE", "/sh1?restype=share", &[], "").await), (404, "ShareNotFound".into()));
}

#[tokio::test]
async fn a_new_share_has_a_hundred_tebibytes() {
    let f = start().await;
    f.share("sh1").await;
    let r = f.send("GET", "/sh1?restype=share", &[], "").await;
    assert_eq!(r.header("x-ms-share-quota"), Some("102400"));
    assert_eq!(r.header("x-ms-access-tier"), Some("TransactionOptimized"));
    assert!(r.header("x-ms-access-tier-transition-state").is_none());
}

#[tokio::test]
async fn share_names() {
    let f = start().await;
    for (name, status, code) in [
        ("ab", 400, "OutOfRangeInput"),
        ("Upper", 400, "InvalidResourceName"),
        ("a--b", 400, "InvalidResourceName"),
        ("-ab", 400, "InvalidResourceName"),
        ("ab-", 400, "InvalidResourceName"),
        ("a_b", 400, "InvalidResourceName"),
        ("a.b", 400, "InvalidResourceName"),
    ] {
        assert_eq!(failed(&f.send("PUT", &format!("/{name}?restype=share"), &[], "").await), (status, code.into()), "{name}");
    }
    let long = "a".repeat(64);
    assert_eq!(failed(&f.send("PUT", &format!("/{long}?restype=share"), &[], "").await), (400, "OutOfRangeInput".into()));
    for ok in ["abc", "1ab", "con", &"a".repeat(63)] {
        assert_eq!(f.send("PUT", &format!("/{ok}?restype=share"), &[], "").await.status, 201, "{ok}");
    }
    // every operation checks the name first
    assert_eq!(failed(&f.send("GET", "/Bad_Name?restype=share", &[], "").await), (400, "InvalidResourceName".into()));
    assert_eq!(failed(&f.send("HEAD", "/Bad_Name/x", &[], "").await), (400, "InvalidResourceName".into()));
}

#[tokio::test]
async fn quota_tier_and_protocol_headers() {
    let f = start().await;
    for (name, value) in [
        ("x-ms-share-quota", "0"),
        ("x-ms-share-quota", "102401"),
        ("x-ms-share-quota", "abc"),
        ("x-ms-access-tier", "Bogus"),
        ("x-ms-access-tier", "Premium"),
        ("x-ms-enabled-protocols", "NFS"),
    ] {
        let r = f.send("PUT", "/qqq?restype=share", &[(name, value)], "").await;
        assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()), "{name}: {value}");
        assert!(r.text().contains(&format!("<HeaderName>{name}</HeaderName><HeaderValue>{value}</HeaderValue>")));
    }
    // Azure took any other protocol word and made an SMB share
    assert_eq!(f.send("PUT", "/qqq?restype=share", &[("x-ms-enabled-protocols", "FTP")], "").await.status, 201);
}

#[tokio::test]
async fn set_properties_changes_the_etag_and_marks_the_tier_change() {
    let f = start().await;
    f.share("sh1").await;
    let before = f.send("GET", "/sh1?restype=share", &[], "").await;
    let r = f.send("PUT", "/sh1?restype=share&comp=properties", &[("x-ms-share-quota", "5"), ("x-ms-access-tier", "Cool")], "").await;
    assert_eq!(r.status, 200);
    assert_ne!(r.header("etag"), before.header("etag"));
    let after = f.send("GET", "/sh1?restype=share", &[], "").await;
    assert_eq!(after.header("x-ms-share-quota"), Some("5"));
    assert_eq!(after.header("x-ms-access-tier"), Some("Cool"));
    assert_eq!(after.header("x-ms-access-tier-transition-state"), Some("pending-from-transactionOptimized"));
    assert_eq!(after.header("x-ms-access-tier-change-count"), Some("1"));
    // no header: still a change of the ETag
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=properties", &[], "").await.status, 200);
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=properties", &[("x-ms-share-quota", "0")], "").await),
        (400, "InvalidHeaderValue".into())
    );
}

#[tokio::test]
async fn metadata_is_replaced_and_checked() {
    let f = start().await;
    f.share("sh1").await;
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-meta-a", "1"), ("x-ms-meta-b", "2")], "").await.status, 200);
    let r = f.send("GET", "/sh1?restype=share&comp=metadata", &[], "").await;
    assert_eq!((r.header("x-ms-meta-a"), r.header("x-ms-meta-b")), (Some("1"), Some("2")));
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-meta-c", "3")], "").await.status, 200);
    let r = f.send("GET", "/sh1?restype=share&comp=metadata", &[], "").await;
    assert_eq!((r.header("x-ms-meta-a"), r.header("x-ms-meta-c")), (None, Some("3")));
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-meta-1abc", "x")], "").await),
        (400, "InvalidMetadata".into())
    );
    let big = "x".repeat(9000);
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-meta-big", &big)], "").await),
        (400, "MetadataTooLarge".into())
    );
}

#[tokio::test]
async fn access_policies_round_trip_with_seven_digit_times() {
    let f = start().await;
    f.share("sh1").await;
    assert!(f.send("GET", "/sh1?restype=share&comp=acl", &[], "").await.text().ends_with("<SignedIdentifiers />"));
    let body = "<SignedIdentifiers><SignedIdentifier><Id>p1</Id><AccessPolicy><Start>2026-01-01T00:00:00Z</Start><Expiry>2027-01-01T00:00:00Z</Expiry><Permission>rcwdl</Permission></AccessPolicy></SignedIdentifier><SignedIdentifier><Id>p2</Id></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=acl", &[], body).await.status, 200);
    let got = f.send("GET", "/sh1?restype=share&comp=acl", &[], "").await;
    let text = got.text();
    assert!(text.contains(
        "<Start>2026-01-01T00:00:00.0000000Z</Start><Expiry>2027-01-01T00:00:00.0000000Z</Expiry><Permission>rcwdl</Permission>"
    ));
    assert!(text.contains("<SignedIdentifier><Id>p2</Id></SignedIdentifier>"));
    // an empty body clears them
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=acl", &[], "").await.status, 200);
    assert!(f.send("GET", "/sh1?restype=share&comp=acl", &[], "").await.text().ends_with("<SignedIdentifiers />"));
}

#[tokio::test]
async fn access_policy_limits() {
    let f = start().await;
    f.share("sh1").await;
    let six: String = (0..6).map(|i| format!("<SignedIdentifier><Id>p{i}</Id></SignedIdentifier>")).collect();
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=acl", &[], format!("<SignedIdentifiers>{six}</SignedIdentifiers>")).await),
        (400, "InvalidXmlDocument".into())
    );
    let dup = "<SignedIdentifiers><SignedIdentifier><Id>a</Id></SignedIdentifier><SignedIdentifier><Id>a</Id></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share&comp=acl", &[], dup).await), (400, "InvalidXmlDocument".into()));
    let bad = "<SignedIdentifiers><SignedIdentifier><Id>a</Id><AccessPolicy><Permission>z</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share&comp=acl", &[], bad).await), (400, "InvalidXmlDocument".into()));
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share&comp=acl", &[], "<Wrong/>").await), (400, "InvalidXmlDocument".into()));
}

#[tokio::test]
async fn stats_count_the_bytes_of_the_files() {
    let f = start().await;
    f.share("sh1").await;
    assert_eq!(tag(&f.send("GET", "/sh1?restype=share&comp=stats", &[], "").await.text(), "ShareUsageBytes").as_deref(), Some("0"));
    f.dir("sh1/d").await;
    f.file("sh1/d/a.bin", 1000).await;
    f.file("sh1/b.bin", 24).await;
    assert_eq!(tag(&f.send("GET", "/sh1?restype=share&comp=stats", &[], "").await.text(), "ShareUsageBytes").as_deref(), Some("1024"));
}

#[tokio::test]
async fn snapshots_keep_the_tree_and_the_ranges_of_the_moment() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    f.file("sh1/d/a.txt", 5).await;
    f.write("sh1/d/a.txt", 0, "hello").await;
    let r = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[("x-ms-meta-why", "t")], "").await;
    assert_eq!(r.status, 201);
    let snap = r.header("x-ms-snapshot").unwrap().to_string();
    assert!(snap.ends_with(".0000000Z"));
    f.write("sh1/d/a.txt", 0, "HELLO").await;
    f.file("sh1/d/b.txt", 1).await;
    let q = format!("sharesnapshot={snap}");
    let old = f.send("GET", &format!("/sh1/d/a.txt?{q}"), &[], "").await;
    assert_eq!(old.text(), "hello");
    assert_eq!(f.read("sh1/d/a.txt").await, "HELLO");
    let list = f.send("GET", &format!("/sh1/d?restype=directory&comp=list&{q}"), &[], "").await.text();
    assert_eq!(all(&list, "Name"), vec!["a.txt"]);
    assert!(list.contains(&format!("ShareSnapshot=\"{snap}\"")));
    let props = f.send("GET", &format!("/sh1?restype=share&{q}"), &[], "").await;
    assert_eq!(props.header("x-ms-meta-why"), Some("t"));
    // the snapshot cannot be changed, and a snapshot that is not there is a missing share
    for (m, p) in [
        ("PUT", format!("/sh1/new?restype=directory&{q}")),
        ("DELETE", format!("/sh1/d/a.txt?{q}")),
        ("PUT", format!("/sh1/d/a.txt?comp=metadata&{q}")),
    ] {
        let r = f.send(m, &p, &[], "").await;
        assert_eq!(failed(&r), (400, "InvalidQueryParameterValue".into()), "{m} {p}");
        assert!(r.text().contains("This operation is only allowed on the base share."));
    }
    assert_eq!(
        failed(&f.send("GET", "/sh1/d/a.txt?sharesnapshot=2020-01-01T00:00:00.0000000Z", &[], "").await),
        (404, "ShareNotFound".into())
    );
    assert_eq!(failed(&f.send("GET", "/sh1/d/a.txt?sharesnapshot=garbage", &[], "").await), (400, "InvalidQueryParameterValue".into()));
}

#[tokio::test]
async fn two_snapshots_in_one_second_get_two_times() {
    let f = start().await;
    f.share("sh1").await;
    let a = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    let b = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    assert_ne!(a, b);
}

#[tokio::test]
async fn deleting_a_share_with_snapshots() {
    let f = start().await;
    f.share("sh1").await;
    let snap = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    assert_eq!(failed(&f.send("DELETE", "/sh1?restype=share", &[], "").await), (409, "ShareHasSnapshots".into()));
    assert_eq!(
        failed(&f.send("DELETE", "/sh1?restype=share", &[("x-ms-delete-snapshots", "bogus")], "").await),
        (400, "InvalidHeaderValue".into())
    );
    // one snapshot goes alone
    assert_eq!(f.send("DELETE", &format!("/sh1?restype=share&sharesnapshot={snap}"), &[], "").await.status, 202);
    assert_eq!(f.send("GET", "/sh1?restype=share", &[], "").await.status, 200);
    f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await;
    assert_eq!(f.send("DELETE", "/sh1?restype=share", &[("x-ms-delete-snapshots", "include")], "").await.status, 202);
}

#[tokio::test]
async fn the_list_of_shares() {
    let f = start().await;
    for n in ["pa1", "pa2", "pb1"] {
        f.send("PUT", &format!("/{n}?restype=share"), &[("x-ms-meta-n", n)], "").await;
    }
    let r = f.send("GET", "/?comp=list&prefix=pa&include=metadata", &[], "").await;
    let t = r.text();
    assert_eq!(all(&t, "Name"), vec!["pa1", "pa2"]);
    assert!(t.contains("<Prefix>pa</Prefix><Shares>"));
    assert!(t.contains("<Metadata><n>pa1</n></Metadata>"));
    assert!(t.ends_with("<NextMarker /></EnumerationResults>"));
    assert!(t.contains("<Quota>102400</Quota><AccessTier>TransactionOptimized</AccessTier>"));
    // paging: the marker is /account/next-share
    let r = f.send("GET", "/?comp=list&maxresults=2", &[], "").await.text();
    assert_eq!(all(&r, "Name"), vec!["pa1", "pa2"]);
    assert_eq!(tag(&r, "NextMarker").as_deref(), Some("/acct1/pb1"));
    assert!(r.contains("<MaxResults>2</MaxResults>"));
    let r = f.send("GET", "/?comp=list&marker=%2Facct1%2Fpb1", &[], "").await.text();
    assert_eq!(all(&r, "Name"), vec!["pb1"]);
    assert_eq!(failed(&f.send("GET", "/?comp=list&marker=garbage", &[], "").await), (400, "OutOfRangeInput".into()));
    assert_eq!(failed(&f.send("GET", "/?comp=list&maxresults=0", &[], "").await), (400, "OutOfRangeQueryParameterValue".into()));
    assert_eq!(failed(&f.send("GET", "/?comp=list&include=bogus", &[], "").await), (400, "InvalidQueryParameterValue".into()));
    assert!(f.send("GET", "/?comp=list&prefix=zz", &[], "").await.text().contains("<Shares />"));
}

#[tokio::test]
async fn the_list_names_snapshots_before_their_share() {
    let f = start().await;
    f.share("sh1").await;
    let snap = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    let t = f.send("GET", "/?comp=list&include=snapshots", &[], "").await.text();
    assert_eq!(all(&t, "Name"), vec!["sh1", "sh1"]);
    assert_eq!(tag(&t, "Snapshot").as_deref(), Some(snap.as_str()));
    assert!(t.find("<Snapshot>").unwrap() < t.rfind("<Name>").unwrap());
    // a snapshot has no access tier
    assert_eq!(t.matches("<AccessTier>").count(), 1);
    assert_eq!(all(&f.send("GET", "/?comp=list", &[], "").await.text(), "Name"), vec!["sh1"]);
}

#[tokio::test]
async fn a_deleted_share_can_be_listed_and_restored() {
    let f = start().await;
    f.send("PUT", "/sh1?restype=share", &[("x-ms-meta-keep", "me")], "").await;
    f.dir("sh1/d").await;
    f.send("DELETE", "/sh1?restype=share", &[], "").await;
    assert!(f.send("GET", "/?comp=list", &[], "").await.text().contains("<Shares />"));
    let t = f.send("GET", "/?comp=list&include=deleted", &[], "").await.text();
    assert!(t.contains("<Deleted>true</Deleted>"));
    assert!(t.contains("<RemainingRetentionDays>7</RemainingRetentionDays>"));
    let version = tag(&t, "Version").unwrap();
    let h = [("x-ms-deleted-share-name", "sh1"), ("x-ms-deleted-share-version", version.as_str())];
    assert_eq!(f.send("PUT", "/sh1?restype=share&comp=undelete", &h, "").await.status, 201);
    assert_eq!(f.send("GET", "/sh1?restype=share", &[], "").await.header("x-ms-meta-keep"), Some("me"));
    assert_eq!(f.send("GET", "/sh1/d?restype=directory", &[], "").await.status, 200);
    // it is only restored once, and not over a share
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share&comp=undelete", &h, "").await), (409, "ShareAlreadyExists".into()));
    f.send("DELETE", "/sh1?restype=share", &[], "").await;
    assert_eq!(
        failed(
            &f.send(
                "PUT",
                "/sh2?restype=share&comp=undelete",
                &[("x-ms-deleted-share-name", "sh1"), ("x-ms-deleted-share-version", "0000")],
                ""
            )
            .await
        ),
        (404, "ShareNotFound".into())
    );
    assert_eq!(failed(&f.send("PUT", "/sh2?restype=share&comp=undelete", &[], "").await), (400, "MissingRequiredHeader".into()));
}

#[tokio::test]
async fn soft_delete_can_be_turned_off() {
    let srv = azemu_core::testing::TestServer::start(vec![std::sync::Arc::new(azemu_file::FileService::with_soft_delete_days(0))]).await;
    let base = srv.url("file.core", ACCOUNT);
    let h = [("Authorization", "SharedKey acct1:c2ln")];
    srv.send("PUT", &format!("{base}/sh1?restype=share"), &h, "").await;
    srv.send("DELETE", &format!("{base}/sh1?restype=share"), &h, "").await;
    let t = srv.send("GET", &format!("{base}/?comp=list&include=deleted"), &h, "").await.text();
    assert!(t.contains("<Shares />"), "{t}");
}

#[tokio::test]
async fn permissions_are_stored_and_returned() {
    let f = start().await;
    f.share("sh1").await;
    let sddl = "O:S-1-5-21-1-2-3-500G:S-1-5-21-1-2-3-513D:(A;;FA;;;SY)";
    let r = f.send("PUT", "/sh1?restype=share&comp=filepermission", &[], format!("{{\"permission\": \"{sddl}\"}}")).await;
    assert_eq!(r.status, 201);
    let key = r.header("x-ms-file-permission-key").unwrap().to_string();
    let g = f.send("GET", "/sh1?restype=share&comp=filepermission", &[("x-ms-file-permission-key", &key)], "").await;
    assert_eq!(g.status, 200);
    let j: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
    assert_eq!(j["format"], "sddl");
    assert!(j["permission"].as_str().unwrap().starts_with(sddl));
    assert_eq!(
        failed(&f.send("GET", "/sh1?restype=share&comp=filepermission", &[("x-ms-file-permission-key", "1*2")], "").await),
        (400, "FileInvalidPermissionKey".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1?restype=share&comp=filepermission", &[("x-ms-file-permission-key", "bogus")], "").await),
        (400, "InvalidHeaderValue".into())
    );
    assert_eq!(failed(&f.send("GET", "/sh1?restype=share&comp=filepermission", &[], "").await), (400, "MissingRequiredHeader".into()));
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=filepermission", &[], "{\"permission\": \"garbage\"}").await),
        (400, "FileInvalidPermission".into())
    );
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=share&comp=filepermission", &[], "{}").await), (400, "InvalidJsonDocument".into()));
    // a directory made with the key, and with the descriptor itself, gets that permission; both together are refused
    let d = f.send("PUT", "/sh1/d1?restype=directory", &[("x-ms-file-permission-key", &key)], "").await;
    assert_eq!(d.header("x-ms-file-permission-key"), Some(key.as_str()));
    let d2 = f.send("PUT", "/sh1/d2?restype=directory", &[("x-ms-file-permission", sddl)], "").await;
    assert_eq!(d2.header("x-ms-file-permission-key"), Some(key.as_str()));
    let both = f.send("PUT", "/sh1/d3?restype=directory", &[("x-ms-file-permission", sddl), ("x-ms-file-permission-key", &key)], "").await;
    assert_eq!(failed(&both), (400, "BothPermissionAndPermissionKeyHeaderPresent".into()));
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d4?restype=directory", &[("x-ms-file-permission-key", "1*2")], "").await),
        (400, "FileInvalidPermissionKey".into())
    );
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d5?restype=directory", &[("x-ms-file-permission", "bogus")], "").await),
        (400, "FileInvalidPermission".into())
    );
}

#[tokio::test]
async fn a_binary_permission_is_taken_and_a_lost_lease_is_named() {
    let f = start().await;
    f.share("sh1").await;
    // base64 of a binary security descriptor (more than 20 bytes), with its format
    let binary = "AQAUhGwAAACIAAAAAAAAABQAAAACAFgAAwAAAAAAFAD/AR8AAQEAAAAAAAUSAAAAAAAYAP8BHwABAgAAAAAABSAAAAAgAgAA";
    let r =
        f.send("PUT", "/sh1/d?restype=directory", &[("x-ms-file-permission", binary), ("x-ms-file-permission-format", "binary")], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(
        failed(&f.send("PUT", "/sh1/e?restype=directory", &[("x-ms-file-permission", binary)], "").await),
        (400, "FileInvalidPermission".into())
    );
}

#[tokio::test]
async fn nfs_operations_on_the_share_are_501() {
    let f = start().await;
    f.share("sh1").await;
    for (m, p) in [("GET", "/sh1?comp=hardlinks&fileid=1"), ("HEAD", "/sh1?fileid=1")] {
        assert_eq!(f.send(m, p, &[], "").await.status, 501, "{m} {p}");
    }
}
