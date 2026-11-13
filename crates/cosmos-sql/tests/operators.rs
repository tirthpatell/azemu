// Some code samples and query examples in this file are reproduced from Microsoft Learn and from Microsoft's SDKs (MIT licence, Copyright Microsoft); see NOTICE and LICENSES/MIT-Microsoft.txt.
//! Operators, constants, undefined propagation. Truth tables and the equality table come from
//! learn.microsoft.com/azure/cosmos-db/nosql/query/operators.

mod common;
use common::*;
use serde_json::{json, Value};

/// `SELECT VALUE {"r": <expr>}`: an undefined result gives `{}`.
fn eval1(expr: &str) -> Value {
    let out = run(&format!("SELECT VALUE {{\"r\": ({expr})}}"), &[]);
    assert_eq!(out.len(), 1, "{expr}");
    out[0].get("r").cloned().unwrap_or_else(|| json!("<undefined>"))
}

fn u() -> Value {
    json!("<undefined>")
}

#[test]
fn equality_table_from_docs() {
    // One sample value per type; pairs of the same type compare, everything else is undefined.
    let samples = [("undefined", 0), ("null", 1), ("true", 2), ("5", 3), ("'s'", 4), ("{\"a\": 1}", 5), ("[1, 2]", 6)];
    for (a, ta) in samples {
        for (b, tb) in samples {
            let got = eval1(&format!("{a} = {b}"));
            let expect = if ta == tb && ta != 0 { json!(true) } else { u() };
            assert_eq!(got, expect, "{a} = {b}");
        }
    }
}

#[test]
fn comparison_operators_by_type() {
    let t = json!(true);
    let f = json!(false);
    let cases: Vec<(&str, Value)> = vec![
        ("1 < 2", t.clone()),
        ("2 < 1", f.clone()),
        ("1 <= 1", t.clone()),
        ("1 >= 2", f.clone()),
        ("2 > 1", t.clone()),
        ("1 != 2", t.clone()),
        ("1 <> 2", t.clone()),
        ("1 <> 1", f.clone()),
        ("1 != 1", f.clone()),
        ("1 = 1.0", t.clone()),
        ("0.1 + 0.2 = 0.3", f.clone()),
        ("'a' < 'b'", t.clone()),
        ("'b' < 'a'", f.clone()),
        ("'a' < 'B'", f.clone()),
        ("'' < 'a'", t.clone()),
        ("'abc' = 'abc'", t.clone()),
        ("'abc' = 'ABC'", f.clone()),
        ("'a' < 'ab'", t.clone()),
        ("false < true", t.clone()),
        ("true > false", t.clone()),
        ("true = true", t.clone()),
        ("true != false", t.clone()),
        ("null = null", t.clone()),
        ("null != null", f.clone()),
        ("null <= null", t.clone()),
        ("null < null", f.clone()),
        // cross-type is undefined for every operator
        ("1 < 'a'", u()),
        ("1 = 'a'", u()),
        ("1 != 'a'", u()),
        ("true < 1", u()),
        ("null < 1", u()),
        ("null = 0", u()),
        ("null = false", u()),
        ("'1' = 1", u()),
        ("0 = '0'", u()),
        ("0 = \"true\"", u()),
        ("[] = 0", u()),
        ("{} = null", u()),
        // undefined on either side
        ("undefined = undefined", u()),
        ("undefined != 1", u()),
        ("1 < undefined", u()),
        ("undefined = null", u()),
        // arrays and objects: equality only
        ("[1,2] = [1,2]", t.clone()),
        ("[1,2] = [2,1]", f.clone()),
        ("[1,2] = [1,2,3]", f.clone()),
        ("{\"a\":1,\"b\":2} = {\"b\":2,\"a\":1}", t.clone()),
        ("{\"a\":1} = {\"a\":2}", f.clone()),
        ("[1] < [2]", u()),
        ("[1] != [2]", u()),
        ("{} < {}", u()),
        ("[[1]] = [[1]]", t.clone()),
        ("[1] = [1.0]", t.clone()),
        ("{\"a\":[1,{\"b\":null}]} = {\"a\":[1,{\"b\":null}]}", t.clone()),
        // NaN and Infinity
        ("NaN = NaN", f.clone()),
        ("NaN != NaN", t.clone()),
        ("NaN < 1", f.clone()),
        ("Infinity > 1", t.clone()),
        ("-Infinity < 1", t.clone()),
        ("Infinity = Infinity", t.clone()),
    ];
    for (e, expect) in cases {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn logical_truth_tables() {
    // OR / AND / NOT tables of the operators page.
    let vals = [("true", Some(true)), ("false", Some(false)), ("undefined", None)];
    for (a, va) in vals {
        for (b, vb) in vals {
            let or = match (va, vb) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            };
            let and = match (va, vb) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            let to = |o: Option<bool>| o.map(Value::Bool).unwrap_or_else(u);
            assert_eq!(eval1(&format!("{a} OR {b}")), to(or), "{a} OR {b}");
            assert_eq!(eval1(&format!("{a} AND {b}")), to(and), "{a} AND {b}");
        }
        let not = va.map(|v| Value::Bool(!v)).unwrap_or_else(u);
        assert_eq!(eval1(&format!("NOT {a}")), not, "NOT {a}");
    }
    // non-boolean operands count as undefined
    for (e, expect) in [
        ("1 AND true", u()),
        ("1 AND false", json!(false)),
        ("'a' OR true", json!(true)),
        ("'a' OR false", u()),
        ("NOT 1", u()),
        ("NOT null", u()),
        ("NOT 'x'", u()),
        ("null AND true", u()),
        ("[] OR false", u()),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn logical_precedence() {
    for (e, expect) in [
        ("true OR false AND false", json!(true)), // AND binds tighter than OR
        ("(true OR false) AND false", json!(false)),
        ("NOT true OR true", json!(true)), // NOT binds tighter than OR
        ("NOT true AND false OR true", json!(true)),
        ("NOT 1 = 2", json!(true)), // NOT applies to the comparison
        ("NOT (1 = 1)", json!(false)),
        ("1 = 1 AND 2 = 2", json!(true)),
        ("1 < 2 AND 2 < 3 AND 3 < 4", json!(true)),
        ("false OR false OR true", json!(true)),
        ("1 + 1 = 2 AND 2 * 2 = 4", json!(true)),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
    // AND does not evaluate past a false operand (no error from the missing parameter)
    assert_eq!(run("SELECT VALUE false AND @missing_param = 1", &[]).len(), 1);
}

#[test]
fn arithmetic() {
    for (e, expect) in [
        ("1 + 2 * 3", json!(7)),
        ("(1 + 2) * 3", json!(9)),
        ("10 / 4", json!(2.5)),
        ("10 / 5", json!(2)),
        ("7 % 3", json!(1)),
        ("-7 % 3", json!(-1)),
        ("7 % -3", json!(1)),
        ("5.5 % 2", json!(1.5)),
        ("2 - 5", json!(-3)),
        ("-(5)", json!(-5)),
        ("- -5", json!(5)),
        ("--5", json!(5)),
        ("+5", json!(5)),
        ("1.5 + 1.5", json!(3)),
        ("2 * 0.5", json!(1)),
        ("0.1 + 0.2", json!(0.30000000000000004)),
        ("1 / 3", json!(0.3333333333333333)),
        ("((2 + 11 % 7) - 2) / 2", json!(2)),
        ("9223372036854775807 + 1", json!(9.223372036854776e18)),
        ("9223372036854775807 - 1", json!(9223372036854775806i64)),
        ("3037000500 * 3037000500", json!(9_223_372_037_000_250_000u64 as f64)),
        ("-9223372036854775808", json!(-9223372036854775808i64)),
        ("1e3", json!(1000)),
        ("1.5e-3", json!(0.0015)),
        ("2E2", json!(200)),
        ("1.0", json!(1)),
        ("100000000000000000000", json!(1e20)),
        ("12345678901234567890", json!(1.2345678901234567e19)),
        ("0x1F", json!(31)),
        ("0xff", json!(255)),
        ("0XA", json!(10)),
        // type errors give undefined
        ("'a' + 1", u()),
        ("1 + 'a'", u()),
        ("null + 1", u()),
        ("true + 1", u()),
        ("[1] + 1", u()),
        ("{} * 2", u()),
        ("1 + undefined", u()),
        ("undefined - 1", u()),
        ("-'a'", u()),
        ("-null", u()),
        ("+'a'", u()),
        ("-true", u()),
        ("'a' - 'b'", u()),
        ("'6' / 2", u()),
        // IEEE results
        ("1 / 0", Value::Null),
        ("0 / 0", Value::Null),
        ("-1 / 0", Value::Null),
        ("5 % 0", Value::Null),
        ("IS_FINITE_NUMBER(1 / 0)", json!(false)),
        ("IS_NUMBER(1 / 0)", json!(true)),
        ("1 / 0 > 5", json!(true)),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn bitwise_operators() {
    // operators page example
    assert_eq!(
        run("SELECT (100 >> 2) AS rightShift, (100 << 2) AS leftShift, (100 >>> 0) AS zeroFillRightShift, (100 & 1000) AS logicalAnd, (100 | 1000) AS logicalOr, (100 ^ 1000) AS logicalExclusiveOr", &[]),
        vec![json!({"rightShift": 25, "leftShift": 400, "zeroFillRightShift": 100, "logicalAnd": 96, "logicalOr": 1004, "logicalExclusiveOr": 908})]
    );
    for (e, expect) in [
        ("~5", json!(-6)),
        ("~0", json!(-1)),
        ("~-1", json!(0)),
        ("1 << 31", json!(-2147483648i64)),
        ("1 << 32", json!(1)),
        ("1 << 33", json!(2)),
        ("-1 >>> 0", json!(4294967295u64)),
        ("-8 >> 1", json!(-4)),
        ("-8 >>> 28", json!(15)),
        ("5 & 3", json!(1)),
        ("5 | 3", json!(7)),
        ("5 ^ 3", json!(6)),
        ("2147483648 | 0", json!(-2147483648i64)),
        ("4294967296 | 0", json!(0)),
        ("1.9 | 0", json!(1)),
        ("-1.9 | 0", json!(-1)),
        ("5 & 1 = 1", json!(true)),
        ("1 | 2 ^ 3 & 4", json!(3)),
        ("1 + 2 << 1", json!(6)),
        ("'a' & 1", u()),
        ("5 & null", u()),
        ("~'a'", u()),
        ("1 << true", u()),
        ("undefined | 1", u()),
        ("NaN | 1", json!(1)),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn string_concat_operator() {
    for (e, expect) in [
        ("'a' || 'b'", json!("ab")),
        ("'a' || 'b' || 'c'", json!("abc")),
        ("'' || ''", json!("")),
        ("'a' || 1", u()),
        ("1 || 'a'", u()),
        ("'a' || undefined", u()),
        ("'a' || null", u()),
        ("'x' || 'y' = 'xy'", json!(true)),
        ("'é' || 'ü'", json!("éü")),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn ternary_and_coalesce() {
    for (e, expect) in [
        ("true ? 1 : 2", json!(1)),
        ("false ? 1 : 2", json!(2)),
        ("undefined ? 1 : 2", u()),
        ("1 ? 1 : 2", json!(2)),
        ("null ? 1 : 2", json!(2)),
        ("true ? undefined : 2", u()),
        ("false ? 1 : undefined", u()),
        ("false ? 1 : false ? 2 : 3", json!(3)),
        ("false ? 1 : true ? 2 : 3", json!(2)),
        ("true ? false ? 1 : 2 : 3", json!(2)),
        ("1 < 2 ? 'y' : 'n'", json!("y")),
        ("true AND false ? 1 : 2", json!(2)),
        ("undefined ?? 5", json!(5)),
        ("null ?? 5", Value::Null),
        ("1 ?? 5", json!(1)),
        ("false ?? 5", json!(false)),
        ("undefined ?? undefined ?? 3", json!(3)),
        ("undefined ?? null ?? 3", Value::Null),
        ("1 ?? undefined", json!(1)),
        ("undefined ?? undefined", u()),
        ("undefined ?? 1 + 1", json!(2)),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
    // operators page: ternary with pricing data
    let d = vec![
        json!({"name": "Stangincy trekking poles", "price": 24.50, "onCloseout": false, "onSale": true, "collapsible": true}),
        json!({"name": "Vimero hiking poles", "price": 24.50, "onCloseout": false, "onSale": false}),
        json!({"name": "Kramundsen trekking poles", "price": 24.50, "onCloseout": true, "onSale": true, "collapsible": false}),
    ];
    check_table(&d, &[
        ("SELECT p.name, p.price AS subtotal, p.onSale ? (p.price * 0.85) : p.price AS total FROM products p",
         r#"[{"name":"Stangincy trekking poles","subtotal":24.5,"total":20.825},{"name":"Vimero hiking poles","subtotal":24.5,"total":24.5},{"name":"Kramundsen trekking poles","subtotal":24.5,"total":20.825}]"#),
        ("SELECT p.name, p.price AS subtotal, p.onCloseout ? (p.price * 0.55) : p.onSale ? (p.price * 0.85) : p.price AS total FROM products p",
         r#"[{"name":"Stangincy trekking poles","subtotal":24.5,"total":20.825},{"name":"Vimero hiking poles","subtotal":24.5,"total":24.5},{"name":"Kramundsen trekking poles","subtotal":24.5,"total":13.475000000000001}]"#),
        ("SELECT p.name, p.collapsible ?? false AS isCollapsible FROM products p",
         r#"[{"name":"Stangincy trekking poles","isCollapsible":true},{"name":"Vimero hiking poles","isCollapsible":false},{"name":"Kramundsen trekking poles","isCollapsible":false}]"#),
        // property missing: undefined condition drops the property
        ("SELECT p.name, p.collapsible ? 'yes' : 'no' AS c FROM products p", r#"[{"name":"Stangincy trekking poles","c":"yes"},{"name":"Vimero hiking poles"},{"name":"Kramundsen trekking poles","c":"no"}]"#),
    ]);
}

#[test]
fn in_between() {
    for (e, expect) in [
        ("2 IN (1, 2, 3)", json!(true)),
        ("4 IN (1, 2, 3)", json!(false)),
        ("'a' IN (1, 'a')", json!(true)),
        ("1 IN ('1')", json!(false)),
        ("undefined IN (1)", u()),
        ("null IN (null)", json!(true)),
        ("null IN (0)", json!(false)),
        ("[1] IN ([1])", json!(true)),
        ("{\"a\": 1} IN ({\"a\": 1})", json!(true)),
        ("1 IN (undefined, 1)", json!(true)),
        ("1 IN (undefined)", json!(false)),
        ("2 NOT IN (1, 2, 3)", json!(false)),
        ("4 NOT IN (1, 2, 3)", json!(true)),
        ("undefined NOT IN (1)", u()),
        ("1 IN (1 + 0, 5)", json!(true)),
        ("true IN (true)", json!(true)),
        ("1.0 IN (1)", json!(true)),
        ("'a' IN ('A')", json!(false)),
        ("5 BETWEEN 1 AND 10", json!(true)),
        ("5 BETWEEN 5 AND 5", json!(true)),
        ("0 BETWEEN 1 AND 10", json!(false)),
        ("10 BETWEEN 1 AND 10", json!(true)),
        ("11 BETWEEN 1 AND 10", json!(false)),
        ("'b' BETWEEN 'a' AND 'c'", json!(true)),
        ("'d' BETWEEN 'a' AND 'c'", json!(false)),
        ("5 BETWEEN 10 AND 1", json!(false)),
        ("5 NOT BETWEEN 1 AND 10", json!(false)),
        ("50 NOT BETWEEN 1 AND 10", json!(true)),
        ("5 BETWEEN 'a' AND 10", u()),
        ("15 BETWEEN 'a' AND 10", json!(false)),
        ("undefined BETWEEN 1 AND 2", u()),
        ("null BETWEEN 1 AND 2", u()),
        ("5 BETWEEN 1 AND 10 AND true", json!(true)),
        ("5 BETWEEN 1 + 1 AND 2 * 5", json!(true)),
        ("true BETWEEN false AND true", json!(true)),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
    // between page
    let hats = vec![
        json!({"name": "Minet Hat", "price": 50, "detailCategory": "apparel-accessories-beanies"}),
        json!({"name": "Klinto Hat", "price": 65, "detailCategory": "apparel-accessories-beanies"}),
        json!({"name": "Benki Hat", "price": 25, "detailCategory": "apparel-accessories-beanies"}),
        json!({"name": "Jontra Hat", "price": 40, "detailCategory": "apparel-accessories-beanies"}),
    ];
    check_table(
        &hats,
        &[
            (
                r#"SELECT VALUE p.name FROM products p WHERE (p.price BETWEEN 0 AND 40) AND p.detailCategory = "apparel-accessories-beanies""#,
                r#"["Benki Hat","Jontra Hat"]"#,
            ),
            (
                r#"SELECT p.name, (p.price BETWEEN 10 AND 20) AS lowPrice FROM products p WHERE p.detailCategory = "apparel-accessories-beanies""#,
                r#"[{"name":"Minet Hat","lowPrice":false},{"name":"Klinto Hat","lowPrice":false},{"name":"Benki Hat","lowPrice":false},{"name":"Jontra Hat","lowPrice":false}]"#,
            ),
            ("SELECT VALUE p.name FROM p WHERE p.price IN (25, 65)", r#"["Klinto Hat","Benki Hat"]"#),
            ("SELECT VALUE p.name FROM p WHERE p.price NOT IN (25, 65, 40)", r#"["Minet Hat"]"#),
        ],
    );
}

#[test]
fn like_patterns() {
    let cases: &[(&str, &str, Option<&str>, bool)] = &[
        ("abc", "a%", None, true),
        ("abc", "%c", None, true),
        ("abc", "%b%", None, true),
        ("abc", "a_c", None, true),
        ("abc", "a__c", None, false),
        ("abc", "abc", None, true),
        ("abc", "ABC", None, false),
        ("", "%", None, true),
        ("", "_", None, false),
        ("", "", None, true),
        ("a", "", None, false),
        ("abc", "%%", None, true),
        ("abc", "a%c%", None, true),
        ("a%c", "a^%c", Some("^"), true),
        ("abc", "a^%c", Some("^"), false),
        ("20%", "%20^%%", Some("^"), true),
        ("20x", "%20^%%", Some("^"), false),
        ("a^c", "a^^c", Some("^"), true),
        ("a_c", "a\\_c", Some("\\"), true),
        ("abd", "ab[cd]", None, true),
        ("abe", "ab[cd]", None, false),
        ("abe", "ab[^cd]", None, true),
        ("abc", "ab[^cd]", None, false),
        ("abx", "ab[a-f]", None, false),
        ("abf", "ab[a-f]", None, true),
        ("abc", "[a-c][a-c][a-c]", None, true),
        ("a%", "a[%]", None, true),
        ("ab", "a[%]", None, false),
        ("a_", "a[_]", None, true),
        ("ab", "a[_]", None, false),
        ("a[", "a[[]", None, true),
        ("a]", "a]", None, true),
        ("abcabc", "%b%b%", None, true),
        ("abc", "%b%b%", None, false),
        ("mississippi", "%iss%ppi", None, true),
        ("xyz", "%y_z", None, false),
        ("xyyz", "%y_z", None, true),
        ("héllo", "h_llo", None, true),
        ("日本語", "日%語", None, true),
        ("Hello World", "hello%", None, false),
        ("Hello World", "Hello%", None, true),
        ("a\nb", "a_b", None, true),
        ("abc", "[a-z]%", None, true),
        ("1abc", "[a-z]%", None, false),
        ("5", "[0-9]", None, true),
        ("a", "[^0-9]", None, true),
    ];
    for (text, pat, esc, expect) in cases {
        let q = match esc {
            Some(e) => format!("SELECT VALUE {{\"r\": {} LIKE {} ESCAPE {}}}", Value::from(*text), Value::from(*pat), Value::from(*e)),
            None => format!("SELECT VALUE {{\"r\": {} LIKE {}}}", Value::from(*text), Value::from(*pat)),
        };
        assert_eq!(run(&q, &[])[0]["r"], json!(expect), "{q}");
        // NOT LIKE is the negation
        let nq = q.replace(" LIKE ", " NOT LIKE ");
        assert_eq!(run(&nq, &[])[0]["r"], json!(!expect), "{nq}");
    }
    for (e, expect) in [
        ("1 LIKE 'a'", u()),
        ("'a' LIKE 1", u()),
        ("undefined LIKE 'a'", u()),
        ("'a' LIKE undefined", u()),
        ("null LIKE 'a'", u()),
        ("'a' LIKE 'a' ESCAPE 'ab'", u()),
        ("'a' NOT LIKE 1", u()),
        ("'x' LIKE 'x' ESCAPE undefined", u()),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
    // like page
    let d = vec![
        json!({"name": "Phillips driver"}),
        json!({"name": "fruit basket"}),
        json!({"name": "Road"}),
        json!({"name": "winter coat"}),
        json!({"description": "save 20% today", "name": "deal"}),
    ];
    check_table(
        &d,
        &[
            (r#"SELECT VALUE p.name FROM products p WHERE p.name LIKE "%driver%""#, r#"["Phillips driver"]"#),
            (r#"SELECT VALUE p.name FROM products p WHERE p.name LIKE "fruit%""#, r#"["fruit basket"]"#),
            (r#"SELECT VALUE p.name FROM products p WHERE p.name LIKE "%Road""#, r#"["Road"]"#),
            (
                r#"SELECT VALUE p.name FROM products p WHERE p.name NOT LIKE "%winter%""#,
                r#"["Phillips driver","fruit basket","Road","deal"]"#,
            ),
            (r#"SELECT VALUE p.name FROM products p WHERE p.description LIKE "%20^%%" ESCAPE "^""#, r#"["deal"]"#),
        ],
    );
}

#[test]
fn constants_and_strings() {
    for (e, expect) in [
        ("null", Value::Null),
        ("NULL", Value::Null),
        ("true", json!(true)),
        ("TRUE", json!(true)),
        ("False", json!(false)),
        ("undefined", u()),
        ("UNDEFINED", u()),
        ("'single'", json!("single")),
        ("\"double\"", json!("double")),
        ("'a\"b'", json!("a\"b")),
        ("\"a'b\"", json!("a'b")),
        ("'it\\'s'", json!("it's")),
        ("\"say \\\"hi\\\"\"", json!("say \"hi\"")),
        ("'\\n\\t\\r'", json!("\n\t\r")),
        ("'\\\\'", json!("\\")),
        ("'\\/'", json!("/")),
        ("'\\b\\f'", json!("\u{8}\u{c}")),
        ("'\\u0041'", json!("A")),
        ("'\\u00e9'", json!("é")),
        ("'\\ud83d\\ude00'", json!("😀")),
        ("'😀'", json!("😀")),
        ("''", json!("")),
        ("[]", json!([])),
        ("{}", json!({})),
        ("[1, [2, [3]]]", json!([1, [2, [3]]])),
        ("{\"a\": {\"b\": [1]}}", json!({"a": {"b": [1]}})),
        ("[null, true, 'x', 1.5]", json!([null, true, "x", 1.5])),
        ("{a: 1, \"b c\": 2}", json!({"a": 1, "b c": 2})),
        ("{\"a\": 1, \"a\": 2}", json!({"a": 2})),
        ("[1, 2, 3][1]", json!(2)),
        ("{\"a\": {\"b\": 2}}.a.b", json!(2)),
        ("[1,2][5]", u()),
        ("[1,2][-1]", u()),
        ("[1,2][0.5]", u()),
        ("[1,2]['a']", u()),
        ("{\"a\":1}[\"a\"]", json!(1)),
        ("{\"a\":1}[0]", u()),
        ("'abc'[0]", u()),
        ("'abc'.length", u()),
        ("null.a", u()),
        ("5.a", u()),
        ("{\"value\": 1}.value", json!(1)),
        ("{\"select\": 2}.select", json!(2)),
        ("[{\"a\": 1}][0].a", json!(1)),
        ("NaN", Value::Null),
        ("Infinity", Value::Null),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
}

#[test]
fn property_access() {
    let d = vec![
        json!({"a": {"b": 1}, "a b": 2, "arr": [10, 20, 30], "n": null, "s": "str", "i": 2, "o": {"k": [ {"z": 9} ]}, "value": 7, "5": "five"}),
    ];
    check_table(
        &d,
        &[
            ("SELECT VALUE c.a.b FROM c", "[1]"),
            ("SELECT VALUE c['a b'] FROM c", "[2]"),
            ("SELECT VALUE c[\"a b\"] FROM c", "[2]"),
            ("SELECT VALUE c.arr[0] FROM c", "[10]"),
            ("SELECT VALUE c.arr[2] FROM c", "[30]"),
            ("SELECT VALUE c.arr[3] FROM c", "[]"),
            ("SELECT VALUE c.arr[-1] FROM c", "[]"),
            ("SELECT VALUE c.arr[1.5] FROM c", "[]"),
            ("SELECT VALUE c.arr['x'] FROM c", "[]"),
            ("SELECT VALUE c.arr[c.i] FROM c", "[30]"),
            ("SELECT VALUE c.arr[c.i - 1] FROM c", "[20]"),
            ("SELECT VALUE c['arr'][1] FROM c", "[20]"),
            ("SELECT VALUE c.o.k[0].z FROM c", "[9]"),
            ("SELECT VALUE c.n.x FROM c", "[]"),
            ("SELECT VALUE c.n FROM c", "[null]"),
            ("SELECT VALUE c.s.length FROM c", "[]"),
            ("SELECT VALUE c.s[0] FROM c", "[]"),
            ("SELECT VALUE c.i.x FROM c", "[]"),
            ("SELECT VALUE c.value FROM c", "[7]"),
            ("SELECT VALUE c['5'] FROM c", r#"["five"]"#),
            ("SELECT VALUE c.missing.deeper.still FROM c", "[]"),
            ("SELECT VALUE c.arr[0] + c.arr[1] FROM c", "[30]"),
            ("SELECT VALUE c.A FROM c", "[]"),
            ("SELECT VALUE c.a['b'] FROM c", "[1]"),
            ("SELECT VALUE c.arr[c.missing] FROM c", "[]"),
            ("SELECT VALUE c[c.s] FROM c", "[]"),
            ("SELECT VALUE c[\"o\"].k[0][\"z\"] FROM c", "[9]"),
        ],
    );
}

#[test]
fn where_keeps_only_true() {
    let d = vec![
        json!({"id": 1, "x": true}),
        json!({"id": 2, "x": false}),
        json!({"id": 3, "x": 1}),
        json!({"id": 4, "x": null}),
        json!({"id": 5}),
    ];
    check_table(
        &d,
        &[
            ("SELECT VALUE c.id FROM c WHERE c.x", "[1]"),
            ("SELECT VALUE c.id FROM c WHERE NOT c.x", "[2]"),
            ("SELECT VALUE c.id FROM c WHERE c.x = true", "[1]"),
            ("SELECT VALUE c.id FROM c WHERE c.x != true", "[2]"),
            ("SELECT VALUE c.id FROM c WHERE c.x = null", "[4]"),
            ("SELECT VALUE c.id FROM c WHERE c.x = 1", "[3]"),
            ("SELECT VALUE c.id FROM c WHERE 1", "[]"),
            ("SELECT VALUE c.id FROM c WHERE 'true'", "[]"),
            ("SELECT VALUE c.id FROM c WHERE null", "[]"),
            ("SELECT VALUE c.id FROM c WHERE undefined", "[]"),
            ("SELECT VALUE c.id FROM c WHERE true", "[1,2,3,4,5]"),
            ("SELECT VALUE c.id FROM c WHERE c.x = true OR c.x = false", "[1,2]"),
            ("SELECT VALUE c.id FROM c WHERE c.x = true OR c.id = 5", "[1,5]"),
            ("SELECT VALUE c.id FROM c WHERE NOT IS_DEFINED(c.x)", "[5]"),
            ("SELECT VALUE c.id FROM c WHERE IS_NULL(c.x)", "[4]"),
            ("SELECT VALUE c.id FROM c WHERE c.id > 2", "[3,4,5]"),
            ("SELECT VALUE c.id FROM c WHERE c.x > 0", "[3]"),
            ("SELECT VALUE c.id FROM c WHERE 0 = \"true\"", "[]"), // operators page
            ("SELECT VALUE c.id FROM c WHERE c.id IN (1, 5, 9)", "[1,5]"),
            ("SELECT VALUE c.x ?? 'none' FROM c", r#"[true,false,1,null,"none"]"#),
        ],
    );
}

#[test]
fn type_order_values_in_arrays_and_objects() {
    // undefined inside constructors is skipped, so later elements shift down (scalar page)
    for (e, expect) in [
        ("[1, undefined, 2]", json!([1, 2])),
        ("[undefined]", json!([])),
        ("{\"a\": undefined}", json!({})),
        ("[{\"a\": undefined}, [undefined]]", json!([{}, []])),
    ] {
        assert_eq!(eval1(e), expect, "{e}");
    }
    // scalar page examples
    check_table(
        &[],
        &[
            ("SELECT VALUE ((2 + 11 % 7) - 2) / 2", "[2]"),
            (
                r#"SELECT ("Redmond" = "WA") AS isCitySameAsState, ("WA" = "WA") AS isStateSameAsState"#,
                r#"[{"isCitySameAsState":false,"isStateSameAsState":true}]"#,
            ),
        ],
    );
}
