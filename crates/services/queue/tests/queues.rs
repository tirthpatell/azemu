mod common;
use common::*;

#[tokio::test]
async fn create_is_201_then_204_for_same_metadata_and_409_for_different() {
    let q = start().await;
    let r = q.send("PUT", "/myqueue", &[("x-ms-meta-env", "test")], "").await;
    assert_eq!(r.status, 201);
    assert!(r.header("x-ms-request-id").is_some());

    let r = q.send("PUT", "/myqueue", &[("x-ms-meta-env", "test")], "").await;
    assert_eq!(r.status, 204, "same name and same metadata is not an error");

    let r = q.send("PUT", "/myqueue", &[("x-ms-meta-env", "prod")], "").await;
    assert_eq!(r.status, 409);
    assert_eq!(error_code(&r), "QueueAlreadyExists");
    assert_eq!(tag(&r.text(), "Code").as_deref(), Some("QueueAlreadyExists"));

    let r = q.send("PUT", "/myqueue", &[], "").await;
    assert_eq!(r.status, 409, "no metadata differs from existing metadata");
}

#[tokio::test]
async fn create_without_metadata_twice_is_204() {
    let q = start().await;
    q.create("plain").await;
    assert_eq!(q.send("PUT", "/plain", &[], "").await.status, 204);
}

#[tokio::test]
async fn queue_name_rules() {
    let q = start().await;
    for bad in ["UpperCase", "my--queue", "-abc", "abc-", "under_score", "%E5%95%8A%E9%BD%84"] {
        let r = q.send("PUT", &format!("/{bad}"), &[], "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidResourceName"), "{bad}");
    }
    for bad in ["ab", &"a".repeat(64)] {
        let r = q.send("PUT", &format!("/{bad}"), &[], "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeInput"), "{bad}");
    }
    for ok in ["abc", &"a".repeat(63), "a-b-c"] {
        assert_eq!(q.send("PUT", &format!("/{ok}"), &[], "").await.status, 201, "{ok}");
    }
    // the name is checked on every queue operation, not only on create
    let r = q.send("GET", "/Bad_Name?comp=metadata", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidResourceName"));
}

#[tokio::test]
async fn invalid_metadata_names_and_sizes_are_rejected() {
    let q = start().await;
    let r = q.send("PUT", "/metaq", &[("x-ms-meta-1bad", "v")], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidMetadata"));
    let big = "x".repeat(9000);
    let r = q.send("PUT", "/metaq", &[("x-ms-meta-big", &big)], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MetadataTooLarge"));
}

#[tokio::test]
async fn delete_and_missing_queue_errors() {
    let q = start().await;
    q.create("gone").await;
    assert_eq!(q.send("DELETE", "/gone", &[], "").await.status, 204);
    let r = q.send("DELETE", "/gone", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
    for (m, p) in [
        ("GET", "/gone?comp=metadata"),
        ("PUT", "/gone?comp=metadata"),
        ("GET", "/gone?comp=acl"),
        ("PUT", "/gone?comp=acl"),
        ("GET", "/gone/messages"),
        ("GET", "/gone/messages?peekonly=true"),
        ("DELETE", "/gone/messages"),
    ] {
        let r = q.send(m, p, &[], "").await;
        assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"), "{m} {p}");
    }
    let r = q.put("gone", "x", "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
    let r = q.send("DELETE", "/gone/messages/abc?popreceipt=x", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
}

#[tokio::test]
async fn error_body_has_standard_shape() {
    let q = start().await;
    let r = q.send("GET", "/nope?comp=metadata", &[("x-ms-client-request-id", "cid-7")], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    assert_eq!(r.header("x-ms-client-request-id"), Some("cid-7"));
    assert!(r.header("server").unwrap().starts_with("Windows-Azure-Queue/1.0"));
    assert_eq!(&r.body[..3], &[0xEF, 0xBB, 0xBF], "body starts with a byte order mark");
    let body = r.text();
    assert!(body.contains("<Message>The specified queue does not exist.\nRequestId:"));
    assert!(body.contains(&format!("RequestId:{}", r.header("x-ms-request-id").unwrap())));
}

#[tokio::test]
async fn x_ms_version_is_echoed() {
    let q = start().await;
    let r = q.send("PUT", "/ver", &[("x-ms-version", "2021-02-12")], "").await;
    assert_eq!(r.header("x-ms-version"), Some("2021-02-12"));
    let r = q.send("GET", "/ver?comp=metadata", &[], "").await;
    assert!(r.header("x-ms-version").is_some());
}

#[tokio::test]
async fn metadata_set_get_and_message_count() {
    let q = start().await;
    q.create("meta1").await;
    let r = q.send("GET", "/meta1?comp=metadata", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-approximate-messages-count"), Some("0"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));

    let r = q.send("PUT", "/meta1?comp=metadata", &[("x-ms-meta-a", "1"), ("x-ms-meta-b", "2")], "").await;
    assert_eq!(r.status, 204);
    q.put("meta1", "m", "").await;
    q.put("meta1", "m", "visibilitytimeout=60").await;
    let r = q.send("GET", "/meta1?comp=metadata", &[], "").await;
    assert_eq!(r.header("x-ms-meta-a"), Some("1"));
    assert_eq!(r.header("x-ms-meta-b"), Some("2"));
    assert_eq!(r.header("x-ms-approximate-messages-count"), Some("2"), "hidden messages are counted");

    // set replaces everything
    q.send("PUT", "/meta1?comp=metadata", &[("x-ms-meta-c", "3")], "").await;
    let r = q.send("GET", "/meta1?comp=metadata", &[], "").await;
    assert_eq!(r.header("x-ms-meta-a"), None);
    assert_eq!(r.header("x-ms-meta-c"), Some("3"));
    // no metadata headers clears it
    q.send("PUT", "/meta1?comp=metadata", &[], "").await;
    let r = q.send("GET", "/meta1?comp=metadata", &[], "").await;
    assert!(r.headers.keys().all(|k| !k.as_str().starts_with("x-ms-meta-")));
}

#[tokio::test]
async fn list_queues_prefix_marker_maxresults_and_metadata() {
    let q = start().await;
    for n in ["list-a", "list-b", "list-c", "list-d", "other-1"] {
        if n == "list-b" {
            q.send("PUT", "/list-b", &[("x-ms-meta-team", "alpha")], "").await;
        } else {
            q.create(n).await;
        }
    }

    let r = q.send("GET", "/?comp=list&prefix=list-", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    let body = r.text();
    assert_eq!(tags(&body, "Name"), ["list-a", "list-b", "list-c", "list-d"]);
    assert!(body.contains("<Prefix>list-</Prefix>"));
    assert!(body.contains(&format!("ServiceEndpoint=\"{}/\"", q.base)));
    assert!(body.contains("<NextMarker />"));
    assert!(!body.contains("<Metadata"), "metadata only with include=metadata");
    assert!(!body.contains("<MaxResults>"));

    // pages of 3
    let r = q.send("GET", "/?comp=list&prefix=list-&maxresults=3", &[], "").await;
    let body = r.text();
    assert_eq!(tags(&body, "Name"), ["list-a", "list-b", "list-c"]);
    assert!(body.contains("<MaxResults>3</MaxResults>"));
    let marker = tag(&body, "NextMarker").unwrap();
    assert_eq!(marker, "/acct1/list-d");
    let r = q.send("GET", &format!("/?comp=list&prefix=list-&maxresults=3&marker={}&include=metadata", urlenc(&marker)), &[], "").await;
    let body = r.text();
    assert_eq!(tags(&body, "Name"), ["list-d"]);
    assert!(body.contains(&format!("<Marker>{marker}</Marker>")));
    assert!(body.contains("<Metadata />"));
    assert!(body.contains("<NextMarker />"));

    let r = q.send("GET", "/?comp=list&prefix=list-b&include=metadata", &[], "").await;
    assert!(r.text().contains("<Metadata><team>alpha</team></Metadata>"));

    // nothing matches
    let r = q.send("GET", "/?comp=list&prefix=zzz", &[], "").await;
    assert!(r.text().contains("<Queues />"));
}

#[tokio::test]
async fn list_queues_bad_parameters() {
    let q = start().await;
    let r = q.send("GET", "/?comp=list&maxresults=0", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"));
    let r = q.send("GET", "/?comp=list&maxresults=abc", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
    let r = q.send("GET", "/?comp=list&include=snapshots", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
}

#[tokio::test]
async fn accounts_are_separate() {
    let q = start().await;
    q.create("shared").await;
    let other = q.srv.url("queue", "acct2");
    let h = [("Authorization", "SharedKey acct2:c2ln")];
    let r = q.srv.send("GET", &format!("{other}/shared?comp=metadata"), &h, "").await;
    assert_eq!(r.status, 404);
    assert_eq!(q.srv.send("PUT", &format!("{other}/shared"), &h, "").await.status, 201);
}

#[tokio::test]
async fn reset_drops_everything() {
    let q = start().await;
    q.create("xq1").await;
    q.put("xq1", "m", "").await;
    q.srv.reset();
    assert_eq!(q.send("GET", "/xq1?comp=metadata", &[], "").await.status, 404);
}

#[tokio::test]
async fn acl_round_trip_and_limits() {
    let q = start().await;
    q.create("aclq").await;

    let r = q.send("GET", "/aclq?comp=acl", &[], "").await;
    assert_eq!(r.status, 200);
    assert!(r.text().ends_with("<SignedIdentifiers />"));

    let one = "<SignedIdentifiers><SignedIdentifier><Id>reader</Id><AccessPolicy><Start>2022-10-27T22:01:36Z</Start><Expiry>2099-10-27T23:06:36Z</Expiry><Permission>rp</Permission></AccessPolicy></SignedIdentifier><SignedIdentifier><Id>bare</Id></SignedIdentifier></SignedIdentifiers>";
    let r = q.send("PUT", "/aclq?comp=acl", &[], one).await;
    assert_eq!(r.status, 204);
    let r = q.send("GET", "/aclq?comp=acl", &[], "").await;
    let body = r.text();
    assert!(body.contains("<Id>reader</Id><AccessPolicy><Start>2022-10-27T22:01:36.0000000Z</Start><Expiry>2099-10-27T23:06:36.0000000Z</Expiry><Permission>rp</Permission></AccessPolicy>"));
    assert!(body.contains("<SignedIdentifier><Id>bare</Id></SignedIdentifier>"));

    // five fit, six do not
    let ids = |n: usize| {
        let mut s = String::from("<SignedIdentifiers>");
        for i in 0..n {
            s.push_str(&format!(
                "<SignedIdentifier><Id>id{i}</Id><AccessPolicy><Permission>r</Permission></AccessPolicy></SignedIdentifier>"
            ));
        }
        s.push_str("</SignedIdentifiers>");
        s
    };
    assert_eq!(q.send("PUT", "/aclq?comp=acl", &[], &ids(5)).await.status, 204);
    let r = q.send("PUT", "/aclq?comp=acl", &[], &ids(6)).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    // the failed request changed nothing
    assert_eq!(tags(&q.send("GET", "/aclq?comp=acl", &[], "").await.text(), "Id").len(), 5);

    // duplicate ids, bad permission letters
    let dup = "<SignedIdentifiers><SignedIdentifier><Id>a</Id></SignedIdentifier><SignedIdentifier><Id>a</Id></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(q.send("PUT", "/aclq?comp=acl", &[], dup).await.status, 400);
    let bad = "<SignedIdentifiers><SignedIdentifier><Id>a</Id><AccessPolicy><Permission>z</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(q.send("PUT", "/aclq?comp=acl", &[], bad).await.status, 400);

    // an empty list clears
    assert_eq!(q.send("PUT", "/aclq?comp=acl", &[], "<SignedIdentifiers />").await.status, 204);
    assert!(q.send("GET", "/aclq?comp=acl", &[], "").await.text().ends_with("<SignedIdentifiers />"));
}

#[tokio::test]
async fn head_works_for_metadata_and_acl() {
    let q = start().await;
    q.send("PUT", "/headq", &[("x-ms-meta-k", "v")], "").await;
    let r = q.send("HEAD", "/headq?comp=metadata", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-meta-k"), Some("v"));
    assert_eq!(r.header("x-ms-approximate-messages-count"), Some("0"));
    assert!(r.body.is_empty());
    assert_eq!(q.send("HEAD", "/headq?comp=acl", &[], "").await.status, 200);
    let r = q.send("HEAD", "/nosuchq?comp=metadata", &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(error_code(&r), "QueueNotFound");
}
