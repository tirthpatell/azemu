//! Blob Storage through `azure_storage_blob` 1.2.0-beta.1: listing that asks for Apache Arrow and falls back to XML,
//! hierarchical listing with a delimiter, asynchronous copy.

#[allow(dead_code)]
#[path = "../../src/lib.rs"]
mod common;

use azure_core::http::{RequestContent, StatusCode, Url};
use azure_storage_blob::models::*;
use azure_storage_blob::{BlobContainerClient, BlobServiceClient};
use common::{cred, emu, uniq};
use futures::TryStreamExt;

async fn container() -> BlobContainerClient {
    let url = Url::parse(&emu().https("blob", &uniq("acct"))).unwrap();
    let svc = BlobServiceClient::new(url, Some(cred()), None).unwrap();
    let c = svc.blob_container_client(&uniq("c"));
    c.create(None).await.unwrap();
    c
}

async fn put(c: &BlobContainerClient, names: &[&str]) {
    for n in names {
        c.blob_client(n).upload(RequestContent::from(b"x".to_vec()), None).await.unwrap();
    }
}

#[tokio::test]
async fn list_blobs_falls_back_to_xml_when_the_server_does_not_return_arrow() {
    let c = container().await;
    put(&c, &["b", "a", "d/e"]).await;
    let mut names = vec![];
    let mut s = c.list_blobs(None).unwrap();
    while let Some(b) = s.try_next().await.unwrap() {
        names.push(b.name.unwrap());
    }
    assert_eq!(names, vec!["a", "b", "d/e"]);
}

#[tokio::test]
async fn list_blobs_pages_with_a_continuation_marker() {
    let c = container().await;
    put(&c, &["b0", "b1", "b2", "b3", "b4"]).await;
    let o = BlobContainerClientListBlobsOptions { maxresults: Some(2), ..Default::default() };
    let mut pages = c.list_blobs(Some(o)).unwrap().into_pages();
    let mut sizes = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        sizes.push(p.into_model().unwrap().blob_items.len());
    }
    assert_eq!(sizes, vec![2, 2, 1]);
}

#[tokio::test]
async fn hierarchical_listing_with_a_slash_delimiter_returns_prefixes_and_blobs() {
    let c = container().await;
    put(&c, &["top.txt", "logs/2024/a", "logs/2025/b", "logs/c", "data/x"]).await;
    let mut blobs = vec![];
    let mut prefixes = vec![];
    let mut pages = c.list_blobs_hierarchical("/", None).unwrap().into_pages();
    while let Some(p) = pages.try_next().await.unwrap() {
        let m = p.into_model().unwrap();
        blobs.extend(m.hierarchical_list.blob_items.into_iter().filter_map(|b| b.name));
        prefixes.extend(m.hierarchical_list.blob_prefixes.unwrap_or_default().into_iter().filter_map(|b| b.name));
    }
    assert_eq!(blobs, vec!["top.txt"]);
    assert_eq!(prefixes, vec!["data/", "logs/"]);
}

#[tokio::test]
async fn hierarchical_listing_below_a_prefix_returns_the_next_level() {
    let c = container().await;
    put(&c, &["logs/2024/a", "logs/2025/b", "logs/c", "data/x"]).await;
    let o = BlobContainerClientListBlobsHierarchicalOptions { prefix: Some("logs/".into()), ..Default::default() };
    let mut blobs = vec![];
    let mut prefixes = vec![];
    let mut pages = c.list_blobs_hierarchical("/", Some(o)).unwrap().into_pages();
    while let Some(p) = pages.try_next().await.unwrap() {
        let m = p.into_model().unwrap();
        blobs.extend(m.hierarchical_list.blob_items.into_iter().filter_map(|b| b.name));
        prefixes.extend(m.hierarchical_list.blob_prefixes.unwrap_or_default().into_iter().filter_map(|b| b.name));
    }
    assert_eq!(blobs, vec!["logs/c"]);
    assert_eq!(prefixes, vec!["logs/2024/", "logs/2025/"]);
}

#[tokio::test]
async fn hierarchical_listing_with_a_multi_character_delimiter() {
    let c = container().await;
    put(&c, &["a--1", "a--2", "b--x", "plain"]).await;
    let mut blobs = vec![];
    let mut prefixes = vec![];
    let mut pages = c.list_blobs_hierarchical("--", None).unwrap().into_pages();
    while let Some(p) = pages.try_next().await.unwrap() {
        let m = p.into_model().unwrap();
        blobs.extend(m.hierarchical_list.blob_items.into_iter().filter_map(|b| b.name));
        prefixes.extend(m.hierarchical_list.blob_prefixes.unwrap_or_default().into_iter().filter_map(|b| b.name));
    }
    assert_eq!(blobs, vec!["plain"]);
    assert_eq!(prefixes, vec!["a--", "b--"]);
}

#[tokio::test]
async fn start_copy_from_url_completes_and_the_copy_has_the_source_content() {
    let c = container().await;
    put(&c, &["src"]).await;
    let dst = c.blob_client("dst");
    let r = dst.start_copy_from_url(c.blob_client("src").url().to_string(), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::Accepted);
    let got = dst.download(None).await.unwrap().body.collect().await.unwrap();
    assert_eq!(got.as_ref(), b"x");
}

#[tokio::test]
async fn download_reports_the_access_tier() {
    let c = container().await;
    put(&c, &["t"]).await;
    let r = c.blob_client("t").download(None).await.unwrap();
    assert_eq!(r.properties.access_tier, Some(AccessTier::Hot));
}
