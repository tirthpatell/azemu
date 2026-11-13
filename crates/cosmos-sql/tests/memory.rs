//! Memory bounds of the query engine, measured with a counting allocator.
//!
//! One test function, because the counter is global: tests in one binary run on parallel threads and would
//! count each other's allocations. Sizes are small (tens of MB at most); what is checked is the shape of the
//! growth, not a big number.

use azemu_cosmos_sql::{parse, ErrorKind, EvalOptions, Item, Params, Query};
use serde_json::{json, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

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

/// How much more memory than the start was in use at the highest point while `f` ran.
fn extra_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let r = f();
    let peak = PEAK.load(Relaxed);
    (r, peak.saturating_sub(base))
}

fn run(q: &Query, docs: &[Arc<Value>], opts: &EvalOptions) -> Result<Vec<Item>, azemu_cosmos_sql::SqlError> {
    q.execute_shared(docs, &Params::new(), opts)
}

const MB: usize = 1024 * 1024;

#[test]
fn queries_are_bounded_in_memory() {
    let opts = EvalOptions::default();

    // ---- Documents are shared, not copied ----
    // 300 documents of 20 KB: 6 MB, the size of one partition in the review's reproduction
    let docs: Vec<Arc<Value>> =
        (0..300).map(|i| Arc::new(json!({"id": format!("d{i}"), "pk": "p", "n": i, "pad": "x".repeat(20_000)}))).collect();
    let data = 300 * 20_000;
    for (sql, rows) in [
        ("SELECT VALUE COUNT(1) FROM c", 1),
        ("SELECT * FROM c WHERE c.id = 'd7'", 1),
        ("SELECT VALUE c FROM c WHERE c.n >= 290", 10),
        ("SELECT TOP 5 * FROM c", 5),
        ("SELECT * FROM c", 300),
        ("SELECT VALUE c FROM c", 300),
        ("SELECT c.id FROM c WHERE LENGTH(c.pad) = 20000 AND c.n < 100", 100),
        ("SELECT * FROM c WHERE c.id IN ('d1', 'd2') OFFSET 0 LIMIT 5", 2),
        ("SELECT VALUE c.id FROM c ORDER BY c.n DESC", 300),
    ] {
        let q = parse(sql).unwrap();
        let (r, extra) = extra_peak(|| run(&q, &docs, &opts).unwrap());
        assert_eq!(r.len(), rows, "{sql}");
        assert!(extra < data / 10, "{sql}: {extra} extra bytes at the peak for {data} bytes of documents");
    }
    // the same query on 8 threads at once: still nothing proportional to the data (the counter is global)
    let q = parse("SELECT VALUE COUNT(1) FROM c").unwrap();
    let (_, extra) = extra_peak(|| {
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| assert_eq!(run(&q, &docs, &opts).unwrap()[0].value(), &json!(300)));
            }
        })
    });
    assert!(extra < data / 4, "8 concurrent counts used {extra} extra bytes for {data} bytes of documents");
    // a document that comes out as it went in is the same allocation
    let q = parse("SELECT * FROM c WHERE c.id = 'd7'").unwrap();
    let r = run(&q, &docs, &opts).unwrap();
    match &r[0] {
        Item::Shared(d) => assert!(Arc::ptr_eq(d, &docs[7])),
        other => panic!("expected the shared document, got {other:?}"),
    }
    // a projection that changes the document is built new
    let q = parse("SELECT c.id FROM c WHERE c.id = 'd7'").unwrap();
    assert!(matches!(&run(&q, &docs, &opts).unwrap()[0], Item::Owned(v) if v == &json!({"id": "d7"})));

    // ---- A join is streamed, what a query keeps is counted ----
    for n in [500usize, 2000] {
        let arr: Vec<usize> = (0..n).collect();
        let one = vec![Arc::new(json!({"id": "j", "arr": arr}))];
        // 4 million rows go through WHERE and COUNT and none is kept
        let q = parse("SELECT VALUE COUNT(1) FROM c JOIN a IN c.arr JOIN b IN c.arr WHERE a < b").unwrap();
        let (r, extra) = extra_peak(|| run(&q, &one, &opts).unwrap());
        assert_eq!(r[0].value(), &json!(n * (n - 1) / 2));
        assert!(extra < 2 * MB, "{n} x {n} join: {extra} extra bytes at the peak");
        // the review's query: TOP 10 of a self join stops after ten rows
        let q = parse("SELECT TOP 10 VALUE 1 FROM c JOIN a IN c.arr JOIN b IN c.arr").unwrap();
        let (r, extra) = extra_peak(|| run(&q, &one, &opts).unwrap());
        assert_eq!(r.len(), 10);
        assert!(extra < MB, "TOP 10 of a {n} x {n} join: {extra} extra bytes");
    }
    // the product of the review (a page of 10 rows asked for, all rows produced): bounded work, bounded memory
    let arr: Vec<usize> = (0..2000).collect();
    let big = vec![Arc::new(json!({"id": "j", "arr": arr}))];
    let q = parse("SELECT VALUE 1 FROM c JOIN a IN c.arr JOIN b IN c.arr WHERE c.id = 'j'").unwrap();
    // keeps 4 million rows: far over the budget, so it is refused, and it never gets near the budget's size
    let small = EvalOptions { max_bytes: 4 * MB, ..EvalOptions::default() };
    let (r, extra) = extra_peak(|| run(&q, &big, &small));
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::LimitExceeded);
    assert!(e.message.contains("memory"), "{}", e.message);
    assert!(extra < 8 * MB, "refused after {extra} extra bytes with a budget of 4 MB");
    // with the default budget the same query is refused too (4 million rows of at least 100 bytes each)
    let r = run(&q, &big, &opts);
    assert_eq!(r.unwrap_err().kind, ErrorKind::LimitExceeded);

    // ---- other ways to build a big result: all counted, none grows past the budget ----
    // many small documents: 100,000 of them
    let many: Vec<Arc<Value>> =
        (0..100_000).map(|i| Arc::new(json!({"id": format!("id-{i}"), "g": i % 50_000, "s": "abcdefghij"}))).collect();
    let budget = EvalOptions { max_bytes: 2 * MB, ..EvalOptions::default() };
    for (sql, refused) in [
        ("SELECT c.id, c.s FROM c ORDER BY c.id DESC", true),
        ("SELECT DISTINCT VALUE c.id FROM c", true),
        ("SELECT c.id, COUNT(1) AS n FROM c GROUP BY c.id", true),
        ("SELECT DISTINCT VALUE c.id FROM c ORDER BY c.id", true),
        ("SELECT VALUE c.s FROM c", true),
        // answers that fit: few groups, few distinct values, a top n, an aggregate
        ("SELECT c.s, COUNT(1) AS n FROM c GROUP BY c.s", false),
        ("SELECT DISTINCT VALUE c.s FROM c", false),
        ("SELECT TOP 10 c.id, c.s FROM c ORDER BY c.id DESC", false),
        ("SELECT VALUE COUNT(1) FROM c", false),
        ("SELECT VALUE MAX(c.g) FROM c", false),
        ("SELECT VALUE c.id FROM c ORDER BY c.id OFFSET 5 LIMIT 3", false),
    ] {
        let q = parse(sql).unwrap();
        let (r, extra) = extra_peak(|| run(&q, &many, &budget));
        match (r, refused) {
            (Err(e), true) => assert_eq!(e.kind, ErrorKind::LimitExceeded, "{sql}"),
            (Ok(rows), false) => assert!(!rows.is_empty(), "{sql}"),
            (Ok(rows), true) => panic!("{sql}: {} rows came out of a 2 MB budget", rows.len()),
            (Err(e), false) => panic!("{sql}: {}", e.message),
        }
        assert!(extra < 8 * MB, "{sql}: {extra} extra bytes at the peak with a budget of 2 MB");
    }
    // the answers that fit are right
    let q = parse("SELECT TOP 3 VALUE c.id FROM c ORDER BY c.id DESC").unwrap();
    let r: Vec<Value> = run(&q, &many, &budget).unwrap().into_iter().map(Item::into_value).collect();
    assert_eq!(r, vec![json!("id-99999"), json!("id-99998"), json!("id-99997")]);

    // ---- values that multiply a big string or array ----
    let one = vec![Arc::new(json!({"id": "s", "s": "y".repeat(1_500_000), "arr": (0..100_000).collect::<Vec<_>>()}))];
    for sql in [
        // 20,000 copies of a 1.5 MB string would be 30 GB
        &format!("SELECT VALUE CONCAT({}) FROM c", vec!["c.s"; 20_000].join(", ")),
        &format!("SELECT VALUE TOSTRING([{}]) FROM c", vec!["c.s"; 20_000].join(", ")),
        &format!("SELECT VALUE ARRAY_CONCAT({}) FROM c", vec!["c.arr"; 5_000].join(", ")),
        "SELECT VALUE REPLACE(c.s, 'y', c.s) FROM c",
        "SELECT VALUE STRINGJOIN(ARRAY(SELECT VALUE c.s FROM x IN c.arr), c.s) FROM c",
        "SELECT VALUE c.s || c.s || c.s || c.s FROM c",
    ] {
        let q = parse(sql).unwrap();
        let (r, extra) = extra_peak(|| run(&q, &one, &opts));
        assert_eq!(r.unwrap_err().kind, ErrorKind::LimitExceeded, "{}", &sql[..60.min(sql.len())]);
        assert!(extra < 120 * MB, "{}: {extra} extra bytes", &sql[..60.min(sql.len())]);
    }
    // ARRAY(SELECT ...) over a big array is counted like any other result
    let q = parse("SELECT VALUE ARRAY(SELECT VALUE y FROM y IN c.arr) FROM c").unwrap();
    let tiny = EvalOptions { max_bytes: MB, ..EvalOptions::default() };
    let (r, extra) = extra_peak(|| run(&q, &one, &tiny));
    assert_eq!(r.unwrap_err().kind, ErrorKind::LimitExceeded);
    assert!(extra < 20 * MB, "{extra}");
    // a big string that is within the limit still works
    let q = parse("SELECT VALUE LENGTH(c.s || c.s) FROM c").unwrap();
    assert_eq!(run(&q, &one, &opts).unwrap()[0].value(), &json!(3_000_000));
}
