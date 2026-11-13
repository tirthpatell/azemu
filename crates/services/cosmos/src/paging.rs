//! Continuation tokens and paging.
//!
//! A token is a JSON object like the real ones (`token` string and the partition range). It
//! carries up to three things, all optional on the way back in:
//!
//! * `#SKIP:n`: rows of the result already delivered;
//! * `#Q:id`: the result of this query is kept in the [`PageCache`] under `id`, so the next
//!   page is a slice of it and the query is not run again;
//! * `after`: for the plain document feed, the key (partition key and id) of the last document
//!   delivered, so the next page starts right behind it in the ordered map.
//!
//! Tokens stay valid when the cache entry is gone (expired, evicted, the service restarted,
//! another process): the query is run again and `n` rows are skipped. That is also how Azure
//! behaves: a continuation token is not tied to server memory.
//!
//! What a later page shows when the data changed in between: a page that comes out of the cache
//! shows the rows as they were when the first page was made (the query is not re-evaluated);
//! the feed of plain documents (`after`) shows the documents as they are now. Azure promises
//! neither for rows written after the query started.

use std::collections::HashMap;
use std::sync::Arc;

use azemu_cosmos_sql::Item;
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::pk::{DocKey, PkComp};

const MARK: &str = "#SKIP:";
const CACHE_MARK: &str = "#Q:";

/// A response never holds more than this many bytes of rows (Azure: 4 MB per response).
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest number of cached results.
const CACHE_ENTRIES: usize = 256;
/// Largest estimated size of all cached rows together.
const CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Seconds an unused cached result is kept.
const CACHE_TTL_SECS: i64 = 600;

/// What a token says.
#[derive(Default, Debug, PartialEq)]
pub struct Token {
    pub skip: usize,
    pub cache_id: Option<u64>,
    pub after: Option<DocKey>,
}

pub fn encode(t: &Token) -> String {
    let mut text = format!("+RID:~azemu{MARK}{}", t.skip);
    if let Some(id) = t.cache_id {
        text.push_str(&format!("{CACHE_MARK}{id}"));
    }
    let mut v = json!({ "token": text, "range": { "min": "", "max": "FF" } });
    if let Some(k) = &t.after {
        v["after"] = json!({ "pk": k.pk.iter().map(PkComp::to_json).collect::<Vec<_>>(), "id": k.id });
    }
    v.to_string()
}

/// Reads a token; anything that is absent or unreadable is "from the start".
pub fn decode(token: Option<&str>) -> Token {
    let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Token::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(t) else { return Token::default() };
    let mut out = Token::default();
    if let Some(s) = v.get("token").and_then(Value::as_str) {
        if let Some(i) = s.find(MARK) {
            let rest = &s[i + MARK.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            out.skip = digits.parse().unwrap_or(0);
            if let Some(j) = rest.find(CACHE_MARK) {
                let d: String = rest[j + CACHE_MARK.len()..].chars().take_while(char::is_ascii_digit).collect();
                out.cache_id = d.parse().ok();
            }
        }
    }
    if let Some(a) = v.get("after") {
        let pk: Option<Vec<PkComp>> =
            a.get("pk").and_then(Value::as_array).and_then(|items| items.iter().map(|x| PkComp::from_json(x).ok()).collect());
        let id = a.get("id").and_then(Value::as_str);
        if let (Some(pk), Some(id)) = (pk, id) {
            out.after = Some(DocKey { pk, id: id.to_string() });
        }
    }
    out
}

/// Rough size in bytes of a value as JSON, without allocating. Iterative.
pub fn approx_size(v: &Value) -> usize {
    let mut total = 0;
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        total += match v {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(_) => 8,
            Value::String(s) => s.len() + 2,
            Value::Array(a) => {
                stack.extend(a.iter());
                a.len() + 2
            }
            Value::Object(m) => {
                for (k, x) in m {
                    total += k.len() + 4;
                    stack.push(x);
                }
                2
            }
        };
    }
    total
}

/// A row of a result: plain JSON, or a document shared with the container.
pub trait Row {
    fn json(&self) -> &Value;
}

impl Row for Value {
    fn json(&self) -> &Value {
        self
    }
}

impl Row for Item {
    fn json(&self) -> &Value {
        self.value()
    }
}

/// How many rows of `rows[skip..]` fit in one page of at most `size` rows and
/// [`MAX_PAGE_BYTES`] (always at least one row when any remain).
pub fn page_len<R: Row>(rows: &[R], skip: usize, size: Option<usize>) -> usize {
    let rest = rows.get(skip..).unwrap_or(&[]);
    let max = size.unwrap_or(usize::MAX).min(rest.len());
    let mut bytes = 0usize;
    for (i, r) in rest[..max].iter().enumerate() {
        bytes += approx_size(r.json());
        if bytes > MAX_PAGE_BYTES && i > 0 {
            return i;
        }
    }
    max
}

/// Take one page out of `rows` by the token's skip count (used where the whole list is cheap to
/// produce: metadata feeds). Returns the next token when rows remain.
pub fn page(rows: Vec<Value>, token: Option<&str>, size: Option<usize>) -> (Vec<Value>, Option<String>) {
    let skip = decode(token).skip;
    let n = page_len(&rows, skip, size);
    let end = skip.saturating_add(n);
    let more = end < rows.len();
    let page: Vec<Value> = rows.into_iter().skip(skip).take(n).collect();
    let next = more.then(|| encode(&Token { skip: end, ..Token::default() }));
    (page, next)
}

pub struct RunningGuard<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

struct Entry {
    rows: Arc<Vec<Item>>,
    sig: u64,
    scanned: usize,
    bytes: usize,
    last_used: i64,
}

#[derive(Default)]
struct Inner {
    /// Time of the last sweep of expired entries.
    swept: i64,
    next_id: u64,
    bytes: usize,
    entries: HashMap<u64, Entry>,
}

/// Results of queries that have more pages, kept for a short time. Bounded in entries, in
/// estimated bytes and in age. Losing an entry is harmless (see the module text).
#[derive(Default)]
pub struct PageCache {
    inner: Mutex<Inner>,
    /// Queries run (a page served from a kept result is not one); for tests.
    executions: std::sync::atomic::AtomicU64,
    /// Queries running now and the most that ran at the same time; for tests of the concurrency bound.
    running: std::sync::atomic::AtomicUsize,
    peak_running: std::sync::atomic::AtomicUsize,
}

impl PageCache {
    /// The cached result `id`, if it is still there and was made for the same request
    /// (`sig`: path, query, parameters and addressing headers).
    pub fn get(&self, id: u64, sig: u64, now: i64) -> Option<(Arc<Vec<Item>>, usize)> {
        let mut g = self.inner.lock();
        let e = g.entries.get_mut(&id)?;
        if e.sig != sig || now - e.last_used > CACHE_TTL_SECS {
            return None;
        }
        e.last_used = now;
        Some((e.rows.clone(), e.scanned))
    }

    /// Keep `rows`; None when they are too big to keep (the caller then pages by re-running).
    pub fn put(&self, sig: u64, rows: Arc<Vec<Item>>, scanned: usize, now: i64) -> Option<u64> {
        // a shared document counts in full: the cache keeps it alive after the container has dropped or replaced it
        let bytes: usize = rows.iter().map(|r| approx_size(r.value()) + 16).sum();
        if bytes > CACHE_BYTES / 2 {
            return None;
        }
        let mut g = self.inner.lock();
        // drop what expired, then the least recently used until the new entry fits
        let dead: Vec<u64> = g.entries.iter().filter(|(_, e)| now - e.last_used > CACHE_TTL_SECS).map(|(k, _)| *k).collect();
        for k in dead {
            if let Some(e) = g.entries.remove(&k) {
                g.bytes -= e.bytes;
            }
        }
        while g.entries.len() >= CACHE_ENTRIES || g.bytes + bytes > CACHE_BYTES {
            let Some(oldest) = g.entries.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| *k) else {
                break;
            };
            if let Some(e) = g.entries.remove(&oldest) {
                g.bytes -= e.bytes;
            }
        }
        g.next_id += 1;
        let id = g.next_id;
        g.bytes += bytes;
        g.entries.insert(id, Entry { rows, sig, scanned, bytes, last_used: now });
        Some(id)
    }

    /// Drops the entries that nobody asked for within the time to live. `put` does it too, but a client that opens
    /// paged queries and goes away would otherwise leave its results in memory until the next query; every request
    /// calls this, and it does its work at most every 30 seconds.
    pub fn sweep(&self, now: i64) {
        let mut g = self.inner.lock();
        if g.entries.is_empty() || now - g.swept < 30 {
            return;
        }
        g.swept = now;
        let dead: Vec<u64> = g.entries.iter().filter(|(_, e)| now - e.last_used > CACHE_TTL_SECS).map(|(k, _)| *k).collect();
        for k in dead {
            if let Some(e) = g.entries.remove(&k) {
                g.bytes -= e.bytes;
            }
        }
    }

    pub fn count_execution(&self) {
        self.executions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A query started to run. Dropping the guard says it is done.
    pub fn running_guard(&self) -> RunningGuard<'_> {
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.running.fetch_add(1, Relaxed) + 1;
        self.peak_running.fetch_max(now, Relaxed);
        RunningGuard(&self.running)
    }

    pub fn peak_running(&self) -> usize {
        self.peak_running.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn executions(&self) -> u64 {
        self.executions.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn remove(&self, id: u64) {
        let mut g = self.inner.lock();
        if let Some(e) = g.entries.remove(&id) {
            g.bytes -= e.bytes;
        }
    }

    pub fn clear(&self) {
        *self.inner.lock() = Inner::default();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_junk_means_start() {
        let t = Token {
            skip: 40,
            cache_id: Some(7),
            after: Some(DocKey { pk: vec![PkComp::Str("a".into()), PkComp::Num(2.0), PkComp::Undefined], id: "x".into() }),
        };
        assert_eq!(decode(Some(&encode(&t))), t);
        for junk in ["", "  ", "abc", "{}", "{\"token\":5}", "{\"token\":\"+RID:~azemu#SKIP:zz\"}"] {
            assert_eq!(decode(Some(junk)), Token::default(), "{junk}");
        }
        assert_eq!(decode(None), Token::default());
        // tokens written before the cache existed still work
        let old = json!({"token": "+RID:~azemu#SKIP:12", "range": {"min": "", "max": "FF"}}).to_string();
        assert_eq!(decode(Some(&old)).skip, 12);
    }

    #[test]
    fn page_slices_and_reports_more() {
        let rows: Vec<Value> = (0..10).map(|i| json!(i)).collect();
        let (p, next) = page(rows.clone(), None, Some(4));
        assert_eq!(p, vec![json!(0), json!(1), json!(2), json!(3)]);
        let (p, next) = page(rows.clone(), next.as_deref(), Some(4));
        assert_eq!(p, vec![json!(4), json!(5), json!(6), json!(7)]);
        let (p, next) = page(rows, next.as_deref(), Some(4));
        assert_eq!(p, vec![json!(8), json!(9)]);
        assert!(next.is_none());
    }

    #[test]
    fn a_page_stops_at_four_megabytes_but_always_has_a_row() {
        let big = Value::String("x".repeat(3 * 1024 * 1024));
        let rows = vec![big.clone(), big.clone(), big];
        assert_eq!(page_len(&rows, 0, None), 1);
        assert_eq!(page_len(&rows, 2, None), 1);
        assert_eq!(page_len(&rows, 3, None), 0);
    }

    #[test]
    fn cache_is_bounded_and_checks_the_request() {
        let c = PageCache::default();
        let rows = Arc::new(vec![Item::Owned(json!(1))]);
        let id = c.put(5, rows.clone(), 3, 100).unwrap();
        assert!(c.get(id, 6, 100).is_none(), "another request must not read it");
        assert_eq!(c.get(id, 5, 100).unwrap().1, 3);
        assert!(c.get(id, 5, 100 + CACHE_TTL_SECS + 1).is_none(), "expired");
        for i in 0..(CACHE_ENTRIES * 2) {
            c.put(i as u64, rows.clone(), 0, 200 + i as i64).unwrap();
        }
        assert!(c.len() <= CACHE_ENTRIES);
        let huge = Arc::new(vec![Item::Owned(Value::String("x".repeat(CACHE_BYTES)))]);
        assert!(c.put(1, huge, 0, 0).is_none());
        c.clear();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn idle_results_are_dropped_by_the_sweep() {
        let c = PageCache::default();
        let rows = Arc::new(vec![Item::Owned(json!(1))]);
        for i in 0..10 {
            c.put(i, rows.clone(), 0, 1000).unwrap();
        }
        c.sweep(1000 + CACHE_TTL_SECS); // not expired yet
        assert_eq!(c.len(), 10);
        c.sweep(1000 + CACHE_TTL_SECS + 31); // 30 seconds after the last sweep
        assert_eq!(c.len(), 0, "nothing asked for them for longer than the time to live");
        assert_eq!(c.inner.lock().bytes, 0);
    }
}
