//! `LIKE` pattern matching: `%`, `_`, `[abc]`, `[a-f]`, `[^abc]` and `ESCAPE`.

#[derive(Debug)]
enum Item {
    Any,
    One,
    Lit(char),
    Set { negate: bool, ranges: Vec<(char, char)> },
}

fn compile(pattern: &str, escape: Option<char>) -> Vec<Item> {
    let p: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        if Some(c) == escape && i + 1 < p.len() {
            out.push(Item::Lit(p[i + 1]));
            i += 2;
            continue;
        }
        match c {
            '%' => out.push(Item::Any),
            '_' => out.push(Item::One),
            '[' => {
                // Find the closing ']' (a ']' right after '[' or '[^' is a member).
                let mut j = i + 1;
                let negate = j < p.len() && p[j] == '^';
                if negate {
                    j += 1;
                }
                let set_start = j;
                let mut close = None;
                let mut k = j;
                while k < p.len() {
                    if p[k] == ']' && k > set_start {
                        close = Some(k);
                        break;
                    }
                    k += 1;
                }
                match close {
                    None => out.push(Item::Lit('[')),
                    Some(end) => {
                        let members = &p[set_start..end];
                        let mut ranges = Vec::new();
                        let mut m = 0;
                        while m < members.len() {
                            if m + 2 < members.len() && members[m + 1] == '-' {
                                ranges.push((members[m], members[m + 2]));
                                m += 3;
                            } else {
                                ranges.push((members[m], members[m]));
                                m += 1;
                            }
                        }
                        out.push(Item::Set { negate, ranges });
                        i = end;
                    }
                }
            }
            other => out.push(Item::Lit(other)),
        }
        i += 1;
    }
    out
}

fn item_matches(item: &Item, c: char) -> bool {
    match item {
        Item::One => true,
        Item::Lit(l) => *l == c,
        Item::Set { negate, ranges } => ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi) != *negate,
        Item::Any => false,
    }
}

/// Matches the whole `text` against `pattern`.
pub(crate) fn like(text: &str, pattern: &str, escape: Option<char>) -> bool {
    let items = compile(pattern, escape);
    let t: Vec<char> = text.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < items.len() && matches!(items[pi], Item::Any) {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < items.len() && item_matches(&items[pi], t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < items.len() && matches!(items[pi], Item::Any) {
        pi += 1;
    }
    pi == items.len()
}
