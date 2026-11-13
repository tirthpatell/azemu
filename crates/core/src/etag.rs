//! ETag generation and comparison.

use std::sync::atomic::{AtomicU64, Ordering};

use time::OffsetDateTime;

static LAST: AtomicU64 = AtomicU64::new(0);

/// Added to unix-epoch 100 ns ticks so values look like Azure's (`0x8D…`):
/// unix ticks for 2022-10-27T22:06:34.6827829Z plus this give `0x8DAB86A4C0A1B2C`.
const TICK_OFFSET: u64 = 621355979961293559;

fn now_ticks() -> u64 {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos().max(0) as u128;
    ((nanos / 100) as u64).wrapping_add(TICK_OFFSET)
}

/// A new quoted etag like `"0x8DAB86A4C0A1B2C"`. Unique within the process and
/// strictly increasing across calls (timestamp ticks, bumped by one when two
/// calls land on the same tick).
pub fn new_etag() -> String {
    let now = now_ticks();
    let mut prev = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return format!("\"0x{next:X}\""),
            Err(p) => prev = p,
        }
    }
}

/// The opaque value of an etag: surrounding quotes and a `W/` prefix removed.
/// Unquoted values are accepted as they are.
pub fn opaque(tag: &str) -> &str {
    let t = tag.trim();
    let t = t.strip_prefix("W/").unwrap_or(t);
    t.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(t)
}

/// Compare two etags by opaque value (weak comparison: `W/"a"` equals `"a"`).
pub fn eq(a: &str, b: &str) -> bool {
    opaque(a) == opaque(b)
}

/// A parsed `If-Match` / `If-None-Match` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EtagMatch {
    /// `*`
    Any,
    /// A list of opaque values.
    List(Vec<String>),
}

impl EtagMatch {
    /// Parse a header value: `*`, or a comma separated list of (quoted) etags.
    /// Commas inside quotes do not split.
    pub fn parse(header: &str) -> EtagMatch {
        let h = header.trim();
        if h == "*" {
            return EtagMatch::Any;
        }
        let mut items = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        for c in h.chars() {
            match c {
                '"' => {
                    in_quotes = !in_quotes;
                    cur.push(c);
                }
                ',' if !in_quotes => {
                    push_item(&mut items, &cur);
                    cur.clear();
                }
                _ => cur.push(c),
            }
        }
        push_item(&mut items, &cur);
        EtagMatch::List(items)
    }

    /// Does this match an existing resource with the given etag?
    pub fn matches(&self, etag: &str) -> bool {
        match self {
            EtagMatch::Any => true,
            EtagMatch::List(items) => {
                let o = opaque(etag);
                items.iter().any(|i| i == o)
            }
        }
    }
}

fn push_item(items: &mut Vec<String>, raw: &str) {
    let o = opaque(raw);
    if !o.is_empty() || raw.trim() == "\"\"" {
        items.push(o.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn format_and_monotonic() {
        let mut prev = String::new();
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let e = new_etag();
            assert!(e.starts_with("\"0x8D") && e.ends_with('"'), "{e}");
            assert_eq!(e.len(), "\"0x8DAB86A4C0A1B2C\"".len(), "{e}");
            assert!(seen.insert(e.clone()), "duplicate {e}");
            // same length hex, so lexicographic order equals numeric order
            assert!(e > prev, "{e} <= {prev}");
            prev = e;
        }
    }

    #[test]
    fn unique_across_threads() {
        let hs: Vec<_> = (0..4).map(|_| std::thread::spawn(|| (0..2000).map(|_| new_etag()).collect::<Vec<_>>())).collect();
        let mut all = HashSet::new();
        for h in hs {
            for e in h.join().unwrap() {
                assert!(all.insert(e));
            }
        }
        assert_eq!(all.len(), 8000);
    }

    #[test]
    fn tick_range_is_azure_like() {
        // current year is >= 2022, so value must be above the 2022 sample
        let e = new_etag();
        let v = u64::from_str_radix(e.trim_matches('"').trim_start_matches("0x"), 16).unwrap();
        assert!(v >= 0x08DA_B86A_4C0A_1B2C);
    }

    #[test]
    fn opaque_and_eq() {
        assert_eq!(opaque("\"abc\""), "abc");
        assert_eq!(opaque("W/\"abc\""), "abc");
        assert_eq!(opaque("abc"), "abc");
        assert!(eq("W/\"a\"", "\"a\""));
        assert!(eq("a", "\"a\""));
        assert!(!eq("\"a\"", "\"b\""));
    }

    #[test]
    fn parse_lists() {
        assert_eq!(EtagMatch::parse(" * "), EtagMatch::Any);
        assert_eq!(EtagMatch::parse("\"a\", W/\"b\" ,c"), EtagMatch::List(vec!["a".into(), "b".into(), "c".into()]));
        assert_eq!(EtagMatch::parse("\"a,b\",\"c\""), EtagMatch::List(vec!["a,b".into(), "c".into()]));
        assert!(EtagMatch::parse("\"a\",\"b\"").matches("W/\"b\""));
        assert!(!EtagMatch::parse("\"a\"").matches("\"b\""));
        assert!(EtagMatch::Any.matches("\"x\""));
        assert!(!EtagMatch::parse("").matches("\"x\""));
    }
}
