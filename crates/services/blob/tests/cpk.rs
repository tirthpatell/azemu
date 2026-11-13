//! Customer-provided keys: the headers are checked against each other on every request, and a blob written
//! with a key can only be read and written with that key. Nothing is encrypted (`src/cpk.rs` says what is documented).

mod common;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::*;
use sha2::{Digest, Sha256};

fn key(byte: u8) -> (String, String) {
    (B64.encode([byte; 32]), B64.encode(Sha256::digest([byte; 32])))
}

fn headers<'a>(k: &'a (String, String), extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut h =
        vec![("x-ms-encryption-key", k.0.as_str()), ("x-ms-encryption-key-sha256", k.1.as_str()), ("x-ms-encryption-algorithm", "AES256")];
    h.extend_from_slice(extra);
    h
}

async fn put(env: &Env, h: &[(&str, &str)]) -> azemu_core::testing::TestResponse {
    let mut all = vec![("x-ms-blob-type", "BlockBlob")];
    all.extend_from_slice(h);
    env.send("PUT", "/cont1/b", &all, "data").await
}

#[tokio::test]
async fn the_headers_must_agree_with_each_other() {
    let env = start().await;
    env.create_container("cont1").await;
    let k = key(1);
    // the hash is not the hash of the key (the review's `AAAA`)
    let r =
        put(&env, &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-key-sha256", "AAAA"), ("x-ms-encryption-algorithm", "AES256")]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"), "{}", r.text());
    // the hash of another key
    let other = key(2);
    let r = put(&env, &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-key-sha256", &other.1), ("x-ms-encryption-algorithm", "AES256")])
        .await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    assert!(r.text().contains("x-ms-encryption-key"), "{}", r.text());
    assert!(!r.text().contains(&k.0), "the key is not echoed");
    // the key without its hash or algorithm; the hash without the key on a write; a wrong algorithm; a key of the wrong size
    let r = put(&env, &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-algorithm", "AES256")]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    let r = put(&env, &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-key-sha256", &k.1)]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    let r = put(&env, &[("x-ms-encryption-key-sha256", &k.1)]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    let r =
        put(&env, &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-key-sha256", &k.1), ("x-ms-encryption-algorithm", "AES128")]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    let short = B64.encode([1u8; 16]);
    let r =
        put(&env, &[("x-ms-encryption-key", &short), ("x-ms-encryption-key-sha256", &k.1), ("x-ms-encryption-algorithm", "AES256")]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    // nothing was written by any of them; a consistent set works
    assert_eq!(env.send("GET", "/cont1/b", &[], "").await.status, 404);
    let r = put(&env, &headers(&k, &[])).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("x-ms-request-server-encrypted"), Some("true"));
    assert_eq!(r.header("x-ms-encryption-key-sha256"), Some(k.1.as_str()));
    // the headers are checked on reads too, even for a blob without a key
    let r = env
        .send(
            "GET",
            "/cont1/b",
            &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-key-sha256", "AAAA"), ("x-ms-encryption-algorithm", "AES256")],
            "",
        )
        .await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
}

#[tokio::test]
async fn a_blob_written_with_a_key_needs_that_key() {
    let env = start().await;
    env.create_container("cont1").await;
    let (k1, k2) = (key(1), key(2));
    let r = env.send("PUT", "/cont1/secret", &headers(&k1, &[("x-ms-blob-type", "BlockBlob")]), "hello").await;
    assert_eq!(r.status, 201, "{}", r.text());

    // reads and property reads: the key works, none or another key is refused
    for (method, path) in [("GET", "/cont1/secret"), ("HEAD", "/cont1/secret"), ("GET", "/cont1/secret?comp=metadata")] {
        let ok = env.send(method, path, &headers(&k1, &[]), "").await;
        assert_eq!(ok.status, 200, "{method} {path}: {}", ok.text());
        let none = env.send(method, path, &[], "").await;
        assert_eq!((none.status, code(&none).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"), "{method} {path} without a key");
        let wrong = env.send(method, path, &headers(&k2, &[]), "").await;
        assert_eq!((wrong.status, code(&wrong).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"), "{method} {path} with another key");
    }
    let wrong = env.send("GET", "/cont1/secret", &headers(&k2, &[]), "").await;
    assert!(wrong.text().contains("not the one it was written with"), "{}", wrong.text());
    // the hash alone names the key too (property requests send no key)
    let hash_only = env.send("HEAD", "/cont1/secret", &[("x-ms-encryption-key-sha256", &k1.1)], "").await;
    assert_eq!(hash_only.status, 200);
    let hash_wrong = env.send("HEAD", "/cont1/secret", &[("x-ms-encryption-key-sha256", &k2.1)], "").await;
    assert_eq!(hash_wrong.status, 409);

    // writes to the blob: another key and no key are refused, its key works (Put Block, metadata, properties)
    let blk = B64.encode("blk000000001");
    for (what, method, path, extra) in [
        ("put block", "PUT", format!("/cont1/secret?comp=block&blockid={blk}"), vec![]),
        ("set metadata", "PUT", "/cont1/secret?comp=metadata".to_string(), vec![("x-ms-meta-a", "1")]),
        ("set properties", "PUT", "/cont1/secret?comp=properties".to_string(), vec![("x-ms-blob-content-type", "text/plain")]),
    ] {
        let none = env.send(method, &path, &extra, "x").await;
        assert_eq!((none.status, code(&none).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"), "{what} without a key");
        let wrong = env.send(method, &path, &headers(&k2, &extra), "x").await;
        assert_eq!((wrong.status, code(&wrong).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"), "{what} with another key");
        let ok = env.send(method, &path, &headers(&k1, &extra), "x").await;
        assert!(ok.status == 200 || ok.status == 201, "{what} with its key: {} {}", ok.status, ok.text());
    }
    // the data is still there and a blob without a key is not affected
    let r = env.send("GET", "/cont1/secret", &headers(&k1, &[]), "").await;
    assert_eq!(r.text(), "hello");
    env.put_blob("cont1", "plain", "x").await;
    assert_eq!(env.send("GET", "/cont1/plain", &[], "").await.status, 200);
}

#[tokio::test]
async fn append_and_page_blobs_check_the_key_too() {
    let env = start().await;
    env.create_container("cont1").await;
    let (k1, k2) = (key(1), key(2));
    let r = env.send("PUT", "/cont1/log", &headers(&k1, &[("x-ms-blob-type", "AppendBlob")]), "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let wrong = env.send("PUT", "/cont1/log?comp=appendblock", &headers(&k2, &[]), "line").await;
    assert_eq!((wrong.status, code(&wrong).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"));
    let ok = env.send("PUT", "/cont1/log?comp=appendblock", &headers(&k1, &[]), "line").await;
    assert_eq!(ok.status, 201, "{}", ok.text());
    let r = env.send("PUT", "/cont1/pg", &headers(&k1, &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", "512")]), "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let page = vec![0u8; 512];
    let wrong = env
        .send("PUT", "/cont1/pg?comp=page", &headers(&k2, &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=0-511")]), page.clone())
        .await;
    assert_eq!((wrong.status, code(&wrong).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"));
    let ok =
        env.send("PUT", "/cont1/pg?comp=page", &headers(&k1, &[("x-ms-page-write", "update"), ("x-ms-range", "bytes=0-511")]), page).await;
    assert_eq!(ok.status, 201, "{}", ok.text());
}
