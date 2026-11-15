//! Blob Storage through `azure_storage_blob` 1.0.0 with `azure_core` 1.0.0 and `azure_identity` 1.0.0: a credential over https, and an
//! account SAS over plain http with no credential (the way a project that cannot trust the certificate authority, or that
//! has only the development key, reaches the emulator).

mod common;

use azure_core::http::{RequestContent, StatusCode, Url};
use azure_identity::ManagedIdentityCredential;
use azure_storage_blob::models::BlobClientDownloadResult;
use azure_storage_blob::{BlobContainerClient, BlobServiceClient};
use common::{dev_account_sas, emu, static_cred, uniq, DEV_ACCOUNT};
use futures::TryStreamExt;

async fn body(r: BlobClientDownloadResult) -> Vec<u8> {
    r.body.collect().await.unwrap().to_vec()
}

async fn names(c: &BlobContainerClient) -> Vec<String> {
    let mut out = vec![];
    let mut pages = c.list_blobs(None).unwrap();
    while let Some(b) = pages.try_next().await.unwrap() {
        out.push(b.name.unwrap());
    }
    out
}

/// Create, upload, download, list and delete: what a project's own tests do. The same steps for both ways of signing in.
async fn round_trip(svc: &BlobServiceClient) {
    let c = svc.blob_container_client(&uniq("box"));
    c.create(None).await.unwrap();
    let a = c.blob_client("dir/a.txt");
    a.upload(RequestContent::from(b"alpha".to_vec()), None).await.unwrap();
    c.blob_client("b.txt").upload(RequestContent::from(b"beta".to_vec()), None).await.unwrap();
    assert_eq!(body(a.download(None).await.unwrap()).await, b"alpha");
    assert_eq!(names(&c).await, vec!["b.txt", "dir/a.txt"]);
    a.delete(None).await.unwrap();
    assert!(!a.exists().await.unwrap());
    assert_eq!(a.download(None).await.unwrap_err().http_status(), Some(StatusCode::NotFound));
    assert_eq!(names(&c).await, vec!["b.txt"]);
    c.delete(None).await.unwrap();
    assert!(!c.exists().await.unwrap());
}

#[tokio::test]
async fn managed_identity_credential_over_https() {
    let url = Url::parse(&emu().https("blob", &uniq("acct"))).unwrap();
    // The credential your service uses on Azure: it reads IDENTITY_ENDPOINT and IDENTITY_HEADER, which `emu()` sets.
    let svc = BlobServiceClient::new(url, Some(ManagedIdentityCredential::new(None).unwrap()), None).unwrap();
    round_trip(&svc).await;
}

#[tokio::test]
async fn a_fixed_token_over_https() {
    let url = Url::parse(&emu().https("blob", &uniq("acct"))).unwrap();
    round_trip(&BlobServiceClient::new(url, Some(static_cred()), None).unwrap()).await;
}

#[tokio::test]
async fn account_sas_over_plain_http_with_no_credential() {
    let url = Url::parse(&format!("{}?{}", emu().http("blob", DEV_ACCOUNT), dev_account_sas())).unwrap();
    round_trip(&BlobServiceClient::new(url, None, None).unwrap()).await;
}

#[tokio::test]
async fn a_credential_over_plain_http_is_refused_by_the_client() {
    let url = Url::parse(&emu().http("blob", &uniq("acct"))).unwrap();
    let e = BlobServiceClient::new(url, Some(static_cred()), None).err().expect("must refuse");
    assert!(e.to_string().contains("must use https"), "{e}");
}

#[tokio::test]
async fn a_wrong_sas_signature_is_403() {
    // The signature has the shape of a real one (base64 of 32 bytes) but is not the right one for the development key.
    let sas = dev_account_sas();
    let (head, _) = sas.rsplit_once("&sig=").unwrap();
    let wrong = format!("{head}&sig=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D");
    let url = Url::parse(&format!("{}?{wrong}", emu().http("blob", DEV_ACCOUNT))).unwrap();
    let svc = BlobServiceClient::new(url, None, None).unwrap();
    let e = svc.blob_container_client(&uniq("box")).create(None).await.unwrap_err();
    assert_eq!(e.http_status(), Some(StatusCode::Forbidden));
}

#[tokio::test]
async fn no_credential_and_no_sas_is_refused() {
    let url = Url::parse(&emu().http("blob", &uniq("acct"))).unwrap();
    let svc = BlobServiceClient::new(url, None, None).unwrap();
    let e = svc.blob_container_client(&uniq("box")).create(None).await.unwrap_err();
    assert_eq!(e.http_status(), Some(StatusCode::Unauthorized));
}
