//! Blob Storage through `azure_storage_blob` (Azure SDK for Rust).
//! Every client talks HTTPS to `https://{account}.blob.core.localhost:{port}/` with a bearer token.

use std::collections::HashMap;

use azemu_sdk_rust_tests::{cred, emu, uniq};
use azure_core::http::{Etag, RequestContent, StatusCode, Url};
use azure_core::Error;
use azure_storage_blob::models::*;
use azure_storage_blob::{BlobContainerClient, BlobServiceClient};
use futures::TryStreamExt;

fn account() -> String {
    uniq("acct")
}

fn service(account: &str) -> BlobServiceClient {
    let url = Url::parse(&emu().https("blob", account)).unwrap();
    BlobServiceClient::new(url, Some(cred()), None).unwrap()
}

/// A fresh account and an empty container in it.
async fn container() -> (BlobServiceClient, BlobContainerClient) {
    let svc = service(&account());
    let c = svc.blob_container_client(&uniq("c"));
    c.create(None).await.unwrap();
    (svc, c)
}

fn status(e: &Error) -> Option<StatusCode> {
    e.http_status()
}

fn error_code(e: &Error) -> Option<String> {
    match e.kind() {
        azure_core::error::ErrorKind::HttpResponse { error_code, .. } => error_code.clone(),
        _ => None,
    }
}

async fn body(r: BlobClientDownloadResult) -> Vec<u8> {
    r.body.collect().await.unwrap().to_vec()
}

fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

// ---- containers ---------------------------------------------------------------------------

#[tokio::test]
async fn create_container_then_it_exists() {
    let svc = service(&account());
    let c = svc.blob_container_client(&uniq("c"));
    assert!(!c.exists().await.unwrap());
    c.create(None).await.unwrap();
    assert!(c.exists().await.unwrap());
}

#[tokio::test]
async fn create_existing_container_is_409_container_already_exists() {
    let (_s, c) = container().await;
    let e = c.create(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
    assert_eq!(error_code(&e).as_deref(), Some("ContainerAlreadyExists"));
}

#[tokio::test]
async fn delete_container_then_properties_is_404_container_not_found() {
    let (_s, c) = container().await;
    c.delete(None).await.unwrap();
    let e = c.get_properties(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(error_code(&e).as_deref(), Some("ContainerNotFound"));
}

#[tokio::test]
async fn delete_missing_container_is_404() {
    let svc = service(&account());
    let e = svc.blob_container_client("nosuch").delete(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(error_code(&e).as_deref(), Some("ContainerNotFound"));
}

#[tokio::test]
async fn container_metadata_roundtrip() {
    let (_s, c) = container().await;
    let md = HashMap::from([("owner".to_string(), "rust".to_string()), ("env".to_string(), "test".to_string())]);
    c.set_metadata(&md, None).await.unwrap();
    let props = c.get_properties(None).await.unwrap();
    assert_eq!(props.metadata().unwrap(), md);
    assert!(props.etag().unwrap().is_some());
    assert!(props.last_modified().unwrap().is_some());
}

#[tokio::test]
async fn list_containers_returns_created_containers_filtered_by_prefix() {
    let svc = service(&account());
    let prefix = uniq("lc");
    let mut names = vec![];
    for i in 0..3 {
        let n = format!("{prefix}-{i}");
        svc.blob_container_client(&n).create(None).await.unwrap();
        names.push(n);
    }
    svc.blob_container_client(&uniq("other")).create(None).await.unwrap();
    let opts = BlobServiceClientListContainersOptions { prefix: Some(prefix.clone()), ..Default::default() };
    let mut got: Vec<String> = vec![];
    let mut pages = svc.list_containers(Some(opts)).unwrap();
    while let Some(c) = pages.try_next().await.unwrap() {
        got.push(c.name.unwrap());
    }
    got.sort();
    assert_eq!(got, names);
}

#[tokio::test]
async fn list_containers_follows_continuation_marker() {
    let svc = service(&account());
    let prefix = uniq("pg");
    for i in 0..5 {
        svc.blob_container_client(&format!("{prefix}-{i}")).create(None).await.unwrap();
    }
    let opts = BlobServiceClientListContainersOptions { prefix: Some(prefix), maxresults: Some(2), ..Default::default() };
    let mut count = 0;
    let mut pages = svc.list_containers(Some(opts)).unwrap().into_pages();
    let mut page_count = 0;
    while let Some(p) = pages.try_next().await.unwrap() {
        page_count += 1;
        count += p.into_model().unwrap().container_items.len();
    }
    assert_eq!(count, 5);
    assert_eq!(page_count, 3);
}

// ---- block blobs ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_then_download_returns_same_bytes() {
    let (_s, c) = container().await;
    let b = c.blob_client("hello.txt");
    b.upload(RequestContent::from(b"hello world".to_vec()), None).await.unwrap();
    let r = b.download(None).await.unwrap();
    assert_eq!(r.properties.content_length, Some(11));
    assert_eq!(body(r).await, b"hello world");
}

#[tokio::test]
async fn upload_of_existing_blob_with_if_not_exists_is_409_blob_already_exists() {
    let (_s, c) = container().await;
    let b = c.blob_client("once");
    b.upload(RequestContent::from(b"1".to_vec()), None).await.unwrap();
    let opts = BlockBlobClientUploadOptions::default().if_not_exists();
    let e = b.upload(RequestContent::from(b"2".to_vec()), Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
    assert_eq!(error_code(&e).as_deref(), Some("BlobAlreadyExists"));
}

#[tokio::test]
async fn download_of_range_returns_only_those_bytes() {
    let (_s, c) = container().await;
    let b = c.blob_client("r");
    let d = data(1000);
    b.upload(RequestContent::from(d.clone()), None).await.unwrap();
    let opts = BlobClientDownloadOptions { range: Some((100u64..200).into()), ..Default::default() };
    let r = b.download(Some(opts)).await.unwrap();
    assert_eq!(body(r).await, d[100..200].to_vec());
}

#[tokio::test]
async fn download_with_open_ended_range_returns_the_tail() {
    let (_s, c) = container().await;
    let b = c.blob_client("r");
    let d = data(1000);
    b.upload(RequestContent::from(d.clone()), None).await.unwrap();
    let opts = BlobClientDownloadOptions { range: Some((900u64..).into()), ..Default::default() };
    let r = b.download(Some(opts)).await.unwrap();
    assert_eq!(body(r).await, d[900..].to_vec());
}

#[tokio::test]
async fn download_of_empty_blob_returns_no_bytes() {
    let (_s, c) = container().await;
    let b = c.blob_client("empty");
    b.upload(RequestContent::from(Vec::<u8>::new()), None).await.unwrap();
    let r = b.download(None).await.unwrap();
    assert_eq!(body(r).await.len(), 0);
}

#[tokio::test]
async fn get_properties_reports_block_blob_type_size_and_content_type() {
    let (_s, c) = container().await;
    let b = c.blob_client("p.json");
    let opts = BlockBlobClientUploadOptions { blob_content_type: Some("application/json".into()), ..Default::default() };
    b.upload(RequestContent::from(b"{}".to_vec()), Some(opts)).await.unwrap();
    let p = b.get_properties(None).await.unwrap();
    assert_eq!(p.blob_type().unwrap(), Some(BlobType::BlockBlob));
    assert_eq!(p.content_length().unwrap(), Some(2));
    assert_eq!(p.content_type().unwrap().as_deref(), Some("application/json"));
    assert!(p.etag().unwrap().is_some());
    assert_eq!(p.access_tier().unwrap().as_deref(), Some("Hot"));
}

#[tokio::test]
async fn upload_with_metadata_returns_it_in_properties_and_download() {
    let (_s, c) = container().await;
    let b = c.blob_client("m");
    let md = HashMap::from([("k1".to_string(), "v1".to_string()), ("k2".to_string(), "v 2".to_string())]);
    let opts = BlockBlobClientUploadOptions { metadata: Some(md.clone()), ..Default::default() };
    b.upload(RequestContent::from(b"x".to_vec()), Some(opts)).await.unwrap();
    assert_eq!(b.get_properties(None).await.unwrap().metadata().unwrap(), md);
    assert_eq!(b.download(None).await.unwrap().properties.metadata, md);
}

#[tokio::test]
async fn set_metadata_replaces_all_metadata() {
    let (_s, c) = container().await;
    let b = c.blob_client("m");
    let opts = BlockBlobClientUploadOptions { metadata: Some(HashMap::from([("a".into(), "1".into())])), ..Default::default() };
    b.upload(RequestContent::from(b"x".to_vec()), Some(opts)).await.unwrap();
    let new = HashMap::from([("b".to_string(), "2".to_string())]);
    b.set_metadata(&new, None).await.unwrap();
    assert_eq!(b.get_properties(None).await.unwrap().metadata().unwrap(), new);
}

#[tokio::test]
async fn set_properties_changes_content_type_and_keeps_content() {
    let (_s, c) = container().await;
    let b = c.blob_client("sp");
    b.upload(RequestContent::from(b"abc".to_vec()), None).await.unwrap();
    let opts = BlobClientSetPropertiesOptions {
        blob_content_type: Some("text/plain".into()),
        blob_cache_control: Some("max-age=60".into()),
        ..Default::default()
    };
    b.set_properties(Some(opts)).await.unwrap();
    let p = b.get_properties(None).await.unwrap();
    assert_eq!(p.content_type().unwrap().as_deref(), Some("text/plain"));
    assert_eq!(p.cache_control().unwrap().as_deref(), Some("max-age=60"));
    assert_eq!(body(b.download(None).await.unwrap()).await, b"abc");
}

#[tokio::test]
async fn blob_tags_roundtrip() {
    let (_s, c) = container().await;
    let b = c.blob_client("t");
    b.upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    let tags = HashMap::from([("project".to_string(), "azemu".to_string()), ("env".to_string(), "test".to_string())]);
    let bt: BlobTags = tags.clone().into();
    b.set_tags(bt.try_into().unwrap(), None).await.unwrap();
    let got: HashMap<String, String> = b.get_tags(None).await.unwrap().into_model().unwrap().into();
    assert_eq!(got, tags);
    assert_eq!(b.get_properties(None).await.unwrap().tag_count().unwrap(), Some(2));
}

#[tokio::test]
async fn upload_with_tags_header_stores_the_tags() {
    let (_s, c) = container().await;
    let b = c.blob_client("t");
    // Tag keys and values may hold letters, digits, space and + - . / : = _ (not &).
    let tags = HashMap::from([("a b".to_string(), "c d+e/f:g=h_i.j-k".to_string())]);
    let opts = BlockBlobClientUploadOptions::default().with_tags(tags.clone());
    b.upload(RequestContent::from(b"x".to_vec()), Some(opts)).await.unwrap();
    let got: HashMap<String, String> = b.get_tags(None).await.unwrap().into_model().unwrap().into();
    assert_eq!(got, tags);
}

#[tokio::test]
async fn delete_blob_then_download_is_404_blob_not_found() {
    let (_s, c) = container().await;
    let b = c.blob_client("gone");
    b.upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    b.delete(None).await.unwrap();
    assert!(!b.exists().await.unwrap());
    let e = b.download(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(error_code(&e).as_deref(), Some("BlobNotFound"));
}

#[tokio::test]
async fn download_from_missing_container_is_404_container_not_found() {
    let svc = service(&account());
    let e = svc.blob_container_client("nosuch").blob_client("b").download(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(error_code(&e).as_deref(), Some("ContainerNotFound"));
}

// ---- block upload and commit ---------------------------------------------------------------

#[tokio::test]
async fn stage_blocks_then_commit_block_list_assembles_the_blob() {
    let (_s, c) = container().await;
    let bb = c.blob_client("staged").block_blob_client();
    let blocks: [(&[u8], &[u8]); 3] = [(b"block-1", b"Hello, "), (b"block-2", b"block "), (b"block-3", b"blobs!")];
    for (id, d) in blocks {
        bb.stage_block(id, d.len() as u64, RequestContent::from(d.to_vec()), None).await.unwrap();
    }
    // Before the commit the blob does not exist but the uncommitted blocks are listed.
    let uncommitted = bb.get_block_list(BlockListType::Uncommitted, None).await.unwrap().into_model().unwrap();
    assert_eq!(uncommitted.uncommitted_blocks.as_deref().unwrap_or(&[]).len(), 3);
    let list = BlockLookupList { committed: None, latest: Some(blocks.iter().map(|(i, _)| i.to_vec()).collect()), uncommitted: None };
    bb.commit_block_list(list.try_into().unwrap(), None).await.unwrap();
    let committed = bb.get_block_list(BlockListType::Committed, None).await.unwrap().into_model().unwrap();
    let cb = committed.committed_blocks.unwrap();
    assert_eq!(cb.len(), 3);
    assert_eq!(cb[0].size, Some(7));
    assert_eq!(body(c.blob_client("staged").download(None).await.unwrap()).await, b"Hello, block blobs!");
}

#[tokio::test]
async fn commit_block_list_with_unknown_block_is_400_invalid_block_list() {
    let (_s, c) = container().await;
    let bb = c.blob_client("bad").block_blob_client();
    let list = BlockLookupList { committed: None, latest: Some(vec![b"nope".to_vec()]), uncommitted: None };
    let e = bb.commit_block_list(list.try_into().unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
    assert_eq!(error_code(&e).as_deref(), Some("InvalidBlockList"));
}

#[tokio::test]
async fn large_upload_of_several_mebibytes_roundtrips_intact() {
    let (_s, c) = container().await;
    let b = c.blob_client("big");
    let d = data(9 * 1024 * 1024 + 123);
    b.upload(RequestContent::from(d.clone()), None).await.unwrap();
    let p = b.get_properties(None).await.unwrap();
    assert_eq!(p.content_length().unwrap(), Some(d.len() as u64));
    let got = body(b.download(None).await.unwrap()).await;
    assert_eq!(got.len(), d.len());
    assert!(got == d, "downloaded bytes differ");
}

// ---- listing -------------------------------------------------------------------------------

async fn list_names(c: &BlobContainerClient, opts: Option<BlobContainerClientListBlobsOptions<'_>>) -> Vec<String> {
    let mut out = vec![];
    let mut s = c.list_blobs(opts).unwrap();
    while let Some(b) = s.try_next().await.unwrap() {
        out.push(b.name.unwrap());
    }
    out
}

#[tokio::test]
async fn list_blobs_returns_names_in_ascending_order() {
    let (_s, c) = container().await;
    for n in ["b", "a", "c/1", "c/0"] {
        c.blob_client(n).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
    assert_eq!(list_names(&c, None).await, vec!["a", "b", "c/0", "c/1"]);
}

#[tokio::test]
async fn list_blobs_with_prefix_returns_only_matching_blobs() {
    let (_s, c) = container().await;
    for n in ["logs/1", "logs/2", "data/1"] {
        c.blob_client(n).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
    let o = BlobContainerClientListBlobsOptions { prefix: Some("logs/".into()), ..Default::default() };
    assert_eq!(list_names(&c, Some(o)).await, vec!["logs/1", "logs/2"]);
}

#[tokio::test]
async fn list_blobs_pages_by_maxresults() {
    let (_s, c) = container().await;
    for i in 0..7 {
        c.blob_client(&format!("b{i}")).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
    let o = BlobContainerClientListBlobsOptions { maxresults: Some(3), ..Default::default() };
    let mut pages = c.list_blobs(Some(o)).unwrap().into_pages();
    let mut sizes = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        sizes.push(p.into_model().unwrap().blob_items.len());
    }
    assert_eq!(sizes, vec![3, 3, 1]);
}

#[tokio::test]
async fn list_blobs_includes_metadata_and_properties_when_asked() {
    let (_s, c) = container().await;
    let md = HashMap::from([("k".to_string(), "v".to_string())]);
    let opts = BlockBlobClientUploadOptions { metadata: Some(md.clone()), ..Default::default() };
    c.blob_client("a").upload(RequestContent::from(b"12345".to_vec()), Some(opts)).await.unwrap();
    let o = BlobContainerClientListBlobsOptions { include: Some(vec![ListBlobsIncludeItem::Metadata]), ..Default::default() };
    let mut s = c.list_blobs(Some(o)).unwrap();
    let item = s.try_next().await.unwrap().unwrap();
    assert_eq!(item.properties.as_ref().unwrap().content_length, Some(5));
    let m = item.metadata.unwrap();
    assert_eq!(m.values.unwrap().get("k").map(String::as_str), Some("v"));
}

// ---- names that need URL encoding ----------------------------------------------------------

#[tokio::test]
async fn blob_names_with_special_characters_roundtrip_and_list_back_unchanged() {
    let (_s, c) = container().await;
    let names = [
        "with space.txt",
        "unicodé/日本語.txt",
        "percent%20sign",
        "100%",
        "plus+sign",
        "hash#tag",
        "question?mark",
        "amp&equals=semi;colon",
        "dir/sub dir/file",
        "tilde~and'quote\"",
    ];
    for (i, n) in names.iter().enumerate() {
        let b = c.blob_client(n);
        let content = format!("content-{i}").into_bytes();
        b.upload(RequestContent::from(content.clone()), None).await.unwrap_or_else(|e| panic!("upload {n:?}: {e}"));
        let got = body(b.download(None).await.unwrap_or_else(|e| panic!("download {n:?}: {e}"))).await;
        assert_eq!(got, content, "blob name {n:?}");
    }
    let mut listed = list_names(&c, None).await;
    listed.sort();
    let mut expected: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(listed, expected);
}

// ---- leases --------------------------------------------------------------------------------

#[tokio::test]
async fn blob_write_without_lease_id_while_leased_is_412_lease_id_missing() {
    let (_s, c) = container().await;
    let b = c.blob_client("leased");
    b.upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    let lease = b.acquire_lease(60, None).await.unwrap();
    let id = lease.lease_id().unwrap().unwrap();
    let e = b.upload(RequestContent::from(b"y".to_vec()), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::PreconditionFailed));
    assert_eq!(error_code(&e).as_deref(), Some("LeaseIdMissing"));
    // With the lease id the write works.
    let opts = BlockBlobClientUploadOptions { lease_id: Some(id.clone()), ..Default::default() };
    b.upload(RequestContent::from(b"y".to_vec()), Some(opts)).await.unwrap();
    let p = b.get_properties(None).await.unwrap();
    assert_eq!(p.lease_state().unwrap(), Some(LeaseState::Leased));
    assert_eq!(p.lease_status().unwrap(), Some(LeaseStatus::Locked));
    b.release_lease(id, None).await.unwrap();
    assert_eq!(b.get_properties(None).await.unwrap().lease_state().unwrap(), Some(LeaseState::Available));
}

#[tokio::test]
async fn acquiring_a_second_lease_is_409_lease_already_present() {
    let (_s, c) = container().await;
    let b = c.blob_client("leased");
    b.upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    b.acquire_lease(-1, None).await.unwrap();
    let e = b.acquire_lease(-1, None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
    assert_eq!(error_code(&e).as_deref(), Some("LeaseAlreadyPresent"));
}

#[tokio::test]
async fn container_lease_blocks_container_delete_without_lease_id() {
    let (_s, c) = container().await;
    let lease = c.acquire_lease(60, None).await.unwrap();
    let id = lease.lease_id().unwrap().unwrap();
    let e = c.delete(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::PreconditionFailed));
    assert_eq!(error_code(&e).as_deref(), Some("LeaseIdMissing"));
    c.release_lease(id, None).await.unwrap();
    c.delete(None).await.unwrap();
}

// ---- copy ----------------------------------------------------------------------------------

#[tokio::test]
async fn copy_from_url_within_the_account_copies_content_and_metadata_succeeds() {
    let (_s, c) = container().await;
    let src = c.blob_client("src");
    src.upload(RequestContent::from(b"copy me".to_vec()), None).await.unwrap();
    let dst = c.blob_client("dst");
    let r = dst.start_copy_from_url(src.url().to_string(), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::Accepted);
    assert_eq!(r.copy_status().unwrap(), Some(CopyStatus::Success));
    assert!(r.copy_id().unwrap().is_some());
    assert_eq!(body(dst.download(None).await.unwrap()).await, b"copy me");
    let p = dst.get_properties(None).await.unwrap();
    assert_eq!(p.copy_status().unwrap(), Some(CopyStatus::Success));
}

#[tokio::test]
async fn upload_blob_from_url_copies_the_source_bytes() {
    let (_s, c) = container().await;
    let src = c.blob_client("src");
    src.upload(RequestContent::from(b"from url".to_vec()), None).await.unwrap();
    let dst = c.blob_client("dst");
    dst.block_blob_client().upload_blob_from_url(src.url().to_string(), None).await.unwrap();
    assert_eq!(body(dst.download(None).await.unwrap()).await, b"from url");
}

// ---- conditional requests ------------------------------------------------------------------

#[tokio::test]
async fn upload_with_stale_etag_is_412_condition_not_met() {
    let (_s, c) = container().await;
    let b = c.blob_client("e");
    b.upload(RequestContent::from(b"1".to_vec()), None).await.unwrap();
    let opts = BlockBlobClientUploadOptions { if_match: Some(Etag::from("\"0x8DDDDDDDDDDDDDD\"")), ..Default::default() };
    let e = b.upload(RequestContent::from(b"2".to_vec()), Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::PreconditionFailed));
    assert_eq!(error_code(&e).as_deref(), Some("ConditionNotMet"));
}

#[tokio::test]
async fn upload_with_current_etag_succeeds_and_changes_the_etag() {
    let (_s, c) = container().await;
    let b = c.blob_client("e");
    b.upload(RequestContent::from(b"1".to_vec()), None).await.unwrap();
    let etag = b.get_properties(None).await.unwrap().etag().unwrap().unwrap();
    let opts = BlockBlobClientUploadOptions { if_match: Some(etag.clone()), ..Default::default() };
    b.upload(RequestContent::from(b"2".to_vec()), Some(opts)).await.unwrap();
    let etag2 = b.get_properties(None).await.unwrap().etag().unwrap().unwrap();
    assert_ne!(etag, etag2);
}

#[tokio::test]
async fn download_with_matching_if_none_match_is_304_not_modified() {
    let (_s, c) = container().await;
    let b = c.blob_client("e");
    b.upload(RequestContent::from(b"1".to_vec()), None).await.unwrap();
    let etag = b.get_properties(None).await.unwrap().etag().unwrap().unwrap();
    let opts = BlobClientDownloadOptions { if_none_match: Some(etag), ..Default::default() };
    let e = b.download(Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotModified));
}

#[tokio::test]
async fn delete_with_stale_etag_is_412_and_blob_survives() {
    let (_s, c) = container().await;
    let b = c.blob_client("e");
    b.upload(RequestContent::from(b"1".to_vec()), None).await.unwrap();
    let opts = BlobClientDeleteOptions { if_match: Some(Etag::from("\"0x1\"")), ..Default::default() };
    let e = b.delete(Some(opts)).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::PreconditionFailed));
    assert!(b.exists().await.unwrap());
}

// ---- snapshots, other blob types, service --------------------------------------------------

#[tokio::test]
async fn snapshot_keeps_old_content_after_the_blob_is_overwritten() {
    let (_s, c) = container().await;
    let b = c.blob_client("snap");
    b.upload(RequestContent::from(b"v1".to_vec()), None).await.unwrap();
    let snap = b.create_snapshot(None).await.unwrap();
    let id = snap.snapshot().unwrap().unwrap();
    b.upload(RequestContent::from(b"v2".to_vec()), None).await.unwrap();
    let old = b.with_snapshot(&id).unwrap();
    assert_eq!(body(old.download(None).await.unwrap()).await, b"v1");
    assert_eq!(body(b.download(None).await.unwrap()).await, b"v2");
}

#[tokio::test]
async fn append_blob_blocks_are_appended_in_order() {
    let (_s, c) = container().await;
    let b = c.blob_client("log");
    let ab = b.append_blob_client();
    ab.create(None).await.unwrap();
    for part in ["one,", "two,", "three"] {
        ab.append_block(RequestContent::from(part.as_bytes().to_vec()), part.len() as u64, None).await.unwrap();
    }
    assert_eq!(body(b.download(None).await.unwrap()).await, b"one,two,three");
    assert_eq!(b.get_properties(None).await.unwrap().blob_type().unwrap(), Some(BlobType::AppendBlob));
}

#[tokio::test]
async fn page_blob_written_pages_read_back_and_unwritten_pages_are_zero() {
    let (_s, c) = container().await;
    let b = c.blob_client("pg");
    let pb = b.page_blob_client();
    pb.create(1024, None).await.unwrap();
    let page = vec![7u8; 512];
    pb.upload_pages(RequestContent::from(page.clone()), 512, (512u64..1024).into(), None).await.unwrap();
    let got = body(b.download(None).await.unwrap()).await;
    assert_eq!(got.len(), 1024);
    assert_eq!(&got[..512], &vec![0u8; 512][..]);
    assert_eq!(&got[512..], &page[..]);
}

#[tokio::test]
async fn account_info_reports_storage_v2() {
    let svc = service(&account());
    let r = svc.get_account_info(None).await.unwrap();
    assert!(r.account_kind().unwrap().is_some());
}

#[tokio::test]
async fn service_properties_roundtrip_delete_retention() {
    let svc = service(&account());
    let mut p = svc.get_properties(None).await.unwrap().into_model().unwrap();
    p.delete_retention_policy = Some(RetentionPolicy { enabled: Some(true), days: Some(3), allow_permanent_delete: None });
    svc.set_properties(p.try_into().unwrap(), None).await.unwrap();
    let got = svc.get_properties(None).await.unwrap().into_model().unwrap();
    let d = got.delete_retention_policy.unwrap();
    assert_eq!(d.enabled, Some(true));
    assert_eq!(d.days, Some(3));
}

#[tokio::test]
async fn access_tier_can_be_changed_to_cool() {
    let (_s, c) = container().await;
    let b = c.blob_client("tier");
    b.upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    b.set_tier(AccessTier::Cool, None).await.unwrap();
    assert_eq!(b.get_properties(None).await.unwrap().access_tier().unwrap().as_deref(), Some("Cool"));
}

#[tokio::test]
async fn find_blobs_by_tags_returns_blobs_matching_the_filter() {
    let (svc, c) = container().await;
    let tag_value = uniq("v");
    let tagged = c.blob_client("tagged");
    tagged
        .upload(
            RequestContent::from(b"x".to_vec()),
            Some(BlockBlobClientUploadOptions::default().with_tags(HashMap::from([("team".to_string(), tag_value.clone())]))),
        )
        .await
        .unwrap();
    c.blob_client("plain").upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    let mut found = vec![];
    let mut s = svc.find_blobs_by_tags(&format!("\"team\"='{tag_value}'"), None).unwrap();
    while let Some(b) = s.try_next().await.unwrap() {
        found.push(b.name.unwrap());
    }
    assert_eq!(found, vec!["tagged"]);
}

#[tokio::test]
async fn container_stored_access_policy_roundtrip() {
    let (_s, c) = container().await;
    let expiry = azure_core::time::OffsetDateTime::from_unix_timestamp(1_798_761_600).unwrap();
    let policy = AccessPolicy { start: None, expiry: Some(expiry), permission: Some("rl".into()) };
    let ids: SignedIdentifiers = HashMap::from([("read-list".to_string(), policy)]).into();
    c.set_access_policy(ids.try_into().unwrap(), None).await.unwrap();
    let got = c.get_access_policy(None).await.unwrap().into_model().unwrap();
    let items = got.items.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id.as_deref(), Some("read-list"));
    assert_eq!(items[0].access_policy.as_ref().unwrap().permission.as_deref(), Some("rl"));
    assert_eq!(items[0].access_policy.as_ref().unwrap().expiry, Some(expiry));
}

#[tokio::test]
async fn an_account_without_versioning_keeps_no_versions() {
    let (_s, c) = container().await;
    let b = c.blob_client("nover");
    b.upload(RequestContent::from(b"v1".to_vec()), None).await.unwrap();
    assert!(b.get_properties(None).await.unwrap().version_id().unwrap().is_none());
    b.upload(RequestContent::from(b"v2".to_vec()), None).await.unwrap();
    assert!(b.get_properties(None).await.unwrap().version_id().unwrap().is_none());
}

// versioning is switched on for this test's own account through the management API (ARM), the way a team does it
#[tokio::test]
async fn overwrite_creates_a_new_version_and_old_version_stays_readable() {
    let svc = service(&azemu_sdk_rust_tests::account_with_versioning("verblob"));
    let c = svc.blob_container_client(&uniq("c"));
    c.create(None).await.unwrap();
    let b = c.blob_client("ver");
    b.upload(RequestContent::from(b"v1".to_vec()), None).await.unwrap();
    let v1 = b.get_properties(None).await.unwrap().version_id().unwrap().expect("version id on a versioned account");
    b.upload(RequestContent::from(b"v2".to_vec()), None).await.unwrap();
    let v2 = b.get_properties(None).await.unwrap().version_id().unwrap().unwrap();
    assert_ne!(v1, v2);
    assert_eq!(body(b.with_version(&v1).unwrap().download(None).await.unwrap()).await, b"v1");
}

#[tokio::test]
async fn download_into_fills_the_caller_buffer() {
    let (_s, c) = container().await;
    let b = c.blob_client("into");
    let d = data(3 * 1024 * 1024);
    b.upload(RequestContent::from(d.clone()), None).await.unwrap();
    let mut buf = vec![0u8; d.len()];
    let r = b.download_into(&mut buf, None).await.unwrap();
    assert_eq!(r.len, d.len());
    assert!(buf == d);
}

#[tokio::test]
async fn anonymous_client_reads_a_blob_container_with_public_blob_access_over_plain_http() {
    let acct = account();
    let svc = service(&acct);
    let name = uniq("pub");
    let c = svc.blob_container_client(&name);
    c.create(Some(BlobContainerClientCreateOptions { access: Some(PublicAccessType::Blob), ..Default::default() })).await.unwrap();
    c.blob_client("open.txt").upload(RequestContent::from(b"public".to_vec()), None).await.unwrap();
    let url = Url::parse(&format!("{}{name}/open.txt", emu().http("blob", &acct))).unwrap();
    let anon = azure_storage_blob::BlobClient::new(url, None, None).unwrap();
    assert_eq!(body(anon.download(None).await.unwrap()).await, b"public");
}

#[tokio::test]
async fn anonymous_client_on_private_container_is_refused() {
    let acct = account();
    let svc = service(&acct);
    let name = uniq("priv");
    svc.blob_container_client(&name).create(None).await.unwrap();
    svc.blob_container_client(&name).blob_client("b").upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    let url = Url::parse(&format!("{}{name}/b", emu().http("blob", &acct))).unwrap();
    let anon = azure_storage_blob::BlobClient::new(url, None, None).unwrap();
    let e = anon.download(None).await.unwrap_err();
    // Azure answers 404 ResourceNotFound (older service versions) or 401 NoAuthenticationInformation
    // (versions with the bearer challenge, which the SDK uses); either way it refuses.
    assert!(matches!(status(&e), Some(StatusCode::NotFound) | Some(StatusCode::Unauthorized)), "{e}");
}

#[tokio::test]
async fn list_blobs_with_a_prefix_that_contains_a_space() {
    let (_s, c) = container().await;
    for n in ["dir with space/a", "dir with space/b", "dir/c"] {
        c.blob_client(n).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
    let o = BlobContainerClientListBlobsOptions { prefix: Some("dir with space/".into()), ..Default::default() };
    assert_eq!(list_names(&c, Some(o)).await, vec!["dir with space/a", "dir with space/b"]);
}

#[tokio::test]
async fn list_blobs_with_a_prefix_that_contains_a_plus_sign_matches_a_literal_plus() {
    // The SDK percent-encodes a literal plus as %2B, which must stay a plus.
    let (_s, c) = container().await;
    for n in ["a+b/1", "a b/2"] {
        c.blob_client(n).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
    let o = BlobContainerClientListBlobsOptions { prefix: Some("a+b/".into()), ..Default::default() };
    assert_eq!(list_names(&c, Some(o)).await, vec!["a+b/1"]);
}
