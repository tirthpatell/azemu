mod common;
use common::*;

fn base() -> MapProperties {
    props().user("quantity", 10).user("priority", "high").user("color", "red")
}

#[test]
fn set_new_user_property() {
    let mut p = base();
    act("SET x = 5", &mut p);
    assert_eq!(p.get(Scope::User, "x"), Some(Value::Long(5)));
}

#[test]
fn set_overwrites_existing() {
    let mut p = base();
    act("SET color = 'blue'", &mut p);
    assert_eq!(p.get(Scope::User, "color"), Some(Value::Str("blue".into())));
}

#[test]
fn set_user_scope_explicit() {
    let mut p = base();
    act("SET user.color = 'blue'", &mut p);
    assert_eq!(p.get(Scope::User, "color"), Some(Value::Str("blue".into())));
}

#[test]
fn set_sys_property() {
    let mut p = base();
    act("SET sys.CorrelationId = 'low'", &mut p);
    assert_eq!(p.get(Scope::Sys, "CorrelationId"), Some(Value::Str("low".into())));
    assert_eq!(p.get(Scope::User, "CorrelationId"), None);
}

#[test]
fn set_bracketed_sys_label() {
    let mut p = base();
    act("SET [sys.Label] = 'x'", &mut p);
    // A bracketed name containing a dot is a user property named "sys.Label".
    assert_eq!(p.get(Scope::User, "sys.Label"), Some(Value::Str("x".into())));
    act("SET sys.[Label] = 'y'", &mut p);
    assert_eq!(p.get(Scope::Sys, "Label"), Some(Value::Str("y".into())));
}

#[test]
fn remove_user_property() {
    let mut p = base();
    act("REMOVE priority", &mut p);
    assert_eq!(p.get(Scope::User, "priority"), None);
    assert!(p.get(Scope::User, "color").is_some());
}

#[test]
fn remove_missing_is_not_an_error() {
    let mut p = base();
    act("REMOVE nothing", &mut p);
    assert_eq!(p.get(Scope::User, "color"), Some(Value::Str("red".into())));
}

#[test]
fn remove_is_case_insensitive_via_impl() {
    let mut p = base();
    act("remove PRIORITY", &mut p);
    assert_eq!(p.get(Scope::User, "priority"), None);
}

#[test]
fn docs_example_multiple_statements() {
    let mut p = base();
    act("SET quantity = quantity / 2; REMOVE priority;SET sys.CorrelationId = 'low';", &mut p);
    assert_eq!(p.get(Scope::User, "quantity"), Some(Value::Long(5)));
    assert_eq!(p.get(Scope::User, "priority"), None);
    assert_eq!(p.get(Scope::Sys, "CorrelationId"), Some(Value::Str("low".into())));
}

#[test]
fn statements_without_semicolons() {
    let mut p = base();
    act("SET a = 1 SET b = 2 REMOVE color", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Long(2)));
    assert_eq!(p.get(Scope::User, "color"), None);
}

#[test]
fn comma_separated_assignments() {
    let mut p = base();
    act("SET a = 1, b = 2, [c d] = 3", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Long(2)));
    assert_eq!(p.get(Scope::User, "c d"), Some(Value::Long(3)));
}

#[test]
fn comma_separated_statements() {
    let mut p = base();
    act("SET a = 1, REMOVE color, SET b = 2", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
    assert_eq!(p.get(Scope::User, "color"), None);
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Long(2)));
}

#[test]
fn comma_separated_removes() {
    let mut p = base();
    act("REMOVE color, priority", &mut p);
    assert_eq!(p.get(Scope::User, "color"), None);
    assert_eq!(p.get(Scope::User, "priority"), None);
}

#[test]
fn later_statements_see_earlier_ones() {
    let mut p = base();
    act("SET a = 1; SET b = a + 1; SET c = b * 10", &mut p);
    assert_eq!(p.get(Scope::User, "c"), Some(Value::Long(20)));
}

#[test]
fn set_then_remove_then_read_is_unknown() {
    let mut p = base();
    act("SET a = 1; REMOVE a; SET b = a", &mut p);
    assert_eq!(p.get(Scope::User, "a"), None);
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Null));
}

#[test]
fn arithmetic_on_right_side() {
    let mut p = base();
    act("SET total = quantity * 3 + 1; SET neg = -quantity; SET m = quantity % 4", &mut p);
    assert_eq!(p.get(Scope::User, "total"), Some(Value::Long(31)));
    assert_eq!(p.get(Scope::User, "neg"), Some(Value::Long(-10)));
    assert_eq!(p.get(Scope::User, "m"), Some(Value::Long(2)));
}

#[test]
fn arithmetic_promotes_to_double() {
    let mut p = base();
    act("SET half = quantity / 4.0", &mut p);
    assert_eq!(p.get(Scope::User, "half"), Some(Value::Double(2.5)));
}

#[test]
fn string_concat_and_copy_property() {
    let mut p = base();
    act("SET label2 = color + '-' + priority; SET sys.Label = color", &mut p);
    assert_eq!(p.get(Scope::User, "label2"), Some(Value::Str("red-high".into())));
    assert_eq!(p.get(Scope::Sys, "Label"), Some(Value::Str("red".into())));
}

#[test]
fn set_literals_of_each_type() {
    let mut p = MapProperties::new();
    act("SET a = 1; SET b = 2.5; SET c = 'x''y'; SET d = TRUE; SET e = NULL; SET f = 1E2", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Double(2.5)));
    assert_eq!(p.get(Scope::User, "c"), Some(Value::Str("x'y".into())));
    assert_eq!(p.get(Scope::User, "d"), Some(Value::Bool(true)));
    assert_eq!(p.get(Scope::User, "e"), Some(Value::Null));
    assert_eq!(p.get(Scope::User, "f"), Some(Value::Double(100.0)));
}

#[test]
fn set_from_missing_user_property_stores_null() {
    let mut p = base();
    act("SET a = nothing", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Null));
}

#[test]
fn set_uses_parameters() {
    let mut p = base();
    act_with("SET a = @v; SET b = @v * 2", &mut p, &params(&[("v", Value::Long(21))]));
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(21)));
    assert_eq!(p.get(Scope::User, "b"), Some(Value::Long(42)));
}

#[test]
fn set_functions() {
    let mut p = base();
    act("SET copy = property('color'); SET g = newid(); SET h = p('quantity') + 1", &mut p);
    assert_eq!(p.get(Scope::User, "copy"), Some(Value::Str("red".into())));
    assert_eq!(p.get(Scope::User, "h"), Some(Value::Long(11)));
    match p.get(Scope::User, "g") {
        Some(Value::Str(s)) => assert_eq!(s.len(), 36),
        other => panic!("expected a guid string, got {other:?}"),
    }
}

#[test]
fn set_converts_string_to_existing_timestamp() {
    let mut p = base().user("when", Value::Timestamp(0));
    act("SET when = '2024-03-05T10:20:30.500Z'", &mut p);
    assert_eq!(p.get(Scope::User, "when"), Some(Value::Timestamp(1_709_634_030_500)));
}

#[test]
fn set_timestamp_with_offset() {
    let mut p = base().user("when", Value::Timestamp(0));
    act("SET when = '1970-01-01T01:00:00+01:00'", &mut p);
    assert_eq!(p.get(Scope::User, "when"), Some(Value::Timestamp(0)));
}

#[test]
fn set_integer_into_existing_double_converts() {
    let mut p = base().user("ratio", 1.5);
    act("SET ratio = 2", &mut p);
    assert_eq!(p.get(Scope::User, "ratio"), Some(Value::Double(2.0)));
}

#[test]
fn set_string_over_number_replaces() {
    let mut p = base();
    act("SET quantity = 'many'", &mut p);
    assert_eq!(p.get(Scope::User, "quantity"), Some(Value::Str("many".into())));
}

#[test]
fn failing_conversion_is_error_and_atomic() {
    let mut p = base().user("when", Value::Timestamp(0));
    let a = Action::parse("SET a = 1; SET when = 'not a date'").unwrap();
    assert!(a.try_apply(&mut p, &Params::new()).is_err());
    assert_eq!(p.get(Scope::User, "a"), None, "earlier statements are rolled back");
    assert_eq!(p.get(Scope::User, "when"), Some(Value::Timestamp(0)));
}

#[test]
fn missing_sys_property_fails_action() {
    let mut p = base();
    let a = Action::parse("SET a = 1; SET b = sys.Bogus").unwrap();
    assert!(a.try_apply(&mut p, &Params::new()).is_err());
    assert_eq!(p.get(Scope::User, "a"), None);
    // `apply` swallows the error and leaves the message untouched.
    a.apply(&mut p, &Params::new());
    assert_eq!(p.get(Scope::User, "a"), None);
}

#[test]
fn unset_known_sys_property_is_unknown_not_error() {
    let mut p = base();
    act("SET a = sys.Label", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Null));
}

#[test]
fn division_by_zero_fails_action() {
    let mut p = base();
    let a = Action::parse("SET a = 1 / 0").unwrap();
    assert!(a.try_apply(&mut p, &Params::new()).is_err());
}

#[test]
fn action_keywords_case_insensitive() {
    let mut p = base();
    act("set A = 1; Remove color", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
    assert_eq!(p.get(Scope::User, "color"), None);
}

#[test]
fn trailing_semicolon_ok_and_whitespace() {
    let mut p = base();
    act("  SET a = 1 ;  ", &mut p);
    assert_eq!(p.get(Scope::User, "a"), Some(Value::Long(1)));
}

#[test]
fn action_then_filter_roundtrip() {
    let mut p = props().user("color", "red").user("quantity", 8);
    let f = Filter::parse("user.color = 'red'").unwrap();
    assert!(f.matches(&p, &Params::new()));
    act("SET quantity = quantity / 2; SET sys.Label = 'half'", &mut p);
    assert!(m("quantity = 4 AND sys.Label = 'half'", &p));
}

// ---- parse errors ----
fn bad(sql: &str) -> ParseError {
    Action::parse(sql).expect_err(sql)
}

#[test]
fn action_empty_is_error() {
    assert!(bad("").message.contains("empty"));
    assert!(bad("   ").message.contains("empty"));
}

#[test]
fn action_remove_sys_is_error() {
    assert!(bad("REMOVE sys.Label").message.contains("system"));
}

#[test]
fn action_syntax_errors() {
    for sql in [
        "SET",
        "SET a",
        "SET a =",
        "SET = 1",
        "a = 1",
        "SET a = 1,",
        "SET a = 1;;",
        ";",
        "REMOVE",
        "REMOVE a = 1",
        "SET a = (1",
        "SET a = 1 )",
        "SET a = 1 b = 2",
        "DELETE a",
        "SET a == ",
        "SET a = 'x",
        "SET [a = 1",
        "SET a = TRUE AND FALSE",
        "SET a = b > 1",
        "SET sys.x.y = 1",
        "SET 1 = 1",
        "SET a = foo()",
        "SET a = newid(1)",
    ] {
        assert!(Action::parse(sql).is_err(), "{sql:?} should not parse");
    }
}

#[test]
fn action_statement_limit() {
    let ok: String = (0..32).map(|i| format!("SET a{i} = {i};")).collect();
    assert!(Action::parse(&ok).is_ok());
    let too_many: String = (0..33).map(|i| format!("SET a{i}={i};")).collect();
    assert!(too_many.len() < MAX_SQL_LEN);
    assert!(bad(&too_many).message.contains("at most 32"));
}

#[test]
fn action_length_limit() {
    let long = format!("SET a = '{}'", "x".repeat(1100));
    assert!(bad(&long).message.contains("longer"));
}
