//! Tests of the parser.

use super::sql::*;

fn ok(s: &str) -> Select {
    parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn bad(s: &str) -> SqlError {
    parse(s).expect_err(s)
}

#[test]
fn selects_stars_columns_and_aliases() {
    assert_eq!(ok("SELECT * FROM BlobStorage").projection, Projection::Star);
    assert_eq!(ok("select * from blobstorage").from, vec![]);
    let s = ok("SELECT _1, name AS n, \"Full Name\" f, a.b FROM BlobStorage s WHERE s.x > 1 LIMIT 5;");
    let Projection::List(items) = &s.projection else { panic!() };
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].expr, Expr::Ref(vec![Step::Ordinal(1)]));
    assert_eq!(items[1].alias.as_deref(), Some("n"));
    assert_eq!(items[2].alias.as_deref(), Some("f"));
    assert_eq!(items[2].expr, Expr::Ref(vec![Step::Field { name: "Full Name".into(), quoted: true }]));
    assert_eq!(s.limit, Some(5));
    assert!(s.filter.is_some());
    // the alias of the table is not a field
    let Projection::List(items) = &ok("SELECT s.name FROM BlobStorage s").projection else { panic!() };
    assert_eq!(items[0].expr, Expr::Ref(vec![Step::Field { name: "name".into(), quoted: false }]));
    let Projection::List(items) = &ok("SELECT BlobStorage._2 FROM BlobStorage").projection else { panic!() };
    assert_eq!(items[0].expr, Expr::Ref(vec![Step::Ordinal(2)]));
}

#[test]
fn json_paths() {
    assert_eq!(ok("SELECT latitude FROM BlobStorage[*].warehouses[*]").from, vec![FromStep::Field("warehouses".into()), FromStep::All]);
    assert_eq!(ok("SELECT length FROM BlobStorage[*].dimensions").from, vec![FromStep::Field("dimensions".into())]);
    assert_eq!(ok("SELECT * FROM BlobStorage[*]").from, vec![]);
    let Projection::List(items) = &ok("SELECT weight,warehouses[0].longitude,id,tags[1] FROM BlobStorage[*]").projection else { panic!() };
    assert_eq!(items.len(), 4);
    assert_eq!(
        items[1].expr,
        Expr::Ref(vec![
            Step::Field { name: "warehouses".into(), quoted: false },
            Step::Index(0),
            Step::Field { name: "longitude".into(), quoted: false }
        ])
    );
}

#[test]
fn operators_functions_and_documented_examples() {
    for q in [
        "SELECT * FROM BlobStorage WHERE _1 > 250 AND (_2 <= 3 OR NOT _3 = 'x') AND _4 <> 1 AND _5 != 2",
        "SELECT * FROM BlobStorage WHERE a BETWEEN 1 AND 5 AND b NOT BETWEEN 1 AND 2 AND c IN (1, 2, 'x') AND d NOT IN (1) AND e LIKE 'a%' ESCAPE '\\' AND f NOT LIKE '_x'",
        "SELECT * FROM BlobStorage WHERE a IS NULL OR b IS NOT NULL OR c IS MISSING OR d IS NOT MISSING",
        "SELECT CAST(_1 AS INT), CAST(_2 AS FLOAT), CAST(_3 AS STRING), CAST(_4 AS TIMESTAMP), CAST(_5 AS BOOLEAN), CAST(_6 AS DECIMAL(10,2)) FROM BlobStorage",
        "SELECT CHARACTER_LENGTH('abcdefg'), CHAR_LENGTH(_1), LOWER('AbCdEfG'), UPPER('AbCdEfG') from BlobStorage",
        "SELECT SUBSTRING('123456789', 1, 5), SUBSTRING('123456789' FROM 2 FOR 3), SUBSTRING(_1 FROM 2) FROM BlobStorage",
        "SELECT TRIM(BOTH '123' FROM '1112211Microsoft22211122'), TRIM(LEADING FROM ' a'), TRIM(TRAILING 'x' FROM _1), TRIM(' a ') FROM BlobStorage",
        "SELECT DATE_ADD('minute', 1, CAST('2017-01-02T03:04:05.006Z' AS TIMESTAMP)), DATE_DIFF('hour','2018-11-09T00:00+05:30','2018-11-09T01:00:23-08:00') FROM BlobStorage",
        "SELECT EXTRACT(YEAR FROM '2010-01-01T'), EXTRACT(timezone_hour FROM _1), TO_STRING(CAST('1969-07-20T20:18Z' AS TIMESTAMP),  'MMMM d, y'), TO_TIMESTAMP('2007T'), UTCNOW() FROM BlobStorage",
        "SELECT COUNT(*), COUNT(_1), SUM(CAST(_2 AS INT)), AVG(_3), MIN(_4), MAX(_5) FROM BlobStorage WHERE _1 > 1",
        "SELECT COALESCE(_1, _2, 'x'), NULLIF(_1, ''), _1 || '-' || _2, -_1 + 2 * 3 % 4 / 5 - 6 FROM BlobStorage",
    ] {
        ok(q);
    }
    let s = ok("SELECT COUNT(*), SUM(_1) FROM BlobStorage");
    assert!(s.aggregate);
    assert!(!ok("SELECT _1 FROM BlobStorage").aggregate);
}

#[test]
fn syntax_errors_name_the_problem_and_the_place() {
    assert!(bad("SELECT").message.contains("FROM"));
    assert!(bad("SELECT * FROM Other").message.contains("BlobStorage"));
    assert!(bad("SELECT * FROM BlobStorage WHERE").message.contains("expression"));
    assert!(bad("SELECT * FROM BlobStorage WHERE _1 >").message.contains("expression"));
    assert!(bad("SELECT * FROM BlobStorage LIMIT x").message.contains("LIMIT"));
    assert!(bad("SELECT 'abc FROM BlobStorage").message.contains("not closed"));
    assert!(bad("SELECT FOO(1) FROM BlobStorage").message.contains("FOO"));
    assert!(bad("SELECT _0 FROM BlobStorage").message.contains("_1"));
    assert!(bad("SELECT COUNT(*), _1 FROM BlobStorage").message.contains("aggregates"));
    assert!(bad("SELECT * FROM BlobStorage WHERE COUNT(*) > 1").message.contains("WHERE"));
    assert!(bad("SELECT SUM(COUNT(*)) FROM BlobStorage").message.contains("nested"));
    assert!(bad("SELECT CAST(_1 AS BLOB) FROM BlobStorage").message.contains("BLOB"));
    assert!(bad("SELECT DATE_ADD('week', 1, _1) FROM BlobStorage").message.contains("week"));
    assert!(bad("SELECT sys.split(10485760) FROM BlobStorage").message.contains("sys.split"));
    assert!(bad("SELECT CASE WHEN 1 THEN 2 END FROM BlobStorage").message.contains("CASE"));
    assert!(bad("SELECT * FROM BlobStorage WHERE select = 1").message.contains("double quotes"));
    assert_eq!(bad("SELECT * FROM BlobStorage WHERE _1 # 1").pos, 35);
}

#[test]
fn nesting_is_bounded_both_ways() {
    // parentheses
    let q = format!("SELECT {}1{} FROM BlobStorage", "(".repeat(50_000), ")".repeat(50_000));
    assert!(bad(&q).message.contains("nested"));
    // an operator chain builds a tree as deep as it is long
    let q = format!("SELECT 1{} FROM BlobStorage", "+1".repeat(20_000));
    assert!(bad(&q).message.contains("nested"));
    // prefix operators
    let q = format!("SELECT {}1 FROM BlobStorage", "- ".repeat(100_000));
    assert!(bad(&q).message.contains("nested"));
    let q = format!("SELECT * FROM BlobStorage WHERE {}true", "NOT ".repeat(100_000));
    assert!(bad(&q).message.contains("nested"));
    // inside the limit
    ok(&format!("SELECT 1{} FROM BlobStorage", "+1".repeat(50)));
    ok(&format!("SELECT {}1{} FROM BlobStorage", "(".repeat(50), ")".repeat(50)));
}

#[test]
fn a_long_flat_list_is_fine() {
    let list: Vec<String> = (0..50_000).map(|i| i.to_string()).collect();
    ok(&format!("SELECT * FROM BlobStorage WHERE _1 IN ({})", list.join(",")));
}
