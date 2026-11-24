//! Strict authorisation on the Blob endpoint (`docs/strict-auth.md`): the answers are the ones Azure gave in November 2026 (`findings`, sections
//! 1.1 to 1.5), with the roles of the four principals the SDK suite uses.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::auth::{Assignment, Auth, Origin, TestAuthority};
use azemu_core::testing::{TestResponse, TestServer};

const VERSION: &str = "2024-11-04";
const BLOB_OWNER: &str = "b7e6dc6d-f1e8-4753-8033-0f276bb0955b";
const BLOB_CONTRIBUTOR: &str = "ba92f5b4-2d11-453d-a403-e96b0029c9fe";
const BLOB_READER: &str = "2a2b9908-6ea1-4ae2-8e65-a410df84e7d1";
const READER_ROLE: &str = "acdd72a7-3385-48ef-bd42-f606fba81ae7";
const ACCT_ID: &str = "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.Storage/storageAccounts/acct";

fn give(auth: &Auth, principal: &str, role: &str, scope: &str) {
    auth.registry()
        .put_assignment(Assignment {
            id: uuid_for(principal, role, scope),
            scope: scope.into(),
            role_id: role.into(),
            principal_id: principal.into(),
            principal_type: "ServicePrincipal".into(),
            condition: None,
            condition_version: None,
            description: None,
            created_on: String::new(),
            updated_on: String::new(),
            origin: Origin::Api,
        })
        .unwrap();
}

fn uuid_for(p: &str, r: &str, s: &str) -> String {
    azemu_core::auth::config::derived_guid(&[p, r, s])
}

struct Env {
    srv: TestServer,
    auth: Auth,
}

async fn start() -> Env {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.Storage/storageAccounts", "acct", ACCT_ID);
    let blob = BlobService::new();
    blob.set_auth(auth.clone());
    let srv = TestServer::start(vec![Arc::new(blob)]).await;
    // main owns the blobs; the reader reads them and may read the account (Reader), the scoped one reads one container
    give(&auth, "main", BLOB_OWNER, ACCT_ID);
    give(&auth, "reader", BLOB_READER, ACCT_ID);
    give(&auth, "reader", READER_ROLE, ACCT_ID);
    give(&auth, "scoped", BLOB_READER, &format!("{ACCT_ID}/blobServices/default/containers/con1"));
    // a writer with a role on `con2` only: what it may copy from `con1` is the question of the copy tests
    give(&auth, "writer", BLOB_CONTRIBUTOR, &format!("{ACCT_ID}/blobServices/default/containers/con2"));
    Env { srv, auth }
}

impl Env {
    async fn call(&self, who: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let token = TestAuthority::token(who, "https://storage.azure.com");
        self.call_with(&token, VERSION, method, path, extra, body).await
    }

    async fn call_with(&self, token: &str, version: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let auth = format!("Bearer {token}");
        let mut h: Vec<(&str, &str)> = vec![("authorization", &auth), ("x-ms-version", version)];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.srv.url("blob", "acct")), &h, body.to_string()).await
    }

    /// The same, to another account of the service.
    async fn call_on(&self, account: &str, who: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> TestResponse {
        let auth = format!("Bearer {}", TestAuthority::token(who, "https://storage.azure.com"));
        let mut h: Vec<(&str, &str)> = vec![("authorization", &auth), ("x-ms-version", VERSION)];
        h.extend_from_slice(extra);
        self.srv.send(method, &format!("{}{path}", self.srv.url("blob", account)), &h, body.to_string()).await
    }

    async fn setup(&self) {
        for c in ["con1", "con2"] {
            let r = self.call("main", "PUT", &format!("/{c}?restype=container"), &[], "").await;
            assert_eq!(r.status, 201, "{}", r.text());
            let r = self.call("main", "PUT", &format!("/{c}/b"), &[("x-ms-blob-type", "BlockBlob")], "data").await;
            assert_eq!(r.status, 201, "{}", r.text());
        }
    }
}

fn code(r: &TestResponse) -> String {
    r.header("x-ms-error-code").unwrap_or("").to_string()
}

const MISMATCH: &str = "This request is not authorized to perform this operation using this permission.";

#[tokio::test]
async fn a_principal_with_no_role_is_refused_everywhere_with_the_same_answer() {
    let e = start().await;
    e.setup().await;
    // get, head, a missing blob and a missing container: the same 403, so nothing leaks
    for (m, p) in [
        ("GET", "/con1/b"),
        ("HEAD", "/con1/b"),
        ("GET", "/con1/missing"),
        ("GET", "/nope/b"),
        ("GET", "/con1?restype=container&comp=list"),
        ("GET", "/?comp=list"),
        ("GET", "/?restype=service&comp=properties"),
    ] {
        let r = e.call("norole", m, p, &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{m} {p}");
        assert_eq!(r.header("x-ms-version"), Some(VERSION));
        if m == "HEAD" {
            assert!(r.body.is_empty());
        } else {
            let t = r.text();
            assert!(t.contains("<Code>AuthorizationPermissionMismatch</Code>"), "{t}");
            assert!(t.contains(&format!("<Message>{MISMATCH}\nRequestId:")), "{t}");
        }
    }
    // writes too
    let r = e.call("norole", "PUT", "/con1/new", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!(code(&r), "AuthorizationPermissionMismatch");
    let r = e.call("norole", "DELETE", "/con1/b", &[], "").await;
    assert_eq!(code(&r), "AuthorizationPermissionMismatch");
    // create container has its own code and a shorter text
    let r = e.call("norole", "PUT", "/con3?restype=container", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
    assert!(r.text().contains("<Message>This request is not authorized to perform this operation.\nRequestId:"));
    // nothing was changed
    assert_eq!(e.call("main", "GET", "/con3?restype=container", &[], "").await.status, 404);
    assert_eq!(e.call("main", "GET", "/con1/b", &[], "").await.text(), "data");
}

#[tokio::test]
async fn a_data_reader_reads_and_nothing_more() {
    let e = start().await;
    e.setup().await;
    let ok = [
        ("GET", "/con1/b"),
        ("HEAD", "/con1/b"),
        ("GET", "/con1/b?comp=metadata"),
        ("GET", "/con1?restype=container&comp=list"),
        ("GET", "/con1?restype=container&comp=list&include=metadata,versions,deleted"),
        ("GET", "/?comp=list"),
        ("HEAD", "/con1?restype=container"),
        ("GET", "/?restype=service&comp=properties"),
    ];
    for (m, p) in ok {
        let r = e.call("reader", m, p, &[], "").await;
        assert!(r.status < 300, "{m} {p}: {} {}", r.status, r.text());
    }
    assert_eq!(e.call("reader", "GET", "/con1/b", &[("range", "bytes=0-1")], "").await.status, 206);
    // a missing blob is the ordinary 404 for a principal that may read
    let r = e.call("reader", "GET", "/con1/missing", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "BlobNotFound"));
    // user delegation key: a Data Reader may
    let key = r#"<?xml version="1.0" encoding="utf-8"?><KeyInfo><Start>2026-01-01T00:00:00Z</Start><Expiry>2026-01-02T00:00:00Z</Expiry></KeyInfo>"#;
    let r = e.call("reader", "POST", "/?restype=service&comp=userdelegationkey", &[], key).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("<SignedOid>reader</SignedOid>"), "{}", r.text());
    // what a reader may not do
    for (m, p, body) in [
        ("GET", "/con1?restype=container&comp=list&include=tags", ""),
        ("GET", "/con1/b?comp=tags", ""),
        ("PUT", "/con1/b?comp=tags", "<Tags/>"),
        ("GET", "/?comp=blobs&where=x", ""),
        ("PUT", "/con1/b2", "x"),
        ("DELETE", "/con1/b", ""),
        ("PUT", "/con1/b?comp=metadata", ""),
        ("PUT", "/con1/b?comp=lease", ""),
        ("GET", "/con1?restype=container&comp=acl", ""),
        ("PUT", "/con1?restype=container&comp=acl", ""),
        ("PUT", "/con1?restype=container&comp=metadata", ""),
        ("DELETE", "/con1?restype=container", ""),
        ("GET", "/?restype=account&comp=properties", ""),
    ] {
        let extra: &[(&str, &str)] = if m == "PUT" && p == "/con1/b2" { &[("x-ms-blob-type", "BlockBlob")] } else { &[] };
        let r = e.call("reader", m, p, extra, body).await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{m} {p}: {}", r.text());
    }
    let r = e.call("reader", "PUT", "/con9?restype=container", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
}

#[tokio::test]
async fn a_role_on_one_container_reaches_only_that_container() {
    let e = start().await;
    e.setup().await;
    for p in ["/con1/b", "/con1?restype=container&comp=list", "/con1?restype=container"] {
        let r = e.call("scoped", "GET", p, &[], "").await;
        assert_eq!(r.status, 200, "{p}: {}", r.text());
    }
    assert_eq!(e.call("scoped", "GET", "/con1/missing", &[], "").await.status, 404);
    // outside: 403, for what exists and what does not
    for p in ["/con2/b", "/con2/missing", "/con2?restype=container&comp=list", "/nope/b", "/?comp=list"] {
        let r = e.call("scoped", "GET", p, &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{p}");
    }
    // user delegation key needs the account
    let r = e.call("scoped", "POST", "/?restype=service&comp=userdelegationkey", &[], "<KeyInfo/>").await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn a_batch_is_accepted_and_each_part_is_judged() {
    let e = start().await;
    e.setup().await;
    let body = "--b\r\nContent-Type: application/http\r\nContent-ID: 0\r\n\r\nDELETE /con1/b HTTP/1.1\r\n\r\n\r\n--b\r\nContent-Type: application/http\r\nContent-ID: 1\r\n\r\nDELETE /con2/b HTTP/1.1\r\n\r\n\r\n--b--\r\n";
    let ct = [("content-type", "multipart/mixed; boundary=b")];
    for who in ["norole", "reader"] {
        let r = e.call(who, "POST", "/?comp=batch", &ct, body).await;
        assert_eq!(r.status, 202, "{who}: {}", r.text());
        let t = r.text();
        assert_eq!(t.matches(&format!("HTTP/1.1 403 {MISMATCH}")).count(), 2, "{t}");
        assert!(t.contains("x-ms-error-code: AuthorizationPermissionMismatch"));
    }
    assert_eq!(e.call("main", "GET", "/con1/b", &[], "").await.status, 200);
    // the owner's batch deletes
    let r = e.call("main", "POST", "/?comp=batch", &ct, body).await;
    assert_eq!(r.status, 202);
    assert_eq!(r.text().matches("HTTP/1.1 202").count(), 2, "{}", r.text());
    assert_eq!(e.call("main", "GET", "/con1/b", &[], "").await.status, 404);
}

#[tokio::test]
async fn a_service_properties_body_that_is_not_xml_is_400_before_the_role_is_asked() {
    let e = start().await;
    let r = e.call("norole", "PUT", "/?restype=service&comp=properties", &[], "not xml").await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = e.call("norole", "PUT", "/?restype=service&comp=properties", &[], "<StorageServiceProperties/>").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
}

#[tokio::test]
async fn a_bad_token_is_told_by_the_version_of_the_request() {
    let e = start().await;
    e.setup().await;
    let wrong = TestAuthority::token("main", "https://vault.azure.net");
    let r = e.call_with(&wrong, VERSION, "GET", "/con1/b", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "InvalidAuthenticationInfo"));
    assert!(r.header("www-authenticate").unwrap().starts_with("Bearer authorization_uri=https://login.microsoftonline.com/"));
    assert!(r.header("www-authenticate").unwrap().ends_with("resource_id=https://storage.azure.com"));
    assert!(
        r.text().contains("<AuthenticationErrorDetail>Audience validation failed. Audience did not match.</AuthenticationErrorDetail>"),
        "{}",
        r.text()
    );
    let r = e.call_with(&wrong, "2019-02-02", "GET", "/con1/b", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    assert!(r.header("www-authenticate").is_none());
    // a valid token with a version from before OAuth: no error code header at all
    let ok = TestAuthority::token("main", "https://storage.azure.com");
    let r = e.call_with(&ok, "2017-04-17", "GET", "/con1/b", &[], "").await;
    assert_eq!(r.status, 403);
    assert!(r.header("x-ms-error-code").is_none());
    assert!(r.text().contains("Authentication scheme Bearer is not supported in this version."));
    // the signature, garbage, expiry
    let bad_sig = format!("{ok}x");
    let r = e.call_with(&bad_sig, VERSION, "GET", "/con1/b", &[], "").await;
    assert!(r.text().contains("Signature validation failed. Signature verification failed."), "{}", r.text());
    let r = e.call_with("garbage", VERSION, "GET", "/con1/b", &[], "").await;
    assert!(r.text().contains("The access token was missing or malformed."));
    let now = azemu_core::time::OffsetDateTime::now_utc().unix_timestamp();
    let old = TestAuthority::token_with("main", "https://storage.azure.com", now - 7200, now - 3600);
    let r = e.call_with(&old, VERSION, "GET", "/con1/b", &[], "").await;
    assert_eq!(r.status, 401);
    assert!(r.text().contains("Lifetime validation failed"), "{}", r.text());
    // the account's own URL is an audience too
    let own = TestAuthority::token("main", "https://acct.blob.core.windows.net");
    assert_eq!(e.call_with(&own, VERSION, "GET", "/con1/b", &[], "").await.status, 200);
}

#[tokio::test]
async fn a_key_and_a_request_without_credentials_are_not_touched_and_the_mode_off_checks_nothing() {
    let e = start().await;
    e.setup().await;
    // no credentials: today's answer
    let r = e.srv.send("GET", &format!("{}/con1/b", e.srv.url("blob", "acct")), &[("x-ms-version", VERSION)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"));
    // the mode off: any token, as before
    let blob = BlobService::new();
    blob.set_auth(Auth::disabled());
    let srv = TestServer::start(vec![Arc::new(blob)]).await;
    let url = srv.url("blob", "acct");
    let h = [("authorization", "Bearer anything"), ("x-ms-version", VERSION)];
    assert_eq!(srv.send("PUT", &format!("{url}/cont?restype=container"), &h, "").await.status, 201);
    assert_eq!(srv.send("GET", &format!("{url}/cont?restype=container"), &h, "").await.status, 200);
}

#[tokio::test]
async fn a_role_given_later_and_taken_away_again_applies_at_once() {
    let e = start().await;
    e.setup().await;
    assert_eq!(e.call("late", "GET", "/con1/b", &[], "").await.status, 403);
    give(&e.auth, "late", BLOB_READER, ACCT_ID);
    assert_eq!(e.call("late", "GET", "/con1/b", &[], "").await.status, 200);
    e.auth.registry().delete_assignment_by_name(&uuid_for("late", BLOB_READER, ACCT_ID));
    assert_eq!(e.call("late", "GET", "/con1/b", &[], "").await.status, 403);
}

#[tokio::test]
async fn data_lake_answers_in_its_own_json() {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.Storage/storageAccounts", "acct", ACCT_ID);
    let blob = BlobService::new();
    blob.set_auth(auth.clone());
    let dfs = blob.datalake();
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    give(&auth, "main", BLOB_OWNER, ACCT_ID);
    give(&auth, "reader", BLOB_READER, ACCT_ID);
    give(&auth, "reader", READER_ROLE, ACCT_ID);
    let call = |who: &'static str, method: &'static str, path: &'static str| {
        let srv = &srv;
        async move {
            let t = format!("Bearer {}", TestAuthority::token(who, "https://storage.azure.com"));
            srv.send(method, &format!("{}{path}", srv.url("dfs", "acct")), &[("authorization", &t), ("x-ms-version", VERSION)], "").await
        }
    };
    assert_eq!(call("main", "PUT", "/fsys?resource=filesystem").await.status, 201);
    assert_eq!(call("main", "PUT", "/fsys/dir/file?resource=file").await.status, 201);
    // the reader lists and reads
    assert_eq!(call("reader", "GET", "/fsys?resource=filesystem&recursive=true").await.status, 200);
    assert_eq!(call("reader", "HEAD", "/fsys/dir/file").await.status, 200);
    // and may not write: JSON, with the code in the header
    let r = call("reader", "PUT", "/fsys/dir/other?resource=file").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    assert_eq!(r.header("content-type"), Some("application/json;charset=utf-8"));
    let v = r.json();
    assert_eq!(v["error"]["code"], "AuthorizationPermissionMismatch");
    assert!(v["error"]["message"].as_str().unwrap().starts_with(&format!("{MISMATCH}\nRequestId:")));
    // norole: also reading
    assert_eq!(call("norole", "GET", "/fsys?resource=filesystem").await.status, 403);
    assert_eq!(call("norole", "PUT", "/other?resource=filesystem").await.status, 403);
    // a wrong audience: JSON with the detail
    let t = format!("Bearer {}", TestAuthority::token("main", "https://vault.azure.net"));
    let r = srv
        .send(
            "GET",
            &format!("{}/fsys?resource=filesystem", srv.url("dfs", "acct")),
            &[("authorization", &t), ("x-ms-version", VERSION)],
            "",
        )
        .await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["detail"]["AuthenticationErrorDetail"], "Audience validation failed. Audience did not match.");
}

// ---- a source is read with the role of the caller: copy and the "from URL" operations ----
//
// Microsoft's table of permissions for Copy Blob (and Put Blob From URL, the block and page forms) names `blobs/read` on the source next to the
// write on the destination. What Azure answers for a caller with no right to the source was not probed: the emulator answers with the
// documented `CannotVerifyCopySource` (403 for a missing permission, 401 for a source token that does not verify).

fn source_url(e: &Env, host_label: &str, path: &str) -> String {
    format!("http://{host_label}.blob.core.localhost:{}{path}", e.srv.port())
}

#[tokio::test]
async fn a_caller_without_a_role_on_the_source_cannot_copy_it_however_the_source_is_written() {
    let e = start().await;
    e.setup().await;
    let port = e.srv.port();
    // the same blob `con1/b`, named in the ways the copy itself reads as the same account: the host form, the secondary host, an escaped name,
    // an upper case name, the path style with the account in the path
    let sources = [
        source_url(&e, "acct", "/con1/b"),
        source_url(&e, "acct-secondary", "/con1/b"),
        source_url(&e, "ACCT", "/con1/b"),
        format!("http://127.0.0.1:{port}/acct/con1/b"),
        format!("http://127.0.0.1:{port}/%61cct/con1/b"),
        format!("http://127.0.0.1:{port}/ACCT/con1/%62"),
    ];
    for (i, src) in sources.iter().enumerate() {
        let src = src.as_str();
        // every operation that takes a source
        for (what, path, extra) in [
            ("Copy Blob", format!("/con2/copy{i}"), vec![("x-ms-copy-source", src)]),
            (
                "Put Blob From URL",
                format!("/con2/url{i}"),
                vec![("x-ms-copy-source", src), ("x-ms-blob-type", "BlockBlob"), ("x-ms-requires-sync", "true")],
            ),
        ] {
            let r = e.call("writer", "PUT", &path, &extra, "").await;
            assert_eq!((r.status, code(&r).as_str()), (403, "CannotVerifyCopySource"), "{what} from {src}: {}", r.text());
            assert_eq!(e.call("main", "HEAD", &path, &[], "").await.status, 404, "{what} from {src} wrote the blob");
        }
        let r = e
            .call(
                "writer",
                "PUT",
                &format!("/con2/blk{i}?comp=block&blockid=AAAA"),
                &[("x-ms-copy-source", src), ("x-ms-source-range", "bytes=0-3")],
                "",
            )
            .await;
        assert_eq!((r.status, code(&r).as_str()), (403, "CannotVerifyCopySource"), "Put Block From URL from {src}: {}", r.text());
        let r = e
            .call(
                "writer",
                "PUT",
                &format!("/con2/app{i}?comp=appendblock"),
                &[("x-ms-copy-source", src), ("x-ms-source-range", "bytes=0-3")],
                "",
            )
            .await;
        assert_eq!((r.status, code(&r).as_str()), (403, "CannotVerifyCopySource"), "Append Block From URL from {src}: {}", r.text());
        let r = e
            .call(
                "writer",
                "PUT",
                &format!("/con2/pg{i}?comp=page"),
                &[
                    ("x-ms-copy-source", src),
                    ("x-ms-source-range", "bytes=0-511"),
                    ("x-ms-range", "bytes=0-511"),
                    ("x-ms-page-write", "update"),
                ],
                "",
            )
            .await;
        assert_eq!((r.status, code(&r).as_str()), (403, "CannotVerifyCopySource"), "Put Page From URL from {src}: {}", r.text());
    }
    // the same call from a caller that may read the source goes through
    let r = e.call("main", "PUT", "/con2/ok", &[("x-ms-copy-source", &sources[0])], "").await;
    assert_eq!(r.status, 202, "{}", r.text());
    assert_eq!(e.call("main", "GET", "/con2/ok", &[], "").await.text(), "data");
    // and a writer who is also given the source may copy it
    give(&e.auth, "writer", BLOB_READER, &format!("{ACCT_ID}/blobServices/default/containers/con1"));
    let r = e.call("writer", "PUT", "/con2/allowed", &[("x-ms-copy-source", &sources[1])], "").await;
    assert_eq!(r.status, 202, "{}", r.text());
}

#[tokio::test]
async fn a_source_token_is_checked_and_judged_for_the_source_and_nothing_else_stands_in() {
    let e = start().await;
    e.setup().await;
    // a second account whose container is private: only a source credential reads it
    e.auth.registry().register_resource(
        "Microsoft.Storage/storageAccounts",
        "other",
        "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.Storage/storageAccounts/other",
    );
    give(&e.auth, "main", BLOB_OWNER, "/subscriptions/s1/resourceGroups/g1/providers/Microsoft.Storage/storageAccounts/other");
    let token = |who: &str, aud: &str| format!("Bearer {}", TestAuthority::token(who, aud));
    let other = |method: &'static str,
                 path: &'static str,
                 who: &'static str,
                 extra: &'static [(&'static str, &'static str)],
                 body: &'static str| { e.call_on("other", who, method, path, extra, body) };
    assert_eq!(other("PUT", "/priv?restype=container", "main", &[], "").await.status, 201);
    assert_eq!(other("PUT", "/priv/s", "main", &[("x-ms-blob-type", "BlockBlob")], "secret").await.status, 201);
    let src = format!("http://other.blob.core.localhost:{}/priv/s", e.srv.port());
    // no source credential and no public access: the destination's own token does not read another account
    let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src)], "").await;
    assert!(r.status == 401 || r.status == 403 || r.status == 409, "{} {}", r.status, r.text());
    assert_ne!(r.status, 202);
    // a source token that is not a token: refused, however the destination's token is
    for junk in ["Bearer junk", "Bearer ", "Basic abc", "junk"] {
        let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src), ("x-ms-copy-source-authorization", junk)], "").await;
        assert!(matches!(r.status, 400 | 401), "{junk}: {} {}", r.status, r.text());
        assert!(!r.text().contains("junk"), "the answer must not repeat the credential: {}", r.text());
        assert_eq!(e.call("main", "HEAD", "/con2/x", &[], "").await.status, 404, "{junk} copied the blob");
    }
    let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src), ("x-ms-copy-source-authorization", "Bearer junk")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "CannotVerifyCopySource"), "{}", r.text());
    // a token for another resource
    let wrong = token("main", "https://vault.azure.net");
    let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src), ("x-ms-copy-source-authorization", &wrong)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "CannotVerifyCopySource"), "{}", r.text());
    // a good token of a principal with no role on the source
    let norole = token("norole", "https://storage.azure.com");
    let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src), ("x-ms-copy-source-authorization", &norole)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "CannotVerifyCopySource"), "{}", r.text());
    // a good token of a principal that reads the source
    let main_token = token("main", "https://storage.azure.com");
    let r = e.call("main", "PUT", "/con2/x", &[("x-ms-copy-source", &src), ("x-ms-copy-source-authorization", &main_token)], "").await;
    assert_eq!(r.status, 202, "{}", r.text());
    assert_eq!(e.call("main", "GET", "/con2/x", &[], "").await.text(), "secret");
}

#[tokio::test]
async fn data_lake_rename_asks_for_the_source_too() {
    let auth = Auth::strict();
    auth.set_authority(Arc::new(TestAuthority));
    auth.registry().register_resource("Microsoft.Storage/storageAccounts", "acct", ACCT_ID);
    let blob = BlobService::new();
    blob.set_auth(auth.clone());
    let dfs = blob.datalake();
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    give(&auth, "main", BLOB_OWNER, ACCT_ID);
    give(&auth, "mover", BLOB_OWNER, &format!("{ACCT_ID}/blobServices/default/containers/out"));
    let call = |who: &'static str, method: &'static str, path: &'static str, extra: Vec<(&'static str, String)>| {
        let srv = &srv;
        async move {
            let t = format!("Bearer {}", TestAuthority::token(who, "https://storage.azure.com"));
            let mut h = vec![("authorization", t), ("x-ms-version", VERSION.to_string())];
            h.extend(extra);
            let hs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            srv.send(method, &format!("{}{path}", srv.url("dfs", "acct")), &hs, "").await
        }
    };
    for fs in ["secret", "out"] {
        assert_eq!(call("main", "PUT", Box::leak(format!("/{fs}?resource=filesystem").into_boxed_str()), vec![]).await.status, 201);
    }
    assert_eq!(call("main", "PUT", "/secret/m.txt?resource=file", vec![]).await.status, 201);
    // the mover owns `out` only: moving a file out of `secret` is refused, and the file stays
    for source in ["/secret/m.txt", "secret/m.txt", "/%73ecret/m.txt", "http://acct.dfs.core.localhost/secret/m.txt?sv=x"] {
        let r = call("mover", "PUT", "/out/moved", vec![("x-ms-rename-source", source.to_string())]).await;
        assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"), "{source}: {}", r.text());
        assert_eq!(call("main", "HEAD", "/secret/m.txt", vec![]).await.status, 200, "{source} moved the file");
        assert_eq!(call("main", "HEAD", "/out/moved", vec![]).await.status, 404, "{source} made the file");
    }
    // inside the one filesystem it may rename
    assert_eq!(call("main", "PUT", "/out/a.txt?resource=file", vec![]).await.status, 201);
    let r = call("mover", "PUT", "/out/b.txt", vec![("x-ms-rename-source", "/out/a.txt".to_string())]).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // and the owner of both may move across
    let r = call("main", "PUT", "/out/moved", vec![("x-ms-rename-source", "/secret/m.txt".to_string())]).await;
    assert_eq!(r.status, 201, "{}", r.text());
}
