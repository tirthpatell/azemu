//! System functions beyond the documentation examples: undefined and wrong-type arguments,
//! boundaries, and the functions whose pages have no runnable example.
//! Expected `undefined` means the result is undefined (the property is missing).

mod common;
use common::*;
use serde_json::Value;

/// Evaluates every expression as `SELECT VALUE {"r": expr}` and compares with the expected JSON
/// text (`undefined` = no `r` property).
fn table(cases: &[(&str, &str)]) {
    let mut failures = Vec::new();
    for (expr, expected) in cases {
        let sql = format!("SELECT VALUE {{\"r\": {expr}}}");
        let want: Option<Value> = if *expected == "undefined" {
            None
        } else {
            Some(serde_json::from_str(expected).unwrap_or_else(|e| panic!("{expected}: {e}")))
        };
        match try_run(&sql, &[], &fixed_clock_opts()) {
            Ok(got) => {
                let got_r = got.first().and_then(|o| o.get("r")).cloned();
                let ok = match (&got_r, &want) {
                    (None, None) => true,
                    (Some(a), Some(b)) => approx_eq(a, b),
                    _ => false,
                };
                if !ok {
                    failures.push(format!("{expr}\n   expected {expected}\n   got      {got_r:?}"));
                }
            }
            Err(e) => failures.push(format!("{expr}\n   error {e}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn type_checks_matrix() {
    let values = [("undefined", 0), ("null", 1), ("true", 2), ("1", 3), ("'a'", 4), ("[1]", 5), ("{}", 6)];
    type Pred = fn(i32) -> bool;
    let fns: [(&str, Pred); 8] = [
        ("IS_DEFINED", |t| t != 0),
        ("IS_NULL", |t| t == 1),
        ("IS_BOOL", |t| t == 2),
        ("IS_NUMBER", |t| t == 3),
        ("IS_STRING", |t| t == 4),
        ("IS_ARRAY", |t| t == 5),
        ("IS_OBJECT", |t| t == 6),
        ("IS_PRIMITIVE", |t| (1..=4).contains(&t)),
    ];
    for (name, pred) in fns {
        for (v, t) in values {
            let sql = format!("SELECT VALUE {{\"r\": {name}({v})}}");
            assert_eq!(run(&sql, &[])[0]["r"], serde_json::json!(pred(t)), "{sql}");
        }
    }
    table(&[
        ("IS_INTEGER(1)", "true"),
        ("IS_INTEGER(1.0)", "true"),
        ("IS_INTEGER(1.5)", "false"),
        ("IS_INTEGER('a')", "false"),
        ("IS_INTEGER(null)", "false"),
        ("IS_INTEGER(undefined)", "undefined"),
        ("IS_INTEGER(1e300)", "false"),
        ("IS_INTEGER(-9223372036854775808)", "true"),
        ("IS_INTEGER(0)", "true"),
        ("IS_INTEGER(NaN)", "false"),
        ("IS_FINITE_NUMBER(1)", "true"),
        ("IS_FINITE_NUMBER('a')", "false"),
        ("IS_FINITE_NUMBER(undefined)", "undefined"),
        ("IS_FINITE_NUMBER(Infinity)", "false"),
        ("IS_FINITE_NUMBER(-Infinity)", "false"),
        ("IS_FINITE_NUMBER(1e308)", "true"),
        ("is_defined(1)", "true"),
        ("Is_Null(null)", "true"),
    ]);
}

#[test]
fn undefined_and_wrong_type_arguments() {
    // every function with a defined-type contract returns undefined for undefined or wrong-typed input
    table(&[
        ("UPPER(undefined)", "undefined"),
        ("UPPER(1)", "undefined"),
        ("LOWER(null)", "undefined"),
        ("LENGTH([])", "undefined"),
        ("LENGTH(undefined)", "undefined"),
        ("TRIM(1)", "undefined"),
        ("LTRIM('a', 1)", "undefined"),
        ("REVERSE(true)", "undefined"),
        ("LEFT('abc', 'x')", "undefined"),
        ("LEFT(1, 1)", "undefined"),
        ("RIGHT(undefined, 1)", "undefined"),
        ("CONCAT('a', 1)", "undefined"),
        ("CONCAT('a', undefined)", "undefined"),
        ("CONCAT('a', null, 'b')", "undefined"),
        ("CONTAINS('a', 1)", "undefined"),
        ("CONTAINS(1, 'a')", "undefined"),
        ("CONTAINS('a', 'a', 'x')", "undefined"),
        ("STARTSWITH(null, 'a')", "undefined"),
        ("ENDSWITH('a', undefined)", "undefined"),
        ("STRINGEQUALS('a', 1)", "undefined"),
        ("INDEX_OF(1, 'a')", "undefined"),
        ("INDEX_OF('a', 'a', 'x')", "undefined"),
        ("REPLACE('a', 'b', 1)", "undefined"),
        ("REPLACE(undefined, 'a', 'b')", "undefined"),
        ("REPLICATE('a', 'x')", "undefined"),
        ("SUBSTRING('abc', 'a', 1)", "undefined"),
        ("SUBSTRING(1, 0, 1)", "undefined"),
        ("TOSTRING(undefined)", "undefined"),
        ("STRINGSPLIT(1, ',')", "undefined"),
        ("STRINGJOIN('a', ',')", "undefined"),
        ("STRINGTOARRAY(1)", "undefined"),
        ("STRINGTONUMBER(1)", "undefined"),
        ("STRINGTOBOOLEAN(true)", "undefined"),
        ("STRINGTONULL(null)", "undefined"),
        ("STRINGTOOBJECT(undefined)", "undefined"),
        ("REGEXMATCH(1, 'a')", "undefined"),
        ("REGEXMATCH('a', 1)", "undefined"),
        ("ABS('a')", "undefined"),
        ("ABS(null)", "undefined"),
        ("ABS(undefined)", "undefined"),
        ("CEILING('1')", "undefined"),
        ("FLOOR(true)", "undefined"),
        ("ROUND([1])", "undefined"),
        ("SQRT({})", "undefined"),
        ("POWER('a', 2)", "undefined"),
        ("POWER(2, undefined)", "undefined"),
        ("LOG('a')", "undefined"),
        ("LOG(2, 'a')", "undefined"),
        ("SIN(null)", "undefined"),
        ("ATN2(1, 'a')", "undefined"),
        ("SIGN('a')", "undefined"),
        ("TRUNC(false)", "undefined"),
        ("SQUARE('a')", "undefined"),
        ("INTADD('a', 1)", "undefined"),
        ("INTBITAND(1, null)", "undefined"),
        ("INTBITNOT('a')", "undefined"),
        ("NUMBERBIN('a')", "undefined"),
        ("NUMBERBIN(5, 'a')", "undefined"),
        ("ARRAY_LENGTH(1)", "undefined"),
        ("ARRAY_LENGTH(null)", "undefined"),
        ("ARRAY_LENGTH(undefined)", "undefined"),
        ("ARRAY_CONCAT([1], 2)", "undefined"),
        ("ARRAY_CONTAINS('x', 1)", "undefined"),
        ("ARRAY_CONTAINS([1], 1, 'y')", "undefined"),
        ("ARRAY_CONTAINS([1], undefined)", "undefined"),
        ("ARRAY_SLICE('a', 1)", "undefined"),
        ("ARRAY_SLICE([1], 'a')", "undefined"),
        ("ARRAY_SUM(1)", "undefined"),
        ("ARRAY_AVG('a')", "undefined"),
        ("SETUNION([1], 2)", "undefined"),
        ("SETINTERSECT(1, [1])", "undefined"),
        ("SETDIFFERENCE([1], null)", "undefined"),
        ("SETEQUAL('a', 'a')", "undefined"),
        ("OBJECTTOARRAY([1])", "undefined"),
        ("OBJECTTOARRAY({}, 'k')", "undefined"),
        ("OBJECTTOARRAY({}, 1, 2)", "undefined"),
        ("CHOOSE('1', 'a')", "undefined"),
        ("CHOOSE(undefined, 'a')", "undefined"),
        ("IIF(undefined, 1, 2)", "undefined"),
        ("IIF(true, undefined, 2)", "undefined"),
        ("YEAR(1)", "undefined"),
        ("DATETIMEADD('dd', 1, undefined)", "undefined"),
        ("DATETIMEPART('dd', null)", "undefined"),
        ("DATETIMETOTICKS(5)", "undefined"),
        ("TICKSTODATETIME('x')", "undefined"),
        ("TIMESTAMPTODATETIME(undefined)", "undefined"),
        ("DATETIMEFORMAT(1, 'yyyy')", "undefined"),
        ("DATETIMEBIN('2020-01-01', 1)", "undefined"),
        ("AGO('dd', undefined)", "undefined"),
    ]);
}

#[test]
fn function_names_are_case_insensitive() {
    table(&[
        ("upper('a')", "\"A\""),
        ("Upper('a')", "\"A\""),
        ("uPPER('a')", "\"A\""),
        ("array_length([1,2])", "2"),
        ("Array_Contains([1], 1)", "true"),
        ("stringsplit('a,b', ',')", "[\"a\",\"b\"]"),
        ("String_Split('a,b', ',')", "[\"a\",\"b\"]"),
    ]);
}

#[test]
fn string_functions() {
    table(&[
        ("CONCAT('a', 'b', 'c', 'd')", "\"abcd\""),
        ("CONCAT('', '')", "\"\""),
        ("CONTAINS('abc', '')", "true"),
        ("CONTAINS('', 'a')", "false"),
        ("CONTAINS('ABC', 'b', true)", "true"),
        ("CONTAINS('ABC', 'b', false)", "false"),
        ("STARTSWITH('abc', '')", "true"),
        ("STARTSWITH('abc', 'ABC', true)", "true"),
        ("STARTSWITH('abc', 'abcd')", "false"),
        ("ENDSWITH('abc', 'BC', true)", "true"),
        ("ENDSWITH('abc', '')", "true"),
        ("ENDSWITH('', 'a')", "false"),
        ("STRINGEQUALS('a', 'A', true)", "true"),
        ("STRINGEQUALS('a', 'a', 'x')", "undefined"),
        ("STRINGEQUALS('', '')", "true"),
        ("INDEX_OF('abc', '')", "0"),
        ("INDEX_OF('abc', 'c', 5)", "-1"),
        ("INDEX_OF('abcabc', 'c', 3)", "5"),
        ("INDEX_OF('abcabc', 'c', -3)", "2"),
        ("INDEX_OF('', 'a')", "-1"),
        ("INDEX_OF('日本語', '語')", "2"),
        ("LASTINDEXOF('abcabc', 'b')", "4"),
        ("LASTINDEXOF('abc', 'abcd')", "-1"),
        ("LENGTH('日本')", "2"),
        ("LENGTH('😀')", "1"),
        ("LENGTH('')", "0"),
        ("LEFT('abc', -1)", "\"\""),
        ("LEFT('abc', 1.9)", "\"a\""),
        ("LEFT('', 5)", "\"\""),
        ("LEFT('日本語', 2)", "\"日本\""),
        ("RIGHT('abc', 0)", "\"\""),
        ("RIGHT('日本語', 2)", "\"本語\""),
        ("RIGHT('abc', -2)", "\"\""),
        ("LOWER('ÀB')", "\"àb\""),
        ("UPPER('straße')", "\"STRASSE\""),
        ("LOWER('')", "\"\""),
        ("LTRIM('  a  ')", "\"a  \""),
        ("RTRIM('  a  ')", "\"  a\""),
        ("TRIM('\\t\\na \\n')", "\"a\""),
        ("TRIM('xxaxx', 'x')", "\"a\""),
        ("TRIM('xyaxy', 'xy')", "\"a\""),
        ("TRIM('', 'x')", "\"\""),
        ("REPLACE('aaa', 'a', 'bb')", "\"bbbbbb\""),
        ("REPLACE('abc', '', 'x')", "\"abc\""),
        ("REPLACE('abc', 'b', '')", "\"ac\""),
        ("REPLACE('', 'a', 'b')", "\"\""),
        ("REPLICATE('ab', 0)", "\"\""),
        ("REPLICATE('a', -1)", "undefined"),
        ("REPLICATE('a', 10001)", "undefined"),
        ("LENGTH(REPLICATE('ab', 5000))", "10000"),
        ("REPLICATE('a', 1.9)", "\"a\""),
        ("REPLICATE('a', Infinity)", "undefined"),
        ("REPLICATE('', 1000000)", "\"\""),
        ("REVERSE('😀a')", "\"a😀\""),
        ("REVERSE('')", "\"\""),
        ("SUBSTRING('abc', 1, 100)", "\"bc\""),
        ("SUBSTRING('abc', 5, 1)", "\"\""),
        ("SUBSTRING('abc', 0, 0)", "\"\""),
        ("SUBSTRING('日本語', 1, 1)", "\"本\""),
        ("TOSTRING(null)", "\"null\""),
        ("TOSTRING(true)", "\"true\""),
        ("TOSTRING(1.5)", "\"1.5\""),
        ("TOSTRING([1, 'a'])", "\"[1,\\\"a\\\"]\""),
        ("TOSTRING({\"a\": null})", "\"{\\\"a\\\":null}\""),
        ("TOSTRING(-Infinity)", "\"-Infinity\""),
        ("TOSTRING('s')", "\"s\""),
        ("TOSTRING({})", "\"{}\""),
        ("TOSTRING([])", "\"[]\""),
        ("STRINGSPLIT('a,b,,c', ',')", "[\"a\",\"b\",\"\",\"c\"]"),
        ("STRINGSPLIT('abc', 'abc')", "[\"\",\"\"]"),
        ("STRINGSPLIT('abc', 'x')", "[\"abc\"]"),
        ("STRINGJOIN([], ',')", "\"\""),
        ("STRINGJOIN(['a'], ',')", "\"a\""),
        ("STRINGJOIN([1], ',')", "undefined"),
        ("STRINGJOIN(['a', 'b'], ', ')", "\"a, b\""),
        ("STRINGTOARRAY('[1,2')", "undefined"),
        ("STRINGTOARRAY('{}')", "undefined"),
        ("STRINGTOARRAY(' [1] ')", "[1]"),
        ("STRINGTOOBJECT('[]')", "undefined"),
        ("STRINGTOOBJECT('{\"a\": [1, {\"b\": null}]}')", "{\"a\":[1,{\"b\":null}]}"),
        ("STRINGTONUMBER('1e2')", "100"),
        ("STRINGTONUMBER('0x10')", "undefined"),
        ("STRINGTONUMBER('')", "undefined"),
        ("STRINGTONUMBER('+1')", "undefined"),
        ("STRINGTONUMBER('-0.5')", "-0.5"),
        ("STRINGTONUMBER('1 2')", "undefined"),
        ("STRINGTONUMBER('true')", "undefined"),
        ("STRINGTOBOOLEAN('TRUE')", "undefined"),
        ("STRINGTOBOOLEAN('yes')", "undefined"),
        ("STRINGTONULL('null')", "null"),
        ("STRINGTONULL('nil')", "undefined"),
        ("REGEXMATCH('abc', '^a')", "true"),
        ("REGEXMATCH('abc', 'b$')", "false"),
        ("REGEXMATCH('a\\nb', '^b', 'm')", "true"),
        ("REGEXMATCH('a\\nb', '^b')", "false"),
        ("REGEXMATCH('a\\nb', 'a.b', 's')", "true"),
        ("REGEXMATCH('a\\nb', 'a.b')", "false"),
        ("REGEXMATCH('abc', '(')", "undefined"),
        ("REGEXMATCH('abc', 'a', 'z')", "undefined"),
        ("REGEXMATCH('ABC', 'abc', 'i')", "true"),
        ("REGEXMATCH('abc', '\\\\d+')", "false"),
        ("REGEXMATCH('a1', '\\\\d')", "true"),
        ("REGEXMATCH('abc', '')", "true"),
        ("CONTAINS_ALL_CI('abc', 'A', 'B')", "true"),
        ("CONTAINS_ALL_CS('abc', 'a', 'B')", "false"),
        ("CONTAINS_ANY_CS('abc', 'x', 'c')", "true"),
        ("CONTAINS_ANY_CI('abc', 'X', 'C')", "true"),
        ("CONTAINS_ALL_CI('abc', 1)", "undefined"),
        ("CONTAINS_ANY_CI(1, 'a')", "undefined"),
        ("CONTAINS_ALL_CS(undefined, 'a')", "undefined"),
        ("CONTAINS_ANY_CS('abc', undefined, undefined)", "undefined"),
        ("CONTAINS_ALL_CS('abc', undefined)", "undefined"),
    ]);
}

#[test]
fn math_functions() {
    table(&[
        ("ABS(-0.5)", "0.5"),
        ("ABS(-9223372036854775808)", "9.223372036854776e18"),
        ("ABS(7)", "7"),
        ("ABS(Infinity)", "null"),
        ("CEILING(1.2)", "2"),
        ("CEILING(-1.2)", "-1"),
        ("CEILING(5)", "5"),
        ("FLOOR(5)", "5"),
        ("FLOOR(-0.5)", "-1"),
        ("FLOOR(1e300)", "1e300"),
        ("ROUND(-2.5)", "-3"),
        ("ROUND(0.5)", "1"),
        ("ROUND(-0.5)", "-1"),
        ("ROUND(1234.5678, -2)", "1200"),
        ("ROUND(5, 2)", "5"),
        ("ROUND(1.5, 0.5)", "undefined"),
        ("ROUND(15, -1)", "20"),
        ("ROUND(2.345, 1)", "2.3"),
        ("ROUND(1234.5678, 3)", "1234.568"),
        ("TRUNC(-0.9)", "0"),
        ("TRUNC(1e300)", "1e300"),
        ("SIGN(-0.0)", "0"),
        ("SIGN(5.5)", "1"),
        ("SIGN(-1e-300)", "-1"),
        ("SQRT(16)", "4"),
        ("SQRT(-1)", "null"),
        ("POWER(2, 10)", "1024"),
        ("POWER(2, -1)", "0.5"),
        ("POWER(2, 0.5)", "1.4142135623730951"),
        ("POWER(0, 0)", "1"),
        ("POWER(10, 20)", "1e20"),
        ("EXP(1)", "2.718281828459045"),
        ("LOG(0)", "null"),
        ("LOG(8, 2)", "3"),
        ("LOG(100, 10)", "2"),
        ("LOG(-1)", "null"),
        ("LOG(27, 3)", "3"),
        ("LOG10(1000)", "3"),
        ("LOG10(0.001)", "-3"),
        ("PI()", "3.141592653589793"),
        ("SIN(0)", "0"),
        ("COS(0)", "1"),
        ("TAN(0)", "0"),
        ("ASIN(1)", "1.5707963267948966"),
        ("ASIN(2)", "null"),
        ("ACOS(1)", "0"),
        ("ATAN(1)", "0.7853981633974483"),
        ("ATN2(1, 1)", "0.7853981633974483"),
        ("ATN2(0, -1)", "3.141592653589793"),
        ("COT(1)", "0.6420926159343306"),
        ("DEGREES(PI())", "180"),
        ("RADIANS(180)", "3.141592653589793"),
        ("NUMBERBIN(7.5, 2.5)", "7.5"),
        ("NUMBERBIN(-0.5)", "-1"),
        ("NUMBERBIN(5)", "5"),
        ("NUMBERBIN(37.5, 25)", "25"),
        ("SQUARE(1.5)", "2.25"),
        ("SQUARE(-3)", "9"),
        ("SQUARE(3037000500)", "9.223372037000250e18"),
        ("INTADD(1.0, 2)", "3"),
        ("INTADD(9223372036854775807, 1)", "undefined"),
        ("INTSUB(5, 7)", "-2"),
        ("INTMUL(4611686018427387904, 2)", "undefined"),
        ("INTMOD(-5, 3)", "-2"),
        ("INTDIV(7, 2)", "3"),
        ("INTDIV(-7, 2)", "-3"),
        ("INTMOD(5, 0)", "undefined"),
        ("INTBITOR(1, 2)", "3"),
        ("INTBITXOR(5, 1)", "4"),
        ("INTBITNOT(-1)", "0"),
        ("INTBITLEFTSHIFT(1, 62)", "4611686018427387904"),
        ("INTBITLEFTSHIFT(1, 64)", "undefined"),
        ("INTBITRIGHTSHIFT(-16, 2)", "-4"),
        ("INTBITRIGHTSHIFT(16, -1)", "undefined"),
        ("INTBITAND(7, 3.0)", "3"),
        ("RAND() >= 0 AND RAND() < 1", "true"),
        ("RAND() * 0", "0"),
        ("RAND() = RAND()", "false"),
        ("IS_NUMBER(RAND())", "true"),
    ]);
    // RAND is deterministic for a given seed
    let a = run("SELECT VALUE [RAND(), RAND(), RAND()]", &[]);
    let b = run("SELECT VALUE [RAND(), RAND(), RAND()]", &[]);
    assert_eq!(a, b);
    let mut o = fixed_clock_opts();
    o.rand_seed = Some(7);
    let c = try_run("SELECT VALUE [RAND(), RAND(), RAND()]", &[], &o).unwrap();
    assert_ne!(a, c);
}

#[test]
fn array_functions() {
    table(&[
        ("ARRAY_CONCAT([1], [2], [3])", "[1,2,3]"),
        ("ARRAY_CONCAT([], [])", "[]"),
        ("ARRAY_CONCAT([[1]], [[2]])", "[[1],[2]]"),
        ("ARRAY_CONTAINS([1, 2], 1.0)", "true"),
        ("ARRAY_CONTAINS([[1, 2]], [1, 2])", "true"),
        ("ARRAY_CONTAINS([null], null)", "true"),
        ("ARRAY_CONTAINS([], 1)", "false"),
        ("ARRAY_CONTAINS([{\"a\": {\"b\": 1, \"c\": 2}}], {\"a\": {\"b\": 1}}, true)", "true"),
        ("ARRAY_CONTAINS([{\"a\": 1}], {\"a\": 1, \"b\": 2}, true)", "false"),
        ("ARRAY_CONTAINS([{\"a\": 1, \"b\": 2}], {\"b\": 2, \"a\": 1})", "true"),
        ("ARRAY_CONTAINS([1], 'a')", "false"),
        ("ARRAY_CONTAINS([\"1\"], 1)", "false"),
        ("ARRAY_CONTAINS_ALL([1, 2], 1, 2)", "true"),
        ("ARRAY_CONTAINS_ALL([1, 2], 1, 3)", "false"),
        ("ARRAY_CONTAINS_ANY([1, 2], 3, 2)", "true"),
        ("ARRAY_CONTAINS_ANY([1], 3)", "false"),
        ("ARRAY_CONTAINS_ALL([1], 1, undefined)", "undefined"),
        ("ARRAY_CONTAINS_ANY([1], 2, undefined)", "undefined"),
        ("ARRAY_CONTAINS_ALL('x', 1)", "undefined"),
        ("ARRAY_LENGTH([[1, 2], 3])", "2"),
        ("ARRAY_SLICE([1, 2, 3], -10)", "[1,2,3]"),
        ("ARRAY_SLICE([1, 2, 3], 5)", "[]"),
        ("ARRAY_SLICE([1, 2, 3], 1, 0)", "[]"),
        ("ARRAY_SLICE([1, 2, 3], 1, -1)", "[]"),
        ("ARRAY_SLICE([1, 2, 3], -1, 5)", "[3]"),
        ("ARRAY_SLICE([], 0)", "[]"),
        ("ARRAY_SUM([])", "0"),
        ("ARRAY_SUM([1, 2.5])", "3.5"),
        ("ARRAY_AVG([])", "undefined"),
        ("ARRAY_AVG([2])", "2"),
        ("ARRAY_MEDIAN([5])", "5"),
        ("ARRAY_MEDIAN([3, 1, 2])", "2"),
        ("ARRAY_MEDIAN([])", "undefined"),
        ("ARRAY_MIN([])", "undefined"),
        ("ARRAY_MAX([[1]])", "undefined"),
        ("ARRAY_MAX([{}])", "undefined"),
        ("ARRAY_MIN([3, 1.5, 2])", "1.5"),
        ("ARRAY_MAX(['b', 'a'])", "\"b\""),
        ("ARRAY_MIN([true, null])", "null"),
        ("ARRAY_SUM([1, 'a'])", "undefined"),
        ("ARRAY_SUM([null])", "undefined"),
        ("SETUNION([1, 1], [1, 2, 2])", "[1,2]"),
        ("SETINTERSECT([1, 2, 3], [3, 2])", "[3,2]"),
        ("SETDIFFERENCE([1, 2, 2, 3], [2])", "[1,3]"),
        ("SETEQUAL([1], [1.0])", "true"),
        ("SETUNION([{\"a\": 1}], [{\"a\": 1}, {\"a\": 2}])", "[{\"a\":1},{\"a\":2}]"),
        ("SETUNION([], [])", "[]"),
        ("SETINTERSECT([], [1])", "[]"),
        ("SETEQUAL([], [])", "true"),
        ("SETEQUAL([1, 2], [1])", "false"),
        ("SETUNION([1, '1'], ['1', 1])", "[1,\"1\"]"),
        ("OBJECTTOARRAY({})", "[]"),
        ("OBJECTTOARRAY({\"a\": 1}, 'key', 'val')", "[{\"key\":\"a\",\"val\":1}]"),
        ("OBJECTTOARRAY({\"a\": {\"b\": 1}})", "[{\"k\":\"a\",\"v\":{\"b\":1}}]"),
        ("CHOOSE(2.0, 'a', 'b')", "\"b\""),
        ("CHOOSE(-1, 'a')", "undefined"),
        ("CHOOSE(1, undefined)", "undefined"),
        ("CHOOSE(2, 'a', undefined)", "undefined"),
        ("CHOOSE(1, 'a', undefined)", "\"a\""),
        ("CHOOSE(1.5, 'a', 'b')", "undefined"),
        ("IIF(true, 'a', 'b')", "\"a\""),
        ("IIF(null, 'a', 'b')", "\"b\""),
        ("IIF(1 < 2, 1, 2)", "1"),
    ]);
}

#[test]
fn date_time_functions() {
    table(&[
        // clock fixed at 2024-10-14T14:20:30.6668888Z
        ("GETCURRENTDATETIME()", "\"2024-10-14T14:20:30.6668888Z\""),
        ("GETCURRENTDATETIMESTATIC()", "\"2024-10-14T14:20:30.6668888Z\""),
        ("GETCURRENTTIMESTAMP()", "1728915630666"),
        ("GETCURRENTTIMESTAMPSTATIC()", "1728915630666"),
        ("GETCURRENTTICKS()", "17289156306668888"),
        ("GETCURRENTTICKSSTATIC()", "17289156306668888"),
        ("NOW()", "\"2024-10-14T14:20:30.6668888Z\""),
        ("AGO('dd', 1)", "\"2024-10-13T14:20:30.6668888Z\""),
        ("NOW('mm', 3)", "\"2025-01-14T14:20:30.6668888Z\""),
        ("AGO('1d')", "\"2024-10-13T14:20:30.6668888Z\""),
        ("AGO('bad')", "undefined"),
        ("AGO('3 hh')", "\"2024-10-14T11:20:30.6668888Z\""),
        ("NOW('x', 1)", "undefined"),
        ("DATETIMEADD('mm', 1, '2020-01-31T00:00:00.0000000')", "\"2020-02-29T00:00:00.0000000Z\""),
        ("DATETIMEADD('yyyy', 1, '2020-02-29')", "\"2021-02-28T00:00:00.0000000Z\""),
        ("DATETIMEADD('dd', 1, '2020-12-31')", "\"2021-01-01T00:00:00.0000000Z\""),
        ("DATETIMEADD('ms', 1500, '2020-01-01T00:00:00.0000000')", "\"2020-01-01T00:00:01.5000000Z\""),
        ("DATETIMEADD('foo', 1, '2020-01-01')", "undefined"),
        ("DATETIMEADD('dd', 1.5, '2020-01-01')", "undefined"),
        ("DATETIMEADD('dd', 1, 'garbage')", "undefined"),
        ("DATETIMEADD('ns', 250, '2020-01-01T00:00:00.0000000')", "\"2020-01-01T00:00:00.0000002Z\""),
        ("DATETIMEADD('mcs', 5, '2020-01-01T00:00:00')", "\"2020-01-01T00:00:00.0000050Z\""),
        ("DATETIMEADD('yyyy', 9000, '2020-01-01')", "undefined"),
        ("DATETIMEADD('mm', -1, '2020-03-31')", "\"2020-02-29T00:00:00.0000000Z\""),
        ("DATETIMEADD('DD', 1, '2020-01-01')", "\"2020-01-02T00:00:00.0000000Z\""),
        ("DATETIMEADD('hour', 25, '2020-01-01 10:00:00')", "\"2020-01-02T11:00:00.0000000Z\""),
        ("DATETIMEDIFF('dd', '2020-01-01T23:59:59', '2020-01-02T00:00:00')", "1"),
        ("DATETIMEDIFF('mi', '2020-01-01T10:00:59', '2020-01-01T10:01:00')", "1"),
        ("DATETIMEDIFF('ms', '2020-01-01T00:00:00.0010000', '2020-01-01T00:00:00.0050000')", "4"),
        ("DATETIMEDIFF('ns', '2020-01-01T00:00:00.0000000', '2020-01-01T00:00:00.0000001')", "100"),
        ("DATETIMEDIFF('yyyy', '2019-12-31', '2020-01-01')", "1"),
        ("DATETIMEDIFF('mm', '2020-01-31', '2020-02-01')", "1"),
        ("DATETIMEDIFF('x', '2020-01-01', '2020-01-02')", "undefined"),
        ("DATETIMEDIFF('dd', '2020-01-01', 'bad')", "undefined"),
        ("DATETIMEPART('dd', '2024-02-29')", "29"),
        ("DATETIMEPART('hh', '2024-02-29 23:59:58.5')", "23"),
        ("DATETIMEPART('ms', '2024-02-29T00:00:00.5')", "500"),
        ("DATETIMEPART('x', '2024-02-29')", "undefined"),
        ("DATETIMEFROMPARTS(2024, 2, 30)", "undefined"),
        ("DATETIMEFROMPARTS(2024, 13, 1)", "undefined"),
        ("DATETIMEFROMPARTS(2024, 2, 29, 24)", "undefined"),
        ("DATETIMEFROMPARTS(2024.5, 1, 1)", "undefined"),
        ("DATETIMEFROMPARTS(2024, 2, 29)", "\"2024-02-29T00:00:00.0000000Z\""),
        ("DATETIMEFROMPARTS(2023, 2, 29)", "undefined"),
        ("DATETIMEFROMPARTS(10000, 1, 1)", "undefined"),
        ("DATETIMEFROMPARTS(2020, 1, 1, 0, 0, 0, 10000000)", "undefined"),
        ("DATETIMEBIN('2021-05-17T10:00:00', 'yyyy')", "\"2021-01-01T00:00:00.0000000Z\""),
        ("DATETIMEBIN('2021-05-17T10:00:00', 'mm')", "\"2021-05-01T00:00:00.0000000Z\""),
        ("DATETIMEBIN('2021-01-08T18:35:00', 'mi', 15)", "\"2021-01-08T18:30:00.0000000Z\""),
        ("DATETIMEBIN('2021-01-08T18:35:00', 'dd', 0)", "undefined"),
        ("DATETIMEBIN('2021-01-08T18:35:00', 'dd', -1)", "undefined"),
        ("DATETIMEBIN('2021-01-08T18:35:00', 'dd', 1, '1500-01-01')", "undefined"),
        ("DATETIMEBIN('2021-01-08T18:35:00', 'dd', 1.5)", "undefined"),
        ("DATETIMEBIN('2021-05-17', 'mm', 5)", "\"2021-04-01T00:00:00.0000000Z\""),
        ("DATETIMEBIN('2021-05-17', 'yyyy', 10)", "\"2020-01-01T00:00:00.0000000Z\""),
        ("DATETIMETOTICKS('1970-01-01')", "0"),
        ("DATETIMETOTICKS('1969-12-31T23:59:59.9999999')", "-1"),
        ("DATETIMETOTICKS('0001-01-01')", "-621355968000000000"),
        ("TICKSTODATETIME(-1)", "\"1969-12-31T23:59:59.9999999Z\""),
        ("TIMESTAMPTODATETIME(1.5)", "undefined"),
        ("TIMESTAMPTODATETIME(253402300800000)", "undefined"),
        ("DATETIMETOTIMESTAMP('1969-12-31T23:59:59.9999999')", "-1"),
        ("DATETIMETOTIMESTAMP('2000-01-01')", "946684800000"),
        ("YEAR('garbage')", "undefined"),
        ("DAY('2024-02-29')", "29"),
        ("MONTH('2024-02-29T10:00:00Z')", "2"),
        ("YEAR('0001-01-01')", "1"),
        ("IS_DATETIME('2024-02-30')", "false"),
        ("IS_DATETIME(5)", "false"),
        ("IS_DATETIME('2024-1-1')", "false"),
        ("IS_DATETIME('2024-01-01T10:00:00.12345678')", "false"),
        ("IS_DATETIME('')", "false"),
        ("IS_DATETIME('2024-01-01T25:00:00')", "false"),
        ("IS_DATETIME(undefined)", "undefined"),
        ("DATETIMEFORMAT('2024-10-15T16:45:30', 'hh:mm tt')", "\"04:45 PM\""),
        ("DATETIMEFORMAT('2024-10-15T00:05:30', 'h:m:s tt')", "\"12:5:30 AM\""),
        ("DATETIMEFORMAT('2005-03-07T00:00:00', 'y yy yyyy')", "\"5 05 2005\""),
        ("DATETIMEFORMAT('2005-03-07T00:00:00', 'M/d/yyyy MM dd')", "\"3/7/2005 03 07\""),
        ("DATETIMEFORMAT('2000-01-01 10:00:00.0123456', 'fff ff f')", "\"012 01 0\""),
        ("DATETIMEFORMAT('2000-01-01 10:00:00.0123456', 'FFF|FF|F')", "\"012|01|\""),
        ("DATETIMEFORMAT('2000-01-01 10:00:00', 'Q yyyy')", "\"Q 2000\""),
        ("DATETIMEFORMAT('bad', 'yyyy')", "undefined"),
        ("DATETIMEFORMAT('2000-01-01', '\\\\\\\\')", "\"\\\\\""),
    ]);
}

#[test]
fn clock_options() {
    use azemu_cosmos_sql::{parse, EvalOptions, Params};
    use std::time::{Duration, SystemTime};
    let q = parse("SELECT VALUE GETCURRENTDATETIME()").unwrap();
    let mut o = EvalOptions { now: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)), ..EvalOptions::default() };
    assert_eq!(q.execute(&[] as &[Value], &Params::new(), &o).unwrap(), vec![serde_json::json!("1970-01-02T00:00:00.0000000Z")]);
    // before the epoch
    o.now = Some(SystemTime::UNIX_EPOCH - Duration::from_secs(1));
    assert_eq!(q.execute(&[] as &[Value], &Params::new(), &o).unwrap(), vec![serde_json::json!("1969-12-31T23:59:59.0000000Z")]);
    // system clock: some date after 2020
    let o = EvalOptions::default();
    let r = q.execute(&[] as &[Value], &Params::new(), &o).unwrap();
    assert!(&r[0].as_str().unwrap()[..4] >= "2024");
    // static functions are one value for all rows, non-static follow the clock (fixed here, so equal)
    let docs = vec![serde_json::json!({"a": 1}), serde_json::json!({"a": 2})];
    let q = parse("SELECT VALUE GETCURRENTTICKSSTATIC() FROM c").unwrap();
    let r = q.execute(&docs, &Params::new(), &EvalOptions::default()).unwrap();
    assert_eq!(r[0], r[1]);
}

#[test]
fn unsupported_and_unknown_functions() {
    use azemu_cosmos_sql::ErrorKind;
    for (q, kind) in [
        ("SELECT VALUE ST_DISTANCE({}, {})", ErrorKind::Unsupported),
        ("SELECT * FROM c WHERE ST_WITHIN(c.loc, {})", ErrorKind::Unsupported),
        ("SELECT VALUE VectorDistance([1], [2])", ErrorKind::Unsupported),
        ("SELECT VALUE FullTextContains(c.t, 'x') FROM c", ErrorKind::Unsupported),
        ("SELECT VALUE udf.myFunc(1)", ErrorKind::Unsupported),
        ("SELECT VALUE DOCUMENTID(c) FROM c", ErrorKind::Unsupported),
        ("SELECT VALUE NOPE(1)", ErrorKind::Semantic),
        ("SELECT VALUE ABS()", ErrorKind::Semantic),
        ("SELECT VALUE ABS(1, 2)", ErrorKind::Semantic),
        ("SELECT VALUE CONCAT('a')", ErrorKind::Semantic),
        ("SELECT VALUE PI(1)", ErrorKind::Semantic),
        ("SELECT VALUE COUNT(1, 2)", ErrorKind::Semantic),
        ("SELECT VALUE COUNT()", ErrorKind::Semantic),
        ("SELECT VALUE SUBSTRING('a', 1)", ErrorKind::Semantic),
    ] {
        let e = err(q);
        assert_eq!(e.kind, kind, "{q}: {e}");
    }
    let e = err("SELECT VALUE NOPE(1)");
    assert_eq!((e.code.as_str(), e.start, e.end), ("SC2005", 13, 17));
    assert_eq!(e.message, "'NOPE' is not a recognized built-in function name.");
    let e = err("SELECT VALUE udf.myFunc(1)");
    assert_eq!((e.start, e.end), (13, 23));
    assert!(e.message.contains("udf.myFunc"));
}

#[test]
fn is_boolean_is_accepted_as_the_name_the_java_sdk_writes() {
    table(&[("IS_BOOLEAN(true)", "true"), ("IS_BOOLEAN(1)", "false"), ("IS_BOOLEAN(undefined)", "false"), ("IS_BOOL(false)", "true")]);
}
