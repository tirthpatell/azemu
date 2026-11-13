//! Error values and positions, depth limits, and fuzzing: the parser and evaluator must never panic.

mod common;
use azemu_cosmos_sql::{parse, ErrorKind, EvalOptions, Params};
use common::*;
use serde_json::{json, Value};

#[test]
fn syntax_error_positions() {
    let mut bad: Vec<String> = Vec::new();
    // (query, near-text, start, end)
    for (q, near, s, end) in [
        ("SELEC 1", "SELEC", 0, 5),
        ("SELECT", "", 6, 6),
        ("SELECT * FORM c", "FORM", 9, 13),
        ("SELECT * FROM", "", 13, 13),
        ("SELECT * FROM c WHERE", "", 21, 21),
        ("SELECT * FROM c WHERE c.a = ", "", 28, 28),
        ("SELECT 1 +", "", 10, 10),
        ("SELECT (1", "", 9, 9),
        ("SELECT [1, 2", "", 12, 12),
        ("SELECT {a: 1", "", 12, 12),
        ("SELECT {1: 2}", "1", 8, 9),
        ("SELECT 1 2", "2", 9, 10),
        ("SELECT c.id FROM c WHERE c.a == 1", "=", 30, 31),
        ("SELECT c.id FROM c ORDER c.id", "c", 25, 26),
        ("SELECT c.id FROM c GROUP c.id", "c", 25, 26),
        ("SELECT 1 FROM c OFFSET 1", "", 24, 24),
        ("SELECT 'abc", "'abc", 7, 11),
        ("SELECT \"abc", "\"abc", 7, 11),
        ("SELECT 1 ; ", ";", 9, 10),
        ("SELECT 1 # x", "#", 9, 10),
        ("SELECT @", "@", 7, 8),
        ("SELECT 0x", "0x", 7, 9),
        ("SELECT 'a\\q'", "\\q", 9, 11),
        ("SELECT 'a\\u12'", "\\u", 9, 11),
        ("SELECT VALUE", "", 12, 12),
        ("SELECT c.id AS FROM c", "FROM", 15, 19),
        ("SELECT * FROM c JOIN", "", 20, 20),
        ("SELECT * FROM c WHERE c.a IN 1", "1", 29, 30),
        ("SELECT * FROM c WHERE c.a BETWEEN 1", "", 35, 35),
        ("SELECT * FROM c WHERE c.a LIKE", "", 30, 30),
        ("SELECT 1 ? 2", "", 12, 12),
        ("SELECT )", ")", 7, 8),
        ("", "", 0, 0),
        ("   ", "", 3, 3),
        ("FROM c", "FROM", 0, 4),
        ("SELECT * FROM c WHERE SELECT", "SELECT", 22, 28),
        ("SELECT * FROM 5", "5", 14, 15),
        ("SELECT * FROM c.5", "5", 16, 17),
        ("SELECT TOP FROM c", "FROM", 11, 15),
        ("SELECT * FROM (SELECT 1", "", 23, 23),
        ("SELECT EXISTS(1)", "1", 14, 15),
    ] {
        let e = err(q);
        let got = (e.kind, e.code.clone(), e.message.clone(), e.start, e.end);
        let want = (ErrorKind::Syntax, "SC1001".to_string(), format!("Syntax error, incorrect syntax near '{near}'."), s, end);
        if got != want {
            bad.push(format!("{q:?}\n   want {want:?}\n   got  {got:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn error_json_shape() {
    let e = err("SELECT * FORM c");
    assert_eq!(
        e.to_cosmos_json(),
        json!({"errors": [{"severity": "Error", "location": {"start": 9, "end": 13}, "code": "SC1001", "message": "Syntax error, incorrect syntax near 'FORM'."}]})
    );
    assert_eq!(e.to_string(), "SC1001 (9..13): Syntax error, incorrect syntax near 'FORM'.");
}

#[test]
fn positions_are_in_characters() {
    // the é and 😀 before the error each count as one character
    let e = err("SELECT 'é😀' FORM c");
    assert_eq!((e.start, e.end), (17, 18));
    let e = err("SELECT c.é FROM c WHERE c.é == 1");
    assert_eq!(e.start, 29);
}

#[test]
fn semantic_errors() {
    for (q, code) in [
        ("SELECT * FROM c WHERE SUM(c.a) > 1", "SC2102"),
        ("SELECT VALUE SUM(COUNT(1)) FROM c", "SC2104"),
        ("SELECT c.a FROM c GROUP BY COUNT(1)", "SC2102"),
        ("SELECT * FROM c GROUP BY c.a", "SC2103"),
        ("SELECT * FROM c, d", "SC1001"),
        ("SELECT TOP 1 * FROM c OFFSET 1 LIMIT 1", "SC1001"),
        ("SELECT * FROM c JOIN d IN c.a", "SC2103"),
        ("SELECT VALUE c FROM c JOIN c IN c.a", "SC2002"),
        ("SELECT x.y", "SC2001"),
        ("SELECT VALUE COUNT(1) FROM c OFFSET COUNT(1) LIMIT 1", "SC2102"),
    ] {
        let e = err(q);
        assert_eq!(e.code, code, "{q}: {e}");
    }
    let e = err("SELECT * FROM c WHERE SUM(c.a) > 1");
    assert_eq!((e.start, e.end), (22, 30));
}

#[test]
fn evaluation_errors() {
    for q in [
        "SELECT VALUE @a",
        "SELECT TOP -1 VALUE 1",
        "SELECT VALUE 1 OFFSET 1.5 LIMIT 1",
        "SELECT VALUE 1 OFFSET -1 LIMIT 1",
        "SELECT TOP 'a' VALUE 1",
    ] {
        let e = err(q);
        assert_eq!(e.kind, ErrorKind::Evaluation, "{q}: {e}");
    }
}

#[test]
fn nesting_limits() {
    let n = 5000;
    let cases = [
        format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n)),
        format!("SELECT {}1", "-".repeat(n)),
        format!("SELECT {}1", "NOT ".repeat(n)),
        format!("SELECT {}1{}", "[".repeat(n), "]".repeat(n)),
        format!("SELECT {}1{}", "{a:".repeat(n), "}".repeat(n)),
        format!("SELECT 1{}", " + 1".repeat(n)),
        format!("SELECT {}1", "ABS(".repeat(n)),
        format!("SELECT {}1{}", "(SELECT VALUE ".repeat(n), ")".repeat(n)),
        format!("SELECT * FROM c WHERE {}true{}", "EXISTS(SELECT VALUE ".repeat(n), ")".repeat(n)),
        format!("SELECT {}c{}", "ARRAY(SELECT VALUE ".repeat(n), ")".repeat(n)),
        format!("SELECT VALUE c{}", ".a".repeat(n)),
        format!("SELECT VALUE c{}", "[0]".repeat(n)),
        format!("SELECT VALUE {}1", "~".repeat(n)),
        format!("SELECT VALUE 1{}", " ? 1 : 2".repeat(n)),
        format!("SELECT VALUE {}1{}", "(".repeat(n / 2), ")".repeat(n / 2)),
    ];
    for q in &cases {
        // long flat chains are fine; deep nesting is rejected without a stack overflow
        match parse(q) {
            Ok(_) | Err(_) => {}
        }
    }
    let e = parse(&cases[0]).unwrap_err();
    assert_eq!(e.kind, ErrorKind::LimitExceeded);
    // moderately deep queries still work
    let ok = format!("SELECT VALUE {}1{}", "(".repeat(20), ")".repeat(20));
    assert_eq!(run(&ok, &[]), vec![json!(1)]);
    let ok = format!("SELECT VALUE {}", "[".repeat(20) + "1" + &"]".repeat(20));
    assert_eq!(run(&ok, &[]).len(), 1);
    // a long flat chain executes without overflow
    let flat = format!("SELECT VALUE 1{}", " + 1".repeat(90));
    assert_eq!(run(&flat, &[]), vec![json!(91)]);
    let flat_or = format!("SELECT VALUE false{}", " OR false".repeat(2000));
    assert_eq!(run(&flat_or, &[]), vec![json!(false)]);
    let path = format!("SELECT VALUE c{} FROM c", ".a".repeat(80));
    assert!(parse(&path).is_ok());
}

#[test]
fn join_row_budget() {
    // 100 x 100 x 100 rows exceeds a small budget; the default budget is generous
    let d = vec![json!({"a": (0..100).collect::<Vec<_>>()})];
    let q = parse("SELECT VALUE COUNT(1) FROM c JOIN x IN c.a JOIN y IN c.a JOIN z IN c.a").unwrap();
    let opts = EvalOptions { max_rows: 10_000, ..fixed_clock_opts() };
    let e = q.execute(&d, &Params::new(), &opts).unwrap_err();
    assert_eq!(e.kind, ErrorKind::LimitExceeded);
    assert_eq!(q.execute(&d, &Params::new(), &fixed_clock_opts()).unwrap(), vec![json!(1_000_000)]);
}

/// Deterministic pseudo random generator (xorshift).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const SEEDS: &[&str] = &[
    "SELECT c.id, COUNT(1) AS n FROM c JOIN t IN c.tags WHERE c.a BETWEEN 1 AND 3 AND c.s LIKE 'a%' ESCAPE '^' GROUP BY c.id ORDER BY c.id DESC OFFSET 1 LIMIT 2",
    "SELECT DISTINCT TOP @n VALUE {\"a\": [c.x ?? 1, c.y ? 2 : 3], b: ARRAY(SELECT VALUE x FROM x IN c.arr)} FROM root AS c WHERE NOT EXISTS (SELECT VALUE 1)",
    "SELECT VALUE (100 >> 2) + (c.a & 1 | 2 ^ ~3) * -4 / 5 % 6 FROM c.items i JOIN (SELECT VALUE t FROM t IN i.tags) AS q",
    "SELECT * FROM c WHERE c.x IN (1, 'a', null) AND CONTAINS(c.s, 'x', true) OR ARRAY_CONTAINS(c.a, {\"k\": 1}, true) AND c.s || 'x' = 'yx'",
];

#[test]
fn fuzz_truncations_and_mutations_never_panic() {
    let docs = vec![json!({"id": 1, "a": [1, 2], "s": "abc", "tags": [1, 2], "x": null, "arr": [1], "items": [{"tags": [1]}]})];
    let params = Params::new();
    let opts = fixed_clock_opts();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut parsed_ok = 0;
    for seed in SEEDS {
        let chars: Vec<char> = seed.chars().collect();
        // every prefix and every suffix
        for i in 0..=chars.len() {
            for q in [chars[..i].iter().collect::<String>(), chars[i..].iter().collect::<String>()] {
                if let Ok(p) = parse(&q) {
                    parsed_ok += 1;
                    let _ = p.execute(&docs, &params, &opts);
                    let _ = p.info(&params);
                    let _ = p.needs_query_plan();
                }
            }
        }
        // random single-character edits
        let alphabet: Vec<char> = "abcXYZ019 .,()[]{}'\"@$_*+-/%<>=!&|^~?:\\é😀\n".chars().collect();
        for _ in 0..1500 {
            let mut c = chars.clone();
            for _ in 0..(1 + rng.next() % 3) {
                let pos = (rng.next() as usize) % c.len().max(1);
                match rng.next() % 3 {
                    0 if !c.is_empty() => {
                        c.remove(pos);
                    }
                    1 => c.insert(pos.min(c.len()), alphabet[(rng.next() as usize) % alphabet.len()]),
                    _ if !c.is_empty() => c[pos] = alphabet[(rng.next() as usize) % alphabet.len()],
                    _ => {}
                }
            }
            let q: String = c.into_iter().collect();
            if let Ok(p) = parse(&q) {
                parsed_ok += 1;
                let _ = p.execute(&docs, &params, &opts);
                let _ = p.info(&params);
                let _ = p.execute_rewritten(&docs, &params, &opts);
            }
        }
    }
    assert!(parsed_ok > 50, "fuzzer produced too few valid queries: {parsed_ok}");
}

#[test]
fn fuzz_random_bytes_never_panic() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    for _ in 0..4000 {
        let len = (rng.next() % 60) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let q = String::from_utf8_lossy(&bytes).to_string();
        let _ = parse(&q);
    }
    // random token soup
    let toks = [
        "SELECT",
        "FROM",
        "WHERE",
        "c",
        "c.a",
        "1",
        "'x'",
        "(",
        ")",
        "[",
        "]",
        "{",
        "}",
        ",",
        ".",
        "AND",
        "OR",
        "NOT",
        "IN",
        "VALUE",
        "TOP",
        "ORDER",
        "BY",
        "GROUP",
        "JOIN",
        "AS",
        "LIKE",
        "ESCAPE",
        "BETWEEN",
        "ARRAY",
        "EXISTS",
        "COUNT",
        "ABS",
        "udf",
        "@p",
        "?",
        ":",
        "??",
        "||",
        "<<",
        ">>>",
        "-",
        "*",
        "undefined",
        "DISTINCT",
        "OFFSET",
        "LIMIT",
    ];
    let docs = vec![json!({"a": 1})];
    for _ in 0..6000 {
        let n = 1 + (rng.next() % 14) as usize;
        let q = (0..n).map(|_| toks[(rng.next() as usize) % toks.len()]).collect::<Vec<_>>().join(" ");
        if let Ok(p) = parse(&q) {
            let mut ps = Params::new();
            ps.insert("p", Value::from(1));
            let _ = p.execute(&docs, &ps, &fixed_clock_opts());
            let _ = p.info(&ps);
        }
    }
}

#[test]
fn hostile_inputs_execute_safely() {
    let d = vec![json!({"s": "a".repeat(5000), "n": 1e308, "i": i64::MAX, "arr": (0..1000).collect::<Vec<_>>()})];
    for q in [
        "SELECT VALUE REPLICATE(c.s, 100) FROM c",
        "SELECT VALUE c.n * c.n FROM c",
        "SELECT VALUE c.i + c.i FROM c",
        "SELECT VALUE -c.i - 2 FROM c",
        "SELECT VALUE c.i * c.i FROM c",
        "SELECT VALUE ABS(-9223372036854775808) FROM c",
        "SELECT VALUE INTMUL(c.i, c.i) FROM c",
        "SELECT VALUE c.i % 0 FROM c",
        "SELECT VALUE -9223372036854775808 / -1",
        "SELECT VALUE -9223372036854775808 % -1",
        "SELECT VALUE INTDIV(-9223372036854775808, -1)",
        "SELECT VALUE INTMOD(-9223372036854775808, -1)",
        "SELECT VALUE ARRAY_SLICE(c.arr, -9223372036854775808, 9223372036854775807) FROM c",
        "SELECT VALUE ARRAY_SLICE(c.arr, 9223372036854775807, 9223372036854775807) FROM c",
        "SELECT VALUE SUBSTRING(c.s, 9223372036854775807, 9223372036854775807) FROM c",
        "SELECT VALUE LEFT(c.s, 1e300) FROM c",
        "SELECT VALUE INDEX_OF(c.s, 'a', 9223372036854775807) FROM c",
        "SELECT VALUE LASTINDEXOF(c.s, 'a', -9223372036854775808) FROM c",
        "SELECT VALUE DATETIMEADD('dd', 9223372036854775807, '2020-01-01') FROM c",
        "SELECT VALUE DATETIMEADD('ns', 9223372036854775807, '2020-01-01') FROM c",
        "SELECT VALUE DATETIMEADD('yyyy', -9223372036854775808, '2020-01-01') FROM c",
        "SELECT VALUE TICKSTODATETIME(9223372036854775807) FROM c",
        "SELECT VALUE TIMESTAMPTODATETIME(9223372036854775807) FROM c",
        "SELECT VALUE DATETIMEDIFF('ns', '0001-01-01', '9999-12-31') FROM c",
        "SELECT VALUE DATETIMEBIN('2020-01-01', 'ns', 9223372036854775807) FROM c",
        "SELECT VALUE DATETIMEBIN('2020-01-01', 'yyyy', 9223372036854775807) FROM c",
        "SELECT VALUE NUMBERBIN(1e308, 1e-308) FROM c",
        "SELECT VALUE ROUND(1e308, 300) FROM c",
        "SELECT VALUE ROUND(5, -9223372036854775808)",
        "SELECT VALUE c.i << 99 FROM c",
        "SELECT VALUE INTBITLEFTSHIFT(1, 9223372036854775807)",
        "SELECT VALUE POWER(c.n, c.n) FROM c",
        "SELECT VALUE REGEXMATCH(c.s, '(a*)*b') FROM c",
        "SELECT VALUE REGEXMATCH(c.s, '(((((((((a{100}){100}){100}){100}){100}){100}){100})))') FROM c",
        "SELECT VALUE CHOOSE(9223372036854775807, 1)",
        "SELECT VALUE DATETIMEFROMPARTS(4294967296, 1, 1)",
        "SELECT VALUE c.arr[9223372036854775807] FROM c",
        "SELECT VALUE c.arr[1e300] FROM c",
        "SELECT VALUE STRINGSPLIT(c.s, 'a') FROM c",
        "SELECT VALUE ARRAY_CONCAT(c.arr, c.arr, c.arr) FROM c",
        "SELECT VALUE c.s LIKE '%a%a%a%a%a%a%a%a%b' FROM c",
        "SELECT VALUE c.s LIKE '[a-z' FROM c",
        "SELECT VALUE c.s LIKE '[]' FROM c",
        "SELECT VALUE c.s LIKE '[^]' FROM c",
        "SELECT VALUE c.s LIKE '^' ESCAPE '^' FROM c",
        "SELECT VALUE SUM(c.i) FROM c JOIN x IN c.arr",
        "SELECT VALUE AVG(c.n) FROM c JOIN x IN c.arr",
        "SELECT VALUE OBJECTTOARRAY({}) FROM c",
        "SELECT TOP 9223372036854775807 VALUE 1 FROM c",
        "SELECT VALUE 1 FROM c OFFSET 9223372036854775807 LIMIT 9223372036854775807",
    ] {
        let _ = try_run(q, &d, &fixed_clock_opts());
    }
    // a few results worth pinning
    assert_eq!(run("SELECT VALUE -9223372036854775808 / -1", &[]), vec![json!(9.223372036854776e18)]);
    assert_eq!(run("SELECT VALUE ROUND(5, 2)", &[]), vec![json!(5)]);
}
