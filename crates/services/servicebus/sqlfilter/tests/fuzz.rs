//! Fuzz-style robustness tests: nothing may panic or hang, whatever the input.
mod common;
use common::*;

/// Small deterministic xorshift PRNG (no dependencies).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const TOKENS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "IN",
    "LIKE",
    "ESCAPE",
    "IS",
    "NULL",
    "EXISTS",
    "TRUE",
    "FALSE",
    "SET",
    "REMOVE",
    "sys.",
    "user.",
    "Label",
    "a",
    "b",
    "price",
    "[x y]",
    "[",
    "]",
    "]]",
    "\"q\"",
    "\"",
    "'",
    "''",
    "'str%'",
    "'_'",
    "(",
    ")",
    ",",
    ";",
    ".",
    "=",
    "==",
    "<>",
    "!=",
    "<",
    ">",
    "<=",
    ">=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "@p",
    "@",
    "1",
    "0",
    "42",
    "1.5",
    "2E3",
    "1e",
    ".5",
    "99999999999999999999",
    "newid()",
    "property(",
    "p(",
    "caf\u{e9}",
    "\u{1F600}",
    "\0",
    " ",
    "\t",
    "\n",
];

fn sample_props() -> MapProperties {
    props().user("a", 1).user("b", "text").user("price", 2.5).user("x y", true).sys("Label", "bus")
}

fn sample_params() -> Params {
    params(&[("p", Value::Long(3))])
}

fn exercise(sql: &str) {
    let ps = sample_props();
    let pm = sample_params();
    if let Ok(f) = Filter::parse(sql) {
        let _ = f.matches(&ps, &pm);
        let _ = f.matches(&props(), &Params::new());
        let _ = f.try_matches(&ps, &pm);
        let _ = f.sql();
    }
    if let Ok(a) = Action::parse(sql) {
        let mut mp = sample_props();
        a.apply(&mut mp, &pm);
        let mut empty = props();
        let _ = a.try_apply(&mut empty, &Params::new());
    }
}

#[test]
fn random_bytes() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..4000 {
        let len = rng.below(80);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        exercise(&String::from_utf8_lossy(&bytes));
    }
}

#[test]
fn random_printable_ascii() {
    let mut rng = Rng(12345);
    for _ in 0..4000 {
        let len = rng.below(60);
        let s: String = (0..len).map(|_| (32 + rng.below(95)) as u8 as char).collect();
        exercise(&s);
    }
}

#[test]
fn random_token_soup() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..6000 {
        let n = 1 + rng.below(24);
        let s: Vec<&str> = (0..n).map(|_| TOKENS[rng.below(TOKENS.len())]).collect();
        exercise(&s.join(if rng.below(2) == 0 { " " } else { "" }));
    }
}

const VALID_FILTERS: &[&str] = &[
    "sys.Label LIKE '%bus%'",
    "user.price > 10 AND user.color = 'red'",
    "sys.To IN ('Ireland','Italy') OR NOT (a = 1)",
    "EXISTS(user.x) AND price * 2 + 1 > -100 / 3 % 7",
    "[Order Type] = 'it''s' AND s LIKE '%!%%' ESCAPE '!'",
    "a = @p OR p('b') IS NOT NULL AND newid() <> 'x'",
    "(a + 1.5E3) * (b - .5) >= 0.5E-2",
];

#[test]
fn truncated_valid_filters() {
    for sql in VALID_FILTERS {
        assert!(Filter::parse(sql).is_ok(), "{sql}");
        let chars: Vec<char> = sql.chars().collect();
        for end in 0..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            exercise(&prefix);
        }
        for start in 0..chars.len() {
            let suffix: String = chars[start..].iter().collect();
            exercise(&suffix);
        }
    }
}

const VALID_ACTIONS: &[&str] = &[
    "SET quantity = quantity / 2; REMOVE priority;SET sys.CorrelationId = 'low';",
    "SET a = 1, b = a + 2, [c d] = 'x''y' REMOVE e",
    "SET t = p('b') + newid(); SET u = -(a % 3) * @p",
];

#[test]
fn truncated_valid_actions() {
    for sql in VALID_ACTIONS {
        assert!(Action::parse(sql).is_ok(), "{sql}");
        let chars: Vec<char> = sql.chars().collect();
        for end in 0..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            exercise(&prefix);
        }
    }
}

#[test]
fn mutated_valid_filters() {
    let mut rng = Rng(777);
    for sql in VALID_FILTERS {
        for _ in 0..400 {
            let mut chars: Vec<char> = sql.chars().collect();
            for _ in 0..1 + rng.below(3) {
                if chars.is_empty() {
                    break;
                }
                let i = rng.below(chars.len());
                match rng.below(3) {
                    0 => {
                        chars.remove(i);
                    }
                    1 => chars[i] = TOKENS[rng.below(TOKENS.len())].chars().next().unwrap_or('x'),
                    _ => chars.insert(i, TOKENS[rng.below(TOKENS.len())].chars().next().unwrap_or('x')),
                }
            }
            exercise(&chars.iter().collect::<String>());
        }
    }
}

#[test]
fn hostile_nesting_and_size() {
    exercise(&"(".repeat(5000));
    exercise(&")".repeat(5000));
    exercise(&"NOT ".repeat(5000));
    exercise(&"-".repeat(5000));
    exercise(&format!("{}1{}", "(".repeat(100), ")".repeat(100)));
    exercise(&format!("{}a = 1{}", "(".repeat(101), ")".repeat(101)));
    exercise(&"'".repeat(2000));
    exercise(&"[".repeat(2000));
    exercise(&"a".repeat(100_000));
    exercise(&"SET a = 1;".repeat(100));
    exercise(&"\u{1F600}".repeat(1024));
}

#[test]
fn pathological_like_patterns_do_not_hang() {
    // Classic exponential-backtracking shape for naive matchers.
    let text = "a".repeat(500);
    let pattern = format!("{}b", "%a".repeat(100));
    let p = props().user("s", text.as_str()).user("p", pattern.as_str());
    let f = Filter::parse("s LIKE p").unwrap();
    assert!(!f.matches(&p, &Params::new()));
    let g = Filter::parse("s LIKE '%a%a%a%a%a%a%a%a%a%a%a%a%a%a%a%a%b'").unwrap();
    assert!(!g.matches(&p, &Params::new()));
}

#[test]
fn arithmetic_extremes_do_not_panic() {
    for sql in [
        "-9223372036854775808 / -1 = 0",
        "-9223372036854775808 % -1 = 0",
        "-(-9223372036854775808) < 0",
        "9223372036854775807 * 9223372036854775807 = 1",
        "1.0E308 * 10 > 0",
        "0 / 0.0 = 0",
        "1 % 0 = 0",
    ] {
        exercise(sql);
    }
}
