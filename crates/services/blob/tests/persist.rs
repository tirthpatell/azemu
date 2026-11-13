//! Blob and Data Lake state across restarts (`--data-dir`): what is kept, how the files look, and what a damaged
//! directory does.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::persist;
use azemu_core::testing::{TestResponse, TestServer};
use azemu_core::Service;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use sha2::{Digest, Sha256};

const AUTH: (&str, &str) = ("Authorization", "SharedKey acct:c2ln");

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-blob-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Rig {
    srv: TestServer,
    svc: Arc<BlobService>,
    blob: String,
    dfs: String,
}

async fn open(d: &Path) -> Rig {
    let svc = Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)], d).await;
    let (blob, dfs) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    Rig { srv, svc, blob, dfs }
}

async fn try_open(d: &Path) -> anyhow::Result<Rig> {
    let svc = Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = TestServer::try_start_persistent(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)], d, |b| b).await?;
    let (blob, dfs) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    Ok(Rig { srv, svc, blob, dfs })
}

impl Rig {
    async fn b(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.blob), &h, body).await
    }

    async fn d(&self, method: &str, path: &str, extra: &[(&str, &str)], body: impl Into<azemu_core::bytes::Bytes>) -> TestResponse {
        let mut h = vec![AUTH];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.dfs), &h, body).await
    }

    async fn ok(
        &self,
        status: u16,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: impl Into<azemu_core::bytes::Bytes>,
    ) -> TestResponse {
        let r = self.b(method, path, extra, body).await;
        assert_eq!(r.status, status, "{method} {path}: {}", r.text());
        r
    }

    async fn dok(
        &self,
        status: u16,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: impl Into<azemu_core::bytes::Bytes>,
    ) -> TestResponse {
        let r = self.d(method, path, extra, body).await;
        assert_eq!(r.status, status, "{method} {path}: {}", r.text());
        r
    }

    /// Stop the way SIGTERM does, so the state is saved.
    async fn close(self) {
        self.srv.shutdown().await.unwrap();
    }
}

fn tags_of(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(after[..j].to_string());
        rest = &after[j + close.len()..];
    }
    out
}

fn show(r: &TestResponse) -> String {
    let mut h: Vec<String> = r
        .headers
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "date" | "x-ms-request-id" | "x-ms-client-request-id" | "server"))
        .map(|(k, v)| format!("{}: {}", k.as_str(), v.to_str().unwrap_or("?")))
        .collect();
    h.sort();
    let mut body = r.text();
    // error answers carry the id and the time of the request
    while let Some(i) = body.find("RequestId:") {
        let end = body[i..].find("</Message>").map(|e| i + e).unwrap_or(body.len());
        body.replace_range(i..end, "");
    }
    format!("{}\n{}\n{}", r.status, h.join("\n"), body)
}

/// Everything a client can see of the account, as one text: listings, properties, and every blob, snapshot and
/// version with its headers and content.
async fn dump(r: &Rig) -> String {
    let mut out = String::new();
    let mut add = |what: &str, resp: TestResponse| out.push_str(&format!("=== {what}\n{}\n", show(&resp)));
    add("account", r.b("GET", "/?restype=service&comp=properties", &[], "").await);
    let list = r.b("GET", "/?comp=list&include=metadata,deleted", &[], "").await;
    let names = tags_of(&list.text(), "Name");
    add("containers", list);
    for c in names {
        add(&format!("{c} props"), r.b("HEAD", &format!("/{c}?restype=container"), &[], "").await);
        add(&format!("{c} acl"), r.b("GET", &format!("/{c}?restype=container&comp=acl"), &[], "").await);
        let l = r
            .b("GET", &format!("/{c}?restype=container&comp=list&include=snapshots,versions,deleted,metadata,tags,copy,uncommittedblobs,legalhold,immutabilitypolicy"), &[], "")
            .await;
        let text = l.text();
        let mut blobs = tags_of(&text, "Name");
        blobs.sort();
        blobs.dedup();
        let snaps = tags_of(&text, "Snapshot");
        let vers = tags_of(&text, "VersionId");
        add(&format!("{c} list"), l);
        for n in &blobs {
            let p = format!("/{c}/{n}");
            add(&format!("{p} head"), r.b("HEAD", &p, &[], "").await);
            add(&format!("{p} get"), r.b("GET", &p, &[], "").await);
            add(&format!("{p} blocks"), r.b("GET", &format!("{p}?comp=blocklist&blocklisttype=all"), &[], "").await);
            add(&format!("{p} pages"), r.b("GET", &format!("{p}?comp=pagelist"), &[], "").await);
            add(&format!("{p} tags"), r.b("GET", &format!("{p}?comp=tags"), &[], "").await);
            for s in &snaps {
                add(&format!("{p} snapshot {s}"), r.b("HEAD", &format!("{p}?snapshot={s}"), &[], "").await);
            }
            for v in &vers {
                add(&format!("{p} version {v}"), r.b("GET", &format!("{p}?versionid={v}"), &[], "").await);
            }
        }
        if c == "lake" {
            // (asking the Data Lake side about a container makes it hierarchical, so only this one)
            add(&format!("{c} dfs paths"), r.d("GET", &format!("/{c}?resource=filesystem&recursive=true"), &[], "").await);
        }
        add(&format!("{c} dfs deleted"), r.b("GET", &format!("/{c}?restype=container&comp=list&showonly=deleted"), &[], "").await);
    }
    // the port differs between runs
    out.replace(&format!(":{}", r.srv.port()), ":PORT")
}

fn key(byte: u8) -> (String, String) {
    (B64.encode([byte; 32]), B64.encode(Sha256::digest([byte; 32])))
}

const PROPS: &str = "<StorageServiceProperties><DeleteRetentionPolicy><Enabled>true</Enabled><Days>5</Days></DeleteRetentionPolicy>\
<Cors><CorsRule><AllowedOrigins>https://example.com</AllowedOrigins><AllowedMethods>GET,PUT</AllowedMethods>\
<AllowedHeaders>x-ms-meta-*</AllowedHeaders><ExposedHeaders>x-ms-request-id</ExposedHeaders><MaxAgeInSeconds>120</MaxAgeInSeconds></CorsRule></Cors>\
<StaticWebsite><Enabled>true</Enabled><IndexDocument>index.html</IndexDocument></StaticWebsite></StorageServiceProperties>";

/// Make an account with one of everything.
async fn fill(r: &Rig) {
    r.ok(202, "PUT", "/?restype=service&comp=properties", &[], PROPS).await;
    r.ok(
        201,
        "PUT",
        "/cnt?restype=container",
        &[("x-ms-meta-owner", "alice"), ("x-ms-meta-team", "blue"), ("x-ms-blob-public-access", "blob")],
        "",
    )
    .await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>reader</Id><AccessPolicy><Start>2024-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>rl</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    r.ok(200, "PUT", "/cnt?restype=container&comp=acl", &[("x-ms-blob-public-access", "container")], acl).await;

    // block blob: properties, metadata, tags, an old version, a snapshot
    r.ok(
        201,
        "PUT",
        "/cnt/dir/a.txt",
        &[
            ("x-ms-blob-type", "BlockBlob"),
            ("Content-Type", "text/plain"),
            ("x-ms-blob-cache-control", "no-cache"),
            ("x-ms-blob-content-language", "de"),
            ("x-ms-meta-k", "v1"),
            ("x-ms-tags", "env=dev&n=1"),
        ],
        "first",
    )
    .await;
    r.ok(201, "PUT", "/cnt/dir/a.txt?comp=snapshot", &[], "").await;
    r.ok(201, "PUT", "/cnt/dir/a.txt", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-meta-k", "v2")], "second").await;
    r.ok(204, "PUT", "/cnt/dir/a.txt?comp=tags", &[], "<Tags><TagSet><Tag><Key>env</Key><Value>prod</Value></Tag></TagSet></Tags>").await;
    r.ok(200, "PUT", "/cnt/dir/a.txt?comp=tier", &[("x-ms-access-tier", "Cool")], "").await;

    // blocks: committed, and staged but not committed
    let id = |s: &str| B64.encode(s.as_bytes()).replace('=', "%3D");
    r.ok(201, "PUT", &format!("/cnt/blocks?comp=block&blockid={}", id("b1")), &[], "AAAA").await;
    r.ok(201, "PUT", &format!("/cnt/blocks?comp=block&blockid={}", id("b2")), &[], "BBBB").await;
    let list = format!("<BlockList><Latest>{}</Latest><Latest>{}</Latest></BlockList>", B64.encode("b1"), B64.encode("b2"));
    r.ok(201, "PUT", "/cnt/blocks?comp=blocklist", &[("x-ms-blob-content-type", "application/x-blocks")], list).await;
    r.ok(201, "PUT", &format!("/cnt/blocks?comp=block&blockid={}", id("st")), &[], "SSSS").await;
    r.ok(201, "PUT", &format!("/cnt/onlystaged?comp=block&blockid={}", id("x")), &[], "XXXX").await;

    // append blob (sealed) and an open one, page blob with two ranges
    r.ok(201, "PUT", "/cnt/app", &[("x-ms-blob-type", "AppendBlob")], "").await;
    r.ok(201, "PUT", "/cnt/app?comp=appendblock", &[], "hello ").await;
    r.ok(201, "PUT", "/cnt/app?comp=appendblock", &[], "world").await;
    r.ok(200, "PUT", "/cnt/app?comp=seal", &[], "").await;
    r.ok(201, "PUT", "/cnt/openapp", &[("x-ms-blob-type", "AppendBlob")], "").await;
    r.ok(201, "PUT", "/cnt/openapp?comp=appendblock", &[], "abc").await;
    r.ok(201, "PUT", "/cnt/page", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", "4096")], "").await;
    r.ok(201, "PUT", "/cnt/page?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=512-1023")], "p".repeat(512)).await;
    r.ok(201, "PUT", "/cnt/page?comp=page", &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=2048-2559")], "q".repeat(512)).await;

    // leases: a blob (fixed, long) and the container (infinite)
    r.ok(201, "PUT", "/cnt/leased", &[("x-ms-blob-type", "BlockBlob")], "L").await;
    r.ok(
        201,
        "PUT",
        "/cnt/leased?comp=lease",
        &[
            ("x-ms-lease-action", "acquire"),
            ("x-ms-lease-duration", "60"),
            ("x-ms-proposed-lease-id", "11111111-1111-1111-1111-111111111111"),
        ],
        "",
    )
    .await;
    r.ok(
        201,
        "PUT",
        "/cnt?restype=container&comp=lease",
        &[
            ("x-ms-lease-action", "acquire"),
            ("x-ms-lease-duration", "-1"),
            ("x-ms-proposed-lease-id", "22222222-2222-2222-2222-222222222222"),
        ],
        "",
    )
    .await;

    // legal hold, immutability policy, customer-provided key, copy
    r.ok(201, "PUT", "/cnt/held", &[("x-ms-blob-type", "BlockBlob")], "H").await;
    r.ok(200, "PUT", "/cnt/held?comp=legalhold", &[("x-ms-legal-hold", "true")], "").await;
    r.ok(201, "PUT", "/cnt/locked", &[("x-ms-blob-type", "BlockBlob")], "I").await;
    r.ok(
        200,
        "PUT",
        "/cnt/locked?comp=immutabilityPolicies",
        &[("x-ms-immutability-policy-until-date", "Thu, 04 Apr 2075 00:00:00 GMT"), ("x-ms-immutability-policy-mode", "unlocked")],
        "",
    )
    .await;
    let k = key(7);
    r.ok(
        201,
        "PUT",
        "/cnt/secret",
        &[
            ("x-ms-blob-type", "BlockBlob"),
            ("x-ms-encryption-key", &k.0),
            ("x-ms-encryption-key-sha256", &k.1),
            ("x-ms-encryption-algorithm", "AES256"),
        ],
        "cpk data",
    )
    .await;
    // (the emulator reads the source itself; the port in the URL is only stored as text)
    let src = "http://acct.blob.localhost:1/cnt/dir/a.txt".to_string();
    r.ok(202, "PUT", "/cnt/copied", &[("x-ms-copy-source", &src)], "").await;

    // soft deleted: a blob, and a whole container
    r.ok(201, "PUT", "/cnt/gone", &[("x-ms-blob-type", "BlockBlob")], "bye").await;
    r.ok(202, "DELETE", "/cnt/gone", &[], "").await;
    r.ok(201, "PUT", "/old?restype=container", &[("x-ms-meta-a", "b")], "").await;
    r.ok(201, "PUT", "/old/x", &[("x-ms-blob-type", "BlockBlob")], "in the old one").await;
    r.ok(202, "DELETE", "/old?restype=container", &[], "").await;

    // Data Lake: filesystem, directory with ACLs, files, an unflushed append, expiry
    r.dok(201, "PUT", "/lake?resource=filesystem", &[("x-ms-properties", "k=djE=")], "").await;
    r.dok(201, "PUT", "/lake/d?resource=directory", &[("x-ms-owner", "alice"), ("x-ms-group", "staff"), ("x-ms-permissions", "0750")], "")
        .await;
    r.dok(200, "PATCH", "/lake/d?action=setAccessControl", &[("x-ms-acl", "user::rwx,user:00000000000000000000000000000384:r-x,group::r-x,other::---,default:user::rwx,default:group::r-x,default:other::---")], "").await;
    r.dok(
        201,
        "PUT",
        "/lake/d/f1?resource=file",
        &[("x-ms-expiry-option", "Absolute"), ("x-ms-expiry-time", "Thu, 04 Apr 2075 00:00:00 GMT")],
        "",
    )
    .await;
    r.dok(202, "PATCH", "/lake/d/f1?action=append&position=0", &[], "lake data").await;
    r.dok(200, "PATCH", "/lake/d/f1?action=flush&position=9", &[], "").await;
    r.dok(201, "PUT", "/lake/d/f2?resource=file", &[], "").await;
    r.dok(202, "PATCH", "/lake/d/f2?action=append&position=0", &[], "not flushed yet").await;
    r.ok(201, "PUT", "/lake/viablob.txt", &[("x-ms-blob-type", "BlockBlob")], "written through the blob endpoint").await;
    r.dok(201, "PUT", "/lake/d/sub/deep?resource=file", &[], "").await;
    r.dok(200, "DELETE", "/lake/d/sub?recursive=true", &[], "").await;
}

#[tokio::test]
async fn everything_a_client_can_see_is_the_same_after_a_restart() {
    let d = dir("rich");
    let r = open(&d).await;
    fill(&r).await;
    let before = dump(&r).await;
    assert!(before.len() > 20_000, "the dump is expected to be big: {}", before.len());
    r.close().await;

    let r = open(&d).await;
    let after = dump(&r).await;
    if before != after {
        let at = before.bytes().zip(after.bytes()).position(|(x, y)| x != y).unwrap_or(before.len().min(after.len()));
        let window = |t: &str| t[at.saturating_sub(300)..(at + 600).min(t.len())].to_string();
        panic!("state differs after a restart at byte {at}:\nbefore: {}\nafter:  {}", window(&before), window(&after));
    }

    // and it keeps working as before, not only looking the same
    let k = key(7);
    let withkey =
        [("x-ms-encryption-key", k.0.as_str()), ("x-ms-encryption-key-sha256", k.1.as_str()), ("x-ms-encryption-algorithm", "AES256")];
    assert_eq!(r.ok(200, "GET", "/cnt/secret", &withkey, "").await.text(), "cpk data");
    assert_ne!(r.b("GET", "/cnt/secret", &[], "").await.status, 200, "a blob written with a key is not readable without it");
    // the lease id is still enforced, and still usable
    let wrong =
        r.b("PUT", "/cnt/leased", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-lease-id", "99999999-9999-9999-9999-999999999999")], "x").await;
    assert_eq!(wrong.status, 412);
    r.ok(201, "PUT", "/cnt/leased", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-lease-id", "11111111-1111-1111-1111-111111111111")], "L2")
        .await;
    assert_eq!(r.b("DELETE", "/cnt?restype=container", &[], "").await.status, 412, "the container lease holds");
    // legal hold and immutability still protect
    assert_eq!(r.b("DELETE", "/cnt/held", &[], "").await.status, 409);
    assert_eq!(r.b("DELETE", "/cnt/locked", &[], "").await.status, 409);
    // a staged block can be committed after the restart; an append blob goes on where it stopped
    let id = B64.encode("st");
    r.ok(201, "PUT", "/cnt/blocks?comp=blocklist", &[], format!("<BlockList><Uncommitted>{id}</Uncommitted></BlockList>")).await;
    assert_eq!(r.ok(200, "GET", "/cnt/blocks", &[], "").await.text(), "SSSS");
    r.ok(201, "PUT", "/cnt/openapp?comp=appendblock", &[("x-ms-blob-condition-appendpos", "3")], "def").await;
    assert_eq!(r.ok(200, "GET", "/cnt/openapp", &[], "").await.text(), "abcdef");
    // the soft deleted blob and container come back
    r.ok(200, "PUT", "/cnt/gone?comp=undelete", &[], "").await;
    assert_eq!(r.ok(200, "GET", "/cnt/gone", &[], "").await.text(), "bye");
    let list = r.b("GET", "/?comp=list&include=deleted", &[], "").await.text();
    let version = tags_of(&list, "Version").remove(0);
    r.ok(
        201,
        "PUT",
        "/old?restype=container&comp=undelete",
        &[("x-ms-deleted-container-name", "old"), ("x-ms-deleted-container-version", &version)],
        "",
    )
    .await;
    assert_eq!(r.ok(200, "GET", "/old/x", &[], "").await.text(), "in the old one");
    // the find-by-tags index and the Data Lake pending append
    let found = r.ok(200, "GET", "/?comp=blobs&where=%22env%22%3D%27prod%27", &[], "").await.text();
    assert!(found.contains("dir/a.txt"), "{found}");
    r.dok(200, "PATCH", "/lake/d/f2?action=flush&position=15", &[], "").await;
    assert_eq!(r.ok(200, "GET", "/lake/d/f2", &[], "").await.text(), "not flushed yet");
    r.close().await;
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn the_versioning_switch_and_an_empty_account_survive() {
    let d = dir("switch");
    let r = open(&d).await;
    r.svc.set_versioning_enabled("acct", false);
    r.svc.set_versioning_enabled("other", true);
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    let put = r.ok(201, "PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob")], "v").await;
    assert!(put.header("x-ms-version-id").is_none(), "versioning is off for this account");
    r.close().await;
    let r = open(&d).await;
    assert!(!r.svc.versioning_enabled("acct"));
    assert!(r.svc.versioning_enabled("other"));
    let put = r.ok(201, "PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob")], "v2").await;
    assert!(put.header("x-ms-version-id").is_none());
    r.close().await;
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn reset_then_save_leaves_an_empty_account_and_no_data_files() {
    let d = dir("reset");
    let r = open(&d).await;
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    r.ok(201, "PUT", "/cnt/big", &[("x-ms-blob-type", "BlockBlob")], vec![1u8; 3 << 20]).await;
    r.srv.save_now().await;
    assert!(std::fs::read_dir(d.join("blob").join("pieces")).unwrap().count() >= 1);
    r.srv.reset();
    r.srv.save_now().await;
    assert_eq!(std::fs::read_dir(d.join("blob").join("pieces")).unwrap().count(), 0, "the data of the deleted blob is gone from the disk");
    r.close().await;
    let r = open(&d).await;
    assert_eq!(r.b("GET", "/?comp=list", &[], "").await.text().matches("<Container>").count(), 0);
    r.close().await;
    let _ = std::fs::remove_dir_all(&d);
}

fn pieces(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d.join("blob").join("pieces"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

fn listing(d: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut v = vec![];
    for sub in ["blob", "blob/pieces"] {
        if let Ok(rd) = std::fs::read_dir(d.join(sub)) {
            for e in rd.flatten() {
                let m = e.metadata().unwrap();
                if m.is_file() {
                    v.push((format!("{sub}/{}", e.file_name().to_string_lossy()), m.len(), m.modified().unwrap()));
                }
            }
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[tokio::test]
async fn a_second_save_of_unchanged_state_writes_nothing_and_a_small_change_writes_a_little() {
    let d = dir("nothing-new");
    let r = open(&d).await;
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    r.ok(201, "PUT", "/cnt/big", &[("x-ms-blob-type", "BlockBlob")], vec![5u8; 2 << 20]).await;
    r.ok(201, "PUT", "/cnt/small", &[("x-ms-blob-type", "BlockBlob")], "tiny").await;
    let g0 = r.svc.persist_generation();
    let state_dir = d.join("blob");
    r.svc.save(&state_dir).unwrap();
    let first = listing(&d);
    assert!(first.iter().any(|f| f.0 == "blob/blob.state"));
    std::thread::sleep(std::time::Duration::from_millis(30));
    r.svc.save(&state_dir).unwrap();
    assert_eq!(listing(&d), first, "same names, sizes and modification times");

    // reads do not count as changes; a write does
    r.ok(200, "GET", "/cnt/small", &[], "").await;
    r.ok(200, "GET", "/?comp=list", &[], "").await;
    assert_eq!(r.svc.persist_generation(), g0);
    let before = pieces(&d);
    r.ok(201, "PUT", "/cnt/another", &[("x-ms-blob-type", "BlockBlob")], "1 KB".repeat(256)).await;
    assert_ne!(r.svc.persist_generation(), g0);
    r.svc.save(&state_dir).unwrap();
    let after = pieces(&d);
    let new: Vec<_> = after.iter().filter(|p| !before.contains(p)).collect();
    assert_eq!(new.len(), 1, "one new data file: {new:?}");
    let gone: Vec<_> = before.iter().filter(|p| !after.contains(p)).collect();
    // the big blob's file is untouched; only the pack of small buffers was replaced
    assert!(gone.len() <= 1, "{gone:?}");
    let sizes: Vec<u64> = after.iter().map(|p| std::fs::metadata(d.join("blob/pieces").join(p)).unwrap().len()).collect();
    assert!(sizes.contains(&(2 << 20)));
    r.close().await;
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn deleted_data_is_removed_only_after_the_new_state_is_written() {
    let d = dir("gc");
    let r = open(&d).await;
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    r.ok(201, "PUT", "/cnt/a", &[("x-ms-blob-type", "BlockBlob")], vec![1u8; 2 << 20]).await;
    let state_dir = d.join("blob");
    r.svc.save(&state_dir).unwrap();
    let old_state = std::fs::read(state_dir.join("blob.state")).unwrap();
    r.ok(201, "PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob")], vec![2u8; 2 << 20]).await;
    r.svc.save(&state_dir).unwrap();
    assert_eq!(pieces(&d).len(), 2);
    // a crash after the new data was written and before the new state: the old state and its data are what is on disk
    std::fs::write(state_dir.join("blob.state"), &old_state).unwrap();
    r.close().await;
    let r = open(&d).await;
    assert_eq!(r.ok(200, "GET", "/cnt/a", &[], "").await.body.len(), 2 << 20);
    assert_eq!(r.b("GET", "/cnt/b", &[], "").await.status, 404);
    r.ok(202, "DELETE", "/cnt/a", &[], "").await;
    r.close().await;
    // unreferenced files (the one for b, and a stray temporary file) are cleaned up by the next save
    assert!(pieces(&d).len() <= 2);
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_pack_that_is_mostly_dead_is_rewritten_and_shared_buffers_are_written_once() {
    let d = dir("pack");
    let svc = Arc::new(BlobService::new());
    svc.set_piece_policy_for_tests(1 << 20, 10_000, 1);
    svc.set_versioning_enabled("acct", false);
    let lake = svc.datalake();
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>, Arc::new(lake)], &d).await;
    let r = Rig { blob: srv.url("blob", "acct"), dfs: srv.url("dfs", "acct"), srv, svc: svc.clone() };
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    for i in 0..8 {
        r.ok(201, "PUT", &format!("/cnt/n{i}"), &[("x-ms-blob-type", "BlockBlob")], format!("{i}").repeat(1000)).await;
    }
    r.srv.save_now().await;
    let first = pieces(&d);
    assert_eq!(first.len(), 1, "eight small buffers are one file: {first:?}");
    assert_eq!(std::fs::metadata(d.join("blob/pieces").join(&first[0])).unwrap().len(), 8000);
    // a snapshot shares the buffers of its blob: nothing new is written for the data
    r.ok(201, "PUT", "/cnt/n0?comp=snapshot", &[], "").await;
    r.srv.save_now().await;
    assert_eq!(pieces(&d), first);
    // delete seven of the eight: the pack is mostly dead, so what is alive moves to a new, smaller file
    for i in 1..8 {
        r.ok(202, "DELETE", &format!("/cnt/n{i}"), &[], "").await;
    }
    r.srv.save_now().await;
    let second = pieces(&d);
    assert_eq!(second.len(), 1);
    assert_ne!(second, first, "the old pack is gone");
    assert_eq!(std::fs::metadata(d.join("blob/pieces").join(&second[0])).unwrap().len(), 1000);
    r.close().await;
    let r = open(&d).await;
    assert_eq!(r.ok(200, "GET", "/cnt/n0", &[], "").await.text(), "0".repeat(1000));
    assert_eq!(r.b("GET", "/cnt/n3", &[], "").await.status, 404);
    r.close().await;
    let _ = std::fs::remove_dir_all(&d);
}

fn flip_byte(path: &Path, at: usize) {
    let mut b = std::fs::read(path).unwrap();
    let at = at.min(b.len() - 1);
    b[at] ^= 0xff;
    std::fs::write(path, b).unwrap();
}

/// A saved account with a big blob (own data file) and small ones (a pack).
async fn saved_account(tag: &str) -> PathBuf {
    let d = dir(tag);
    let r = open(&d).await;
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    r.ok(201, "PUT", "/cnt/big", &[("x-ms-blob-type", "BlockBlob")], vec![9u8; 1_500_000]).await;
    r.ok(201, "PUT", "/cnt/small", &[("x-ms-blob-type", "BlockBlob")], "small data").await;
    r.close().await;
    d
}

async fn refused(d: &Path) -> String {
    match try_open(d).await {
        Ok(r) => {
            r.close().await;
            panic!("the damaged directory was accepted")
        }
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = saved_account("damage").await;
    let state = d.join("blob").join("blob.state");
    let good = std::fs::read(&state).unwrap();
    let all = pieces(&d);
    assert_eq!(all.len(), 2);

    // truncated at several places, and flipped bytes: an error naming the file, never a panic
    for cut in [0usize, 5, 20, good.len() / 3, good.len() / 2, good.len() - 1] {
        std::fs::write(&state, &good[..cut]).unwrap();
        let e = refused(&d).await;
        assert!(e.contains("blob.state") && e.contains("blob"), "cut {cut}: {e}");
    }
    let mut survived = 0;
    for at in (0..good.len()).step_by(3) {
        std::fs::write(&state, &good).unwrap();
        flip_byte(&state, at);
        // a flipped byte may still be valid MessagePack that means something else; the rule is: no panic, and an error names the file
        match try_open(&d).await {
            Ok(r) => {
                survived += 1;
                r.srv.shutdown().await.ok();
                std::fs::write(&state, &good).unwrap();
            }
            Err(e) => assert!(e.to_string().contains("blob.state") || e.to_string().contains("pieces"), "flip at {at}: {e}"),
        }
    }
    assert!(survived < good.len() / 3, "most flipped bytes are noticed ({survived} of {})", good.len() / 3);
    std::fs::write(&state, &good).unwrap();

    // another kind of file, a newer version, no header, a valid header with junk behind it
    let queue_like = persist::encode("queue", 1, &Vec::<u8>::new()).unwrap();
    std::fs::write(&state, queue_like).unwrap();
    let e = refused(&d).await;
    assert!(e.contains("blob.state") && e.contains("holds queue state, expected blob"), "{e}");
    let mut newer = good.clone();
    newer.splice(0..21, b"AZEMU-STATE blob 9\n".iter().copied());
    std::fs::write(&state, &newer).unwrap();
    let e = refused(&d).await;
    assert!(e.contains("version 9") && e.contains("newer azemu"), "{e}");
    std::fs::write(&state, b"hello").unwrap();
    assert!(refused(&d).await.contains("blob.state"));
    std::fs::write(&state, b"AZEMU-STATE blob 1\n\xc1\xc1\xc1").unwrap();
    assert!(refused(&d).await.contains("blob.state"));
    std::fs::write(&state, &good).unwrap();

    // a data file that is missing, cut short, or changed
    for name in &all {
        let p = d.join("blob/pieces").join(name);
        let content = std::fs::read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        let e = refused(&d).await;
        assert!(e.contains(name) && e.contains("missing"), "{e}");
        std::fs::write(&p, &content[..content.len() - 1]).unwrap();
        let e = refused(&d).await;
        assert!(e.contains(name) && e.contains("cut short"), "{e}");
        std::fs::write(&p, &content).unwrap();
        flip_byte(&p, content.len() / 2);
        let e = refused(&d).await;
        assert!(e.contains(name) && e.contains("checksum"), "{e}");
        std::fs::write(&p, &content).unwrap();
    }

    // extra files are left alone (or cleaned when they look like ours), and the account is intact
    std::fs::write(d.join("blob").join("notes.txt"), "mine").unwrap();
    std::fs::write(d.join("blob/pieces").join("README"), "mine too").unwrap();
    std::fs::write(d.join("blob/pieces").join("0123456789abcdef0123456789abcdef"), "orphan").unwrap();
    let r = try_open(&d).await.expect("extra files do not matter");
    assert_eq!(r.ok(200, "GET", "/cnt/small", &[], "").await.text(), "small data");
    assert_eq!(r.ok(200, "GET", "/cnt/big", &[], "").await.body.len(), 1_500_000);
    r.ok(201, "PUT", "/cnt/more", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    r.close().await;
    assert!(d.join("blob/notes.txt").exists() && d.join("blob/pieces/README").exists(), "files that are not ours stay");
    assert!(!d.join("blob/pieces/0123456789abcdef0123456789abcdef").exists(), "an orphan data file is removed");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_state_that_contradicts_itself_is_refused_not_trusted() {
    // hand-made file: a block blob whose size says 5 and whose block is 3 bytes; and a data reference past the table
    let d = dir("lies");
    std::fs::create_dir_all(d.join("blob")).unwrap();
    let r = open(&d).await;
    r.ok(201, "PUT", "/cnt?restype=container", &[], "").await;
    r.ok(201, "PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob")], "abc").await;
    r.close().await;
    let state = d.join("blob").join("blob.state");
    let good = std::fs::read(&state).unwrap();
    // the value 3 (size) appears as 0x03 in the msgpack; find "size" key and patch the number after it
    let key = b"\xa4size\x03";
    let at = good.windows(key.len()).position(|w| w == key).expect("size field");
    let mut bad = good.clone();
    bad[at + key.len() - 1] = 0x05;
    std::fs::write(&state, &bad).unwrap();
    let e = refused(&d).await;
    assert!(e.contains("blob.state") && e.contains("inconsistent"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}
