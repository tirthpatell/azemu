//! Data Lake Gen2 (`dfs` endpoint) through the blob crate's second service, with the Blob
//! endpoint of the same account next to it.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::serde_json::Value;
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
    Lake { srv, dfs, blob }
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

    async fn fs(&self, name: &str) {
        let r = self.d("PUT", &format!("/{name}?resource=filesystem"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    async fn dir(&self, path: &str) {
        let r = self.d("PUT", &format!("/{path}?resource=directory"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
    }

    /// Create a file with `data` (create, append, flush).
    async fn file(&self, path: &str, data: &str) {
        let r = self.d("PUT", &format!("/{path}?resource=file"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
        if !data.is_empty() {
            let r = self.d("PATCH", &format!("/{path}?action=append&position=0"), &[], data.to_string()).await;
            assert_eq!(r.status, 202, "{}", r.text());
        }
        let r = self.d("PATCH", &format!("/{path}?action=flush&position={}", data.len()), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    async fn names(&self, fs: &str, query: &str) -> Vec<String> {
        let r = self.d("GET", &format!("/{fs}?resource=filesystem&{query}"), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        names_of(&r.json())
    }
}

fn names_of(v: &Value) -> Vec<String> {
    v["paths"].as_array().map(|a| a.iter().map(|p| p["name"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
}

fn code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

#[tokio::test]
async fn filesystem_is_a_container_in_both_directions() {
    let l = lake().await;
    // created through dfs, visible as a container
    l.fs("fs1").await;
    let r = l.b("GET", "/?comp=list", &[], "").await;
    assert!(r.text().contains("<Name>fs1</Name>"), "{}", r.text());
    let r = l.b("GET", "/fs1?restype=container", &[], "").await;
    assert_eq!(r.status, 200);
    // created through blob, usable through dfs, listed as a filesystem
    let r = l.b("PUT", "/fs2?restype=container", &[], "").await;
    assert_eq!(r.status, 201);
    l.dir("fs2/d").await;
    let r = l.d("GET", "/?resource=account", &[], "").await;
    assert_eq!(r.status, 200);
    let names: Vec<String> = r.json()["filesystems"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["fs1", "fs2"]);
    // properties
    let r = l.d("HEAD", "/fs1", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-namespace-enabled")), (200, Some("true")));
    // existing / missing
    let r = l.d("PUT", "/fs1?resource=filesystem", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "FilesystemAlreadyExists"));
    let r = l.d("GET", "/nofs?resource=filesystem&recursive=true", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "FilesystemNotFound"));
    // delete through dfs, gone for blob
    assert_eq!(l.d("DELETE", "/fs1", &[], "").await.status, 202);
    assert_eq!(l.b("GET", "/fs1?restype=container", &[], "").await.status, 404);
}

#[tokio::test]
async fn filesystem_properties_are_base64_pairs() {
    let l = lake().await;
    let r = l.d("PUT", "/fsp?resource=filesystem", &[("x-ms-properties", "hello=d29ybGQ=,n=NDI=")], "").await;
    assert_eq!(r.status, 201);
    let r = l.d("HEAD", "/fsp", &[], "").await;
    assert_eq!(r.header("x-ms-properties"), Some("hello=d29ybGQ=,n=NDI="));
    // the blob endpoint sees them as container metadata
    let r = l.b("GET", "/fsp?restype=container", &[], "").await;
    assert_eq!(r.header("x-ms-meta-hello"), Some("world"));
    let r = l.d("PATCH", "/fsp?resource=filesystem", &[("x-ms-properties", "a=Yg==")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(l.d("HEAD", "/fsp", &[], "").await.header("x-ms-properties"), Some("a=Yg=="));
}

#[tokio::test]
async fn hierarchy_rules() {
    let l = lake().await;
    l.fs("hier").await;
    // a file below new directories creates them
    l.file("hier/a/b/c.txt", "hello").await;
    assert_eq!(l.names("hier", "recursive=true").await, vec!["a", "a/b", "a/b/c.txt"]);
    let r = l.d("GET", "/hier?resource=filesystem&recursive=true", &[], "").await;
    let paths = r.json()["paths"].clone();
    assert_eq!(paths[0]["isDirectory"], "true");
    assert!(paths[2].get("isDirectory").is_none());
    assert_eq!(paths[2]["contentLength"], "5");
    assert_eq!(paths[2]["permissions"], "rw-r-----");
    assert_eq!(paths[0]["permissions"], "rwxr-x---");
    assert_eq!(paths[0]["owner"], "$superuser");

    // a path is a file or a directory, never both
    let r = l.d("PUT", "/hier/a/b?resource=file", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
    let r = l.d("PUT", "/hier/a/b/c.txt?resource=directory", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
    let r = l.d("PUT", "/hier/a/b/c.txt/under?resource=file", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
    // names are case sensitive
    l.file("hier/A/b/c.txt", "x").await;
    assert_eq!(l.names("hier", "recursive=true").await.len(), 6);
    // If-None-Match: * on an existing path
    let r = l.d("PUT", "/hier/a/b/c.txt?resource=file", &[("If-None-Match", "*")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"));
    // JSON error body
    assert!(r.text().contains("\"code\":\"PathAlreadyExists\""), "{}", r.text());
    assert!(r.header("content-type").unwrap_or("").starts_with("application/json"));
    assert_eq!(r.header("server"), Some("Windows-Azure-HDFS/1.0 Microsoft-HTTPAPI/2.0"));
    // one level, one directory
    assert_eq!(l.names("hier", "recursive=false").await, vec!["A", "a"]);
    assert_eq!(l.names("hier", "recursive=false&directory=a").await, vec!["a/b"]);
    assert_eq!(l.names("hier", "recursive=true&directory=a/b").await, vec!["a/b/c.txt"]);
    let r = l.d("GET", "/hier?resource=filesystem&recursive=true&directory=zzz", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "PathNotFound"));
}

#[tokio::test]
async fn append_and_flush() {
    let l = lake().await;
    l.fs("appfl").await;
    let r = l.d("PUT", "/appfl/f?resource=file", &[], "").await;
    let created = r.header("etag").unwrap().to_string();
    // chunks may arrive out of order
    let r = l.d("PATCH", "/appfl/f?action=append&position=6", &[], "world").await;
    assert_eq!((r.status, r.header("x-ms-request-server-encrypted")), (202, Some("true")));
    assert_eq!(l.d("PATCH", "/appfl/f?action=append&position=0", &[], "hello ").await.status, 202);
    // nothing is readable before the flush
    assert_eq!(l.b("HEAD", "/appfl/f", &[], "").await.header("content-length"), Some("0"));
    let r = l.d("PATCH", "/appfl/f?action=flush&position=11&retainUncommittedData=false&close=false", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_ne!(r.header("etag"), Some(created.as_str()));
    assert_eq!(l.d("GET", "/appfl/f", &[], "").await.text(), "hello world");
    assert_eq!(l.b("GET", "/appfl/f", &[("x-ms-range", "bytes=6-10")], "").await.text(), "world");
    let r = l.b("HEAD", "/appfl/f", &[], "").await;
    assert_eq!(r.header("x-ms-resource-type"), Some("file"));
    assert_eq!(r.header("x-ms-blob-type"), Some("BlockBlob"));

    // a gap, or a position that does not match the data, is rejected
    l.d("PATCH", "/appfl/f?action=append&position=20", &[], "zz").await;
    let r = l.d("PATCH", "/appfl/f?action=flush&position=22", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidFlushPosition"));
    // flush at the current length only renews the etag
    let r = l.d("PATCH", "/appfl/f?action=flush&position=11", &[], "").await;
    assert_eq!(r.status, 200);
    // retainUncommittedData keeps what lies beyond the flush position
    l.d("PATCH", "/appfl/g?action=append&position=0", &[], "abc").await;
    l.d("PUT", "/appfl/g?resource=file", &[], "").await;
    l.d("PATCH", "/appfl/g?action=append&position=0", &[], "abc").await;
    l.d("PATCH", "/appfl/g?action=append&position=3", &[], "def").await;
    let r = l.d("PATCH", "/appfl/g?action=flush&position=3&retainUncommittedData=true", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(l.d("GET", "/appfl/g", &[], "").await.text(), "abc");
    assert_eq!(l.d("PATCH", "/appfl/g?action=flush&position=6", &[], "").await.status, 200);
    assert_eq!(l.d("GET", "/appfl/g", &[], "").await.text(), "abcdef");
    // append with flush=true, and conditions on flush
    let r = l.d("PATCH", "/appfl/g?action=append&position=6&flush=true", &[], "ghi").await;
    assert_eq!(r.status, 202);
    assert!(r.header("etag").is_some());
    assert_eq!(l.d("GET", "/appfl/g", &[], "").await.text(), "abcdefghi");
    let r = l.d("PATCH", "/appfl/g?action=flush&position=9", &[("If-Match", "\"0x1\"")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    // an append that flushes takes its Content-Type as the file's (recording `test_file_async.pyTestFiletest_read_file_read.json`, entries 2 and 3);
    // one that does not flush, and one that follows a content type the file has already, leave it alone
    l.d("PUT", "/appfl/ct1?resource=file", &[], "").await;
    l.d("PATCH", "/appfl/ct1?action=append&position=0&flush=true", &[("Content-Type", "application/json")], "{}").await;
    assert_eq!(l.d("HEAD", "/appfl/ct1", &[], "").await.header("content-type"), Some("application/json"));
    l.d("PATCH", "/appfl/ct1?action=append&position=2&flush=true", &[("Content-Type", "text/plain")], "x").await;
    assert_eq!(l.d("HEAD", "/appfl/ct1", &[], "").await.header("content-type"), Some("application/json"));
    l.d("PUT", "/appfl/ct2?resource=file", &[], "").await;
    l.d("PATCH", "/appfl/ct2?action=append&position=0", &[("Content-Type", "application/json")], "{}").await;
    l.d("PATCH", "/appfl/ct2?action=flush&position=2", &[], "").await;
    assert_eq!(l.d("HEAD", "/appfl/ct2", &[], "").await.header("content-type"), Some("application/octet-stream"));
    // missing file
    let r = l.d("PATCH", "/appfl/none?action=append&position=0", &[], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "PathNotFound"));
    // a directory takes no data
    l.dir("appfl/dir").await;
    let r = l.d("PATCH", "/appfl/dir?action=append&position=0", &[], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathConflict"));
}

#[tokio::test]
async fn md5_and_crc64_of_an_append_are_checked() {
    let l = lake().await;
    l.fs("mdf").await;
    l.d("PUT", "/mdf/f?resource=file", &[], "").await;
    // md5 of "abc" is 900150983cd24fb0d6963f7d28e17f72
    let r = l.d("PATCH", "/mdf/f?action=append&position=0", &[("Content-MD5", "kAFQmDzST7DWlj99KOF/cg==")], "abc").await;
    assert_eq!((r.status, r.header("content-md5")), (202, Some("kAFQmDzST7DWlj99KOF/cg==")));
    let r = l.d("PATCH", "/mdf/f?action=append&position=3", &[("Content-MD5", "kAFQmDzST7DWlj99KOF/cg==")], "abd").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "Md5Mismatch"));
}

#[tokio::test]
async fn list_paths_with_paging() {
    let l = lake().await;
    l.fs("lstp").await;
    for i in 0..5 {
        l.file(&format!("lstp/f{i}"), "x").await;
    }
    l.dir("lstp/d").await;
    l.file("lstp/d/inner", "yy").await;
    let all = l.names("lstp", "recursive=true").await;
    assert_eq!(all, vec!["d", "d/inner", "f0", "f1", "f2", "f3", "f4"]);
    // pages of three: continuation until the end
    let mut got: Vec<String> = vec![];
    let mut token = String::new();
    let mut pages = 0;
    loop {
        let q = if token.is_empty() {
            "/lstp?resource=filesystem&recursive=true&maxResults=3".to_string()
        } else {
            format!(
                "/lstp?resource=filesystem&recursive=true&maxResults=3&continuation={}",
                token.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
            )
        };
        let r = l.d("GET", &q, &[], "").await;
        assert_eq!(r.status, 200);
        got.extend(names_of(&r.json()));
        pages += 1;
        match r.header("x-ms-continuation") {
            Some(t) => token = t.to_string(),
            None => break,
        }
    }
    assert_eq!(got, all);
    assert_eq!(pages, 3);
    // beginFrom, relative to the listed directory
    assert_eq!(l.names("lstp", "recursive=true&beginFrom=f3").await, vec!["f3", "f4"]);
    assert_eq!(l.names("lstp", "recursive=true&directory=d&beginFrom=inner").await, vec!["d/inner"]);
    // a bad token or parameter
    let r = l.d("GET", "/lstp?resource=filesystem&recursive=true&continuation=!!", &[], "").await;
    assert_eq!(r.status, 400);
    let r = l.d("GET", "/lstp?resource=filesystem", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredQueryParameter"));
    // listing fields
    let r = l.d("GET", "/lstp?resource=filesystem&recursive=false&directory=d", &[], "").await;
    let p = &r.json()["paths"][0];
    assert_eq!(p["name"], "d/inner");
    assert_eq!(p["contentLength"], "2");
    assert!(p["etag"].as_str().unwrap().starts_with("0x"));
    assert_eq!(p["expiryTime"], "0");
    assert!(p["creationTime"].as_str().unwrap().len() >= 17);
    assert!(p["lastModified"].as_str().unwrap().ends_with("GMT"));
}

#[tokio::test]
async fn rename_moves_a_whole_tree_atomically() {
    let l = lake().await;
    l.fs("rfs1").await;
    l.fs("rfs2").await;
    l.file("rfs1/old/sub/a.txt", "aaa").await;
    l.file("rfs1/old/b.txt", "bb").await;
    let before = l.b("HEAD", "/rfs1/old", &[], "").await;
    let etag = before.header("etag").unwrap().to_string();

    // within a filesystem
    let r = l.d("PUT", "/rfs1/new?mode=legacy", &[("x-ms-rename-source", "/rfs1/old"), ("x-ms-source-lease-id", "")], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("etag"), Some(etag.as_str()), "a rename keeps the etag");
    assert_eq!(l.names("rfs1", "recursive=true").await, vec!["new", "new/b.txt", "new/sub", "new/sub/a.txt"]);
    assert_eq!(l.b("GET", "/rfs1/new/sub/a.txt", &[], "").await.text(), "aaa");
    assert_eq!(l.b("HEAD", "/rfs1/old", &[], "").await.status, 404);
    assert_eq!(l.b("HEAD", "/rfs1/old/b.txt", &[], "").await.status, 404);

    // across filesystems (percent-encoded source, SAS-like query ignored)
    let r = l.d("PUT", "/rfs2/moved%20dir?mode=posix", &[("x-ms-rename-source", "/rfs1/new?sv=2020")], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(l.names("rfs1", "recursive=true").await, Vec::<String>::new());
    assert_eq!(l.names("rfs2", "recursive=true").await, vec!["moved dir", "moved dir/b.txt", "moved dir/sub", "moved dir/sub/a.txt"]);

    // a file onto a file replaces it
    l.file("rfs2/x", "1").await;
    l.file("rfs2/y", "22").await;
    let r = l.d("PUT", "/rfs2/y?mode=legacy", &[("x-ms-rename-source", "/rfs2/x")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(l.b("GET", "/rfs2/y", &[], "").await.text(), "1");
    assert_eq!(l.b("HEAD", "/rfs2/x", &[], "").await.status, 404);

    // onto an empty directory it replaces, onto a non-empty directory it fails
    l.dir("rfs2/e1").await;
    l.dir("rfs2/e2").await;
    l.file("rfs2/e2/inside", "").await;
    l.dir("rfs2/src1").await;
    l.dir("rfs2/src2").await;
    let r = l.d("PUT", "/rfs2/e1?mode=legacy", &[("x-ms-rename-source", "/rfs2/src1")], "").await;
    assert_eq!(r.status, 201);
    let r = l.d("PUT", "/rfs2/e2?mode=legacy", &[("x-ms-rename-source", "/rfs2/src2")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"));
    assert_eq!(l.b("HEAD", "/rfs2/src2", &[], "").await.status, 200);
    // type mismatch, itself, missing source, missing parent
    let r = l.d("PUT", "/rfs2/y?mode=legacy", &[("x-ms-rename-source", "/rfs2/e1")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "InvalidSourceOrDestinationResourceType"));
    let r = l.d("PUT", "/rfs2/e1/deeper?mode=legacy", &[("x-ms-rename-source", "/rfs2/e1")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "InvalidRenameSourcePath"));
    let r = l.d("PUT", "/rfs2/z?mode=legacy", &[("x-ms-rename-source", "/rfs2/nothing")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "SourcePathNotFound"));
    let r = l.d("PUT", "/rfs2/no/parent?mode=legacy", &[("x-ms-rename-source", "/rfs2/y")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "RenameDestinationParentPathNotFound"));
    // conditions on the source and the destination
    let r = l.d("PUT", "/rfs2/q?mode=legacy", &[("x-ms-rename-source", "/rfs2/y"), ("x-ms-source-if-match", "\"0x1\"")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "SourceConditionNotMet"));
    let r = l.d("PUT", "/rfs2/e1?mode=legacy", &[("x-ms-rename-source", "/rfs2/y"), ("If-None-Match", "*")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PathAlreadyExists"));
}

#[tokio::test]
async fn delete_and_soft_delete() {
    let l = lake().await;
    l.fs("dele").await;
    l.file("dele/d/f", "x").await;
    l.file("dele/d/s/g", "y").await;
    // a non-empty directory needs recursive=true
    let r = l.d("DELETE", "/dele/d", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "DirectoryNotEmpty"));
    // a file, then the tree; paginated delete is accepted
    let r = l.d("DELETE", "/dele/d/f", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-delete-type-permanent")), (200, Some("true")));
    let r = l.d("DELETE", "/dele/d?recursive=true&paginated=true&continuation=", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(l.names("dele", "recursive=true").await.is_empty());
    assert_eq!(l.b("HEAD", "/dele/d/s/g", &[], "").await.status, 404);
    let r = l.d("DELETE", "/dele/d", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "PathNotFound"));
    // conditions
    l.file("dele/c", "x").await;
    let r = l.d("DELETE", "/dele/c", &[("If-Match", "\"0x1\"")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));

    // soft delete: retention in the service properties
    let props = "<StorageServiceProperties><DeleteRetentionPolicy><Enabled>true</Enabled><Days>3</Days></DeleteRetentionPolicy></StorageServiceProperties>";
    assert_eq!(l.b("PUT", "/?restype=service&comp=properties", &[], props).await.status, 202);
    l.file("dele/keep/inner", "data").await;
    l.file("dele/one", "1").await;
    let r = l.d("DELETE", "/dele/keep?recursive=true", &[], "").await;
    assert_eq!((r.status, r.header("x-ms-delete-type-permanent")), (200, Some("false")));
    let id = r.header("x-ms-deletion-id").unwrap().to_string();
    l.d("DELETE", "/dele/one", &[], "").await;
    assert_eq!(l.b("HEAD", "/dele/keep", &[], "").await.status, 404);
    let r = l.b("GET", "/dele?restype=container&comp=list&showonly=deleted", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let t = r.text();
    assert!(t.contains("<Name>keep</Name>") && t.contains("<Name>one</Name>"), "{t}");
    assert!(t.contains(&format!("<DeletionId>{id}</DeletionId>")) && t.contains("<ResourceType>directory</ResourceType>"), "{t}");
    // undelete the directory: the files below it come back with it
    let r = l.b("PUT", "/dele/keep?comp=undelete", &[("x-ms-undelete-source", &format!("keep?deletionid={id}"))], "").await;
    assert_eq!((r.status, r.header("x-ms-resource-type")), (200, Some("directory")));
    assert_eq!(l.b("GET", "/dele/keep/inner", &[], "").await.text(), "data");
    assert_eq!(l.names("dele", "recursive=true").await, vec!["c", "keep", "keep/inner"]);
    let r = l.b("PUT", "/dele/keep?comp=undelete", &[("x-ms-undelete-source", &format!("keep?deletionid={id}"))], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn permissions_umask_and_default_acl() {
    let l = lake().await;
    l.fs("acls").await;
    l.dir("acls/p").await;
    let r = l.d("HEAD", "/acls/p?action=getAccessControl&upn=false", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-acl"), Some("user::rwx,group::r-x,other::---"));
    assert_eq!((r.header("x-ms-owner"), r.header("x-ms-group")), (Some("$superuser"), Some("$superuser")));
    assert_eq!(r.header("x-ms-permissions"), Some("rwxr-x---"));
    // permissions with and without umask, octal and symbolic, sticky bit
    let r = l.d("PUT", "/acls/q?resource=directory", &[("x-ms-permissions", "rwxrwxrwx"), ("x-ms-umask", "0027")], "").await;
    assert_eq!(r.status, 201);
    let r = l.d("HEAD", "/acls/q?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-permissions"), Some("rwxr-x---"));
    l.d("PUT", "/acls/q2?resource=file", &[("x-ms-permissions", "0777"), ("x-ms-umask", "0000")], "").await;
    let r = l.d("HEAD", "/acls/q2?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-permissions"), Some("rwxrwxrwx"));
    let r = l.d("PATCH", "/acls/q2?action=setAccessControl", &[("x-ms-permissions", "1750")], "").await;
    assert_eq!(r.status, 200);
    let r = l.d("HEAD", "/acls/q2?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-permissions"), Some("rwxr-x--T"));
    // owner and group
    l.d("PATCH", "/acls/q2?action=setAccessControl", &[("x-ms-owner", "alice"), ("x-ms-group", "staff")], "").await;
    let r = l.d("HEAD", "/acls/q2?action=getAccessControl", &[], "").await;
    assert_eq!((r.header("x-ms-owner"), r.header("x-ms-group")), (Some("alice"), Some("staff")));
    // owner, group and ACL given on create
    let r = l
        .d(
            "PUT",
            "/acls/o?resource=file",
            &[("x-ms-owner", "bob"), ("x-ms-group", "g"), ("x-ms-acl", "user::rw-,group::r--,other::r--")],
            "",
        )
        .await;
    assert_eq!(r.status, 201);
    let r = l.d("HEAD", "/acls/o?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-acl"), Some("user::rw-,group::r--,other::r--"));
    assert_eq!(r.header("x-ms-permissions"), Some("rw-r--r--"));
    // the root directory has an ACL too
    let r = l.d("PATCH", "/acls//?action=setAccessControl", &[("x-ms-acl", "user::rwx,group::r-x,other::rwx")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = l.d("HEAD", "/acls//?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-permissions"), Some("rwxr-xrwx"));
    // bad values
    let r = l.d("PATCH", "/acls/q2?action=setAccessControl", &[("x-ms-acl", "user::rwz")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    let r = l.d("PUT", "/acls/bad?resource=file", &[("x-ms-permissions", "rwx")], "").await;
    assert_eq!(r.status, 400);

    // default ACL: new children inherit it
    let acl = "user::rwx,group::r-x,other::---,default:user::rwx,default:group::rwx,default:other::r-x,default:user:11111111-2222-3333-4444-555555555555:r-x";
    assert_eq!(l.d("PATCH", "/acls/p?action=setAccessControl", &[("x-ms-acl", acl)], "").await.status, 200);
    let r = l.d("HEAD", "/acls/p?action=getAccessControl", &[], "").await;
    assert_eq!(
        r.header("x-ms-acl"),
        Some("user::rwx,group::r-x,other::---,default:user::rwx,default:user:11111111-2222-3333-4444-555555555555:r-x,default:group::rwx,default:mask::rwx,default:other::r-x")
    );
    l.dir("acls/p/child").await;
    let r = l.d("HEAD", "/acls/p/child?action=getAccessControl", &[], "").await;
    assert_eq!(
        r.header("x-ms-acl"),
        Some("user::rwx,user:11111111-2222-3333-4444-555555555555:r-x,group::rwx,mask::rwx,other::r-x,default:user::rwx,default:user:11111111-2222-3333-4444-555555555555:r-x,default:group::rwx,default:mask::rwx,default:other::r-x")
    );
    l.file("acls/p/child/file", "").await;
    let r = l.d("HEAD", "/acls/p/child/file?action=getAccessControl", &[], "").await;
    // a file takes the access entries only, masked by 0666
    assert_eq!(r.header("x-ms-acl"), Some("user::rw-,user:11111111-2222-3333-4444-555555555555:r-x,group::rwx,mask::rw-,other::r--"));
    // the blob endpoint carries the same attributes
    let r = l.b("HEAD", "/acls/p/child/file", &[], "").await;
    assert_eq!(r.header("x-ms-permissions"), Some("rw-rw-r--"));
    assert_eq!(r.header("x-ms-owner"), Some("$superuser"));
}

#[tokio::test]
async fn recursive_acl_changes_in_batches() {
    let l = lake().await;
    l.fs("recs").await;
    l.file("recs/top/a", "").await;
    l.file("recs/top/s1/b", "").await;
    l.file("recs/top/s1/c", "").await;
    l.file("recs/top/s2/d", "").await;
    let uid = "11111111-2222-3333-4444-555555555555";
    // 3 directories + 4 files, 3 entries per batch
    let mut token = String::new();
    let (mut dirs, mut files, mut batches) = (0, 0, 0);
    loop {
        let mut q = "/recs/top?action=setAccessControlRecursive&mode=modify&maxRecords=3".to_string();
        if !token.is_empty() {
            q.push_str(&format!("&continuation={}", token.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")));
        }
        let r = l.d("PATCH", &q, &[("x-ms-acl", &format!("user:{uid}:r-x"))], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        let j = r.json();
        assert_eq!(j["failureCount"], 0);
        assert_eq!(j["failedEntries"].as_array().unwrap().len(), 0);
        dirs += j["directoriesSuccessful"].as_i64().unwrap();
        files += j["filesSuccessful"].as_i64().unwrap();
        batches += 1;
        match r.header("x-ms-continuation") {
            Some(t) => token = t.to_string(),
            None => break,
        }
    }
    assert_eq!((dirs, files, batches), (3, 4, 3));
    let r = l.d("HEAD", "/recs/top/s1/b?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-acl"), Some(format!("user::rw-,user:{uid}:r-x,group::r--,mask::r-x,other::---").as_str()));
    // remove: named entries and the mask go
    let r = l.d("PATCH", "/recs/top?action=setAccessControlRecursive&mode=remove", &[("x-ms-acl", &format!("user:{uid},mask"))], "").await;
    assert_eq!(r.status, 200);
    assert_eq!((r.json()["directoriesSuccessful"].as_i64(), r.json()["filesSuccessful"].as_i64()), (Some(3), Some(4)));
    let r = l.d("HEAD", "/recs/top/s1/b?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-acl"), Some("user::rw-,group::r--,other::---"));
    // set: replaces everything (a file takes no default entries)
    let r = l
        .d(
            "PATCH",
            "/recs/top?action=setAccessControlRecursive&mode=set",
            &[("x-ms-acl", "user::rwx,group::r-x,other::r--,default:user::rwx,default:group::r-x,default:other::---")],
            "",
        )
        .await;
    assert_eq!(r.status, 200);
    let r = l.d("HEAD", "/recs/top/s2/d?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-acl"), Some("user::rwx,group::r-x,other::r--"));
    let r = l.d("HEAD", "/recs/top/s2?action=getAccessControl", &[], "").await;
    assert!(r.header("x-ms-acl").unwrap().contains("default:other::---"));
    // mode and acl are required
    let r = l.d("PATCH", "/recs/top?action=setAccessControlRecursive", &[("x-ms-acl", "user::rwx")], "").await;
    assert_eq!(r.status, 400);
    let r = l.d("PATCH", "/recs/top?action=setAccessControlRecursive&mode=set", &[], "").await;
    assert_eq!(r.status, 400);
    let r = l
        .d("PATCH", "/recs/nothing?action=setAccessControlRecursive&mode=set", &[("x-ms-acl", "user::rwx,group::r-x,other::---")], "")
        .await;
    assert_eq!((r.status, code(&r).as_str()), (404, "PathNotFound"));
}

#[tokio::test]
async fn both_endpoints_see_the_same_data() {
    let l = lake().await;
    // blob -> dfs: a blob in a virtual folder makes directories once the container is used through dfs
    assert_eq!(l.b("PUT", "/mix?restype=container", &[], "").await.status, 201);
    assert_eq!(l.b("PUT", "/mix/x/y/z.txt", &[("x-ms-blob-type", "BlockBlob")], "from blob").await.status, 201);
    assert_eq!(l.names("mix", "recursive=true").await, vec!["x", "x/y", "x/y/z.txt"]);
    let r = l.d("GET", "/mix/x/y/z.txt", &[], "").await;
    assert_eq!(r.text(), "from blob");
    // the container keeps adopting blob-endpoint writes later on
    assert_eq!(l.b("PUT", "/mix/p/q", &[("x-ms-blob-type", "BlockBlob")], "late").await.status, 201);
    assert_eq!(l.names("mix", "recursive=true&directory=p").await, vec!["p/q"]);
    // a blob written over a Data Lake file keeps its owner
    l.d("PATCH", "/mix/x/y/z.txt?action=setAccessControl", &[("x-ms-owner", "carol")], "").await;
    l.b("PUT", "/mix/x/y/z.txt", &[("x-ms-blob-type", "BlockBlob")], "again").await;
    let r = l.d("HEAD", "/mix/x/y/z.txt?action=getAccessControl", &[], "").await;
    assert_eq!(r.header("x-ms-owner"), Some("carol"));
    // a directory blob made with the blob SDK convention is a directory
    l.b("PUT", "/mix/folder", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-hdi_isfolder", "true")], "").await;
    let r = l.d("GET", "/mix?resource=filesystem&recursive=false&directory=", &[], "").await;
    let dirs: Vec<String> = r.json()["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["isDirectory"] == "true")
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(dirs, vec!["folder", "p", "x"]);

    // dfs -> blob: a directory is a zero-length blob marked hdi_isfolder
    l.fs("fs3").await;
    l.d("PUT", "/fs3/dir?resource=directory", &[("x-ms-properties", "hello=d29ybGQ=")], "").await;
    l.file("fs3/dir/file.txt", "payload").await;
    let r = l.b("HEAD", "/fs3/dir", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-meta-hdi_isfolder"), Some("true"));
    assert_eq!(r.header("x-ms-meta-hello"), Some("world"));
    assert_eq!(r.header("x-ms-resource-type"), Some("directory"));
    assert_eq!(r.header("content-length"), Some("0"));
    // dfs properties
    let r = l.d("HEAD", "/fs3/dir", &[], "").await;
    assert_eq!(r.header("x-ms-properties"), Some("hello=d29ybGQ="));
    // blob listing: with a delimiter the directory is a blob and a prefix
    let r = l.b("GET", "/fs3?restype=container&comp=list&delimiter=/", &[], "").await;
    assert!(r.text().contains("<Blob><Name>dir</Name>") && r.text().contains("<BlobPrefix><Name>dir/</Name></BlobPrefix>"), "{}", r.text());
    let r = l.b("GET", "/fs3?restype=container&comp=list&prefix=dir/&include=metadata", &[], "").await;
    let names = r.text();
    assert!(names.contains("<Name>dir/file.txt</Name>") && names.contains("<ResourceType>file</ResourceType>"), "{names}");
    // deleting through the blob endpoint is seen by dfs
    assert_eq!(l.b("DELETE", "/fs3/dir/file.txt", &[], "").await.status, 202);
    assert_eq!(l.names("fs3", "recursive=true").await, vec!["dir"]);
    // metadata set through the blob endpoint keeps a directory a directory
    l.b("PUT", "/fs3/dir?comp=metadata", &[("x-ms-meta-k", "v")], "").await;
    let r = l.b("HEAD", "/fs3/dir", &[], "").await;
    assert_eq!((r.header("x-ms-meta-hdi_isfolder"), r.header("x-ms-meta-k")), (Some("true"), Some("v")));
    // account information says the account is hierarchical
    let r = l.b("GET", "/?restype=account&comp=properties", &[], "").await;
    assert_eq!(r.header("x-ms-is-hns-enabled"), Some("true"));
    let r = l.d("GET", "/?restype=account&comp=properties", &[], "").await;
    assert_eq!(r.header("x-ms-is-hns-enabled"), Some("true"));
}

#[tokio::test]
async fn a_container_used_only_through_blob_stays_flat() {
    let l = lake().await;
    assert_eq!(l.b("PUT", "/flat?restype=container", &[], "").await.status, 201);
    l.b("PUT", "/flat/a/b/c", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    let r = l.b("GET", "/flat?restype=container&comp=list", &[], "").await;
    assert_eq!(azemu_names(&r.text()), vec!["a/b/c"]);
    let r = l.b("HEAD", "/flat/a/b/c", &[], "").await;
    assert!(r.header("x-ms-owner").is_none() && r.header("x-ms-resource-type").is_none());
    let r = l.b("GET", "/?restype=account&comp=properties", &[], "").await;
    assert_eq!(r.header("x-ms-is-hns-enabled"), Some("false"));
    // Set Blob Expiry is for hierarchical containers only
    let r = l.b("PUT", "/flat/a/b/c?comp=expiry", &[("x-ms-expiry-option", "NeverExpire")], "").await;
    assert_eq!(r.status, 501);
}

fn azemu_names(xml: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find("<Name>") {
        let a = &rest[i + 6..];
        let Some(j) = a.find("</Name>") else { break };
        out.push(a[..j].to_string());
        rest = &a[j..];
    }
    out
}

#[tokio::test]
async fn leases_expiry_and_properties() {
    let l = lake().await;
    l.fs("leas").await;
    l.file("leas/f", "x").await;
    // lease through the dfs endpoint
    let id = "5b2a7bd8-0f2f-4f5c-a6b7-2b1d4f4cd2aa";
    let r = l
        .d("POST", "/leas/f", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1"), ("x-ms-proposed-lease-id", id)], "")
        .await;
    assert_eq!((r.status, r.header("x-ms-lease-id")), (201, Some(id)));
    assert_eq!(l.b("HEAD", "/leas/f", &[], "").await.header("x-ms-lease-state"), Some("leased"));
    let r = l.d("DELETE", "/leas/f", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"));
    let r = l.d("PATCH", "/leas/f?action=append&position=1", &[], "y").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "LeaseIdMissing"));
    let r = l.d("PATCH", "/leas/f?action=append&position=1", &[("x-ms-lease-id", id)], "y").await;
    assert_eq!(r.status, 202);
    // lease actions on append and flush
    let r = l.d("PATCH", "/leas/f?action=flush&position=2", &[("x-ms-lease-id", id), ("x-ms-lease-action", "release")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(l.b("HEAD", "/leas/f", &[], "").await.header("x-ms-lease-state"), Some("available"));
    // a lease taken on create
    let r = l.d("PUT", "/leas/g?resource=file", &[("x-ms-proposed-lease-id", id), ("x-ms-lease-duration", "15")], "").await;
    assert_eq!(r.status, 201);
    assert_eq!(l.b("HEAD", "/leas/g", &[], "").await.header("x-ms-lease-duration"), Some("fixed"));

    // expiry on create and through Set Expiry
    let r = l
        .d("PUT", "/leas/e?resource=file", &[("x-ms-expiry-option", "Absolute"), ("x-ms-expiry-time", "Thu, 04 Apr 2075 00:00:00 GMT")], "")
        .await;
    assert_eq!(r.status, 201);
    let r = l.b("HEAD", "/leas/e", &[], "").await;
    assert_eq!(r.header("x-ms-expiry-time"), Some("Thu, 04 Apr 2075 00:00:00 GMT"));
    let r = l.b("PUT", "/leas/e?comp=expiry", &[("x-ms-expiry-option", "NeverExpire")], "").await;
    assert_eq!(r.status, 200);
    assert!(l.b("HEAD", "/leas/e", &[], "").await.header("x-ms-expiry-time").is_none());
    let r = l.d("PUT", "/leas/e?comp=expiry", &[("x-ms-expiry-option", "RelativeToNow"), ("x-ms-expiry-time", "60000")], "").await;
    assert_eq!(r.status, 200);
    let r = l.d("GET", "/leas?resource=filesystem&recursive=true&directory=", &[], "").await;
    let e = r.json()["paths"].as_array().unwrap().iter().find(|p| p["name"] == "e").cloned().unwrap();
    assert_ne!(e["expiryTime"], "0");
    let r = l
        .d("PUT", "/leas/e?comp=expiry", &[("x-ms-expiry-option", "Absolute"), ("x-ms-expiry-time", "Mon, 01 Jan 2001 00:00:00 GMT")], "")
        .await;
    assert_eq!(r.status, 400);

    // set properties: replaces the user properties, sets content headers
    let r = l.d("PATCH", "/leas/f?action=setProperties", &[("x-ms-lease-id", "x"), ("x-ms-properties", "k=dg==")], "").await;
    assert_eq!(r.status, 400);
    l.d("POST", "/leas/f", &[("x-ms-lease-action", "release"), ("x-ms-lease-id", id)], "").await;
    let r = l.d("PATCH", "/leas/f?action=setProperties", &[("x-ms-properties", "k=dg=="), ("x-ms-content-type", "text/plain")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = l.b("HEAD", "/leas/f", &[], "").await;
    assert_eq!((r.header("x-ms-meta-k"), r.header("content-type")), (Some("v"), Some("text/plain")));
}

#[tokio::test]
async fn errors_and_auth_on_the_dfs_endpoint() {
    let l = lake().await;
    l.fs("errs").await;
    // unknown operations are 501 in the dfs error form
    let r = l.d("PUT", "/errs/f?bogus=1", &[], "").await;
    assert_eq!(r.status, 501);
    assert!(r.text().contains("\"code\":\"NotImplemented\""), "{}", r.text());
    // anonymous: the authentication error as JSON
    let r = l.srv.send("GET", &format!("{}/errs?resource=filesystem&recursive=true", l.dfs), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"));
    assert!(r.header("www-authenticate").is_some());
    assert!(r.text().starts_with("{\"error\""), "{}", r.text());
    // a SAS with an expiry in the past, and one without the permission
    let past = "sv=2021-08-06&sr=c&sp=rl&se=2001-01-01T00:00:00Z&sig=x";
    let r = l.srv.send("GET", &format!("{}/errs?resource=filesystem&recursive=true&{past}", l.dfs), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    let ok = "sv=2021-08-06&sr=c&sp=r&se=2099-01-01T00:00:00Z&sig=x";
    let r = l.srv.send("GET", &format!("{}/errs?resource=filesystem&recursive=true&{ok}", l.dfs), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let r = l.srv.send("PUT", &format!("{}/errs/created?resource=file&{ok}", l.dfs), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    // ownership and permission changes need `o` / `p`
    l.file("errs/f", "x").await;
    let w = "sv=2021-08-06&sr=c&sp=rw&se=2099-01-01T00:00:00Z&sig=x";
    let r = l.srv.send("PATCH", &format!("{}/errs/f?action=setAccessControl&{w}", l.dfs), &[("x-ms-permissions", "0777")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let wp = "sv=2021-08-06&sr=c&sp=rwp&se=2099-01-01T00:00:00Z&sig=x";
    let r = l.srv.send("PATCH", &format!("{}/errs/f?action=setAccessControl&{wp}", l.dfs), &[("x-ms-permissions", "0777")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = l.srv.send("PATCH", &format!("{}/errs/f?action=setAccessControl&{wp}", l.dfs), &[("x-ms-owner", "me")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    // checkAccess on an existing path
    assert_eq!(l.d("HEAD", "/errs/f?action=checkAccess&fsAction=r--", &[], "").await.status, 200);
    assert_eq!(l.d("HEAD", "/errs/none?action=getAccessControl", &[], "").await.status, 404);
    // bad parameters
    assert_eq!(l.d("GET", "/errs?resource=filesystem&recursive=maybe", &[], "").await.status, 400);
    let r = l.d("PUT", "/errs/x?resource=file", &[("x-ms-version", "garbage")], "").await;
    assert_eq!(r.status, 400);
    // reset drops everything, for both services
    l.srv.reset();
    assert_eq!(l.b("GET", "/errs?restype=container", &[], "").await.status, 404);
}

#[tokio::test]
async fn customer_provided_key_and_content_headers() {
    let l = lake().await;
    l.fs("cpks").await;
    // a real key and its hash: Blob checks that the headers agree; the placeholders "a2V5" and "c2hh" are refused
    let (k, h) = ("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=", "S7Bvjk46dxXSAdVz0KpCN2LlXavWGiwCJ4+lbMbSlOA=");
    let key = [("x-ms-encryption-key", k), ("x-ms-encryption-key-sha256", h), ("x-ms-encryption-algorithm", "AES256")];
    let r = l.d("PUT", "/cpks/f?resource=file", &key, "").await;
    assert_eq!((r.status, r.header("x-ms-encryption-key-sha256")), (201, Some(h)));
    let r = l.d("PATCH", "/cpks/f?action=append&position=0", &key, "abc").await;
    assert_eq!((r.status, r.header("x-ms-encryption-key-sha256")), (202, Some(h)));
    let r = l.d("PATCH", "/cpks/f?action=flush&position=3", &key, "").await;
    assert_eq!((r.status, r.header("x-ms-request-server-encrypted")), (200, Some("true")));
    let r = l.b("HEAD", "/cpks/f", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"));
    assert_eq!(l.b("HEAD", "/cpks/f", &key, "").await.status, 200);
    // content settings on create and flush
    l.d(
        "PUT",
        "/cpks/g?resource=file",
        &[
            ("x-ms-content-type", "text/csv"),
            ("x-ms-content-disposition", "inline"),
            ("x-ms-content-language", "es"),
            ("x-ms-cache-control", "no-cache"),
        ],
        "",
    )
    .await;
    let r = l.d("GET", "/cpks/g", &[], "").await;
    assert_eq!(r.header("content-type"), Some("text/csv"));
    assert_eq!(
        (r.header("content-disposition"), r.header("content-language"), r.header("cache-control")),
        (Some("inline"), Some("es"), Some("no-cache"))
    );
    l.d("PATCH", "/cpks/g?action=append&position=0", &[], "1,2").await;
    l.d("PATCH", "/cpks/g?action=flush&position=3", &[("x-ms-content-type", "text/plain")], "").await;
    assert_eq!(l.d("HEAD", "/cpks/g", &[], "").await.header("content-type"), Some("text/plain"));
}

mod dev_account {
    //! Signatures on the `dfs` endpoint, checked for the development account.

    use super::*;
    use azemu_core::storage::sig::{hmac_sha256, service_sas_string_to_sign, SasFields, StorageService};
    use azemu_core::storage::{DEV_ACCOUNT, DEV_ACCOUNT_KEY};
    use azemu_core::Query;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;

    async fn dev() -> (TestServer, String, String) {
        let blob = BlobService::new();
        let dfs = blob.datalake();
        let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
        let (d, b) = (srv.url("dfs", DEV_ACCOUNT), srv.url("blob", DEV_ACCOUNT));
        (srv, d, b)
    }

    fn sign(sts: &str) -> String {
        let key = B64.decode(DEV_ACCOUNT_KEY).unwrap();
        B64.encode(hmac_sha256(&key, sts.as_bytes()))
    }

    fn shared_key(method: &str, date: &str, resource: &str, query_lines: &str) -> String {
        let sts =
            format!("{method}\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:2023-11-03\n/{DEV_ACCOUNT}/{resource}{query_lines}");
        format!("SharedKey {DEV_ACCOUNT}:{}", sign(&sts))
    }

    #[tokio::test]
    async fn shared_key_on_the_dfs_endpoint() {
        let (srv, d, _) = dev().await;
        let date = "Fri, 27 Mar 2026 01:00:51 GMT";
        let h = |auth: &str| {
            vec![
                ("Authorization".to_string(), auth.to_string()),
                ("x-ms-date".to_string(), date.to_string()),
                ("x-ms-version".to_string(), "2023-11-03".to_string()),
            ]
        };
        let send = |method: &'static str, url: String, auth: String, body: &'static str| {
            let srv = &srv;
            async move {
                let hv = h(&auth);
                let refs: Vec<(&str, &str)> = hv.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                srv.send(method, &url, &refs, body).await
            }
        };
        // create filesystem: PUT /fs?resource=filesystem
        let auth = shared_key("PUT", date, "dfsfs", "\nresource:filesystem");
        let r = send("PUT", format!("{d}/dfsfs?resource=filesystem"), auth.clone(), "").await;
        assert_eq!(r.status, 201, "{}", r.text());
        // the same signature does not open another request
        let r = send("PUT", format!("{d}/other?resource=filesystem"), auth, "").await;
        assert_eq!(r.status, 403);
        assert!(r.text().contains("\"AuthenticationErrorDetail\""), "{}", r.text());
        assert_eq!(r.header("x-ms-error-code"), Some("AuthenticationFailed"));
        // list paths: query parameters are part of the signed resource, sorted by name
        let auth = shared_key("GET", date, "dfsfs", "\nrecursive:true\nresource:filesystem");
        let r = send("GET", format!("{d}/dfsfs?resource=filesystem&recursive=true"), auth, "").await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    fn sas_token(path: &str, sr: &str, sp: &str, depth: Option<usize>) -> String {
        let mut q = format!("sv=2021-08-06&se=2099-01-01T00:00:00Z&sr={sr}&sp={sp}");
        if let Some(d) = depth {
            q.push_str(&format!("&sdd={d}"));
        }
        let f = SasFields::from_query(&Query::parse(&q));
        let sts = service_sas_string_to_sign(StorageService::Blob, DEV_ACCOUNT, &f, path, "", None);
        format!(
            "{q}&sig={}",
            B64.encode(hmac_sha256(&B64.decode(DEV_ACCOUNT_KEY).unwrap(), sts.as_bytes()))
                .replace('+', "%2B")
                .replace('/', "%2F")
                .replace('=', "%3D")
        )
    }

    #[tokio::test]
    async fn directory_sas_covers_the_directory_and_what_is_below_it() {
        let (srv, d, b) = dev().await;
        let key = [("Authorization", format!("SharedKey {DEV_ACCOUNT}:c2ln"))];
        let k: Vec<(&str, &str)> = key.iter().map(|(a, b)| (*a, b.as_str())).collect();
        srv.send("PUT", &format!("{d}/sas?resource=filesystem"), &k, "").await;
        for p in ["d1/sub/f.txt", "d2/f.txt"] {
            assert_eq!(srv.send("PUT", &format!("{d}/sas/{p}?resource=file"), &k, "").await.status, 201);
        }
        let tok = sas_token("sas/d1", "d", "rwcdl", Some(1));
        // below the directory, on both endpoints
        let r = srv.send("HEAD", &format!("{d}/sas/d1/sub/f.txt?action=getStatus&{tok}"), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        let r = srv.send("HEAD", &format!("{b}/sas/d1/sub/f.txt?{tok}"), &[], "").await;
        assert_eq!(r.status, 200);
        let r = srv.send("PUT", &format!("{d}/sas/d1/sub/new?resource=file&{tok}"), &[], "").await;
        assert_eq!(r.status, 201, "{}", r.text());
        // not another directory: the signature covers d1 only
        let r = srv.send("HEAD", &format!("{d}/sas/d2/f.txt?action=getStatus&{tok}"), &[], "").await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthenticationFailed")));
        // a directory SAS is not valid for the filesystem itself (the signed resource differs)
        let r = srv.send("GET", &format!("{d}/sas?resource=filesystem&recursive=true&{tok}"), &[], "").await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthenticationFailed")));
        // a tampered permission string no longer matches the signature
        let bad = tok.replace("sp=rwcdl", "sp=rwcdlm");
        let r = srv.send("HEAD", &format!("{d}/sas/d1/sub/f.txt?action=getStatus&{bad}"), &[], "").await;
        assert_eq!(r.status, 403);
        // the filesystem SAS opens everything in it
        let fs_tok = sas_token("sas", "c", "rl", None);
        let r = srv.send("GET", &format!("{d}/sas?resource=filesystem&recursive=true&{fs_tok}"), &[], "").await;
        assert_eq!(r.status, 200, "{}", r.text());
        let r = srv.send("PUT", &format!("{d}/sas/denied?resource=file&{fs_tok}"), &[], "").await;
        assert_eq!((r.status, r.header("x-ms-error-code")), (403, Some("AuthorizationPermissionMismatch")));
    }
}
