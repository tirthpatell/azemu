//! `LIKE`: `%` matches any run of characters (also none), `_` exactly one character.
//!
//! The pattern is cut at the `%` signs into segments. The first segment must match at the start of the value
//! (unless the pattern starts with `%`), the last at its end (unless it ends with `%`), and each segment between
//! is searched for from where the one before it ended, taking the leftmost match, which is enough for this
//! pattern language (taking the first match never makes a later one impossible). So nothing backtracks:
//!
//! * a segment of plain characters is found with `str::find`, which takes time proportional to the value plus
//!   the segment (two-way search, no worst case that is quadratic);
//! * a segment with `_` in it is found with the shift-and method: one bit per segment position, a bit vector
//!   updated once per character of the value, so time is the value's length times the segment's length divided
//!   by 64. That is the only cost that can grow, and it is counted against the query's time limit as it goes
//!   (`Budget::work`), so it is stopped at the limit like everything else.
//!
//! The value is read in place (it is never copied into a vector of characters).

use std::collections::HashMap;

use super::budget::Budget;
use super::value::{EvalError, EvalResult};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    One,
    Lit(char),
}

#[derive(Debug)]
enum Seg {
    /// Only plain characters.
    Plain(String),
    /// Has at least one `_`.
    Mixed(Mixed),
}

/// Shift-and tables of a segment that has `_`.
#[derive(Debug)]
struct Mixed {
    toks: Vec<Tok>,
    words: usize,
    /// Positions that match any character.
    wild: Vec<u64>,
    /// For each character in the segment, the positions where it matches (the wild ones are not set here).
    by_char: HashMap<char, Vec<u64>>,
}

#[derive(Debug)]
pub struct Like {
    /// The text between the `%` signs, empty ones left out.
    segs: Vec<Seg>,
    /// The first segment is the part before the first `%` (it is anchored at the start); false when the pattern starts with `%`.
    anchored_start: bool,
    anchored_end: bool,
    /// No `%` at all: the one segment must match the whole value.
    exact: bool,
}

impl Like {
    pub fn compile(pattern: &str, esc: Option<char>) -> EvalResult<Like> {
        let mut parts: Vec<Vec<Tok>> = vec![Vec::new()];
        let mut it = pattern.chars();
        while let Some(c) = it.next() {
            let last = parts.len() - 1;
            if Some(c) == esc {
                match it.next() {
                    Some(n) => parts[last].push(Tok::Lit(n)),
                    None => return Err(EvalError::new("InvalidLikePattern", "The pattern ends with its escape character.")),
                }
            } else if c == '%' {
                parts.push(Vec::new());
            } else if c == '_' {
                parts[last].push(Tok::One);
            } else {
                parts[last].push(Tok::Lit(c));
            }
        }
        let exact = parts.len() == 1;
        let anchored_start = !parts[0].is_empty() || exact;
        let anchored_end = !parts[parts.len() - 1].is_empty() || exact;
        let segs = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .map(|p| {
                if p.iter().all(|t| matches!(t, Tok::Lit(_))) {
                    Seg::Plain(p.iter().map(|t| if let Tok::Lit(c) = t { *c } else { ' ' }).collect())
                } else {
                    Seg::Mixed(Mixed::new(p))
                }
            })
            .collect();
        Ok(Like { segs, anchored_start, anchored_end, exact })
    }

    /// Does `text` match? Counts its work against `budget` and gives up (an error) when the limit is reached.
    pub fn matches(&self, text: &str, budget: &Budget) -> EvalResult<bool> {
        budget.bytes(text.len())?;
        if self.segs.is_empty() {
            // `''` matches only the empty value; `%`, `%%` match everything
            return Ok(!self.exact || text.is_empty());
        }
        let n = self.segs.len();
        let mut pos = 0usize;
        let mut first = 0usize;
        let mut last = n; // segments in first..last are searched for
        if self.anchored_start {
            match match_at(&self.segs[0], text, 0, budget)? {
                Some(end) => pos = end,
                None => return Ok(false),
            }
            first = 1;
            if self.exact {
                return Ok(pos == text.len());
            }
        }
        let mut tail: Option<&Seg> = None;
        if self.anchored_end && n > first {
            tail = Some(&self.segs[n - 1]);
            last = n - 1;
        }
        for seg in &self.segs[first..last] {
            match find(seg, text, pos, budget)? {
                Some(end) => pos = end,
                None => return Ok(false),
            }
        }
        if let Some(seg) = tail {
            // the last segment must end at the end of the value: it starts that many characters before it
            let len = seg_chars(seg);
            let Some((start, _)) = text.char_indices().rev().nth(len - 1) else { return Ok(false) };
            budget.bytes(text.len() - start)?;
            if start < pos {
                return Ok(false);
            }
            return Ok(match_at(seg, text, start, budget)? == Some(text.len()));
        }
        Ok(true)
    }
}

fn seg_chars(seg: &Seg) -> usize {
    match seg {
        Seg::Plain(s) => s.chars().count(),
        Seg::Mixed(m) => m.toks.len(),
    }
}

/// If the segment matches `text` starting at byte `pos`, the byte where the match ends.
fn match_at(seg: &Seg, text: &str, pos: usize, budget: &Budget) -> EvalResult<Option<usize>> {
    match seg {
        Seg::Plain(s) => {
            budget.bytes(s.len())?;
            Ok(text.get(pos..).filter(|rest| rest.starts_with(s.as_str())).map(|_| pos + s.len()))
        }
        Seg::Mixed(m) => {
            budget.bytes(m.toks.len())?;
            let mut chars = text.get(pos..).unwrap_or("").char_indices();
            let mut end = pos;
            for t in &m.toks {
                let Some((i, c)) = chars.next() else { return Ok(None) };
                if let Tok::Lit(l) = t {
                    if *l != c {
                        return Ok(None);
                    }
                }
                end = pos + i + c.len_utf8();
            }
            Ok(Some(end))
        }
    }
}

/// The byte where the leftmost match of the segment in `text[from..]` ends.
fn find(seg: &Seg, text: &str, from: usize, budget: &Budget) -> EvalResult<Option<usize>> {
    let rest = text.get(from..).unwrap_or("");
    match seg {
        Seg::Plain(s) => {
            budget.bytes(rest.len())?;
            Ok(rest.find(s.as_str()).map(|i| from + i + s.len()))
        }
        Seg::Mixed(m) => m.find(rest, budget).map(|r| r.map(|e| from + e)),
    }
}

impl Mixed {
    fn new(toks: Vec<Tok>) -> Mixed {
        let words = toks.len().div_ceil(64);
        let mut wild = vec![0u64; words];
        let mut by_char: HashMap<char, Vec<u64>> = HashMap::new();
        for (i, t) in toks.iter().enumerate() {
            match t {
                Tok::One => wild[i / 64] |= 1 << (i % 64),
                Tok::Lit(c) => by_char.entry(*c).or_insert_with(|| vec![0u64; words])[i / 64] |= 1 << (i % 64),
            }
        }
        Mixed { toks, words, wild, by_char }
    }

    /// Shift-and: the state has bit `i` set when the first `i + 1` positions of the segment match the characters
    /// just read. The first time the last bit is set, a match ends at the character just read.
    fn find(&self, text: &str, budget: &Budget) -> EvalResult<Option<usize>> {
        let m = self.toks.len();
        let (last_word, last_bit) = ((m - 1) / 64, 1u64 << ((m - 1) % 64));
        let mut state = vec![0u64; self.words];
        let per_char = self.words as u64;
        for (i, c) in text.char_indices() {
            budget.work(per_char)?;
            let mask = self.by_char.get(&c);
            let mut carry = 1u64; // the bit shifted in at position 0 (a match may start here)
            for w in 0..self.words {
                let shifted = (state[w] << 1) | carry;
                carry = state[w] >> 63;
                let hit = mask.map_or(0, |v| v[w]) | self.wild[w];
                state[w] = shifted & hit;
            }
            if state[last_word] & last_bit != 0 {
                return Ok(Some(i + c.len_utf8()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::CancelToken;
    use std::time::{Duration, Instant};

    fn m(text: &str, pat: &str) -> bool {
        m_esc(text, pat, None)
    }

    fn m_esc(text: &str, pat: &str, esc: Option<char>) -> bool {
        Like::compile(pat, esc).unwrap().matches(text, &Budget::unlimited()).unwrap()
    }

    /// The textbook recursive definition, for comparison on small inputs.
    fn slow(text: &[char], pat: &[char]) -> bool {
        match pat.split_first() {
            None => text.is_empty(),
            Some(('%', rest)) => (0..=text.len()).any(|i| slow(&text[i..], rest)),
            Some(('_', rest)) => !text.is_empty() && slow(&text[1..], rest),
            Some((c, rest)) => text.first() == Some(c) && slow(&text[1..], rest),
        }
    }

    #[test]
    fn the_documented_examples() {
        assert!(m("abc", "a%"));
        assert!(m("abc", "_b_"));
        assert!(!m("abc", "a_"));
        assert!(m("", "%"));
        assert!(m("", ""));
        assert!(!m("a", ""));
        assert!(m("abc", "%%%"));
        assert!(!m("a%c", "a\\%c"));
        assert!(m_esc("a%c", "a\\%c", Some('\\')));
        assert!(!m_esc("abc", "a\\%c", Some('\\')));
        assert!(Like::compile("abc\\", Some('\\')).is_err());
    }

    #[test]
    fn it_agrees_with_the_plain_definition_on_every_small_case() {
        // all patterns up to 5 characters over {a, b, %, _} against all values up to 6 characters over {a, b}
        let alphabet_p = ['a', 'b', '%', '_'];
        let alphabet_t = ['a', 'b'];
        let mut pats: Vec<Vec<char>> = vec![vec![]];
        let mut frontier: Vec<Vec<char>> = vec![vec![]];
        for _ in 0..5 {
            let mut next = vec![];
            for p in &frontier {
                for c in alphabet_p {
                    let mut q = p.clone();
                    q.push(c);
                    next.push(q);
                }
            }
            pats.extend(next.iter().cloned());
            frontier = next;
        }
        let mut texts: Vec<Vec<char>> = vec![vec![]];
        let mut frontier: Vec<Vec<char>> = vec![vec![]];
        for _ in 0..6 {
            let mut next = vec![];
            for t in &frontier {
                for c in alphabet_t {
                    let mut q = t.clone();
                    q.push(c);
                    next.push(q);
                }
            }
            texts.extend(next.iter().cloned());
            frontier = next;
        }
        let b = Budget::unlimited();
        for p in &pats {
            let ps: String = p.iter().collect();
            let like = Like::compile(&ps, None).unwrap();
            for t in &texts {
                let ts: String = t.iter().collect();
                assert_eq!(like.matches(&ts, &b).unwrap(), slow(t, p), "{ts:?} LIKE {ps:?}");
            }
        }
    }

    #[test]
    fn characters_are_not_bytes() {
        assert!(m("héllo", "h_llo"));
        assert!(m("日本語", "日_語"));
        assert!(m("日本語", "%本%"));
        assert!(m("日本語", "%_語"));
        assert!(m("日本語", "%__語"));
        assert!(!m("日本語", "%___語"));
        assert!(m("a😀b", "a_b"));
    }

    #[test]
    fn long_patterns_with_underscores_cross_word_boundaries() {
        let seg: String = (0..150).map(|i| if i % 3 == 0 { '_' } else { 'x' }).collect();
        let text = format!("yy{}zz", "x".repeat(150));
        assert!(m(&text, &format!("%{seg}%")));
        assert!(m(&text, &format!("yy{seg}zz")));
        assert!(!m(&text, &format!("%{seg}q")));
        let text2 = format!("{}y{}", "x".repeat(70), "x".repeat(150));
        assert!(m(&text2, &format!("%y{seg}")));
    }

    #[test]
    fn the_cases_that_took_20_million_steps_take_a_blink() {
        // A value of 4,000,000 `a` against `%aaaab`
        let text = "a".repeat(4_000_000);
        let b = Budget::unlimited();
        let t = Instant::now();
        assert!(!Like::compile("%aaaab", None).unwrap().matches(&text, &b).unwrap());
        assert!(!Like::compile("%a%a%a%a%a%b", None).unwrap().matches(&text, &b).unwrap());
        assert!(Like::compile("%a_a", None).unwrap().matches(&text, &b).unwrap());
        assert!(!Like::compile(&format!("%{}b", "a".repeat(3000)), None).unwrap().matches(&text, &b).unwrap());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn the_time_limit_stops_the_one_slow_shape_that_is_left() {
        // `_` segments cost length x pattern / 64: a 100,000-character segment over 4 MB would take minutes
        let text = "a".repeat(4_000_000);
        let pat = format!("%{}b%", "_".repeat(100_000));
        let b = Budget::new(Duration::from_millis(100), CancelToken::never());
        let t = Instant::now();
        let r = Like::compile(&pat, None).unwrap().matches(&text, &b);
        assert_eq!(r.unwrap_err().name, "QueryTimeout");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
}
