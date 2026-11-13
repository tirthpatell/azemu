mod common;
use common::*;

async fn put_all(e: &Env, c: &str, names: &[&str]) {
    for n in names {
        let r = e.put_blob(c, &n.replace(' ', "%20").replace('#', "%23"), "x").await;
        assert_eq!(r.status, 201, "{n}");
    }
}

#[tokio::test]
async fn flat_listing_order_prefix_and_paging() {
    let e = start().await;
    e.create_container("cnt").await;
    put_all(&e, "cnt", &["b", "a", "B", "logs/1", "logs/2", "logsx"]).await;
    let r = e.send("GET", "/cnt?restype=container&comp=list", &[], "").await;
    let t = r.text();
    assert_eq!(tags(&t, "Name"), ["B", "a", "b", "logs/1", "logs/2", "logsx"]);
    assert!(t.contains("<EnumerationResults ServiceEndpoint=\"http://acct.blob.localhost"));
    assert!(t.contains("ContainerName=\"cnt\""));
    assert!(t.contains("<NextMarker />"));
    // prefix
    let t = e.send("GET", "/cnt?restype=container&comp=list&prefix=logs/", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["logs/1", "logs/2"]);
    // paging with marker
    let mut got = vec![];
    let mut marker = String::new();
    for _ in 0..10 {
        let t = e.send("GET", &format!("/cnt?restype=container&comp=list&maxresults=4&marker={marker}"), &[], "").await.text();
        got.extend(tags(&t, "Name"));
        let next = next_marker(&t);
        if next.is_empty() {
            break;
        }
        marker = next.replace('+', "%2B").replace('=', "%3D").replace('/', "%2F");
    }
    assert_eq!(got, ["B", "a", "b", "logs/1", "logs/2", "logsx"]);
}

#[tokio::test]
async fn hierarchy_listing_with_delimiter() {
    let e = start().await;
    e.create_container("cnt").await;
    put_all(&e, "cnt", &["root.txt", "d1/a", "d1/b", "d1/sub/cnt", "d2/x", "d3/y"]).await;
    let t = e.send("GET", "/cnt?restype=container&comp=list&delimiter=/", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["d1/", "d2/", "d3/", "root.txt"]);
    assert_eq!(t.matches("<BlobPrefix>").count(), 3);
    assert!(t.contains("<Delimiter>/</Delimiter>"));
    let t = e.send("GET", "/cnt?restype=container&comp=list&delimiter=/&prefix=d1/", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["d1/a", "d1/b", "d1/sub/"]);
    // paging across directories
    let t = e.send("GET", "/cnt?restype=container&comp=list&delimiter=/&maxresults=2", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["d1/", "d2/"]);
    let marker = next_marker(&t).replace('+', "%2B").replace('=', "%3D").replace('/', "%2F");
    let t = e.send("GET", &format!("/cnt?restype=container&comp=list&delimiter=/&maxresults=2&marker={marker}"), &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["d3/", "root.txt"]);
    assert!(t.contains("<NextMarker />"));
}

#[tokio::test]
async fn special_character_names_roundtrip() {
    let e = start().await;
    e.create_container("cnt").await;
    for name in ["a b", "é", "日本語/ファイル.txt", "hash#tag", "plus+sign", "dir//double//slash", "100%25", "q%3Fmark"] {
        // `name` is already the percent-encoded path form for the odd ones
        let path = if name.contains('%') || name.contains('#') || name.contains(' ') {
            name.replace(' ', "%20").replace('#', "%23")
        } else {
            name.to_string()
        };
        let enc: String = path
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"%/-_.+".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect();
        let r = e.put_blob("cnt", &enc, name).await;
        assert_eq!(r.status, 201, "{name}");
        let r = e.send("GET", &format!("/cnt/{enc}"), &[], "").await;
        assert_eq!((r.status, r.text().as_str()), (200, name), "{name}");
    }
    let t = e.send("GET", "/cnt?restype=container&comp=list", &[], "").await.text();
    assert!(tags(&t, "Name").contains(&"dir//double//slash".to_string()));
    assert!(tags(&t, "Name").contains(&"日本語/ファイル.txt".to_string()));
    assert!(tags(&t, "Name").contains(&"100%".to_string()));
}

#[tokio::test]
async fn include_options() {
    let e = start().await;
    e.create_container("cnt").await;
    e.send("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-k", "v"), ("x-ms-tags", "t=1")], "xyz").await;
    e.send("PUT", "/cnt/b?comp=snapshot", &[], "").await;
    e.send("PUT", "/cnt/staged?comp=block&blockid=YWJj", &[], "pending").await;
    let t = e.send("GET", "/cnt?restype=container&comp=list", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["b"]);
    assert!(!t.contains("<k>"));
    let t = e.send("GET", "/cnt?restype=container&comp=list&include=metadata,tags,snapshots,uncommittedblobs,copy", &[], "").await.text();
    assert_eq!(tags(&t, "Name"), ["b", "b", "staged"]);
    assert!(t.contains("<k>v</k>"));
    assert!(t.contains("<Key>t</Key>"));
    assert_eq!(tags(&t, "Snapshot").len(), 1);
    assert!(t.contains("<Etag>0x"), "etag is unquoted in listings");
    assert_eq!(tags(&t, "BlobType").len(), 3);
}

#[tokio::test]
async fn plus_in_a_query_string_is_a_space_and_in_a_path_it_is_a_plus() {
    // The Rust SDKs form-encode the query (`prefix=dir+with+space%2F`); Python, JavaScript and Go send `%20`.
    // A literal plus is `%2B` in a query. In the path of a blob, `+` and `%2B` are both a plus.
    let e = start().await;
    e.create_container("cnt").await;
    for (path, name) in [("dir%20with%20space/a", "dir with space/a"), ("dir%2Bplus/b", "dir+plus/b"), ("a+b", "a+b"), ("c%2Bd", "c+d")] {
        assert_eq!(e.put_blob("cnt", path, "x").await.status, 201, "{name}");
    }
    let list = |q: &str| {
        let url = format!("/cnt?restype=container&comp=list&{q}");
        let e = &e;
        async move { tags(&e.send("GET", &url, &[], "").await.text(), "Name") }
    };
    assert_eq!(list("prefix=dir+with+space%2F").await, ["dir with space/a"], "space as +");
    assert_eq!(list("prefix=dir%20with%20space/").await, ["dir with space/a"], "space as %20");
    assert_eq!(list("prefix=dir%2Bplus").await, ["dir+plus/b"], "plus as %2B");
    assert!(list("prefix=dir+plus").await.is_empty(), "a raw + in the query is a space, which matches no name");
    assert_eq!(list("prefix=a%2Bb").await, ["a+b"]);
    // the path: + and %2B are the same plus, %20 is a space
    assert_eq!(e.send("GET", "/cnt/a+b", &[], "").await.status, 200);
    assert_eq!(e.send("GET", "/cnt/a%2Bb", &[], "").await.status, 200);
    assert_eq!(e.send("GET", "/cnt/c+d", &[], "").await.status, 200);
    assert_eq!(e.send("GET", "/cnt/a%20b", &[], "").await.status, 404);
    // a marker: sent with a literal plus escaped, the listing continues from it; names listed in order
    let t = e.send("GET", "/cnt?restype=container&comp=list&maxresults=1", &[], "").await.text();
    let next = next_marker(&t);
    assert!(!next.is_empty());
    let enc = next.replace('+', "%2B").replace('=', "%3D").replace('/', "%2F");
    let t2 = e.send("GET", &format!("/cnt?restype=container&comp=list&maxresults=10&marker={enc}"), &[], "").await.text();
    assert_eq!(tags(&t2, "Name").len(), 3, "the other three names follow the marker");
}
