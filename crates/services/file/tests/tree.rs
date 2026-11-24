//! Directories and files: create, properties, metadata, SMB attributes, listing, rename, delete, names.

mod common;
use common::*;

#[tokio::test]
async fn directories_are_created_read_and_deleted() {
    let f = start().await;
    f.share("sh1").await;
    let r = f.send("PUT", "/sh1/d1?restype=directory", &[("x-ms-meta-k", "v")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("x-ms-file-attributes"), Some("Directory"));
    assert_eq!(r.header("x-ms-file-permission-key"), Some("4905121398068318397*3227211760911779636"));
    assert_eq!(r.header("x-ms-request-server-encrypted"), Some("true"));
    for h in ["x-ms-file-creation-time", "x-ms-file-last-write-time", "x-ms-file-change-time", "x-ms-file-id", "x-ms-file-parent-id"] {
        assert!(r.header(h).is_some(), "{h}");
    }
    assert_eq!(failed(&f.send("PUT", "/sh1/d1?restype=directory", &[], "").await), (409, "ResourceAlreadyExists".into()));
    let g = f.send("GET", "/sh1/d1?restype=directory", &[], "").await;
    assert_eq!((g.status, g.header("x-ms-meta-k"), g.header("x-ms-server-encrypted")), (200, Some("v"), Some("true")));
    assert_eq!(g.header("x-ms-file-id"), r.header("x-ms-file-id"));
    assert_eq!(f.send("DELETE", "/sh1/d1?restype=directory", &[], "").await.status, 202);
    assert_eq!(failed(&f.send("GET", "/sh1/d1?restype=directory", &[], "").await), (404, "ResourceNotFound".into()));
    assert_eq!(failed(&f.send("HEAD", "/sh1/d1?restype=directory", &[], "").await), (404, "ResourceNotFound".into()));
}

#[tokio::test]
async fn the_root_of_a_share_is_a_directory() {
    let f = start().await;
    f.share("sh1").await;
    for p in ["/sh1?restype=directory", "/sh1/?restype=directory"] {
        let r = f.send("GET", p, &[], "").await;
        assert_eq!(r.status, 200);
        assert_eq!(
            (r.header("x-ms-file-id"), r.header("x-ms-file-parent-id"), r.header("x-ms-server-encrypted")),
            (Some("0"), Some("0"), Some("false"))
        );
        assert!(r.header("x-ms-file-permission-key").is_none());
    }
    assert_eq!(failed(&f.send("PUT", "/sh1?restype=directory", &[], "").await), (409, "ResourceAlreadyExists".into()));
    assert_eq!(failed(&f.send("DELETE", "/sh1?restype=directory", &[], "").await), (409, "ResourceTypeMismatch".into()));
}

#[tokio::test]
async fn parents_must_exist_and_types_must_match() {
    let f = start().await;
    f.share("sh1").await;
    assert_eq!(failed(&f.send("PUT", "/sh1/a/b?restype=directory", &[], "").await), (404, "ParentNotFound".into()));
    assert_eq!(failed(&f.send("GET", "/sh1/a/b?restype=directory", &[], "").await), (404, "ParentNotFound".into()));
    assert_eq!(failed(&f.send("DELETE", "/sh1/a/b?restype=directory", &[], "").await), (404, "ParentNotFound".into()));
    assert_eq!(failed(&f.send("GET", "/sh1/a/f.txt", &[], "").await), (404, "ParentNotFound".into()));
    f.file("sh1/f.txt", 1).await;
    f.dir("sh1/d").await;
    assert_eq!(failed(&f.send("PUT", "/sh1/f.txt?restype=directory", &[], "").await), (409, "ResourceTypeMismatch".into()));
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d", &[("x-ms-type", "file"), ("x-ms-content-length", "1")], "").await),
        (409, "ResourceTypeMismatch".into())
    );
    // a file is not found as a directory, nor a directory as a file
    assert_eq!(failed(&f.send("GET", "/sh1/f.txt?restype=directory", &[], "").await), (404, "ResourceNotFound".into()));
    assert_eq!(failed(&f.send("GET", "/sh1/d", &[], "").await), (404, "ResourceNotFound".into()));
    assert_eq!(failed(&f.send("DELETE", "/sh1/d", &[], "").await), (404, "ResourceNotFound".into()));
    // a file in the way of a directory path
    assert_eq!(failed(&f.send("PUT", "/sh1/f.txt/sub?restype=directory", &[], "").await), (404, "ParentNotFound".into()));
}

#[tokio::test]
async fn delete_needs_an_empty_directory() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    f.file("sh1/d/f.txt", 1).await;
    assert_eq!(failed(&f.send("DELETE", "/sh1/d?restype=directory", &[], "").await), (409, "DirectoryNotEmpty".into()));
    assert_eq!(f.send("DELETE", "/sh1/d/f.txt", &[], "").await.status, 202);
    assert_eq!(f.send("DELETE", "/sh1/d?restype=directory", &[], "").await.status, 202);
}

#[tokio::test]
async fn names_follow_the_rules_of_a_windows_file_system() {
    let f = start().await;
    f.share("sh1").await;
    for bad in ["a:b", "a*b", "a%3Fb", "a%3Cb", "a%3Eb", "a%7Cb", "a%22b", "CON", "aux", "Nul", "com1", "LPT9"] {
        let r = f.send("PUT", &format!("/sh1/{bad}?restype=directory"), &[], "").await;
        assert_eq!(failed(&r), (400, "InvalidResourceName".into()), "{bad}");
    }
    let r = f.send("PUT", &format!("/sh1/{}?restype=directory", "x".repeat(256)), &[], "").await;
    assert_eq!(failed(&r), (400, "InvalidResourceName".into()));
    for ok in ["con.txt", "a%20b", "%24a", "%C3%BCn%C3%AF", "a%23b", "a%25b", &"x".repeat(255)] {
        assert_eq!(f.send("PUT", &format!("/sh1/{ok}?restype=directory"), &[], "").await.status, 201, "{ok}");
    }
    // depth and length of the path
    let deep = vec!["d"; 251].join("/");
    let r = f.send("PUT", &format!("/sh1/{deep}?restype=directory"), &[], "").await;
    assert_eq!(failed(&r), (400, "InvalidFileOrDirectoryPathName".into()));
    assert!(r.text().contains("File or directory path has too many subdirectories."));
    let long = vec!["x".repeat(200); 11].join("/");
    let r = f.send("PUT", &format!("/sh1/{long}?restype=directory"), &[], "").await;
    assert!(r.text().contains("File or directory path is too long."));
}

#[tokio::test]
async fn names_ignore_case_trailing_dots_and_backslashes() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/MixedCase").await;
    assert_eq!(failed(&f.send("PUT", "/sh1/mixedcase?restype=directory", &[], "").await), (409, "ResourceAlreadyExists".into()));
    assert_eq!(f.send("GET", "/sh1/MIXEDCASE?restype=directory", &[], "").await.status, 200);
    let l = f.send("GET", "/sh1?restype=directory&comp=list", &[], "").await.text();
    assert_eq!(all(&l, "Name"), vec!["MixedCase"]);
    // a trailing dot is dropped unless the request says to keep it
    f.file("sh1/dot.", 1).await;
    assert_eq!(f.send("HEAD", "/sh1/dot", &[], "").await.status, 200);
    let r =
        f.send("PUT", "/sh1/dot2.", &[("x-ms-type", "file"), ("x-ms-content-length", "1"), ("x-ms-allow-trailing-dot", "true")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(f.send("HEAD", "/sh1/dot2", &[], "").await.status, 404);
    assert_eq!(f.send("HEAD", "/sh1/dot2.", &[("x-ms-allow-trailing-dot", "true")], "").await.status, 200);
    // a backslash separates
    f.dir("sh1/back").await;
    assert_eq!(f.send("PUT", "/sh1/back%5Csub?restype=directory", &[], "").await.status, 201);
    assert_eq!(f.send("GET", "/sh1/back/sub?restype=directory", &[], "").await.status, 200);
}

#[tokio::test]
async fn directory_attributes_and_times() {
    let f = start().await;
    f.share("sh1").await;
    let t = [("x-ms-file-creation-time", "2020-01-02T03:04:05.0000000Z"), ("x-ms-file-last-write-time", "2021-06-07T08:09:10.1234567Z")];
    let mut h = t.to_vec();
    h.push(("x-ms-file-attributes", "Hidden|Directory"));
    let r = f.send("PUT", "/sh1/d?restype=directory", &h, "").await;
    assert_eq!(r.header("x-ms-file-attributes"), Some("Hidden | Directory"));
    assert_eq!(r.header("x-ms-file-creation-time"), Some("2020-01-02T03:04:05.0000000Z"));
    assert_eq!(r.header("x-ms-file-last-write-time"), Some("2021-06-07T08:09:10.1234567Z"));
    // Hidden alone is no directory attribute; None means Directory; attributes are not case sensitive; `now` is a time at creation
    assert_eq!(
        failed(&f.send("PUT", "/sh1/e?restype=directory", &[("x-ms-file-attributes", "Hidden")], "").await),
        (400, "InvalidHeaderValue".into())
    );
    assert_eq!(
        f.send("PUT", "/sh1/e?restype=directory", &[("x-ms-file-attributes", "None")], "").await.header("x-ms-file-attributes"),
        Some("Directory")
    );
    assert_eq!(
        f.send(
            "PUT",
            "/sh1/g?restype=directory",
            &[("x-ms-file-attributes", "hidden | directory"), ("x-ms-file-creation-time", "now")],
            ""
        )
        .await
        .status,
        201
    );
    // an RFC 1123 date is not a time here
    let r = f.send("PUT", "/sh1/h?restype=directory", &[("x-ms-file-creation-time", "Wed, 01 Jan 2020 00:00:00 GMT")], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    // Set Properties: nothing sent keeps everything; preserve keeps; `now` is not a time here
    let before = f.send("GET", "/sh1/d?restype=directory", &[], "").await;
    let r = f.send("PUT", "/sh1/d?restype=directory&comp=properties", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-file-attributes"), Some("Hidden | Directory"));
    assert_eq!(r.header("x-ms-file-creation-time"), before.header("x-ms-file-creation-time"));
    assert_ne!(r.header("etag"), before.header("etag"));
    let r = f
        .send(
            "PUT",
            "/sh1/d?restype=directory&comp=properties",
            &[("x-ms-file-attributes", "Directory"), ("x-ms-file-last-write-time", "preserve")],
            "",
        )
        .await;
    assert_eq!(r.header("x-ms-file-attributes"), Some("Directory"));
    assert_eq!(r.header("x-ms-file-last-write-time"), Some("2021-06-07T08:09:10.1234567Z"));
    assert_eq!(
        failed(&f.send("PUT", "/sh1/d?restype=directory&comp=properties", &[("x-ms-file-creation-time", "now")], "").await),
        (400, "InvalidHeaderValue".into())
    );
}

#[tokio::test]
async fn directory_metadata_is_replaced() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    assert_eq!(f.send("PUT", "/sh1/d?restype=directory&comp=metadata", &[("x-ms-meta-a", "1")], "").await.status, 200);
    assert_eq!(f.send("GET", "/sh1/d?restype=directory", &[], "").await.header("x-ms-meta-a"), Some("1"));
    assert_eq!(f.send("GET", "/sh1/d?restype=directory&comp=metadata", &[], "").await.header("x-ms-meta-a"), Some("1"));
    f.send("PUT", "/sh1/d?restype=directory&comp=metadata", &[], "").await;
    assert_eq!(f.send("GET", "/sh1/d?restype=directory", &[], "").await.header("x-ms-meta-a"), None);
    assert_eq!(failed(&f.send("PUT", "/sh1/nope?restype=directory&comp=metadata", &[], "").await), (404, "ResourceNotFound".into()));
}

#[tokio::test]
async fn the_listing_of_a_directory() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    for n in ["b.txt", "a.txt", "c.txt"] {
        f.file(&format!("sh1/d/{n}"), 5).await;
    }
    for n in ["sub2", "Sub1", "sub3"] {
        f.dir(&format!("sh1/d/{n}")).await;
    }
    let t = f.send("GET", "/sh1/d?restype=directory&comp=list", &[], "").await.text();
    // files first, then directories, each in name order without regard to case
    assert_eq!(all(&t, "Name"), vec!["a.txt", "b.txt", "c.txt", "Sub1", "sub2", "sub3"]);
    assert!(t.contains("ShareName=\"sh1\" DirectoryPath=\"d\">"));
    assert!(t.contains("<File><Name>a.txt</Name><FileId>"));
    assert!(t.contains("<Properties><Content-Length>5</Content-Length></Properties></File>"));
    assert!(t.contains("<Directory><Name>Sub1</Name><FileId>"));
    assert!(t.contains("<Properties /></Directory>"));
    // prefix ignores case
    let t = f.send("GET", "/sh1/d?restype=directory&comp=list&prefix=s", &[], "").await.text();
    assert_eq!(all(&t, "Name"), vec!["Sub1", "sub2", "sub3"]);
    assert!(t.contains("<Prefix>s</Prefix>"));
    // pages
    let t = f.send("GET", "/sh1/d?restype=directory&comp=list&maxresults=2", &[], "").await.text();
    assert_eq!(all(&t, "Name"), vec!["a.txt", "b.txt"]);
    assert_eq!(tag(&t, "NextMarker").as_deref(), Some("1!8!Yy50eHQ-"));
    let t = f.send("GET", "/sh1/d?restype=directory&comp=list&maxresults=2&marker=1!8!Yy50eHQ-", &[], "").await.text();
    assert_eq!(all(&t, "Name"), vec!["c.txt", "Sub1"]);
    assert!(t.contains("<Marker>1!8!Yy50eHQ-</Marker><MaxResults>2</MaxResults>"));
    // a plain name is a marker too
    let t = f.send("GET", "/sh1/d?restype=directory&comp=list&marker=sub2", &[], "").await.text();
    assert_eq!(all(&t, "Name"), vec!["sub2", "sub3"]);
    assert_eq!(
        failed(&f.send("GET", "/sh1/d?restype=directory&comp=list&marker=1!9!Yy50eHQ-", &[], "").await),
        (400, "OutOfRangeInput".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1/d?restype=directory&comp=list&maxresults=0", &[], "").await),
        (400, "OutOfRangeQueryParameterValue".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1/d?restype=directory&comp=list&include=bogus", &[], "").await),
        (400, "InvalidQueryParameterValue".into())
    );
    assert!(f.send("GET", "/sh1/d/sub2?restype=directory&comp=list", &[], "").await.text().contains("<Entries />"));
    assert_eq!(failed(&f.send("GET", "/sh1/nope?restype=directory&comp=list", &[], "").await), (404, "ResourceNotFound".into()));
}

#[tokio::test]
async fn the_listing_can_carry_timestamps_etags_attributes_and_permission_keys() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f.txt", 1).await;
    f.dir("sh1/d").await;
    let t = f
        .send(
            "GET",
            "/sh1?restype=directory&comp=list&include=Timestamps,ETag,Attributes,PermissionKey",
            &[("x-ms-file-extended-info", "true")],
            "",
        )
        .await
        .text();
    assert!(t.contains("<CreationTime>") && t.contains("<LastAccessTime>") && t.contains("<LastWriteTime>") && t.contains("<ChangeTime>"));
    assert!(t.contains("<Last-Modified>") && t.contains("<Etag>\"0x"));
    assert!(t.contains("<Attributes>Archive</Attributes><PermissionKey>9541354914390863802*3227211760911779636</PermissionKey></File>"));
    assert!(
        t.contains("<Attributes>Directory</Attributes><PermissionKey>4905121398068318397*3227211760911779636</PermissionKey></Directory>")
    );
    assert!(t.contains("<DirectoryId>0</DirectoryId>"));
    // each include on its own
    let t = f.send("GET", "/sh1?restype=directory&comp=list&include=ETag", &[], "").await.text();
    assert!(t.contains("<Properties><Content-Length>1</Content-Length><Etag>") && !t.contains("<CreationTime>"));
    // Azure refuses the form the Python SDK sends
    assert_eq!(
        failed(&f.send("GET", "/sh1?restype=directory&comp=list&include=permission-key", &[], "").await),
        (400, "InvalidQueryParameterValue".into())
    );
    assert_eq!(
        failed(&f.send("GET", "/sh1?restype=directory&comp=list", &[("x-ms-file-extended-info", "false")], "").await),
        (400, "InvalidHeaderValue".into())
    );
}

#[tokio::test]
async fn files_are_created_with_a_size_and_read_back() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    let r = f
        .send(
            "PUT",
            "/sh1/d/f.txt",
            &[
                ("x-ms-type", "file"),
                ("x-ms-content-length", "1024"),
                ("x-ms-content-type", "text/plain"),
                ("x-ms-meta-k", "v"),
                ("x-ms-content-language", "en"),
                ("x-ms-cache-control", "no-cache"),
                ("x-ms-content-disposition", "inline"),
            ],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("x-ms-file-attributes"), Some("Archive"));
    assert_eq!(r.header("x-ms-file-permission-key"), Some("9541354914390863802*3227211760911779636"));
    let h = f.send("HEAD", "/sh1/d/f.txt", &[], "").await;
    assert_eq!(h.status, 200);
    assert_eq!(
        (h.header("content-length"), h.header("content-type"), h.header("x-ms-type")),
        (Some("1024"), Some("text/plain"), Some("File"))
    );
    assert_eq!(
        (h.header("content-language"), h.header("cache-control"), h.header("content-disposition"), h.header("x-ms-meta-k")),
        (Some("en"), Some("no-cache"), Some("inline"), Some("v"))
    );
    assert_eq!(
        (h.header("x-ms-lease-state"), h.header("x-ms-lease-status"), h.header("x-ms-server-encrypted")),
        (Some("available"), Some("unlocked"), Some("true"))
    );
    assert_eq!(h.header("x-ms-file-parent-id"), f.send("GET", "/sh1/d?restype=directory", &[], "").await.header("x-ms-file-id"));
    let g = f.send("GET", "/sh1/d/f.txt", &[], "").await;
    assert_eq!((g.body.len(), g.header("accept-ranges")), (1024, Some("bytes")));
    assert!(g.body.iter().all(|b| *b == 0));
    // creating again replaces it and keeps its id
    let again = f.send("PUT", "/sh1/d/f.txt", &[("x-ms-type", "file"), ("x-ms-content-length", "3")], "").await;
    assert_eq!(again.header("x-ms-file-id"), r.header("x-ms-file-id"));
    assert_ne!(again.header("etag"), r.header("etag"));
    assert_eq!(f.send("HEAD", "/sh1/d/f.txt", &[], "").await.header("content-length"), Some("3"));
}

#[tokio::test]
async fn creating_a_file_checks_its_headers() {
    let f = start().await;
    f.share("sh1").await;
    let r = f.send("PUT", "/sh1/f", &[("x-ms-type", "file")], "").await;
    assert_eq!(failed(&r), (400, "MissingRequiredHeader".into()));
    assert!(r.text().contains("<HeaderName>x-ms-content-length</HeaderName>"));
    let r = f.send("PUT", "/sh1/f", &[("x-ms-content-length", "1")], "").await;
    assert!(r.text().contains("<HeaderName>x-ms-type</HeaderName>"));
    let r = f.send("PUT", "/sh1/f", &[("x-ms-type", "block"), ("x-ms-content-length", "1")], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    for size in [(4u64 << 40) + 1, 0] {
        let r = f.send("PUT", "/sh1/big", &[("x-ms-type", "file"), ("x-ms-content-length", &size.to_string())], "").await;
        assert_eq!(r.status, if size == 0 { 201 } else { 400 }, "{size}");
    }
    let r = f.send("PUT", "/sh1/neg", &[("x-ms-type", "file"), ("x-ms-content-length", "-1")], "").await;
    assert_eq!(failed(&r), (400, "OutOfRangeInput".into()));
    let r = f.send("PUT", "/sh1/f", &[("x-ms-type", "file"), ("x-ms-content-length", "1"), ("x-ms-content-md5", "not base64!")], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    let r =
        f.send("PUT", "/sh1/f", &[("x-ms-type", "file"), ("x-ms-content-length", "1"), ("x-ms-file-attributes", "Directory")], "").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
}

#[tokio::test]
async fn set_properties_clears_what_it_does_not_send() {
    let f = start().await;
    f.share("sh1").await;
    f.send(
        "PUT",
        "/sh1/f",
        &[("x-ms-type", "file"), ("x-ms-content-length", "100"), ("x-ms-content-type", "text/plain"), ("x-ms-content-language", "fr")],
        "",
    )
    .await;
    f.write("sh1/f", 0, "abcdefghij").await;
    let r = f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-content-type", "application/json"), ("x-ms-content-length", "10")], "").await;
    assert_eq!(r.status, 200);
    let h = f.send("HEAD", "/sh1/f", &[], "").await;
    assert_eq!(
        (h.header("content-type"), h.header("content-language"), h.header("content-length")),
        (Some("application/json"), None, Some("10"))
    );
    assert_eq!(f.read("sh1/f").await, "abcdefghij");
    // a bigger size gives zeros; a smaller one drops the end for good
    f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-content-length", "12")], "").await;
    assert_eq!(f.read("sh1/f").await, "abcdefghij\0\0");
    f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-content-length", "3")], "").await;
    f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-content-length", "6")], "").await;
    assert_eq!(f.read("sh1/f").await, "abc\0\0\0");
    // attributes: None stays None here (on create it is Archive); `now` is no time for Set Properties
    f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-file-attributes", "None")], "").await;
    assert_eq!(f.send("HEAD", "/sh1/f", &[], "").await.header("x-ms-file-attributes"), Some("None"));
    assert_eq!(
        failed(&f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-file-attributes", "Hidden|None")], "").await),
        (400, "InvalidHeaderValue".into())
    );
    assert_eq!(
        failed(&f.send("PUT", "/sh1/f?comp=properties", &[("x-ms-file-last-write-time", "now")], "").await),
        (400, "InvalidHeaderValue".into())
    );
}

#[tokio::test]
async fn file_metadata_delete_and_missing_files() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 1).await;
    assert_eq!(f.send("PUT", "/sh1/f?comp=metadata", &[("x-ms-meta-a", "1")], "").await.status, 200);
    assert_eq!(f.send("GET", "/sh1/f?comp=metadata", &[], "").await.header("x-ms-meta-a"), Some("1"));
    assert_eq!(f.send("HEAD", "/sh1/f?comp=metadata", &[], "").await.header("x-ms-meta-a"), Some("1"));
    assert_eq!(f.send("DELETE", "/sh1/f", &[], "").await.status, 202);
    for (m, p) in [
        ("GET", "/sh1/f"),
        ("HEAD", "/sh1/f"),
        ("DELETE", "/sh1/f"),
        ("PUT", "/sh1/f?comp=metadata"),
        ("PUT", "/sh1/f?comp=properties"),
        ("GET", "/sh1/f?comp=rangelist"),
    ] {
        assert_eq!(f.send(m, p, &[], "").await.status, 404, "{m} {p}");
    }
}

#[tokio::test]
async fn rename_follows_the_source_and_checks_the_destination() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/a").await;
    f.file("sh1/a/f.txt", 3).await;
    f.write("sh1/a/f.txt", 0, "abc").await;
    f.dir("sh1/b").await;
    let url = |p: &str| format!("{}/{p}", f.base);
    let rename = |dest: &'static str, src: String, extra: Vec<(&'static str, &'static str)>| {
        let f = &f;
        async move {
            let src_header = [("x-ms-file-rename-source", src.as_str())];
            let mut h: Vec<(&str, &str)> = src_header.to_vec();
            h.extend(extra);
            f.send("PUT", dest, &h, "").await
        }
    };
    // a file, to another directory; the id stays
    let id = f.send("HEAD", "/sh1/a/f.txt", &[], "").await.header("x-ms-file-id").unwrap().to_string();
    let r = rename("/sh1/b/g.txt?comp=rename", url("sh1/a/f.txt"), vec![]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-file-id"), Some(id.as_str()));
    assert_eq!(f.read("sh1/b/g.txt").await, "abc");
    assert_eq!(failed(&f.send("HEAD", "/sh1/a/f.txt", &[], "").await), (404, "ResourceNotFound".into()));
    // the restype is ignored: the source decides
    let r = rename("/sh1/c?restype=directory&comp=rename", url("sh1/b/g.txt"), vec![]).await;
    assert_eq!(r.status, 200);
    assert_eq!(f.send("HEAD", "/sh1/c", &[], "").await.status, 200);
    // the source must be a URL of this share
    let r = rename("/sh1/x?comp=rename", "/sh1/c".into(), vec![]).await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    let r = rename("/sh1/x?comp=rename", url("other/c"), vec![]).await;
    assert_eq!(failed(&r), (400, "RenameAcrossAccountsAndSharesNotSupported".into()));
    assert_eq!(failed(&f.send("PUT", "/sh1/x?comp=rename", &[], "").await), (400, "MissingRequiredHeader".into()));
    // missing source and missing parents
    assert_eq!(failed(&rename("/sh1/x?comp=rename", url("sh1/nope"), vec![]).await), (404, "ResourceNotFound".into()));
    assert_eq!(failed(&rename("/sh1/x?comp=rename", url("sh1/no/dir"), vec![]).await), (404, "ParentNotFound".into()));
    assert_eq!(failed(&rename("/sh1/no/x?comp=rename", url("sh1/c"), vec![]).await), (404, "ParentNotFound".into()));
    // an existing destination needs the flag, and a directory is never replaced
    f.file("sh1/d.txt", 1).await;
    assert_eq!(failed(&rename("/sh1/d.txt?comp=rename", url("sh1/c"), vec![]).await), (409, "ResourceAlreadyExists".into()));
    assert_eq!(rename("/sh1/d.txt?comp=rename", url("sh1/c"), vec![("x-ms-file-rename-replace-if-exists", "true")]).await.status, 200);
    assert_eq!(
        failed(&rename("/sh1/b?comp=rename", url("sh1/d.txt"), vec![("x-ms-file-rename-replace-if-exists", "true")]).await),
        (400, "RenameDirectoryOperationNotSupported".into())
    );
    assert_eq!(failed(&rename("/sh1/b?restype=directory&comp=rename", url("sh1/a"), vec![]).await), (409, "ResourceAlreadyExists".into()));
    // a directory cannot go into itself, and can go onto itself
    assert_eq!(failed(&rename("/sh1/a/in?restype=directory&comp=rename", url("sh1/a"), vec![]).await), (409, "RenameCycle".into()));
    assert_eq!(rename("/sh1/a?restype=directory&comp=rename", url("sh1/a"), vec![]).await.status, 200);
    // new properties and metadata go with it
    let r = rename(
        "/sh1/e.txt?comp=rename",
        url("sh1/d.txt"),
        vec![("x-ms-file-attributes", "Hidden"), ("x-ms-meta-q", "1"), ("x-ms-content-type", "text/z")],
    )
    .await;
    assert_eq!(r.status, 200);
    let h = f.send("HEAD", "/sh1/e.txt", &[], "").await;
    assert_eq!(
        (h.header("x-ms-file-attributes"), h.header("x-ms-meta-q"), h.header("content-type")),
        (Some("Hidden"), Some("1"), Some("text/z"))
    );
}

#[tokio::test]
async fn handles_do_not_exist_on_a_share_nobody_mounted() {
    let f = start().await;
    f.share("sh1").await;
    f.dir("sh1/d").await;
    f.file("sh1/d/f", 1).await;
    for p in ["/sh1?comp=listhandles", "/sh1/d?comp=listhandles", "/sh1/d/f?comp=listhandles"] {
        let r = f.send("GET", p, &[], "").await;
        assert_eq!(r.status, 200, "{p}");
        assert!(r.text().ends_with("<EnumerationResults><Entries /><NextMarker /></EnumerationResults>"));
    }
    assert!(f.send("GET", "/sh1/d?comp=listhandles&maxresults=1", &[], "").await.text().contains("<MaxResults>1</MaxResults>"));
    let r = f.send("PUT", "/sh1/d?comp=forceclosehandles", &[("x-ms-handle-id", "*")], "").await;
    assert_eq!(
        (r.status, r.header("x-ms-number-of-handles-closed"), r.header("x-ms-number-of-handles-failed")),
        (200, Some("0"), Some("0"))
    );
    assert_eq!(failed(&f.send("PUT", "/sh1/d?comp=forceclosehandles", &[], "").await), (400, "MissingRequiredHeader".into()));
    assert_eq!(failed(&f.send("GET", "/sh1/nope?comp=listhandles", &[], "").await), (404, "ResourceNotFound".into()));
    assert_eq!(
        failed(&f.send("PUT", "/sh1/nope?comp=forceclosehandles", &[("x-ms-handle-id", "*")], "").await),
        (404, "ResourceNotFound".into())
    );
}
