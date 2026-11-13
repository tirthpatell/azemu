mod common;
use common::*;

#[test]
fn empty_and_blank_are_errors() {
    assert!(parse_err("").message.contains("empty"));
    assert!(parse_err("   \t\n").message.contains("empty"));
}

#[test]
fn unbalanced_parens() {
    for sql in ["(a = 1", "a = 1)", "((a = 1)", "(a = 1))", "a IN (1, 2", "a IN 1, 2)", ")", "(", "EXISTS(a", "p('a'"] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
    let e = parse_err("(a = 1");
    assert!(e.message.contains("')'"), "{}", e.message);
    assert_eq!(e.position, 6);
}

#[test]
fn unknown_tokens() {
    for sql in ["a = 1 #", "a ? 1", "a = $x", "a ! 1", "a = 1 & b = 2", "a = 1 && b = 2", "a || b", "a = {1}", "a = `x`", "a ~ 1"] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
    let e = parse_err("a = 1 # 2");
    assert!(e.message.contains('#'));
    assert_eq!(e.position, 6);
}

#[test]
fn trailing_garbage() {
    for sql in ["a = 1 b", "a = 1 2", "a = 1 AND", "a = 1 OR", "a = 1 NOT", "a = 1 = 2", "TRUE FALSE", "a = 1 , b = 2", "a = 1;"] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
}

#[test]
fn dangling_operators() {
    for sql in [
        "a =",
        "= 1",
        "a = = 1",
        "a +",
        "* 2 = 2",
        "a IN ()",
        "a IN (1,)",
        "a LIKE",
        "a IS",
        "a IS NOT",
        "a IS 5",
        "a NOT 1",
        "NOT",
        "a LIKE 'x' ESCAPE",
        "a < > 1",
    ] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
}

#[test]
fn unterminated_literals() {
    assert!(parse_err("a = 'abc").message.contains("unterminated"));
    assert!(parse_err("[abc = 1").message.contains("unterminated"));
    assert!(parse_err("\"abc = 1").message.contains("unterminated"));
    assert!(parse_err("a = 'it''s").message.contains("unterminated"));
}

#[test]
fn bad_numbers() {
    for sql in ["a = 1abc", "a = 1.2.3", "a = 99999999999999999999", "a = 1e999", "a = 0x10", "a = 1_000"] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
    assert!(parse_err("a = 9223372036854775808").message.contains("out of range"));
}

#[test]
fn reserved_keywords_cannot_be_names() {
    for sql in ["and = 1", "in = 1", "like = 1", "user.not = 1", "a = null.x"] {
        assert!(Filter::parse(sql).is_err(), "{sql:?}");
    }
}

#[test]
fn unknown_scope_and_function() {
    assert!(parse_err("foo.bar = 1").message.contains("scope"));
    assert!(parse_err("foo(1) = 1").message.contains("unknown function"));
    assert!(parse_err("newid(1) = 'a'").message.contains("')'"));
    assert!(Filter::parse("user. = 1").is_err());
    assert!(Filter::parse("sys..x = 1").is_err());
}

#[test]
fn identifier_cannot_start_with_underscore_or_digit() {
    assert!(Filter::parse("_a = 1").is_err());
    assert!(Filter::parse("1a = 1").is_err());
}

#[test]
fn not_boolean_filters_are_rejected() {
    for sql in ["1", "'abc'", "NULL", "1 + 2", "-a", "newid()", "2.5"] {
        let e = parse_err(sql);
        assert!(e.message.contains("boolean"), "{sql:?}: {}", e.message);
    }
}

#[test]
fn between_is_not_in_the_documented_grammar() {
    assert!(Filter::parse("a BETWEEN 1 AND 2").is_err());
}

#[test]
fn error_display_has_message_and_position() {
    let e = parse_err("a = 1 #");
    let text = e.to_string();
    assert!(text.contains("unexpected character"));
    assert!(text.contains("position 6"));
    let boxed: Box<dyn std::error::Error> = Box::new(e);
    assert!(!boxed.to_string().is_empty());
}

#[test]
fn length_limit_is_1024_characters() {
    let pad = |n: usize| format!("a = 1{}", " ".repeat(n));
    assert!(Filter::parse(&pad(1024 - 5)).is_ok());
    let e = parse_err(&pad(1024 - 4));
    assert!(e.message.contains("1024"));
    // The limit counts characters, not bytes.
    let multi = format!("a = '{}'", "\u{e9}".repeat(1024 - 7));
    assert!(Filter::parse(&multi).is_ok());
}

#[test]
fn nesting_limit_is_100_levels() {
    let nested = |n: usize| format!("{}a = 1{}", "(".repeat(n), ")".repeat(n));
    assert!(Filter::parse(&nested(50)).is_ok());
    assert!(Filter::parse(&nested(99)).is_ok());
    assert!(parse_err(&nested(101)).message.contains("nested"));
    // Hostile: far deeper than the limit, within the length limit.
    assert!(Filter::parse(&"(".repeat(1000)).is_err());
    assert!(Filter::parse(&format!("{}TRUE", "NOT ".repeat(250))).is_err());
    assert!(Filter::parse(&format!("a = {}1", "-".repeat(1000))).is_err());
    assert!(Filter::parse(&format!("a = {}1", "- ".repeat(450))).is_err());
}

#[test]
fn long_flat_chains_do_not_overflow_the_stack() {
    let chain = vec!["a = 1"; 100].join(" AND ");
    assert!(chain.len() <= MAX_SQL_LEN);
    let f = Filter::parse(&chain).unwrap();
    assert!(f.matches(&props().user("a", 1), &Params::new()));
    let sum = format!("a = {}", vec!["1"; 400].join("+"));
    let f = Filter::parse(&sum).unwrap();
    assert!(f.matches(&props().user("a", 400), &Params::new()));
}

#[test]
fn non_ascii_error_positions_are_character_indexes() {
    let e = parse_err("caf\u{e9} = # 1");
    assert_eq!(e.position, 7);
}
