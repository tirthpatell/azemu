//! Concurrent queries over a partition do not copy its documents.
//! A counting allocator measures the memory in use at the highest point; the review's script measured 8
//! concurrent `SELECT VALUE COUNT(1)` over a 6 MB partition: +49 MB (8 x 6 MB). Own test binary, because the
//! counter is global.

mod common;

use common::start;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// The one `unsafe` in the repository: a test needs it to count allocations (a `GlobalAlloc` impl cannot be safe).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = CURRENT.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        CURRENT.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new >= l.size() {
                let now = CURRENT.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Relaxed);
            } else {
                CURRENT.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queries_do_not_copy_the_documents() {
    let h = Arc::new(start().await);
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    let pad = "x".repeat(20_000);
    for i in 0..300 {
        let r = h.doc("d", "c", &json!("p"), &json!({"id": format!("d{i}"), "pk": "p", "n": i, "pad": pad})).await;
        assert_eq!(r.status, 201);
    }
    let data = 300 * 20_000;
    for (sql, want) in [
        ("SELECT VALUE COUNT(1) FROM c", 1usize),
        ("SELECT TOP 1 * FROM c WHERE c.id = 'd3'", 1),
        ("SELECT VALUE c.id FROM c WHERE c.n % 100 = 7", 3),
    ] {
        // warm up: what the server allocates once (buffers, connections)
        let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": sql})).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let base = CURRENT.load(Relaxed);
        PEAK.store(base, Relaxed);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let h = h.clone();
            tasks.push(tokio::spawn(async move {
                let r = h.query("d", "c", &[("x-ms-documentdb-partitionkey", "[\"p\"]")], json!({"query": sql})).await;
                assert_eq!(r.status, 200);
                r.json()["Documents"].as_array().unwrap().len()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), want, "{sql}");
        }
        let extra = PEAK.load(Relaxed).saturating_sub(base);
        // 8 copies of the partition would be 8 * 6 MB = 48 MB; the HTTP buffers of 8 requests are far below 3 MB
        assert!(extra < data / 2, "{sql}: 8 concurrent queries added {extra} bytes at the peak for {data} bytes of documents");
    }
}
