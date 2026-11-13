//! `x-ms-structured-body`: uploads are taken apart (every CRC-64 checked), downloads are wrapped.
mod common;
use common::*;

/// CRC-64/NVME, bit by bit (a second implementation, not the service's table).
fn crc64(data: &[u8]) -> u64 {
    let mut c = u64::MAX;
    for &b in data {
        c ^= b as u64;
        for _ in 0..8 {
            c = if c & 1 == 1 { (c >> 1) ^ 0x9A6C_9329_AC4B_C9B5 } else { c >> 1 };
        }
    }
    !c
}

/// A message built by hand from the layout in the Go SDK (`structured_message.go`).
fn message(data: &[u8], seg: usize) -> Vec<u8> {
    let parts: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(seg).collect() };
    let total = 13 + parts.iter().map(|p| 10 + p.len() + 8).sum::<usize>() + 8;
    let mut m = vec![1u8];
    m.extend((total as u64).to_le_bytes());
    m.extend(1u16.to_le_bytes());
    m.extend((parts.len() as u16).to_le_bytes());
    for (i, p) in parts.iter().enumerate() {
        m.extend((i as u16 + 1).to_le_bytes());
        m.extend((p.len() as u64).to_le_bytes());
        m.extend_from_slice(p);
        m.extend(crc64(p).to_le_bytes());
    }
    m.extend(crc64(data).to_le_bytes());
    m
}

const XSM: &str = "XSM/1.0; properties=crc64";

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[tokio::test]
async fn put_blob_stores_the_payload_and_a_download_can_ask_for_a_message() {
    let e = start().await;
    e.create_container("cnt").await;
    let data = payload(100_000);
    let msg = message(&data, 32 * 1024);
    assert_eq!(msg.len(), 100_000 + 13 + 4 * 18 + 8);
    let len = data.len().to_string();
    let r = e
        .send(
            "PUT",
            "/cnt/b",
            &[("x-ms-blob-type", "BlockBlob"), ("x-ms-structured-body", XSM), ("x-ms-structured-content-length", &len)],
            msg.clone(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("x-ms-structured-body"), Some(XSM));
    // The stored blob is the payload.
    let r = e.send("GET", "/cnt/b", &[], "").await;
    assert_eq!(r.body.as_ref(), &data[..]);
    assert_eq!(r.header("x-ms-structured-body"), None);
    // Asking for a message gets a valid one (one segment of up to 4 MiB: 39 bytes of framing).
    let r = e.send("GET", "/cnt/b", &[("x-ms-structured-body", XSM)], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-structured-body"), Some(XSM));
    assert_eq!(r.header("x-ms-structured-content-length"), Some(len.as_str()));
    assert_eq!(r.body.as_ref(), &message(&data, 4 * 1024 * 1024)[..]);
    assert!(r.header("content-md5").is_none());
    // A range: the message holds the range; Content-Range is the range of the blob.
    let r = e.send("GET", "/cnt/b", &[("x-ms-range", "bytes=10-19"), ("x-ms-structured-body", XSM)], "").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("x-ms-structured-content-length"), Some("10"));
    assert_eq!(r.header("content-range"), Some("bytes 10-19/100000"));
    assert_eq!(r.body.as_ref(), &message(&data[10..20], 1 << 20)[..]);
}

#[tokio::test]
async fn a_bad_message_is_refused_and_nothing_is_stored() {
    let e = start().await;
    e.create_container("cnt").await;
    let data = payload(1000);
    let put = |body: Vec<u8>, len: &str| {
        let len = len.to_string();
        let e = &e;
        async move {
            e.send(
                "PUT",
                "/cnt/b",
                &[("x-ms-blob-type", "BlockBlob"), ("x-ms-structured-body", XSM), ("x-ms-structured-content-length", &len)],
                body,
            )
            .await
        }
    };
    // A flipped byte in the data: segment CRC.
    let mut m = message(&data, 400);
    m[13 + 10 + 5] ^= 1;
    let r = put(m, "1000").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "Crc64Mismatch"));
    // A wrong trailer.
    let mut m = message(&data, 400);
    let n = m.len();
    m[n - 1] ^= 1;
    assert_eq!(code(&put(m, "1000").await), "Crc64Mismatch");
    // Not a message at all (the plain data): 400 naming the header.
    let r = put(data.clone(), "1000").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
    assert!(r.text().contains("x-ms-structured-body"), "{}", r.text());
    // Wrong content length.
    let r = put(message(&data, 400), "999").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    // Unknown schema.
    let r = e
        .send(
            "PUT",
            "/cnt/b",
            &[
                ("x-ms-blob-type", "BlockBlob"),
                ("x-ms-structured-body", "XSM/9.0; properties=crc64"),
                ("x-ms-structured-content-length", "1000"),
            ],
            message(&data, 400),
        )
        .await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
    // No length.
    let r = e.send("PUT", "/cnt/b", &[("x-ms-blob-type", "BlockBlob"), ("x-ms-structured-body", XSM)], message(&data, 400)).await;
    assert_eq!(r.status, 400);
    assert_eq!(e.send("GET", "/cnt/b", &[], "").await.status, 404);
    // A download asks for a message the service does not know.
    e.put_blob("cnt", "ok", "x").await;
    let r = e.send("GET", "/cnt/ok", &[("x-ms-structured-body", "XSM/9.0; properties=crc64")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidHeaderValue"));
}

#[tokio::test]
async fn blocks_pages_and_appends_take_messages_too() {
    let e = start().await;
    e.create_container("cnt").await;
    let hdr = |len: usize| (len.to_string(), XSM);
    // Put Block + Put Block List
    let (a, b) = (payload(5000), payload(300));
    for (id, d) in [("QQ==", &a), ("Qg==", &b)] {
        let (l, x) = hdr(d.len());
        let r = e
            .send(
                "PUT",
                &format!("/cnt/blk?comp=block&blockid={}", id.replace('=', "%3D")),
                &[("x-ms-structured-body", x), ("x-ms-structured-content-length", &l)],
                message(d, 2048),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        assert_eq!(r.header("x-ms-structured-body"), Some(XSM));
    }
    let list = "<BlockList><Latest>QQ==</Latest><Latest>Qg==</Latest></BlockList>";
    assert_eq!(e.send("PUT", "/cnt/blk?comp=blocklist", &[], list).await.status, 201);
    let got = e.send("GET", "/cnt/blk", &[], "").await;
    assert_eq!(got.body.len(), 5300);
    assert_eq!(&got.body[..5000], &a[..]);
    // Append Block
    assert_eq!(e.send("PUT", "/cnt/app", &[("x-ms-blob-type", "AppendBlob")], "").await.status, 201);
    for d in [&a, &b] {
        let (l, x) = hdr(d.len());
        let r = e
            .send(
                "PUT",
                "/cnt/app?comp=appendblock",
                &[("x-ms-structured-body", x), ("x-ms-structured-content-length", &l)],
                message(d, 1000),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        assert_eq!(r.header("x-ms-structured-body"), Some(XSM));
    }
    assert_eq!(e.send("GET", "/cnt/app", &[], "").await.body.len(), 5300);
    // Put Page
    assert_eq!(e.send("PUT", "/cnt/pg", &[("x-ms-blob-type", "PageBlob"), ("x-ms-blob-content-length", "1024")], "").await.status, 201);
    let page = payload(512);
    let (l, x) = hdr(512);
    let r = e
        .send(
            "PUT",
            "/cnt/pg?comp=page",
            &[
                ("x-ms-page-write", "update"),
                ("x-ms-range", "bytes=512-1023"),
                ("x-ms-structured-body", x),
                ("x-ms-structured-content-length", &l),
            ],
            message(&page, 200),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let got = e.send("GET", "/cnt/pg", &[], "").await;
    assert_eq!(&got.body[512..], &page[..]);
    assert!(got.body[..512].iter().all(|b| *b == 0));
}
