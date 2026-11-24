//! The content of files: ranges, reads, range lists, leases, copies.

mod common;
use common::*;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use md5::{Digest, Md5};

fn md5(data: &[u8]) -> String {
    B64.encode(Md5::digest(data))
}

async fn put(f: &F, path: &str, range: &str, data: &str) -> azemu_core::testing::TestResponse {
    f.send("PUT", &format!("/{path}?comp=range"), &[("x-ms-range", range), ("x-ms-write", "update")], data.to_string()).await
}

#[tokio::test]
async fn put_range_writes_and_answers_with_the_md5() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 10).await;
    let r = put(&f, "sh1/f", "bytes=0-4", "hello").await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("content-md5"), Some(md5(b"hello").as_str()));
    assert_eq!(r.header("x-ms-request-server-encrypted"), Some("true"));
    assert!(r.header("x-ms-file-last-write-time").is_some() && r.header("etag").is_some());
    assert_eq!(f.read("sh1/f").await, "hello\0\0\0\0\0");
    // the `Range` header works as well
    let r = f.send("PUT", "/sh1/f?comp=range", &[("Range", "bytes=5-6"), ("x-ms-write", "update")], "ab").await;
    assert_eq!(r.status, 201);
    assert_eq!(f.read("sh1/f").await, "helloab\0\0\0");
    // a matching Content-MD5 passes; a wrong one is named with both values
    let good = md5(b"xy");
    assert_eq!(
        f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-1"), ("x-ms-write", "update"), ("Content-MD5", &good)], "xy")
            .await
            .status,
        201
    );
    let bad = md5(b"zz");
    let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-1"), ("x-ms-write", "update"), ("Content-MD5", &bad)], "xy").await;
    assert_eq!(failed(&r), (400, "Md5Mismatch".into()));
    assert_eq!(tag(&r.text(), "UserSpecifiedMd5").as_deref(), Some(bad.as_str()));
    assert_eq!(tag(&r.text(), "ServerCalculatedMd5").as_deref(), Some(good.as_str()));
}

#[tokio::test]
async fn put_range_checks_its_headers() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 10).await;
    let q = "/sh1/f?comp=range";
    let bad = |r: azemu_core::testing::TestResponse, code: &str, extra: &str| {
        assert_eq!(failed(&r), (400, code.to_string()));
        assert!(r.text().contains(extra), "{}", r.text());
    };
    bad(
        f.send("PUT", q, &[("x-ms-range", "bytes=0-3"), ("x-ms-write", "update")], "hello").await,
        "InvalidHeaderValue",
        "<HeaderName>Content-Length</HeaderName><HeaderValue>5</HeaderValue>",
    );
    bad(
        f.send("PUT", q, &[("x-ms-range", "bytes=5-2"), ("x-ms-write", "update")], "x").await,
        "InvalidHeaderValue",
        "<HeaderName>Range</HeaderName>",
    );
    bad(f.send("PUT", q, &[("x-ms-write", "update")], "x").await, "MissingRequiredHeader", "<HeaderName>Range</HeaderName>");
    bad(f.send("PUT", q, &[("x-ms-range", "bytes=0-0")], "x").await, "MissingRequiredHeader", "<HeaderName>x-ms-write</HeaderName>");
    bad(
        f.send("PUT", q, &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "bogus")], "x").await,
        "InvalidHeaderValue",
        "<HeaderValue>bogus</HeaderValue>",
    );
    bad(
        f.send("PUT", q, &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "clear")], "x").await,
        "InvalidHeaderValue",
        "<HeaderName>Content-Length</HeaderName>",
    );
    bad(
        f.send("PUT", q, &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update"), ("x-ms-file-last-write-time", "later")], "x").await,
        "InvalidHeaderValue",
        "x-ms-file-last-write-time",
    );
    // past the end of the file
    let r = f.send("PUT", q, &[("x-ms-range", "bytes=8-11"), ("x-ms-write", "update")], "abcd").await;
    assert_eq!(failed(&r), (416, "InvalidRange".into()));
    let r = f.send("PUT", q, &[("x-ms-range", "bytes=8-11"), ("x-ms-write", "clear")], "").await;
    assert_eq!(failed(&r), (416, "InvalidRange".into()));
}

#[tokio::test]
async fn the_last_write_time_can_be_preserved() {
    let f = start().await;
    f.share("sh1").await;
    f.send(
        "PUT",
        "/sh1/f",
        &[("x-ms-type", "file"), ("x-ms-content-length", "5"), ("x-ms-file-last-write-time", "2020-01-01T00:00:00.0000000Z")],
        "",
    )
    .await;
    let r = f
        .send(
            "PUT",
            "/sh1/f?comp=range",
            &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update"), ("x-ms-file-last-write-time", "Preserve")],
            "x",
        )
        .await;
    assert_eq!(r.header("x-ms-file-last-write-time"), Some("2020-01-01T00:00:00.0000000Z"));
    let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update")], "y").await;
    assert_ne!(r.header("x-ms-file-last-write-time"), Some("2020-01-01T00:00:00.0000000Z"));
}

#[tokio::test]
async fn a_range_bigger_than_four_mebibytes_is_refused_before_it_is_read() {
    let f = start().await;
    f.share("sh1").await;
    f.send("PUT", "/sh1/f", &[("x-ms-type", "file"), ("x-ms-content-length", "6291456")], "").await;
    let big = vec![b'x'; (4 << 20) + 1];
    let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-4194304"), ("x-ms-write", "update")], big).await;
    assert_eq!(failed(&r), (413, "RequestBodyTooLarge".into()));
    assert!(r.text().contains("<MaxLimit>4194304</MaxLimit>"));
    let ok = vec![b'x'; 4 << 20];
    assert_eq!(f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-4194303"), ("x-ms-write", "update")], ok).await.status, 201);
    // a clear has no body, so it can cover more
    assert_eq!(f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-5242879"), ("x-ms-write", "clear")], "").await.status, 201);
    assert!(f.send("GET", "/sh1/f?comp=rangelist", &[], "").await.text().ends_with("<Ranges />"));
}

#[tokio::test]
async fn reads_by_range() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 10).await;
    f.write("sh1/f", 0, "0123456789").await;
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=2-4")], "").await;
    assert_eq!(
        (r.status, r.text().as_str(), r.header("content-range"), r.header("content-length")),
        (206, "234", Some("bytes 2-4/10"), Some("3"))
    );
    let r = f.send("GET", "/sh1/f", &[("Range", "bytes=7-")], "").await;
    assert_eq!((r.status, r.text().as_str(), r.header("content-range")), (206, "789", Some("bytes 7-9/10")));
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=8-99")], "").await;
    assert_eq!((r.text().as_str(), r.header("content-range")), ("89", Some("bytes 8-9/10")));
    // x-ms-range wins over Range
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=0-0"), ("Range", "bytes=1-1")], "").await;
    assert_eq!(r.text(), "0");
    // md5 of the range
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=0-3"), ("x-ms-range-get-content-md5", "true")], "").await;
    assert_eq!(r.header("content-md5"), Some(md5(b"0123").as_str()));
    assert_eq!(failed(&f.send("GET", "/sh1/f", &[("x-ms-range-get-content-md5", "true")], "").await), (400, "InvalidHeaderValue".into()));
    // past the end
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=10-12")], "").await;
    assert_eq!((failed(&r), r.header("content-range")), ((416, "InvalidRange".into()), Some("bytes */10")));
    // a range that is not one is ignored
    assert_eq!(f.send("GET", "/sh1/f", &[("x-ms-range", "bogus")], "").await.status, 200);
    assert_eq!(f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=5-2")], "").await.status, 200);
    // a file of no size has no range
    f.file("sh1/empty", 0).await;
    let r = f.send("GET", "/sh1/empty", &[("x-ms-range", "bytes=0-0")], "").await;
    assert_eq!((r.status, r.header("content-range")), (416, Some("bytes */0")));
    assert_eq!(f.send("GET", "/sh1/empty", &[], "").await.status, 200);
}

#[tokio::test]
async fn a_whole_file_read_has_the_md5_the_client_gave() {
    let f = start().await;
    f.share("sh1").await;
    let m = md5(b"whole");
    f.send("PUT", "/sh1/f", &[("x-ms-type", "file"), ("x-ms-content-length", "5"), ("x-ms-content-md5", &m)], "").await;
    assert_eq!(f.send("GET", "/sh1/f", &[], "").await.header("content-md5"), Some(m.as_str()));
    assert_eq!(f.send("GET", "/sh1/f", &[("x-ms-range", "bytes=0-1")], "").await.header("content-md5"), None);
}

#[tokio::test]
async fn ranges_are_whole_pages_cut_at_the_size() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 2048).await;
    f.write("sh1/f", 0, "hello").await;
    f.write("sh1/f", 1030, "world").await;
    let r = f.send("GET", "/sh1/f?comp=rangelist", &[], "").await;
    assert_eq!(r.header("x-ms-content-length"), Some("2048"));
    assert!(r
        .text()
        .ends_with("<Ranges><Range><Start>0</Start><End>511</End></Range><Range><Start>1024</Start><End>1535</End></Range></Ranges>"));
    // clearing part of a page leaves the page; clearing all of it removes it
    f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-4"), ("x-ms-write", "clear")], "").await;
    assert_eq!(all(&f.send("GET", "/sh1/f?comp=rangelist", &[], "").await.text(), "Start"), vec!["1024"]);
    f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=1024-1535"), ("x-ms-write", "clear")], "").await;
    assert!(f.send("GET", "/sh1/f?comp=rangelist", &[], "").await.text().ends_with("<Ranges />"));
    // a small file ends its range at its size
    f.file("sh1/small", 11).await;
    f.write("sh1/small", 0, "hello").await;
    assert_eq!(all(&f.send("GET", "/sh1/small?comp=rangelist", &[], "").await.text(), "End"), vec!["10"]);
    // the window
    f.write("sh1/f", 600, "x").await;
    let t = f.send("GET", "/sh1/f?comp=rangelist", &[("x-ms-range", "bytes=0-599")], "").await.text();
    // the page 512 to 1023 holds the byte; the window stops at 599
    assert_eq!((all(&t, "Start"), all(&t, "End")), (vec!["512".to_string()], vec!["599".to_string()]));
}

#[tokio::test]
async fn range_differences_against_a_snapshot() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 2048).await;
    f.write("sh1/f", 0, "a").await;
    f.write("sh1/f", 1536, "b").await;
    let snap = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    f.write("sh1/f", 1024, "c").await;
    f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-511"), ("x-ms-write", "clear")], "").await;
    let t = f.send("GET", &format!("/sh1/f?comp=rangelist&prevsharesnapshot={snap}"), &[], "").await.text();
    assert!(
        t.contains(
            "<Ranges><Range><Start>1024</Start><End>1535</End></Range><ClearRange><Start>0</Start><End>511</End></ClearRange></Ranges>"
        ),
        "{t}"
    );
    // a file that was not there is all new
    f.file("sh1/new", 512).await;
    f.write("sh1/new", 0, "n").await;
    let t = f.send("GET", &format!("/sh1/new?comp=rangelist&prevsharesnapshot={snap}"), &[], "").await.text();
    assert!(t.contains("<Range><Start>0</Start><End>511</End></Range>"));
    assert_eq!(
        failed(&f.send("GET", "/sh1/f?comp=rangelist&prevsharesnapshot=2020-01-01T00:00:00.0000000Z", &[], "").await),
        (409, "PreviousSnapshotNotFound".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1/f?comp=rangelist&prevsharesnapshot=x", &[], "").await),
        (400, "InvalidQueryParameterValue".into())
    );
}

#[tokio::test]
async fn file_leases() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 4).await;
    let (a, b) = ("11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222");
    let lease = |h: Vec<(&'static str, &'static str)>| {
        let f = &f;
        async move { f.send("PUT", "/sh1/f?comp=lease", &h, "").await }
    };
    // no lease yet: an ID is wrong, nothing else is
    let r = f.send("PUT", "/sh1/f?comp=metadata", &[("x-ms-lease-id", a), ("x-ms-meta-k", "v")], "").await;
    assert_eq!(failed(&r), (412, "LeaseNotPresentWithFileOperation".into()));
    assert_eq!(
        failed(&lease(vec![("x-ms-lease-action", "release"), ("x-ms-lease-id", a)]).await),
        (409, "LeaseIdMismatchWithLeaseOperation".into())
    );
    assert_eq!(failed(&lease(vec![("x-ms-lease-action", "break")]).await), (409, "LeaseNotPresentWithLeaseOperation".into()));
    assert_eq!(
        failed(&lease(vec![("x-ms-lease-action", "change"), ("x-ms-lease-id", a), ("x-ms-proposed-lease-id", b)]).await),
        (409, "LeaseNotPresentWithLeaseOperation".into())
    );
    let r = lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", a)]).await;
    assert_eq!((r.status, r.header("x-ms-lease-id")), (201, Some(a)));
    let h = f.send("HEAD", "/sh1/f", &[], "").await;
    assert_eq!(
        (h.header("x-ms-lease-state"), h.header("x-ms-lease-status"), h.header("x-ms-lease-duration")),
        (Some("leased"), Some("locked"), Some("infinite"))
    );
    // what changes the file needs the ID; a read does not
    for (m, p) in [("PUT", "/sh1/f?comp=metadata"), ("PUT", "/sh1/f?comp=properties"), ("DELETE", "/sh1/f")] {
        assert_eq!(failed(&f.send(m, p, &[], "").await), (412, "LeaseIdMissing".into()), "{m} {p}");
        assert_eq!(failed(&f.send(m, p, &[("x-ms-lease-id", b)], "").await), (412, "LeaseIdMismatchWithFileOperation".into()), "{m} {p}");
    }
    assert_eq!(
        failed(&f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update")], "x").await),
        (412, "LeaseIdMissing".into())
    );
    assert_eq!(
        f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update"), ("x-ms-lease-id", a)], "x")
            .await
            .status,
        201
    );
    assert_eq!(f.send("GET", "/sh1/f", &[], "").await.status, 200);
    assert_eq!(
        failed(&f.send("PUT", "/sh1/f", &[("x-ms-type", "file"), ("x-ms-content-length", "1")], "").await),
        (412, "LeaseIdMissing".into())
    );
    // a file lease has no renew; a second acquire with another ID is refused, with the same one it is not
    assert_eq!(failed(&lease(vec![("x-ms-lease-action", "renew"), ("x-ms-lease-id", a)]).await), (400, "InvalidHeaderValue".into()));
    assert_eq!(
        failed(&lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", b)]).await),
        (409, "LeaseAlreadyPresent".into())
    );
    assert_eq!(
        lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", a)]).await.status,
        201
    );
    let r = lease(vec![("x-ms-lease-action", "change"), ("x-ms-lease-id", a), ("x-ms-proposed-lease-id", b)]).await;
    assert_eq!((r.status, r.header("x-ms-lease-id")), (200, Some(b)));
    assert_eq!(failed(&lease(vec![("x-ms-lease-action", "change"), ("x-ms-lease-id", b)]).await), (400, "MissingRequiredHeader".into()));
    // a break is at once: the file is free
    let r = lease(vec![("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "20")]).await;
    assert_eq!((r.status, r.header("x-ms-lease-time")), (202, Some("0")));
    let h = f.send("HEAD", "/sh1/f", &[], "").await;
    assert_eq!((h.header("x-ms-lease-state"), h.header("x-ms-lease-status")), (Some("broken"), Some("unlocked")));
    assert_eq!(f.send("PUT", "/sh1/f?comp=metadata", &[("x-ms-meta-k", "v")], "").await.status, 200);
    // release
    lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", a)]).await;
    assert_eq!(lease(vec![("x-ms-lease-action", "release"), ("x-ms-lease-id", a)]).await.status, 200);
    assert_eq!(f.send("HEAD", "/sh1/f", &[], "").await.header("x-ms-lease-state"), Some("available"));
    // header checks come first
    assert_eq!(
        failed(&lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "5")]).await),
        (400, "InvalidHeaderValue".into())
    );
    assert_eq!(failed(&lease(vec![("x-ms-lease-action", "acquire")]).await), (400, "MissingRequiredHeader".into()));
    assert_eq!(failed(&lease(vec![("x-ms-lease-action", "bogus")]).await), (400, "InvalidHeaderValue".into()));
    assert_eq!(failed(&lease(vec![]).await), (400, "MissingRequiredHeader".into()));
    assert_eq!(
        failed(&lease(vec![("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", "nope")]).await),
        (400, "InvalidHeaderValue".into())
    );
}

#[tokio::test]
async fn share_leases() {
    let f = start().await;
    f.share("sh1").await;
    let (a, b) = ("11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222");
    let q = "/sh1?restype=share&comp=lease";
    let r = f.send("PUT", q, &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", a)], "").await;
    assert_eq!((r.status, r.header("x-ms-lease-id")), (201, Some(a)));
    let g = f.send("GET", "/sh1?restype=share", &[], "").await;
    assert_eq!((g.header("x-ms-lease-state"), g.header("x-ms-lease-duration")), (Some("leased"), Some("infinite")));
    // the share is protected, its content is not
    assert_eq!(failed(&f.send("DELETE", "/sh1?restype=share", &[], "").await), (412, "LeaseIdMissing".into()));
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-lease-id", b)], "").await),
        (412, "LeaseIdMismatchWithContainerOperation".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1?restype=share", &[("x-ms-lease-id", b)], "").await),
        (412, "LeaseIdMismatchWithContainerOperation".into())
    );
    assert_eq!(f.send("GET", "/sh1?restype=share", &[("x-ms-lease-id", a)], "").await.status, 200);
    assert_eq!(f.send("PUT", "/sh1/d?restype=directory", &[], "").await.status, 201);
    assert_eq!(f.send("GET", "/sh1?restype=directory&comp=list", &[("x-ms-lease-id", b)], "").await.status, 200);
    // renew, change
    assert_eq!(f.send("PUT", q, &[("x-ms-lease-action", "renew"), ("x-ms-lease-id", a)], "").await.status, 200);
    let r = f.send("PUT", q, &[("x-ms-lease-action", "change"), ("x-ms-lease-id", a), ("x-ms-proposed-lease-id", b)], "").await;
    assert_eq!(r.header("x-ms-lease-id"), Some(b));
    assert_eq!(
        failed(&f.send("PUT", q, &[("x-ms-lease-action", "release"), ("x-ms-lease-id", a)], "").await),
        (409, "LeaseIdMismatchWithLeaseOperation".into())
    );
    // a break of an infinite lease takes the period asked for; a second break can only shorten it
    let r = f.send("PUT", q, &[("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "30")], "").await;
    assert_eq!((r.status, r.header("x-ms-lease-time")), (202, Some("30")));
    assert_eq!(f.send("GET", "/sh1?restype=share", &[], "").await.header("x-ms-lease-state"), Some("breaking"));
    let r = f.send("PUT", q, &[("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "5")], "").await;
    assert_eq!(r.header("x-ms-lease-time"), Some("5"));
    let r = f.send("PUT", q, &[("x-ms-lease-action", "break"), ("x-ms-lease-break-period", "0")], "").await;
    assert_eq!(r.header("x-ms-lease-time"), Some("0"));
    assert_eq!(f.send("GET", "/sh1?restype=share", &[], "").await.header("x-ms-lease-state"), Some("broken"));
    assert_eq!(
        failed(&f.send("PUT", q, &[("x-ms-lease-action", "renew"), ("x-ms-lease-id", b)], "").await),
        (409, "LeaseIsBrokenAndCannotBeRenewed".into())
    );
    // a fixed lease lasts 15 to 60 seconds
    assert_eq!(
        failed(&f.send("PUT", q, &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "70")], "").await),
        (400, "InvalidHeaderValue".into())
    );
    let r = f.send("PUT", q, &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "15")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(f.send("GET", "/sh1?restype=share", &[], "").await.header("x-ms-lease-duration"), Some("fixed"));
    assert_eq!(
        failed(&f.send("PUT", "/sh1?restype=share&comp=metadata", &[("x-ms-lease-id", a)], "").await),
        (412, "LeaseIdMismatchWithContainerOperation".into())
    );
    // a lease ID on a share with no lease
    f.share("sh2").await;
    assert_eq!(
        failed(&f.send("PUT", "/sh2?restype=share&comp=metadata", &[("x-ms-lease-id", a)], "").await),
        (412, "LeaseNotPresentWithContainerOperation".into())
    );
}

#[tokio::test]
async fn copy_file_within_a_share_and_between_accounts() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/src", 5).await;
    f.write("sh1/src", 0, "hello").await;
    f.send("PUT", "/sh1/src?comp=metadata", &[("x-ms-meta-m", "src")], "").await;
    f.send("PUT", "/sh1/src?comp=properties", &[("x-ms-content-type", "text/x-src"), ("x-ms-file-attributes", "ReadOnly|Archive")], "")
        .await;
    let src = format!("{}/sh1/src", f.base);
    let r = f.send("PUT", "/sh1/dst", &[("x-ms-copy-source", &src)], "").await;
    assert_eq!(r.status, 202);
    assert_eq!(r.header("x-ms-copy-status"), Some("success"));
    let id = r.header("x-ms-copy-id").unwrap().to_string();
    assert_eq!(f.read("sh1/dst").await, "hello");
    let h = f.send("HEAD", "/sh1/dst", &[], "").await;
    assert_eq!(
        (h.header("content-type"), h.header("x-ms-meta-m"), h.header("x-ms-copy-id"), h.header("x-ms-copy-status")),
        (Some("text/x-src"), Some("src"), Some(id.as_str()), Some("success"))
    );
    assert_eq!((h.header("x-ms-copy-progress"), h.header("x-ms-copy-source")), (Some("5/5"), Some(src.as_str())));
    assert!(h.header("x-ms-copy-completion-time").is_some());
    // what a copy takes: Archive and no more of the source's attributes, the permission of the source
    assert_eq!(h.header("x-ms-file-attributes"), Some("Archive"));
    let h2 =
        f.send("PUT", "/sh1/dst2", &[("x-ms-copy-source", &src), ("x-ms-file-attributes", "Hidden"), ("x-ms-meta-m", "dst")], "").await;
    assert_eq!(h2.status, 202);
    let h2 = f.send("HEAD", "/sh1/dst2", &[], "").await;
    assert_eq!((h2.header("x-ms-file-attributes"), h2.header("x-ms-meta-m")), (Some("Hidden | Archive"), Some("dst")));
    // a read-only destination
    f.send("PUT", "/sh1/ro", &[("x-ms-type", "file"), ("x-ms-content-length", "1"), ("x-ms-file-attributes", "ReadOnly")], "").await;
    assert_eq!(failed(&f.send("PUT", "/sh1/ro", &[("x-ms-copy-source", &src)], "").await), (409, "ReadOnlyAttribute".into()));
    assert_eq!(f.send("PUT", "/sh1/ro", &[("x-ms-copy-source", &src), ("x-ms-file-copy-ignore-readonly", "true")], "").await.status, 202);
    // from a snapshot
    let snap = f.send("PUT", "/sh1?restype=share&comp=snapshot", &[], "").await.header("x-ms-snapshot").unwrap().to_string();
    f.write("sh1/src", 0, "HELLO").await;
    let old = format!("{src}?sharesnapshot={snap}");
    f.send("PUT", "/sh1/fromsnap", &[("x-ms-copy-source", &old)], "").await;
    assert_eq!(f.read("sh1/fromsnap").await, "hello");
    // a second account of the emulator
    let other = format!("{}/sh1/src", f.srv.url("file.core", "acct2"));
    assert_eq!(failed(&f.send("PUT", "/sh1/x", &[("x-ms-copy-source", &other)], "").await), (404, "CannotVerifyCopySource".into()));
}

#[tokio::test]
async fn copy_errors_and_abort() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/src", 1).await;
    let src = format!("{}/sh1/src", f.base);
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d", &[("x-ms-copy-source", &format!("{}/sh1/nope", f.base))], "").await),
        (404, "ResourceNotFound".into())
    );
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d", &[("x-ms-copy-source", &format!("{}/nosh/x", f.base))], "").await),
        (404, "ShareNotFound".into())
    );
    assert_eq!(failed(&f.send("PUT", "/sh1/d", &[("x-ms-copy-source", "bogus")], "").await), (400, "InvalidHeaderValue".into()));
    assert_eq!(failed(&f.send("PUT", "/sh1/nodir/d", &[("x-ms-copy-source", &src)], "").await), (404, "ParentNotFound".into()));
    f.dir("sh1/dir").await;
    assert_eq!(failed(&f.send("PUT", "/sh1/dir", &[("x-ms-copy-source", &src)], "").await), (409, "ResourceTypeMismatch".into()));
    // the source can be a blob in Azure; here that is not built
    let blob = "http://acct1.blob.core.localhost:1/c/b";
    assert_eq!(f.send("PUT", "/sh1/d", &[("x-ms-copy-source", blob)], "").await.status, 501);
    // every copy is done when it answers, so there is nothing to abort
    let id = f.send("PUT", "/sh1/d", &[("x-ms-copy-source", &src)], "").await.header("x-ms-copy-id").unwrap().to_string();
    let abort = |id: &str| format!("/sh1/d?comp=copy&copyid={id}");
    assert_eq!(failed(&f.send("PUT", &abort(&id), &[("x-ms-copy-action", "abort")], "").await), (409, "NoPendingCopyOperation".into()));
    assert_eq!(
        failed(&f.send("PUT", &abort("00000000-0000-0000-0000-000000000000"), &[("x-ms-copy-action", "abort")], "").await),
        (409, "NoPendingCopyOperation".into())
    );
    assert_eq!(failed(&f.send("PUT", &abort("x"), &[("x-ms-copy-action", "abort")], "").await), (400, "InvalidQueryParameterValue".into()));
    assert_eq!(failed(&f.send("PUT", &abort(&id), &[], "").await), (400, "MissingRequiredHeader".into()));
}

#[tokio::test]
async fn put_range_from_url() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/src", 10).await;
    f.write("sh1/src", 0, "0123456789").await;
    f.file("sh1/dst", 20).await;
    let src = format!("{}/sh1/src", f.base);
    let r = f
        .send(
            "PUT",
            "/sh1/dst?comp=range",
            &[("x-ms-copy-source", &src), ("x-ms-source-range", "bytes=2-5"), ("x-ms-range", "bytes=10-13"), ("x-ms-write", "update")],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(f.send("GET", "/sh1/dst", &[("x-ms-range", "bytes=10-13")], "").await.text(), "2345");
    let bad = |source_range: &'static str, range: &'static str| {
        let (f, src) = (&f, src.clone());
        async move {
            f.send(
                "PUT",
                "/sh1/dst?comp=range",
                &[("x-ms-copy-source", &src), ("x-ms-source-range", source_range), ("x-ms-range", range), ("x-ms-write", "update")],
                "",
            )
            .await
        }
    };
    assert_eq!(failed(&bad("bytes=2-5", "bytes=10-12").await), (400, "InvalidHeaderValue".into()));
    assert_eq!(failed(&bad("bytes=8-12", "bytes=0-4").await), (416, "InvalidRange".into()));
    assert_eq!(failed(&bad("bytes=0-3", "bytes=18-21").await), (416, "InvalidRange".into()));
    let r = f
        .send("PUT", "/sh1/dst?comp=range", &[("x-ms-copy-source", &src), ("x-ms-range", "bytes=0-3"), ("x-ms-write", "update")], "")
        .await;
    assert_eq!(failed(&r), (400, "MissingRequiredHeader".into()));
    let r = f
        .send(
            "PUT",
            "/sh1/dst?comp=range",
            &[
                ("x-ms-copy-source", &format!("{}/sh1/nope", f.base)),
                ("x-ms-source-range", "bytes=0-3"),
                ("x-ms-range", "bytes=0-3"),
                ("x-ms-write", "update"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 404);
}
