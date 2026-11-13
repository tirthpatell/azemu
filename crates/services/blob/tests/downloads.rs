//! A download made of several pieces is sent piece by piece (not joined into one buffer first), with the right
//! `Content-Length`, over HTTP/1.1.

mod common;

use common::*;

fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}

async fn put_block(env: &Env, blob: &str, id: &str, data: Vec<u8>) {
    let r = env.send("PUT", &format!("/cnt/{blob}?comp=block&blockid={}", b64(id).replace('=', "%3D")), &[], data).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn a_blob_of_several_blocks_downloads_complete_with_its_length() {
    let env = start().await;
    env.create_container("cnt").await;
    let sizes = [700 * 1024, 900 * 1024, 5, 1_300_000];
    let mut expected: Vec<u8> = vec![];
    let mut list = String::from("<BlockList>");
    for (i, n) in sizes.iter().enumerate() {
        let data: Vec<u8> = (0..*n).map(|j| ((i * 31 + j) % 251) as u8).collect();
        put_block(&env, "multi", &format!("b{i}"), data.clone()).await;
        expected.extend_from_slice(&data);
        list.push_str(&format!("<Latest>{}</Latest>", b64(&format!("b{i}"))));
    }
    list.push_str("</BlockList>");
    let r = env.send("PUT", "/cnt/multi?comp=blocklist", &[], list).await;
    assert_eq!(r.status, 201, "{}", r.text());

    let r = env.send("GET", "/cnt/multi", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), expected.len());
    assert!(r.body.as_ref() == expected.as_slice(), "the body is the concatenation of the blocks");
    assert_eq!(r.header("content-length"), Some(expected.len().to_string().as_str()));
    assert!(r.header("transfer-encoding").is_none());
    assert!(r.header("etag").is_some() && r.header("x-ms-blob-type") == Some("BlockBlob"));

    // a range over a block boundary, big enough to be streamed too
    let r = env.send("GET", "/cnt/multi", &[("x-ms-range", "bytes=100000-2000000")], "").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body.as_ref(), &expected[100_000..=2_000_000]);
    assert_eq!(r.header("content-range"), Some(format!("bytes 100000-2000000/{}", expected.len()).as_str()));

    // a range with a hash is small by rule and answered with the hash header
    let r = env.send("GET", "/cnt/multi", &[("x-ms-range", "bytes=0-1000"), ("x-ms-range-get-content-md5", "true")], "").await;
    assert_eq!(r.status, 206);
    assert!(r.header("content-md5").is_some());

    let r = env.send("HEAD", "/cnt/multi", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some(expected.len().to_string().as_str()));
}

#[tokio::test]
async fn a_page_blob_with_holes_downloads_zeros_and_data_in_order() {
    let env = start().await;
    env.create_container("cnt").await;
    let size = 3 * 1024 * 1024;
    let r = env.send("PUT", "/cnt/pg", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", &size.to_string())], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let page = vec![9u8; 1024 * 1024];
    let r = env
        .send(
            "PUT",
            "/cnt/pg?comp=page",
            &[("x-ms-page-write", "update"), ("x-ms-range", &format!("bytes={}-{}", 1024 * 1024, 2 * 1024 * 1024 - 1))],
            page.clone(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = env.send("GET", "/cnt/pg", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), size);
    assert!(r.body[..1024 * 1024].iter().all(|b| *b == 0));
    assert!(r.body[1024 * 1024..2 * 1024 * 1024].iter().all(|b| *b == 9));
    assert!(r.body[2 * 1024 * 1024..].iter().all(|b| *b == 0));
}

#[tokio::test]
async fn a_download_is_the_data_the_blob_had_when_it_was_asked_for() {
    // the pieces are shared handles: the blob may be overwritten while the body is sent
    let env = start().await;
    env.create_container("cnt").await;
    for (i, n) in [600 * 1024usize, 600 * 1024, 600 * 1024].iter().enumerate() {
        put_block(&env, "w", &format!("b{i}"), vec![b'a' + i as u8; *n]).await;
    }
    let list: String =
        format!("<BlockList>{}</BlockList>", (0..3).map(|i| format!("<Latest>{}</Latest>", b64(&format!("b{i}")))).collect::<String>());
    assert_eq!(env.send("PUT", "/cnt/w?comp=blocklist", &[], list).await.status, 201);
    // start the download and read only the first bytes; the rest waits in the connection
    let mut c = tokio::net::TcpStream::connect(("127.0.0.1", env.srv.port())).await.unwrap();
    let req = format!(
        "GET /cnt/w HTTP/1.1\r\nHost: acct.blob.localhost:{}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",
        env.srv.port(),
        AUTH.1
    );
    tokio::io::AsyncWriteExt::write_all(&mut c, req.as_bytes()).await.unwrap();
    let mut first = vec![0u8; 400];
    tokio::io::AsyncReadExt::read_exact(&mut c, &mut first).await.unwrap();
    assert_eq!(env.put_blob("cnt", "w", "short").await.status, 201);
    let r = env.send("DELETE", "/cnt/w", &[], "").await;
    assert_eq!(r.status, 202);
    let mut all = first;
    tokio::io::AsyncReadExt::read_to_end(&mut c, &mut all).await.unwrap();
    let head_end = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let body = &all[head_end..];
    assert_eq!(body.len(), 3 * 600 * 1024);
    assert_eq!(body[0], b'a');
    assert_eq!(body[body.len() - 1], b'c');
}
