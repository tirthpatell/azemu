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
        ("SELECT 0x", "0x", 7, 9),
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
        // the end of the text is `SC1002`, as on Azure
        let want = if near.is_empty() {
            (ErrorKind::Syntax, "SC1002".to_string(), "Syntax error, unexpected end-of-file.".to_string(), s, end)
        } else {
            (ErrorKind::Syntax, "SC1001".to_string(), format!("Syntax error, incorrect syntax near '{near}'."), s, end)
        };
        if got != want {
            bad.push(format!("{q:?}\n   want {want:?}\n   got  {got:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Codes of the tokens the language cannot read (a real account answers these, November 2026).
#[test]
fn token_errors() {
    for (q, code, msg, s, end) in [
        ("SELECT 'abc", "SC1012", "Syntax error, invalid string literal token ''abc'.", 7, 11),
        ("SELECT \"abc", "SC1012", "Syntax error, invalid string literal token '\"abc'.", 7, 11),
        ("SELECT 1 ; ", "SC1010", "Syntax error, invalid token ';'.", 9, 10),
        ("SELECT 1 # x", "SC1010", "Syntax error, invalid token '#'.", 9, 10),
        ("SELECT @", "SC1010", "Syntax error, invalid token '@'.", 7, 8),
        ("SELECT @1x", "SC1010", "Syntax error, invalid token '@'.", 7, 8),
        ("SELECT 1e", "SC1011", "Syntax error, invalid numeric value token '1e'.", 7, 9),
        ("SELECT 1e400", "SC1011", "Syntax error, invalid numeric value token '1e400'.", 7, 12),
        ("SELECT *", "SC1021", "Syntax error, 'SELECT *' is not valid if FROM clause is omitted.", 7, 8),
    ] {
        let e = err(q);
        assert_eq!((e.code.as_str(), e.message.as_str(), e.start, e.end), (code, msg, s, end), "{q}");
    }
    // a comment runs to the end of the line; an unknown escape stands for the character
    assert_eq!(run("SELECT VALUE 1 -- one\n + 2", &[]), vec![json!(3)]);
    assert_eq!(run("SELECT VALUE 5--3", &[]), vec![json!(5)]);
    assert_eq!(run("SELECT VALUE 'a\\qb'", &[]), vec![json!("aqb")]);
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
        ("SELECT * FROM c WHERE SUM(c.a) > 1", "SC2104"),
        ("SELECT VALUE SUM(COUNT(1)) FROM c", "SC2101"),
        ("SELECT c.a FROM c GROUP BY COUNT(1)", "SC2111"),
        ("SELECT * FROM c GROUP BY c.a", "SC2041"),
        ("SELECT * FROM c, d", "SC1001"),
        ("SELECT TOP 1 * FROM c OFFSET 1 LIMIT 1", "SC2205"),
        ("SELECT * FROM c JOIN d IN c.a", "SC2042"),
        ("SELECT VALUE c FROM c JOIN c IN c.a", "SC2021"),
        ("SELECT x.y", "SC2001"),
        ("SELECT c.id, c.id FROM c", "SC2022"),
        ("SELECT c.id, COUNT(1) FROM c", "SC2102"),
        ("SELECT UPPER(c.a, c.a) FROM c", "SC2050"),
        ("SELECT c.a FROM c WHERE c.a LIKE 'x' ESCAPE '!!'", "SC2207"),
        ("SELECT c.a, (SELECT VALUE x FROM x IN c.arr) FROM c", "SC2201"),
        ("SELECT c.select FROM c", "SC1001"),
        ("SELECT VALUE c.arr[1 + 1] FROM c", "SC2990"),
    ] {
        let e = err(q);
        assert_eq!(e.code, code, "{q}: {e}");
    }
    let e = err("SELECT * FROM c WHERE SUM(c.a) > 1");
    assert_eq!((e.start, e.end), (22, 30));
    // Azure lists every problem it finds: the second source repeats `c`, and `SELECT *` needs one source
    let e = err("SELECT * FROM c JOIN c IN c.arr");
    assert_eq!((e.code.as_str(), e.start, e.end), ("SC2021", 21, 22));
    assert_eq!((e.more[0].code.as_str(), e.more[0].start, e.more[0].end), ("SC2042", 7, 8));
    // eleven JOINs, counted over the whole query
    let sql = format!("SELECT 1 FROM c {}", (0..11).map(|i| format!("JOIN j{i} IN c.a")).collect::<Vec<_>>().join(" "));
    let e = err(&sql);
    assert_eq!((e.code.as_str(), e.start, e.end), ("SC3002", 0, sql.chars().count()));
    // no more than ten
    let ok = format!("SELECT 1 FROM c {}", (0..10).map(|i| format!("JOIN j{i} IN c.a")).collect::<Vec<_>>().join(" "));
    assert!(parse(&ok).is_ok());
}

#[test]
fn evaluation_errors() {
    // a parameter nobody supplied is undefined, not an error
    assert_eq!(run("SELECT VALUE @a", &[]), Vec::<Value>::new());
    // a count that is not a whole number from 0 up is an error on the text of the count
    for (q, code, start) in [
        ("SELECT TOP @n VALUE 1", "SC2060", 11),
        ("SELECT VALUE 1 OFFSET @n LIMIT 1", "SC2062", 22),
        ("SELECT VALUE 1 OFFSET 0 LIMIT @n", "SC2061", 30),
    ] {
        for v in [json!(-1), json!(1.5), json!("a"), json!(null)] {
            let p = Params::from_cosmos(&json!([{"name": "@n", "value": v}]));
            let e = parse(q).unwrap().execute(&[] as &[Value], &p, &EvalOptions::default()).unwrap_err();
            assert_eq!((e.code.as_str(), e.start), (code, start), "{q} with {v}");
        }
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
