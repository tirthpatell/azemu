mod common;
use common::*;

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;
use azemu_core::Service;

fn sas(extra: &str) -> String {
    format!("sv=2021-08-06&sig=ignored&{extra}")
}

#[tokio::test]
async fn anonymous_access_follows_public_access() {
    let e = start().await;
    e.create_container("priv").await;
    e.create_container("pub").await;
    e.send("PUT", "/pub?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], "").await;
    e.create_container("lst").await;
    e.send("PUT", "/lst?restype=container&comp=acl", &[("x-ms-blob-public-access", "container")], "").await;
    for c in ["priv", "pub", "lst"] {
        e.put_blob(c, "b", "data").await;
    }
    // private: 401 NoAuthenticationInformation with a challenge
    let r = e.anon("GET", "/priv/b", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"));
    assert!(r.header("www-authenticate").is_some());
    assert!(!r.text().contains("data"));
    // blob-level: reads of blobs only
    assert_eq!(e.anon("GET", "/pub/b", &[], "").await.text(), "data");
    assert_eq!(e.anon("HEAD", "/pub/b", &[], "").await.status, 200);
    assert_eq!(e.anon("GET", "/pub?restype=container&comp=list", &[], "").await.status, 401);
    // container-level: listing too
    let r = e.anon("GET", "/lst?restype=container&comp=list", &[], "").await;
    assert!(r.text().contains("<Name>b</Name>"));
    assert_eq!(e.anon("GET", "/lst?restype=container", &[], "").await.status, 200);
    // writes are never anonymous
    let r = e.anon("PUT", "/lst/w", &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"));
    assert_eq!(e.send("GET", "/lst/w", &[], "").await.status, 404);
    // the service level needs credentials
    assert_eq!(e.anon("GET", "/?comp=list", &[], "").await.status, 401);
}

#[tokio::test]
async fn sas_permissions_expiry_and_scope() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "data").await;
    // read-only blob SAS
    let q = sas("sr=b&sp=r&se=2099-01-01T00:00:00Z");
    let r = e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "data"));
    let r = e.anon("PUT", &format!("/cnt/b?{q}"), &[("x-ms-blob-type", "BlockBlob")], "x").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let r = e.anon("DELETE", &format!("/cnt/b?{q}"), &[], "").await;
    assert_eq!(code(&r), "AuthorizationPermissionMismatch");
    // create-only cannot overwrite but can create
    let q = sas("sr=b&sp=c&se=2099-01-01T00:00:00Z");
    assert_eq!(e.anon("PUT", &format!("/cnt/fresh?{q}"), &[("x-ms-blob-type", "BlockBlob")], "x").await.status, 201);
    assert_eq!(
        code(&e.anon("PUT", &format!("/cnt/b?{q}"), &[("x-ms-blob-type", "BlockBlob")], "x").await),
        "AuthorizationPermissionMismatch"
    );
    // expired and not yet valid
    let q = sas("sr=b&sp=r&se=2000-01-01T00:00:00Z");
    let r = e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    assert!(r.text().contains("<AuthenticationErrorDetail>"));
    let q = sas("sr=b&sp=r&st=2098-01-01T00:00:00Z&se=2099-01-01T00:00:00Z");
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await), "AuthenticationFailed");
    // https only
    let q = sas("sr=b&sp=r&spr=https&se=2099-01-01T00:00:00Z");
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await), "AuthorizationProtocolMismatch");
    // ip range
    let q = sas("sr=b&sp=r&sip=203.0.113.7&se=2099-01-01T00:00:00Z");
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await), "AuthorizationSourceIPMismatch");
    let q = sas("sr=b&sp=r&sip=127.0.0.0-127.255.255.255&se=2099-01-01T00:00:00Z");
    assert_eq!(e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await.status, 200);
    // account SAS: services, resource types, permissions
    let q = sas("ss=bfqt&srt=o&sp=rl&se=2099-01-01T00:00:00Z");
    assert_eq!(e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await.status, 200);
    assert_eq!(code(&e.anon("GET", &format!("/?comp=list&{q}"), &[], "").await), "AuthorizationResourceTypeMismatch");
    let q = sas("ss=fq&srt=sco&sp=rl&se=2099-01-01T00:00:00Z");
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await), "AuthorizationServiceMismatch");
    let q = sas("ss=b&srt=sco&sp=rl&se=2099-01-01T00:00:00Z");
    assert_eq!(e.anon("GET", &format!("/?comp=list&{q}"), &[], "").await.status, 200);
    // response header overrides
    let q = sas("sr=b&sp=r&se=2099-01-01T00:00:00Z&rsct=text%2Fx-test&rscd=attachment");
    let r = e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await;
    assert_eq!(r.header("content-type"), Some("text/x-test"));
    assert_eq!(r.header("content-disposition"), Some("attachment"));
}

#[tokio::test]
async fn stored_access_policy_sets_permissions_and_expiry() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "data").await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>ro</Id><AccessPolicy><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>rl</Permission></AccessPolicy></SignedIdentifier><SignedIdentifier><Id>old</Id><AccessPolicy><Expiry>2000-01-01T00:00:00Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/cnt?restype=container&comp=acl", &[], acl).await.status, 200);
    let q = sas("sr=c&si=ro");
    assert_eq!(e.anon("GET", &format!("/cnt/b?{q}"), &[], "").await.status, 200);
    assert_eq!(e.anon("GET", &format!("/cnt?restype=container&comp=list&{q}"), &[], "").await.status, 200);
    assert_eq!(
        code(&e.anon("PUT", &format!("/cnt/w?{q}"), &[("x-ms-blob-type", "BlockBlob")], "x").await),
        "AuthorizationPermissionMismatch"
    );
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{}", sas("sr=c&si=old")), &[], "").await), "AuthenticationFailed");
    assert_eq!(code(&e.anon("GET", &format!("/cnt/b?{}", sas("sr=c&si=nope")), &[], "").await), "AuthenticationFailed");
}

#[tokio::test]
async fn error_bodies_and_common_headers() {
    let e = start().await;
    e.create_container("cnt").await;
    let r = e.send("GET", "/cnt/missing", &[("x-ms-client-request-id", "abc-123"), ("x-ms-version", "2021-08-06")], "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().starts_with("\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>BlobNotFound</Code><Message>The specified blob does not exist.\nRequestId:"));
    assert_eq!(r.header("x-ms-client-request-id"), Some("abc-123"));
    assert_eq!(r.header("x-ms-version"), Some("2021-08-06"));
    assert_eq!(r.header("content-type"), Some("application/xml"));
    assert_eq!(r.header("content-length").unwrap().parse::<usize>().unwrap(), r.body.len());
    // an impossible service version
    let r = e.send("GET", "/cnt/missing", &[("x-ms-version", "2099-11-05")], "").await;
    assert_eq!(code(&r), "InvalidHeaderValue");
    assert!(r.text().contains("<HeaderName>x-ms-version</HeaderName>"));
}

// What Azure answers a Blob request with no credential, case by case. The recordings of the Python SDK
// (refs/recordings/python/sdk/storage/azure-storage-blob/tests/recordings) hold Azure's answers to the anonymous request that
// the service itself makes when it reads a copy source with no SAS; the cases below are those, plus the same rules for a
// request a client sends.

/// A private container in an account that allows public access, a blob in it or not: 401 NoAuthenticationInformation.
/// Recordings: `test_common_blob.pyTestStorageCommonBlobtest_copy_blob_async_private_blob_no_sas` (the blob exists) and
/// `test_block_blob.pyTestStorageBlockBlobtest_upload_blob_copy_source_error_and_status_code` (it does not): both 401, and
/// the second one's body carries CopySourceErrorCode NoAuthenticationInformation.
#[tokio::test]
async fn an_anonymous_read_of_a_private_container_is_401_whether_or_not_the_blob_exists() {
    let e = start().await;
    e.create_container("priv").await;
    e.put_blob("priv", "there", "data").await;
    for path in ["/priv/there", "/priv/absent"] {
        let r = e.anon("GET", path, &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"), "{path}");
        assert!(r.header("www-authenticate").is_some_and(|h| h.starts_with("Bearer authorization_uri=")), "{path}");
        assert!(r
            .text()
            .contains("Server failed to authenticate the request. Please refer to the information in the www-authenticate header."));
        let head = e.anon("HEAD", path, &[], "").await;
        assert_eq!(head.status, 401, "{path}");
    }
    let r = e.anon("GET", "/priv?restype=container&comp=list", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"));
}

/// A request that no container setting could ever allow without a credential is 401 whatever the container is; an anonymous
/// request that could be allowed asks the container, and one for a container that does not exist is 404 ResourceNotFound
/// (the answer for that case is not in the recordings; it is what the emulator has always said).
#[tokio::test]
async fn an_anonymous_create_or_upload_is_401_even_when_the_container_does_not_exist() {
    let e = start().await;
    for (method, path, extra) in [
        ("PUT", "/fresh?restype=container", vec![]),
        ("PUT", "/fresh/blob", vec![("x-ms-blob-type", "BlockBlob")]),
        ("DELETE", "/fresh?restype=container", vec![]),
        ("GET", "/?comp=list", vec![]),
    ] {
        let r = e.anon(method, path, &extra, "x").await;
        assert_eq!((r.status, code(&r).as_str()), (401, "NoAuthenticationInformation"), "{method} {path}");
    }
    assert_eq!(e.send("GET", "/fresh?restype=container", &[], "").await.status, 404, "nothing was created");
    let r = e.anon("GET", "/fresh/blob", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "ResourceNotFound"));
}

/// An account that does not allow public access: an anonymous read is 409 PublicAccessNotPermitted, a private container
/// with a missing blob included. Recordings: `test_common_blob.pyTestStorageCommonBlobtest_copy_blob_copy_source_error_and_status_code`
/// (the container exists, the blob does not: 409, "Public access is not permitted on this storage account."), and the same
/// answer in the recordings of `stage_block_from_url`, `upload_blob_from_url` and `upload_pages_from_url`.
#[tokio::test]
async fn an_account_that_does_not_allow_public_access_answers_anonymous_reads_with_409() {
    let svc = Arc::new(BlobService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let e = Env { base: srv.url("blob", "acct"), srv };
    e.create_container("pub").await;
    e.send("PUT", "/pub?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], "").await;
    e.put_blob("pub", "b", "data").await;
    e.create_container("priv").await;
    assert_eq!(e.anon("GET", "/pub/b", &[], "").await.status, 200);

    svc.set_public_access_allowed("acct", false);
    for path in ["/pub/b", "/priv/absent", "/priv?restype=container&comp=list"] {
        let r = e.anon("GET", path, &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (409, "PublicAccessNotPermitted"), "{path}");
        assert!(r.text().contains("Public access is not permitted on this storage account."), "{}", r.text());
    }
    // what needs a credential still says so, and a request with one is not affected
    assert_eq!(code(&e.anon("PUT", "/priv/w", &[("x-ms-blob-type", "BlockBlob")], "x").await), "NoAuthenticationInformation");
    assert_eq!(e.send("GET", "/pub/b", &[], "").await.text(), "data");
    // a container cannot be made public, at creation or afterwards; making it private is fine
    let r = e.send("PUT", "/new?restype=container", &[("x-ms-blob-public-access", "container")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PublicAccessNotPermitted"));
    let r = e.send("PUT", "/priv?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "PublicAccessNotPermitted"));
    assert_eq!(e.send("PUT", "/pub?restype=container&comp=acl", &[], "").await.status, 200);
    // another account is not affected
    let other = srv_for(&e, "other");
    assert_eq!(
        e.srv.send("PUT", &format!("{other}/ctr?restype=container"), &[AUTH, ("x-ms-blob-public-access", "blob")], "").await.status,
        201
    );

    svc.set_public_access_allowed("acct", true);
    assert_eq!(e.send("PUT", "/priv?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], "").await.status, 200);
}

fn srv_for(e: &Env, account: &str) -> String {
    e.base.replace("acct.", &format!("{account}."))
}

/// A copy whose source is in another account that does not allow public access is refused the way the source's own answer
/// says: 409 CannotVerifyCopySource with CopySourceErrorCode PublicAccessNotPermitted (recordings as above), where a source in
/// an account that allows it, in a private container, is 401 (`private_blob_no_sas`).
#[tokio::test]
async fn a_copy_source_in_an_account_without_public_access_is_409() {
    let svc = Arc::new(BlobService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let e = Env { base: srv.url("blob", "acct"), srv };
    let other = srv_for(&e, "other");
    e.create_container("dst").await;
    assert_eq!(e.srv.send("PUT", &format!("{other}/src?restype=container"), &[AUTH], "").await.status, 201);
    let put = e.srv.send("PUT", &format!("{other}/src/b"), &[AUTH, ("x-ms-blob-type", "BlockBlob")], "data").await;
    assert_eq!(put.status, 201);
    let source = format!("{other}/src/b");
    let r = e.send("PUT", "/dst/copy", &[("x-ms-copy-source", &source)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (401, "CannotVerifyCopySource"));
    assert!(r.text().contains("<CopySourceErrorCode>NoAuthenticationInformation</CopySourceErrorCode>"), "{}", r.text());
    svc.set_public_access_allowed("other", false);
    let r = e.send("PUT", "/dst/copy", &[("x-ms-copy-source", &source)], "").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "CannotVerifyCopySource"));
    assert!(r.text().contains("<CopySourceErrorCode>PublicAccessNotPermitted</CopySourceErrorCode>"), "{}", r.text());
    assert!(r.text().contains("<CopySourceStatusCode>409</CopySourceStatusCode>"), "{}", r.text());
}

/// An Authorization header that no scheme can be read from is 400 InvalidAuthenticationInfo with the text of the recordings
/// (`test_common_blob.pyTestStorageCommonBlobtest_access_token_refresh_after_retry` and `..._test_token_credential`):
/// "Authentication information is not given in the correct format. Check the value of Authorization header."
#[tokio::test]
async fn a_malformed_authorization_header_is_400_invalid_authentication_info() {
    let e = start().await;
    e.create_container("cnt").await;
    for value in ["Basic dXNlcjpwYXNz", "garbage", "Bearer", "Bearer   ", "Token abc"] {
        let r = e.anon("GET", "/cnt?restype=container", &[("Authorization", value)], "").await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidAuthenticationInfo"), "{value:?}");
        assert!(
            r.text().contains("Authentication information is not given in the correct format. Check the value of Authorization header."),
            "{value:?}: {}",
            r.text()
        );
    }
    // the readable ones, and an empty header (the same as none)
    assert_eq!(e.anon("GET", "/cnt?restype=container", &[("Authorization", "Bearer eyJ0.abc.def")], "").await.status, 200);
    assert_eq!(e.anon("GET", "/cnt?restype=container", &[("Authorization", "SharedKey acct:c2ln")], "").await.status, 200);
    assert_eq!(e.anon("GET", "/cnt?restype=container", &[("Authorization", "SharedKeyLite acct:c2ln")], "").await.status, 200);
    assert_eq!(e.anon("GET", "/cnt?restype=container", &[("Authorization", " ")], "").await.status, 401);
}
