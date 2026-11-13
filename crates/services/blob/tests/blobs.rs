mod common;
use common::*;

#[tokio::test]
async fn put_get_head_and_ranges() {
    let e = start_versioned().await;
    e.create_container("cnt").await;
    let r = e
        .send(
            "PUT",
            "/cnt/dir/a%20b.txt",
            &[
                ("x-ms-blob-type", "BlockBlob"),
                ("Content-Type", "text/plain"),
                ("x-ms-meta-k", "v"),
                ("x-ms-blob-cache-control", "no-cache"),
            ],
            "0123456789",
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("content-md5"), Some("eB5eJF1ptWaXm4bijSPyxw=="));
    assert!(r.header("x-ms-content-crc64").is_some());
    assert_eq!(r.header("x-ms-request-server-encrypted"), Some("true"));
    assert!(r.header("x-ms-version-id").is_some());

    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), "0123456789");
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("x-ms-blob-type"), Some("BlockBlob"));
    assert_eq!(r.header("x-ms-meta-k"), Some("v"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("x-ms-lease-status"), Some("unlocked"));
    assert!(r.header("x-ms-creation-time").is_some());

    let r = e.send("HEAD", "/cnt/dir/a%20b.txt", &[], "").await;
    assert_eq!((r.status, r.header("content-length")), (200, Some("10")));
    assert_eq!(r.header("x-ms-access-tier"), Some("Hot"));
    assert_eq!(r.header("x-ms-access-tier-inferred"), Some("true"));

    // ranges
    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[("x-ms-range", "bytes=2-5")], "").await;
    assert_eq!((r.status, r.text().as_str(), r.header("content-range")), (206, "2345", Some("bytes 2-5/10")));
    assert!(r.header("x-ms-blob-content-md5").is_some());
    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[("Range", "bytes=7-")], "").await;
    assert_eq!((r.status, r.text().as_str()), (206, "789"));
    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[("x-ms-range", "bytes=0-3"), ("x-ms-range-get-content-md5", "true")], "").await;
    assert_eq!(r.header("content-md5"), Some("62L2uTBttXXC1ZaxJ5YnpA=="));
    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[("x-ms-range", "bytes=10-12")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (416, "InvalidRange"));
    // a range past the end is cut
    let r = e.send("GET", "/cnt/dir/a%20b.txt", &[("x-ms-range", "bytes=8-100")], "").await;
    assert_eq!((r.status, r.text().as_str()), (206, "89"));

    // errors
    let r = e.send("GET", "/cnt/missing", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "BlobNotFound"));
    let r = e.send("HEAD", "/cnt/missing", &[], "").await;
    assert_eq!((r.status, code(&r).as_str(), r.text().len()), (404, "BlobNotFound", 0));
    let r = e.send("PUT", "/cnt/nohdr", &[], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    let r = e.send("GET", "/nocontainer/b", &[], "").await;
    assert_eq!(code(&r), "ContainerNotFound");
}

#[tokio::test]
async fn md5_validation_and_bad_metadata() {
    let e = start().await;
    e.create_container("cnt").await;
    let r =
        e.send("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob"), ("Content-MD5", "XrY7u+Ae7tCTyyK7j1rNww==")], "not hello world").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "Md5Mismatch"));
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.status, 404);
    let r = e.send("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-bad-name", "v")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidMetadata"));
}

#[tokio::test]
async fn blocks_commit_semantics() {
    let e = start().await;
    e.create_container("cnt").await;
    let id = |s: &str| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(s)
    };
    let (b1, b2) = (id("block-1"), id("block-2"));
    for (b, data) in [(&b1, "aa"), (&b2, "bb")] {
        let r = e.send("PUT", &format!("/cnt/blk?comp=block&blockid={}", b.replace('=', "%3D")), &[], data).await;
        assert_eq!(r.status, 201);
    }
    // not visible yet
    assert_eq!(e.send("GET", "/cnt/blk", &[], "").await.status, 404);
    let r = e.send("GET", "/cnt/blk?comp=blocklist&blocklisttype=all", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(tags(&r.text(), "Size"), ["2", "2"]);
    let list = format!("<BlockList><Latest>{b2}</Latest><Latest>{b1}</Latest></BlockList>");
    let r = e.send("PUT", "/cnt/blk?comp=blocklist", &[("x-ms-blob-content-type", "text/x")], list).await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("GET", "/cnt/blk", &[], "").await.text(), "bbaa");
    let r = e.send("GET", "/cnt/blk?comp=blocklist&blocklisttype=committed", &[], "").await;
    assert_eq!(tags(&r.text(), "Name"), [b2.clone(), b1.clone()]);
    assert!(!r.text().contains("UncommittedBlocks"));
    // a new staged block with the same id: <Committed> ignores it, <Latest> takes it
    e.send("PUT", &format!("/cnt/blk?comp=block&blockid={}", b1.replace('=', "%3D")), &[], "ZZ").await;
    let list = format!("<BlockList><Committed>{b1}</Committed></BlockList>");
    assert_eq!(e.send("PUT", "/cnt/blk?comp=blocklist", &[], list).await.status, 201);
    assert_eq!(e.send("GET", "/cnt/blk", &[], "").await.text(), "aa");
    let list = format!("<BlockList><Latest>{b1}</Latest></BlockList>");
    e.send("PUT", &format!("/cnt/blk?comp=block&blockid={}", b1.replace('=', "%3D")), &[], "ZZ").await;
    e.send("PUT", "/cnt/blk?comp=blocklist", &[], list).await;
    assert_eq!(e.send("GET", "/cnt/blk", &[], "").await.text(), "ZZ");
    // unknown block
    let list = format!("<BlockList><Latest>{}</Latest></BlockList>", id("nope"));
    let r = e.send("PUT", "/cnt/blk?comp=blocklist", &[], list).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidBlockList"));
}

#[tokio::test]
async fn conditional_requests() {
    let e = start().await;
    e.create_container("cnt").await;
    let etag = e.put_blob("cnt", "b", "x").await.header("etag").unwrap().to_string();
    let r = e.send("GET", "/cnt/b", &[("If-None-Match", &etag)], "").await;
    assert_eq!((r.status, r.text().len()), (304, 0));
    assert_eq!(code(&r), "ConditionNotMet");
    let r = e.send("GET", "/cnt/b", &[("If-Match", "\"0xBAD\"")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    let r = e.send("HEAD", "/cnt/b", &[("If-Modified-Since", "Fri, 01 Jan 2100 00:00:00 GMT")], "").await;
    assert_eq!(r.status, 304);
    let r = e.send("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob"), ("If-None-Match", "*")], "y").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobAlreadyExists"));
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("If-Match", "\"0xBAD\""), ("x-ms-meta-a", "b")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("If-Match", &etag), ("x-ms-meta-a", "b")], "").await;
    assert_eq!(r.status, 200);
    assert_ne!(r.header("etag").unwrap(), etag);
    // missing blob with If-Match fails the condition
    let r = e.send("PUT", "/cnt/new", &[("x-ms-blob-type", "BlockBlob"), ("If-Match", "*")], "y").await;
    assert_eq!(r.status, 412);
}

#[tokio::test]
async fn lease_state_machine() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    let lid = "11111111-1111-1111-1111-111111111111";
    let r = e
        .send(
            "PUT",
            "/cnt/b?comp=lease",
            &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", lid)],
            "",
        )
        .await;
    assert_eq!((r.status, r.header("x-ms-lease-id")), (201, Some(lid)));
    let r = e.send("HEAD", "/cnt/b", &[], "").await;
    assert_eq!(r.header("x-ms-lease-state"), Some("leased"));
    assert_eq!(r.header("x-ms-lease-duration"), Some("infinite"));
    // writes need the id
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-meta-a", "b")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"));
    let r =
        e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-meta-a", "b"), ("x-ms-lease-id", "22222222-2222-2222-2222-222222222222")], "").await;
    assert_eq!(code(&r), "LeaseIdMismatchWithBlobOperation");
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-meta-a", "b"), ("x-ms-lease-id", lid)], "").await;
    assert_eq!(r.status, 200);
    // another acquire conflicts
    let r = e.send("PUT", "/cnt/b?comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "15")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "LeaseAlreadyPresent"));
    // break with a period
    let r = e.send("PUT", "/cnt/b?comp=lease", &[("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "10")], "").await;
    assert_eq!((r.status, r.header("x-ms-lease-time")), (202, Some("10")));
    assert_eq!(e.send("HEAD", "/cnt/b", &[], "").await.header("x-ms-lease-state"), Some("breaking"));
    let r = e.send("PUT", "/cnt/b?comp=lease", &[("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "0")], "").await;
    assert_eq!(r.header("x-ms-lease-time"), Some("0"));
    assert_eq!(e.send("HEAD", "/cnt/b", &[], "").await.header("x-ms-lease-state"), Some("broken"));
    // release on a broken lease and re-acquire
    let r = e.send("PUT", "/cnt/b?comp=lease", &[("x-ms-lease-action", "release"), ("x-ms-lease-id", lid)], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(e.send("HEAD", "/cnt/b", &[], "").await.header("x-ms-lease-state"), Some("available"));
    // not-a-guid lease id
    let r = e.send("PUT", "/cnt/b?comp=metadata", &[("x-ms-lease-id", "'x'")], "").await;
    assert_eq!(code(&r), "InvalidHeaderValue");

    // container leases
    let r =
        e.send("PUT", "/cnt?restype=container&comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")], "").await;
    assert_eq!(r.status, 201);
    let r = e.send("DELETE", "/cnt?restype=container", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"));
}

#[tokio::test]
async fn snapshots_versions_and_delete_options() {
    let e = start_versioned().await;
    e.create_container("cnt").await;
    let v1 = e.put_blob("cnt", "b", "one").await.header("x-ms-version-id").unwrap().to_string();
    let r = e.send("PUT", "/cnt/b?comp=snapshot", &[], "").await;
    assert_eq!(r.status, 201);
    let snap = r.header("x-ms-snapshot").unwrap().to_string();
    e.put_blob("cnt", "b", "two").await;
    assert_eq!(e.send("GET", &format!("/cnt/b?snapshot={snap}"), &[], "").await.text(), "one");
    assert_eq!(e.send("GET", &format!("/cnt/b?versionid={v1}"), &[], "").await.text(), "one");
    let r = e.send("DELETE", "/cnt/b", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "SnapshotsPresent"));
    assert_eq!(e.send("DELETE", "/cnt/b", &[("x-ms-delete-snapshots", "only")], "").await.status, 202);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.text(), "two");
    assert_eq!(e.send("GET", &format!("/cnt/b?snapshot={snap}"), &[], "").await.status, 404);
    // versions are listed on request
    let r = e.send("GET", "/cnt?restype=container&comp=list&include=versions", &[], "").await;
    assert_eq!(tags(&r.text(), "VersionId").len(), 3, "{}", r.text());
    assert!(r.text().contains("<IsCurrentVersion>true</IsCurrentVersion>"));
    assert_eq!(e.send("DELETE", "/cnt/b", &[], "").await.status, 202);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.status, 404);
    assert_eq!(e.send("GET", &format!("/cnt/b?versionid={v1}"), &[], "").await.text(), "one");
}

#[tokio::test]
async fn soft_delete_and_undelete() {
    let e = start().await;
    e.create_container("cnt").await;
    let props = "<StorageServiceProperties><DeleteRetentionPolicy><Enabled>true</Enabled><Days>2</Days></DeleteRetentionPolicy></StorageServiceProperties>";
    e.send("PUT", "/?restype=service&comp=properties", &[], props).await;
    e.put_blob("cnt", "b", "precious").await;
    assert_eq!(e.send("DELETE", "/cnt/b", &[], "").await.status, 202);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.status, 404);
    let r = e.send("GET", "/cnt?restype=container&comp=list&include=deleted", &[], "").await;
    let t = r.text();
    assert!(t.contains("<Deleted>true</Deleted>"));
    assert!(t.contains("<RemainingRetentionDays>2</RemainingRetentionDays>"));
    assert_eq!(e.send("GET", "/cnt?restype=container&comp=list", &[], "").await.text().matches("<Blob>").count(), 0);
    assert_eq!(e.send("PUT", "/cnt/b?comp=undelete", &[], "").await.status, 200);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.text(), "precious");
    // never existed
    assert_eq!(e.send("PUT", "/cnt/never?comp=undelete", &[], "").await.status, 404);
    // overwrite keeps a soft-deleted snapshot of the old content
    e.put_blob("cnt", "b", "newer").await;
    let t = e.send("GET", "/cnt?restype=container&comp=list&include=deleted,snapshots", &[], "").await.text();
    assert_eq!(t.matches("<Snapshot>").count(), 1);
    // a deleted container can be restored
    assert_eq!(e.send("DELETE", "/cnt?restype=container", &[], "").await.status, 202);
    let t = e.send("GET", "/?comp=list&include=deleted", &[], "").await.text();
    let version = tags(&t, "Version").remove(0);
    let r = e
        .send(
            "PUT",
            "/cnt?restype=container&comp=undelete",
            &[("x-ms-deleted-container-name", "cnt"), ("x-ms-deleted-container-version", &version)],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.text(), "newer");
}

#[tokio::test]
async fn tier_properties_and_set_http_headers() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "data").await;
    let r = e.send("PUT", "/cnt/b?comp=tier", &[("x-ms-access-tier", "Cool")], "").await;
    assert_eq!(r.status, 200);
    let r = e.send("HEAD", "/cnt/b", &[], "").await;
    assert_eq!(r.header("x-ms-access-tier"), Some("Cool"));
    assert!(r.header("x-ms-access-tier-change-time").is_some());
    assert_eq!(code(&e.send("PUT", "/cnt/b?comp=tier", &[("x-ms-access-tier", "Lukewarm")], "").await), "InvalidHeaderValue");
    assert_eq!(e.send("PUT", "/cnt/b?comp=tier", &[("x-ms-access-tier", "Archive")], "").await.status, 200);
    let r = e.send("GET", "/cnt/b", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobArchived"));
    // Set Blob Properties clears what it does not mention
    e.put_blob("cnt", "p", "x").await;
    e.send("PUT", "/cnt/p?comp=properties", &[("x-ms-blob-content-type", "a/b"), ("x-ms-blob-content-language", "de")], "").await;
    e.send("PUT", "/cnt/p?comp=properties", &[("x-ms-blob-cache-control", "no-store")], "").await;
    let r = e.send("HEAD", "/cnt/p", &[], "").await;
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(r.header("content-language").is_none());
    assert_eq!(r.header("content-type"), Some("application/octet-stream"));
}

#[tokio::test]
async fn append_page_and_tags() {
    let e = start().await;
    e.create_container("cnt").await;
    // append
    assert_eq!(e.send("PUT", "/cnt/ab", &[("x-ms-blob-type", "AppendBlob")], "").await.status, 201);
    let r = e.send("PUT", "/cnt/ab?comp=appendblock", &[], "hello ").await;
    assert_eq!((r.status, r.header("x-ms-blob-append-offset"), r.header("x-ms-blob-committed-block-count")), (201, Some("0"), Some("1")));
    let r = e.send("PUT", "/cnt/ab?comp=appendblock", &[("x-ms-blob-condition-appendpos", "6")], "world").await;
    assert_eq!(r.header("x-ms-blob-append-offset"), Some("6"));
    let r = e.send("PUT", "/cnt/ab?comp=appendblock", &[("x-ms-blob-condition-appendpos", "0")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "AppendPositionConditionNotMet"));
    let r = e.send("PUT", "/cnt/ab?comp=appendblock", &[("x-ms-blob-condition-maxsize", "5")], "x").await;
    assert_eq!(code(&r), "MaxBlobSizeConditionNotMet");
    assert_eq!(e.send("GET", "/cnt/ab", &[], "").await.text(), "hello world");
    assert_eq!(e.send("PUT", "/cnt/ab?comp=seal", &[], "").await.status, 200);
    assert_eq!(code(&e.send("PUT", "/cnt/ab?comp=appendblock", &[], "x").await), "BlobIsSealed");
    assert_eq!(
        code(
            &e.send("PUT", "/cnt/nopage?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=0-511")], "a".repeat(512)).await
        ),
        "BlobNotFound"
    );

    // page
    let r = e.send("PUT", "/cnt/pb", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", "2048")], "").await;
    assert_eq!(r.status, 201);
    let page = "a".repeat(512);
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=512-1023")], page.clone()).await;
    assert_eq!(r.status, 201);
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=1-512")], page.clone()).await;
    assert_eq!((r.status, code(&r).as_str()), (416, "InvalidPageRange"));
    let r = e.send("GET", "/cnt/pb?comp=pagelist", &[], "").await;
    assert_eq!(tags(&r.text(), "Start"), ["512"]);
    assert_eq!(tags(&r.text(), "End"), ["1023"]);
    let body = e.send("GET", "/cnt/pb", &[("x-ms-range", "bytes=0-1023")], "").await;
    assert_eq!(&body.body[..512], &[0u8; 512][..]);
    assert_eq!(&body.body[512..], page.as_bytes());
    assert_eq!(code(&e.send("GET", "/cnt/ab?comp=pagelist", &[], "").await), "InvalidBlobType");

    // tags
    let tag_xml = "<Tags><TagSet><Tag><Key>env</Key><Value>test</Value></Tag><Tag><Key>n</Key><Value>1</Value></Tag></TagSet></Tags>";
    assert_eq!(e.send("PUT", "/cnt/ab?comp=tags", &[], tag_xml).await.status, 204);
    assert_eq!(e.send("HEAD", "/cnt/ab", &[], "").await.header("x-ms-tag-count"), Some("2"));
    let r = e.send("GET", "/cnt/ab?comp=tags", &[], "").await;
    assert_eq!(tags(&r.text(), "Key"), ["env", "n"]);
    let r = e.send("GET", "/?comp=blobs&where=%22env%22%3D%27test%27%20AND%20%22n%22%3E%3D%270%27", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(tags(&r.text(), "Name"), ["ab"]);
    assert_eq!(tags(&r.text(), "ContainerName"), ["cnt"]);
    let r = e.send("GET", "/cnt?restype=container&comp=blobs&where=%22env%22%3D%27prod%27", &[], "").await;
    assert!(tags(&r.text(), "Name").is_empty());
    let r = e.send("GET", "/?comp=blobs&where=env", &[], "").await;
    assert_eq!(code(&r), "InvalidQueryParameterValue");
    // x-ms-if-tags
    let r = e.send("PUT", "/cnt/ab?comp=metadata", &[("x-ms-if-tags", "\"env\"='prod'")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    let r = e.send("PUT", "/cnt/ab?comp=metadata", &[("x-ms-if-tags", "\"env\"='test'")], "").await;
    assert_eq!(r.status, 200);
    // too many tags
    let many: String = (0..11).map(|i| format!("<Tag><Key>k{i}</Key><Value>v</Value></Tag>")).collect();
    let r = e.send("PUT", "/cnt/ab?comp=tags", &[], format!("<Tags><TagSet>{many}</TagSet></Tags>")).await;
    assert_eq!(r.status, 400);
}

/// Recording `test_block_blob.pyTestStorageBlockBlobtest_upload_blob_from_url_overwrite_properties.json`, entries 2 to 4: the source says
/// `spanish`, Put Blob From URL names `english` (and no content type): the copy answers `english`, and the source's content type stays.
#[tokio::test]
async fn put_blob_from_url_takes_each_property_the_request_names_and_the_rest_from_the_source() {
    let e = start().await;
    e.create_container("cnt").await;
    e.send(
        "PUT",
        "/cnt/src",
        &[
            ("x-ms-blob-type", "BlockBlob"),
            ("Content-Type", "text/plain"),
            ("x-ms-blob-content-language", "spanish"),
            ("x-ms-blob-cache-control", "no-cache"),
        ],
        "source data",
    )
    .await;
    let src = format!("{}/cnt/src", e.base);
    let r = e
        .send(
            "PUT",
            "/cnt/copy",
            &[
                ("x-ms-copy-source", &src),
                ("x-ms-blob-type", "BlockBlob"),
                ("x-ms-copy-source-blob-properties", "true"),
                ("x-ms-blob-content-language", "english"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    let r = e.send("HEAD", "/cnt/copy", &[], "").await;
    assert_eq!(r.header("content-language"), Some("english"));
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
}

/// `x-ms-delete-type-permanent`: `true` when nothing stays recoverable. Recordings
/// `test_blob_access_conditions.pyTestStorageBlobAccessConditionstest_delete_blob_with_if_match.json` entry 3 (no versioning, no soft delete: `true`),
/// `test_append_blob.pyTestStorageAppendBlobtest_create_append_blob_with_immutability_policy.json` entry 10 (versioning on: `false`) and
/// `test_common_blob.pyTestStorageCommonBlobtest_delete_blob_snapshot.json` entry 4 (a snapshot, no soft delete: `true`).
#[tokio::test]
async fn delete_says_whether_anything_stays_recoverable() {
    let e = start().await;
    e.create_container("cnt").await;
    e.send("PUT", "/cnt/plain", &[("x-ms-blob-type", "BlockBlob")], "data").await;
    let r = e.send("DELETE", "/cnt/plain", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-delete-type-permanent")), (202, Some("true")));

    e.send("PUT", "/cnt/snap", &[("x-ms-blob-type", "BlockBlob")], "data").await;
    let snap = e.send("PUT", "/cnt/snap?comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    let r = e.send("DELETE", &format!("/cnt/snap?snapshot={snap}"), &[], "").await;
    assert_eq!((r.status, r.header("x-ms-delete-type-permanent")), (202, Some("true")));

    let v = start_versioned().await;
    v.create_container("cnt").await;
    v.send("PUT", "/cnt/kept", &[("x-ms-blob-type", "BlockBlob")], "data").await;
    let r = v.send("DELETE", "/cnt/kept", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-delete-type-permanent")), (202, Some("false")), "versioning keeps a version");
}

#[tokio::test]
async fn copy_within_the_emulator() {
    let e = start().await;
    e.create_container("cnt").await;
    e.send("PUT", "/cnt/src", &[("x-ms-blob-type", "BlockBlob"), ("Content-Type", "text/plain"), ("x-ms-meta-o", "src")], "source data")
        .await;
    let src = format!("{}/cnt/src", e.base);
    let r = e.send("PUT", "/cnt/dst", &[("x-ms-copy-source", &src)], "").await;
    assert_eq!(r.status, 202);
    assert_eq!(r.header("x-ms-copy-status"), Some("success"));
    let copy_id = r.header("x-ms-copy-id").unwrap().to_string();
    let r = e.send("HEAD", "/cnt/dst", &[], "").await;
    assert_eq!(r.header("x-ms-copy-id"), Some(copy_id.as_str()));
    assert_eq!(r.header("x-ms-copy-progress"), Some("11/11"));
    assert_eq!(r.header("x-ms-meta-o"), Some("src"));
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(e.send("GET", "/cnt/dst", &[], "").await.text(), "source data");
    // metadata given -> replaces
    e.send("PUT", "/cnt/dst2", &[("x-ms-copy-source", &src), ("x-ms-meta-n", "new")], "").await;
    let r = e.send("HEAD", "/cnt/dst2", &[], "").await;
    assert_eq!((r.header("x-ms-meta-n"), r.header("x-ms-meta-o")), (Some("new"), None));
    // missing source
    let r = e.send("PUT", "/cnt/dst3", &[("x-ms-copy-source", &format!("{}/cnt/none", e.base))], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "CannotVerifyCopySource"));
    // abort on a finished copy
    let r = e.send("PUT", &format!("/cnt/dst?comp=copy&copyid={copy_id}"), &[("x-ms-copy-action", "abort")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "NoPendingCopyOperation"));
    // put blob from url
    let r = e.send("PUT", "/cnt/fromurl", &[("x-ms-copy-source", &src), ("x-ms-blob-type", "BlockBlob")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(e.send("GET", "/cnt/fromurl", &[], "").await.text(), "source data");
    // block from url with a range
    let r = e
        .send("PUT", "/cnt/blk?comp=block&blockid=YmxrMQ%3D%3D", &[("x-ms-copy-source", &src), ("x-ms-source-range", "bytes=0-5")], "")
        .await;
    assert_eq!(r.status, 201);
    e.send("PUT", "/cnt/blk?comp=blocklist", &[], "<BlockList><Latest>YmxrMQ==</Latest></BlockList>").await;
    assert_eq!(e.send("GET", "/cnt/blk", &[], "").await.text(), "source");
}

#[tokio::test]
async fn not_built_operations_answer_501_and_name_themselves() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    // (Query Blob Contents was in this list until it was built: see tests/query.rs)
    let r = e.send("PUT", "/cnt?restype=container&comp=rename", &[], "").await;
    assert_eq!(r.status, 501);
    assert_eq!(code(&r), "NotImplemented");
    assert!(r.text().contains("Rename Container"), "{}", r.text());
    let r = e.send("PUT", "/cnt/b?comp=incrementalcopy", &[], "").await;
    assert_eq!(r.status, 501);
    let r = e.send("PUT", "/cnt/b?comp=bogus", &[], "").await;
    assert_eq!(r.status, 501);
}

#[tokio::test]
async fn batch_delete_and_set_tier() {
    let e = start().await;
    e.create_container("cnt").await;
    for n in ["a", "b", "c1"] {
        e.put_blob("cnt", n, "x").await;
    }
    let boundary = "batch_1234";
    let part = |i: usize, req: &str| {
        format!("--{boundary}\r\nContent-Type: application/http\r\nContent-ID: {i}\r\nContent-Transfer-Encoding: binary\r\n\r\n{req}\r\nx-ms-date: Wed, 01 Jan 2026 00:00:00 GMT\r\nAuthorization: SharedKey acct:c2ln\r\nContent-Length: 0\r\n\r\n\r\n")
    };
    let body = format!(
        "{}{}{}--{boundary}--\r\n",
        part(0, "DELETE /cnt/a HTTP/1.1"),
        part(1, "DELETE /cnt/missing HTTP/1.1"),
        part(2, "PUT /cnt/b?comp=tier HTTP/1.1\r\nx-ms-access-tier: Cool")
    );
    let ct = format!("multipart/mixed; boundary={boundary}");
    let r = e.send("POST", "/cnt?restype=container&comp=batch", &[("Content-Type", &ct)], body).await;
    assert_eq!(r.status, 202, "{}", r.text());
    let t = r.text();
    assert!(r.header("content-type").unwrap().starts_with("multipart/mixed; boundary=batchresponse_"));
    assert!(t.contains("HTTP/1.1 202 Accepted"));
    assert!(t.contains("HTTP/1.1 404 Not Found"));
    assert!(t.contains("HTTP/1.1 200 OK"));
    // Byte layout of the answer, as in the recorded Azure answers: a part without a body is the headers and one
    // blank line, then the next delimiter (no extra empty line: the Java batch client reads one as an error body);
    // a part with a body (the 404) has the body and a line break; the closing delimiter has no line break after it.
    let rb = r.header("content-type").unwrap().rsplit('=').next().unwrap().to_string();
    let parts: Vec<&str> = t.split(&format!("--{rb}")).collect();
    assert_eq!(parts.len(), 5, "{t:?}");
    assert_eq!(parts[0], "");
    assert_eq!(parts[4], "--", "no line break after the closing delimiter: {t:?}");
    for i in [1usize, 3] {
        assert!(parts[i].ends_with("\r\n\r\n") && !parts[i].ends_with("\r\n\r\n\r\n"), "{:?}", parts[i]);
    }
    let after_status = parts[2].split_once("HTTP/1.1 404").unwrap().1;
    let body = after_status.split_once("\r\n\r\n").unwrap().1;
    assert!(
        body.trim_start_matches('\u{feff}').starts_with("<?xml") && body.ends_with("</Error>\r\n") && !body.ends_with("\r\n\r\n"),
        "{:?}",
        parts[2]
    );
    assert_eq!(e.send("GET", "/cnt/a", &[], "").await.status, 404);
    assert_eq!(e.send("HEAD", "/cnt/b", &[], "").await.header("x-ms-access-tier"), Some("Cool"));
}

#[tokio::test]
async fn page_operations_take_range_or_x_ms_range() {
    // REST docs, Put Page: "Either the Range header or the x-ms-range header is required"; the Rust SDK sends `Range`.
    // When both are sent, x-ms-range is the one that counts.
    let e = start().await;
    e.create_container("cnt").await;
    assert_eq!(e.send("PUT", "/cnt/pb", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", "4096")], "").await.status, 201);
    let page = |c: char| c.to_string().repeat(512);
    // Put Page with Range only
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "update"), ("Range", "bytes=512-1023")], page('a')).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // both headers: x-ms-range wins (1024-1535), the Range value is ignored
    let r = e
        .send(
            "PUT",
            "/cnt/pb?comp=page",
            &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=1024-1535"), ("Range", "bytes=0-511")],
            page('b'),
        )
        .await;
    assert_eq!(r.status, 201);
    let r = e.send("GET", "/cnt/pb?comp=pagelist", &[], "").await;
    // 512-1023 and 1024-1535 touch, so they are one range
    assert_eq!(tags(&r.text(), "Start"), ["512"]);
    assert_eq!(tags(&r.text(), "End"), ["1535"]);
    // neither header: 400 naming x-ms-range
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "update")], page('c')).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    // a bad Range value is an invalid header, named as it was sent
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "update"), ("Range", "bytes=1-512")], page('c')).await;
    assert_eq!((r.status, code(&r).as_str()), (416, "InvalidPageRange"));
    // Get Page Ranges window with Range
    let r = e.send("GET", "/cnt/pb?comp=pagelist", &[("Range", "bytes=1024-2047")], "").await;
    assert_eq!(tags(&r.text(), "Start"), ["1024"]);
    // Clear Pages with Range
    let r = e.send("PUT", "/cnt/pb?comp=page", &[("x-ms-page-write", "clear"), ("Range", "bytes=512-1023")], "").await;
    assert_eq!(r.status, 201);
    let r = e.send("GET", "/cnt/pb?comp=pagelist", &[], "").await;
    assert_eq!(tags(&r.text(), "Start"), ["1024"]);
    // Get Blob with Range, and x-ms-range wins over it
    let r = e.send("GET", "/cnt/pb", &[("Range", "bytes=1024-1535")], "").await;
    assert_eq!((r.status, r.body.len()), (206, 512));
    let r = e.send("GET", "/cnt/pb", &[("Range", "bytes=0-9"), ("x-ms-range", "bytes=1024-1033")], "").await;
    assert_eq!(r.body.len(), 10);
    assert_eq!(&r.body[..], b"bbbbbbbbbb");
    // Put Page From URL with Range
    assert_eq!(e.send("PUT", "/cnt/src", &[("x-ms-blob-type", "BlockBlob")], "z".repeat(512)).await.status, 201);
    let src = format!("{}/cnt/src", e.base);
    let r = e
        .send(
            "PUT",
            "/cnt/pb?comp=page",
            &[
                ("x-ms-page-write", "update"),
                ("x-ms-copy-source", &src),
                ("x-ms-source-range", "bytes=0-511"),
                ("Range", "bytes=2048-2559"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = e.send("GET", "/cnt/pb", &[("x-ms-range", "bytes=2048-2051")], "").await;
    assert_eq!(&r.body[..], b"zzzz");
}
