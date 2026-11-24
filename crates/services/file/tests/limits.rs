//! What a request cannot make the service do: allocate the size of a file that is only declared, hold the lock for as long as
//! a range is long, panic on a range written in the largest number, or lose a directory in a rename. Azure Files has no
//! documented cap on one Get File range; the service answers a read in pieces, so memory in use does not depend on its size.

mod common;
use common::*;

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIB4: u64 = 4 << 40;
const MIB: usize = 1 << 20;

/// Resident memory of this process, in bytes (the server runs in the same process).
fn rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    statm.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()).unwrap_or(0) * 4096
}

async fn sparse(f: &F, path: &str, size: u64) {
    f.share("sh1").await;
    f.file(path, size).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_range_from_the_middle_of_a_four_tebibyte_sparse_file_is_read_without_allocating_the_file() {
    let f = start().await;
    sparse(&f, "sh1/big", TIB4).await;
    let before = rss();
    for (len, partial) in [(MIB, true), (3 * MIB, true)] {
        let start = TIB4 / 2;
        let range = format!("bytes={start}-{}", start + len as u64 - 1);
        let r = f.send("GET", "/sh1/big", &[("x-ms-range", &range)], "").await;
        assert_eq!(r.status, if partial { 206 } else { 200 }, "{}", r.text());
        assert_eq!(r.body.len(), len);
        assert!(r.body.iter().all(|b| *b == 0));
        assert_eq!(r.header("content-range"), Some(format!("bytes {start}-{}/{TIB4}", start + len as u64 - 1).as_str()));
    }
    // a range with no end, from near the end of the file, is a short read
    let near = TIB4 - 1000;
    let r = f.send("GET", "/sh1/big", &[("Range", &format!("bytes={near}-"))], "").await;
    assert_eq!((r.status, r.body.len()), (206, 1000));
    let grown = rss().saturating_sub(before);
    assert!(grown < 256 << 20, "reading a few megabytes of a sparse file grew the process by {grown} bytes");
}

/// Send a `GET` by hand and read the answer until `want` bytes of the body arrived; the connection is dropped after that.
async fn read_head_of_body(f: &F, path: &str, want: usize) -> (String, usize) {
    let host = f.base.trim_start_matches("http://").to_string();
    let port = f.srv.port();
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let date = azemu_core::time::rfc1123(time::OffsetDateTime::now_utc());
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: SharedKey acct1:c2ln\r\nx-ms-version: 2025-05-05\r\nx-ms-date: {date}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut body_at = None;
    loop {
        let n = tokio::time::timeout(Duration::from_secs(20), s.read(&mut chunk)).await.expect("the answer stalled").unwrap();
        assert!(n > 0, "the connection closed after {} bytes: {}", buf.len(), String::from_utf8_lossy(&buf));
        buf.extend_from_slice(&chunk[..n]);
        if body_at.is_none() {
            body_at = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
        }
        if let Some(at) = body_at {
            if buf.len() - at >= want {
                let head = String::from_utf8_lossy(&buf[..at]).to_string();
                return (head, buf.len() - at);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_read_of_a_four_tebibyte_sparse_file_streams_and_stops_when_the_client_goes() {
    let f = start().await;
    sparse(&f, "sh1/big", TIB4).await;
    let before = rss();
    let (head, got) = read_head_of_body(&f, "/sh1/big", MIB).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.to_ascii_lowercase().contains(&format!("content-length: {TIB4}")), "{head}");
    assert!(got >= MIB);
    // the connection is dropped; give the server a moment to notice and then ask it for something else
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    let again = f.send("HEAD", "/sh1/big", &[], "").await;
    assert_eq!(again.status, 200);
    assert!(t.elapsed() < Duration::from_secs(5));
    let grown = rss().saturating_sub(before);
    assert!(grown < 256 << 20, "starting a read of a 4 TiB sparse file grew the process by {grown} bytes");
    // the same with a range that starts in the middle and has no end
    let (head, _) = read_head_of_body(&f, "/sh1/big", MIB).await;
    assert!(head.starts_with("HTTP/1.1 200"));
    let grown = rss().saturating_sub(before);
    assert!(grown < 256 << 20, "{grown}");
}

#[tokio::test]
async fn a_streamed_read_has_the_stored_data_where_it_was_written() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 5 * MIB as u64).await;
    f.write("sh1/f", 1000, "head").await;
    f.write("sh1/f", 2 * MIB as u64 + 600, "middle").await;
    f.write("sh1/f", 5 * MIB as u64 - 3, "end").await;
    let r = f.send("GET", "/sh1/f", &[], "").await;
    assert_eq!((r.status, r.body.len()), (200, 5 * MIB));
    assert_eq!(&r.body[1000..1004], b"head");
    assert_eq!(&r.body[2 * MIB + 600..2 * MIB + 606], b"middle");
    assert_eq!(&r.body[5 * MIB - 3..], b"end");
    assert_eq!(r.body.iter().filter(|b| **b != 0).count(), 4 + 6 + 3);
    // a range that starts in the middle of a page and stops in the middle of another, over the 1 MiB where streaming starts
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", &format!("bytes=1002-{}", 2 * MIB + 603))], "").await;
    assert_eq!((r.status, r.body.len()), (206, 2 * MIB + 603 - 1002 + 1));
    assert_eq!(&r.body[..2], b"ad");
    assert_eq!(&r.body[r.body.len() - 4..], b"midd");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clearing_a_range_of_four_tebibytes_looks_only_at_the_pages_that_exist() {
    let f = std::sync::Arc::new(start().await);
    sparse(&f, "sh1/big", TIB4).await;
    f.write("sh1/big", 0, "start").await;
    f.write("sh1/big", 700, "middle").await;
    f.write("sh1/big", TIB4 / 2, "half").await;
    f.write("sh1/big", TIB4 - 4, "end!").await;
    let t = Instant::now();
    let clear = {
        let f = f.clone();
        tokio::spawn(async move {
            f.send("PUT", "/sh1/big?comp=range", &[("x-ms-range", &format!("bytes=1-{}", TIB4 - 2)), ("x-ms-write", "clear")], "").await
        })
    };
    // a request for the whole service, while the clear runs: it must not wait for the length of the range
    let list = f.send("GET", "/?comp=list", &[], "").await;
    let waited = t.elapsed();
    let r = clear.await.unwrap();
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(list.status, 200);
    assert!(t.elapsed() < Duration::from_secs(5), "clearing took {:?}", t.elapsed());
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    // the first and the last byte are still there, the rest is zeros
    let ranges = f.send("GET", "/sh1/big?comp=rangelist", &[], "").await;
    assert_eq!(all(&ranges.text(), "Start"), vec!["0", &(TIB4 - 512).to_string()]);
    let r = f.send("GET", "/sh1/big", &[("x-ms-range", "bytes=0-9")], "").await;
    assert_eq!(r.body.to_vec(), b"s\0\0\0\0\0\0\0\0\0");
    let r = f.send("GET", "/sh1/big", &[("x-ms-range", &format!("bytes={}-", TIB4 - 4))], "").await;
    assert_eq!(r.body.to_vec(), b"\0\0\0!");
}

#[tokio::test]
async fn clearing_keeps_what_is_outside_the_range_page_by_page() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 4096).await;
    let data: String = (0..4096).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    f.write("sh1/f", 0, &data).await;
    let clear = |a: u64, b: u64| {
        let f = &f;
        async move {
            let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", &format!("bytes={a}-{b}")), ("x-ms-write", "clear")], "").await;
            assert_eq!(r.status, 201, "{}", r.text());
        }
    };
    let mut expect: Vec<u8> = data.clone().into_bytes();
    // inside one page, across a page edge, whole pages, an unaligned start with an aligned end, an aligned start with an unaligned end
    for (a, b) in [(10u64, 20u64), (500, 520), (1024, 2047), (2100, 3071), (3072, 3500), (4000, 4095), (0, 0), (511, 512)] {
        clear(a, b).await;
        expect[a as usize..=b as usize].fill(0);
        assert_eq!(f.read("sh1/f").await.into_bytes(), expect, "after clearing {a}-{b}");
    }
}

#[tokio::test]
async fn a_range_written_as_the_largest_number_is_an_answer_not_a_panic() {
    let f = start().await;
    f.share("sh1").await;
    f.file("sh1/f", 100).await;
    f.file("sh1/g", 100).await;
    let max = u64::MAX.to_string();
    let range = format!("bytes=0-{max}");
    // a read past the end is cut at the size
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", &range)], "").await;
    assert_eq!((r.status, r.body.len()), (206, 100));
    assert_eq!(r.header("content-range"), Some("bytes 0-99/100"));
    // writes: too long to be one Put Range, and clearing past the end
    let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", &range), ("x-ms-write", "update")], "x").await;
    assert_eq!(failed(&r), (400, "InvalidHeaderValue".into()));
    let r = f.send("PUT", "/sh1/f?comp=range", &[("x-ms-range", &range), ("x-ms-write", "clear")], "").await;
    assert_eq!(failed(&r), (416, "InvalidRange".into()));
    let r = f.send("GET", "/sh1/f?comp=rangelist", &[("x-ms-range", &range)], "").await;
    assert_eq!(failed(&r), (416, "InvalidRange".into()));
    // Put Range From URL: the source range
    let src = format!("{}/sh1/g", f.base);
    let r = f
        .send(
            "PUT",
            "/sh1/f?comp=range",
            &[("x-ms-range", "bytes=0-0"), ("x-ms-write", "update"), ("x-ms-copy-source", &src), ("x-ms-source-range", &range)],
            "",
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = f
        .send(
            "PUT",
            "/sh1/f?comp=range",
            &[("x-ms-range", &range), ("x-ms-write", "update"), ("x-ms-copy-source", &src), ("x-ms-source-range", "bytes=0-0")],
            "",
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    // and the largest start
    let r = f.send("GET", "/sh1/f", &[("x-ms-range", &format!("bytes={max}-"))], "").await;
    assert_eq!(r.status, 416);
}

#[tokio::test]
async fn a_rename_into_itself_written_in_another_case_does_not_lose_the_directory() {
    let f = start().await;
    f.share("sh1").await;
    // `Ä` and `ä` are one name to the service (it folds case the way Windows does), although they are not the same ASCII letters
    f.dir("sh1/%C3%84").await;
    f.file("sh1/%C3%84/f.txt", 3).await;
    f.write("sh1/%C3%84/f.txt", 0, "abc").await;
    let url = format!("{}/sh1/%C3%84", f.base);
    let r = f.send("PUT", "/sh1/%C3%A4/sub?restype=directory&comp=rename", &[("x-ms-file-rename-source", &url)], "").await;
    assert_eq!(failed(&r), (409, "RenameCycle".into()), "{}", r.text());
    assert_eq!(f.read("sh1/%C3%84/f.txt").await, "abc");
    // a failure after the node was taken out puts it back: the destination's parent is gone when the walk is made again
    let r = f.send("PUT", "/sh1/nothere/x?restype=directory&comp=rename", &[("x-ms-file-rename-source", &url)], "").await;
    assert_eq!(failed(&r), (404, "ParentNotFound".into()));
    assert_eq!(f.read("sh1/%C3%84/f.txt").await, "abc");
}
