mod common;
use common::*;

// ---- unknown / missing property semantics ----
t!(missing_cmp_false, "x = 1", props(), false);
t!(missing_ne_false, "x <> 1", props(), false);
t!(missing_lt_false, "x < 1", props(), false);
t!(missing_not_cmp_unknown, "NOT x = 1", props(), false);
t!(missing_arith_unknown, "x + 1 = 1", props(), false);
t!(missing_unary_unknown, "-x = 0", props(), false);
t!(missing_like_unknown, "x LIKE '%'", props(), false);
t!(missing_not_like_unknown, "x NOT LIKE 'a'", props(), false);
t!(missing_in_unknown, "x IN (1,2)", props(), false);
t!(missing_not_in_unknown, "x NOT IN (1,2)", props(), false);
// AND / OR truth tables with unknown (U)
t!(and_true_unknown, "TRUE AND x = 1", props(), false);
t!(or_unknown_true, "x = 1 OR TRUE", props(), true);
t!(or_true_unknown, "TRUE OR x = 1", props(), true);
t!(or_false_unknown, "FALSE OR x = 1", props(), false);
t!(and_false_unknown_is_false_not_unknown, "NOT (FALSE AND x = 1)", props(), true);
t!(and_unknown_false_is_false, "NOT (x = 1 AND FALSE)", props(), true);
t!(and_unknown_true_is_unknown, "NOT (x = 1 AND TRUE)", props(), false);
t!(or_unknown_false_is_unknown, "NOT (x = 1 OR FALSE)", props(), false);
t!(not_unknown_stays_unknown, "NOT (NOT (x = 1))", props(), false);
t!(null_prop_value_is_unknown, "x = 1", props().user("x", Value::Null), false);
t!(unknown_in_right_side_of_in, "1 IN (y, 1)", props(), true);

// ---- types and promotion ----
t!(long_double_equal, "n = 2.0", props().user("n", 2), true);
t!(double_long_equal, "n = 2", props().user("n", 2.0), true);
t!(long_double_less, "n < 2.5", props().user("n", 2), true);
t!(string_vs_number_unknown_eq, "n = 1", props().user("n", "1"), false);
t!(string_vs_number_unknown_ne, "n <> 1", props().user("n", "1"), false);
t!(bool_vs_number_unknown, "n = 1", props().user("n", true), false);
t!(string_compare_is_case_sensitive, "n = 'Red'", props().user("n", "red"), false);
t!(arith_on_string_prop_unknown, "n * 2 = 4", props().user("n", "2"), false);
t!(int_div_by_zero_is_error_no_match, "1 / 0 = 1", props(), false);
t!(int_mod_by_zero_is_error_no_match, "NOT (1 % 0 = 1)", props(), false);
t!(double_div_by_zero_is_infinity, "1.0 / 0 > 1000", props(), true);
t!(timestamp_vs_long_less_unknown, "ts < 2000", props().user("ts", Value::Timestamp(1000)), false);
t!(timestamp_diff_is_long_ms, "ts2 - ts1 = 500", props().user("ts1", Value::Timestamp(1000)).user("ts2", Value::Timestamp(1500)), true);
t!(
    timestamp_minus_long_is_timestamp,
    "ts - 500 = ts2",
    props().user("ts", Value::Timestamp(1000)).user("ts2", Value::Timestamp(500)),
    true
);
t!(timestamp_vs_long_unknown, "ts = 1000", props().user("ts", Value::Timestamp(1000)), false);
t!(nan_not_equal_itself, "0.0 / 0 = 0.0 / 0", props(), false);

// ---- LIKE ----
t!(like_infix, "s LIKE '%bus%'", props().user("s", "servicebus-x"), true);
t!(like_percent_matches_empty, "s LIKE '%'", props().user("s", ""), true);
t!(like_suffix, "s LIKE '%-complete'", props().user("s", "order-complete"), true);
t!(like_suffix_miss, "s LIKE '%-complete'", props().user("s", "order-complete-extra"), false);
t!(like_underscore_needs_char, "s LIKE 'item-_'", props().user("s", "item-"), false);
t!(like_underscore_one_char, "s LIKE 'item-_'", props().user("s", "item-7"), true);
t!(like_underscore_too_many, "s LIKE 'item-_'", props().user("s", "item-AB"), false);
t!(like_case_sensitive, "s LIKE 'abc'", props().user("s", "ABC"), false);
t!(like_no_wildcards_exact, "s LIKE 'abc'", props().user("s", "abc"), true);
t!(like_multi_percent, "s LIKE 'a%b%c'", props().user("s", "aXXbYYc"), true);
t!(like_backtracking, "s LIKE '%ab%ab'", props().user("s", "ababab"), true);
t!(like_regex_chars_literal, "s LIKE 'a.b*(c)[d]^$|\\+?{}'", props().user("s", "a.b*(c)[d]^$|\\+?{}"), true);
t!(like_dot_is_not_wildcard, "s LIKE 'a.c'", props().user("s", "abc"), false);
t!(like_backslash_not_special_without_escape, "s LIKE 'a\\%'", props().user("s", "a\\xyz"), true);
t!(like_escape_percent, "s LIKE '%!%%' ESCAPE '!'", props().user("s", "discount%code"), true);
t!(like_escape_percent_miss, "s LIKE '%!%%' ESCAPE '!'", props().user("s", "discount10code"), false);
t!(like_escape_percent_alone, "s LIKE '%!%%' ESCAPE '!'", props().user("s", "%"), true);
t!(like_escape_underscore, "s LIKE 'order!_%' ESCAPE '!'", props().user("s", "order_123"), true);
t!(like_escape_underscore_miss, "s LIKE 'order!_%' ESCAPE '!'", props().user("s", "orderX123"), false);
t!(like_escape_escape, "s LIKE 'folder!!%' ESCAPE '!'", props().user("s", "folder!archive"), true);
t!(like_escape_escape_miss, "s LIKE 'folder!!%' ESCAPE '!'", props().user("s", "folderarchive"), false);
t!(like_escape_backslash, "s LIKE 'ABC\\%' ESCAPE '\\'", props().user("s", "ABC%"), true);
t!(like_escape_backslash_not_prefix, "s LIKE 'ABC\\%' ESCAPE '\\'", props().user("s", "ABCDEF"), false);
t!(like_not_like_miss, "s NOT LIKE 'test-%'", props().user("s", "test-123"), false);
t!(like_not_like_hit, "s NOT LIKE 'test-%'", props().user("s", "order-123"), true);
t!(like_on_number_unknown, "n LIKE '1%'", props().user("n", 12), false);
t!(like_unicode_underscore, "s LIKE '_'", props().user("s", "\u{1F600}"), true);
t!(like_pattern_from_property, "s LIKE p", props().user("s", "abc").user("p", "a%"), true);
t!(like_escape_not_one_char_is_error, "s LIKE 'a' ESCAPE 'ab'", props().user("s", "a"), false);
t!(equals_does_not_use_wildcards, "s = 'discount%_code'", props().user("s", "discount10_code"), false);

// ---- scopes and names ----
t!(unscoped_is_user, "color = 'red'", props().user("color", "red"), true);
t!(user_scope, "user.color = 'red'", props().user("color", "red"), true);
t!(user_scope_upper, "USER.color = 'red'", props().user("color", "red"), true);
t!(sys_scope, "sys.Label = 'x'", props().sys("Label", "x"), true);
t!(sys_scope_lowercase_name, "sys.label LIKE '%bus%'", props().sys("Label", "bus"), true);
t!(sys_does_not_see_user, "sys.Label = 'x'", props().user("Label", "x"), false);
t!(user_does_not_see_sys, "Label = 'x'", props().sys("Label", "x"), false);
t!(sys_unset_known_is_unknown, "sys.Label = 'x' OR TRUE", props(), true);
t!(sys_unknown_name_is_error, "NOT (sys.NoSuchThing = 'x')", props(), false);
t!(sys_unknown_name_is_null_check_ok, "sys.NoSuchThing IS NULL", props(), true);
t!(names_case_insensitive_via_impl, "COLOR = 'red'", props().user("color", "red"), true);
t!(bracketed_name_space, "[Order Type] = 'priority'", props().user("Order Type", "priority"), true);
t!(bracketed_with_scope, "user.[HR-EmployeeID] = 'E123'", props().user("HR-EmployeeID", "E123"), true);
t!(bracketed_escaped_bracket, "[Order]]Type] = 'priority'", props().user("Order]Type", "priority"), true);
t!(
    bracketed_url,
    "[http://schemas.microsoft.com/xrm/2011/Claims/EntityLogicalName] = 'account'",
    props().user("http://schemas.microsoft.com/xrm/2011/Claims/EntityLogicalName", "account"),
    true
);
t!(quoted_identifier, "\"Contoso & Northwind\" = 1", props().user("Contoso & Northwind", 1), true);
t!(quoted_identifier_escaped, "\"a\"\"b\" = 1", props().user("a\"b", 1), true);
t!(bracketed_keyword_name, "[and] = 1", props().user("and", 1), true);
t!(unicode_identifier, "caf\u{e9} = 1", props().user("caf\u{e9}", 1), true);
t!(identifier_with_digits_underscore, "a_1b2 = 1", props().user("a_1b2", 1), true);
t!(property_fn, "property('my prop') = 3", props().user("my prop", 3), true);
t!(p_fn_short, "p('color') = 'red'", props().user("color", "red"), true);
t!(p_fn_computed_name, "P('co' + 'lor') = 'red'", props().user("color", "red"), true);
t!(p_fn_missing, "p('nope') IS NULL", props(), true);
t!(p_fn_non_string_unknown, "p(1) IS NULL", props().user("1", 5), true);
t!(function_case_insensitive, "PROPERTY('a') = 1", props().user("a", 1), true);
t!(newid_is_guid_string, "newid() LIKE '________-____-4___-____-____________'", props(), true);
t!(newid_unique, "newid() <> newid()", props(), true);
t!(exists_bracketed, "EXISTS([a b])", props().user("a b", 1), true);

// ---- parameters ----
#[test]
fn param_comparison() {
    let p = props().user("a", 5);
    assert!(m_with("a = @p", &p, &params(&[("p", Value::Long(5))])));
    assert!(!m_with("a = @p", &p, &params(&[("p", Value::Long(6))])));
}

#[test]
fn param_missing_is_unknown() {
    let p = props().user("a", 5);
    assert!(!m_with("a = @p", &p, &Params::new()));
    assert!(!m_with("a <> @p", &p, &Params::new()));
    assert!(m_with("@p IS NULL", &p, &Params::new()));
}

#[test]
fn param_null_value_is_unknown() {
    let p = props().user("a", 5);
    assert!(!m_with("a = @p", &p, &params(&[("p", Value::Null)])));
}

#[test]
fn param_types() {
    let p = props().user("s", "x").user("d", 1.5).user("b", true);
    let ps = params(&[("s", Value::Str("x".into())), ("d", Value::Double(1.5)), ("b", Value::Bool(true))]);
    assert!(m_with("s = @s AND d = @d AND b = @b", &p, &ps));
}

#[test]
fn param_in_arithmetic_and_in_list() {
    let p = props().user("a", 10);
    let ps = params(&[("n", Value::Long(5)), ("x", Value::Long(10))]);
    assert!(m_with("a = @n * 2", &p, &ps));
    assert!(m_with("a IN (1, @x)", &p, &ps));
}

#[test]
fn param_timestamp_comparison() {
    let p = props().user("when", Value::Timestamp(1_000));
    let ps = params(&[("t", Value::Timestamp(2_000))]);
    assert!(m_with("when < @t", &p, &ps));
    assert!(!m_with("when > @t", &p, &ps));
}

#[test]
fn param_names_are_case_sensitive_keys_without_at() {
    let p = props().user("a", 1);
    assert!(m_with("a = @Value", &p, &params(&[("Value", Value::Long(1))])));
    assert!(!m_with("a = @Value", &p, &params(&[("value", Value::Long(1))])));
}

#[test]
fn bare_param_as_boolean() {
    assert!(m_with("@b", &props(), &params(&[("b", Value::Bool(true))])));
    assert!(!m_with("@b", &props(), &params(&[("b", Value::Bool(false))])));
}

#[test]
fn try_matches_reports_errors() {
    let f = Filter::parse("1 / 0 = 1").unwrap();
    assert!(f.try_matches(&props(), &Params::new()).is_err());
    let g = Filter::parse("sys.Bogus = 1").unwrap();
    assert!(g.try_matches(&props(), &Params::new()).is_err());
}

#[test]
fn and_short_circuits_errors() {
    let f = Filter::parse("FALSE AND 1 / 0 = 1").unwrap();
    assert_eq!(f.try_matches(&props(), &Params::new()), Ok(false));
    let g = Filter::parse("TRUE OR 1 / 0 = 1").unwrap();
    assert_eq!(g.try_matches(&props(), &Params::new()), Ok(true));
}

#[test]
fn sql_text_is_preserved() {
    let sql = "  a   =  1 ";
    assert_eq!(Filter::parse(sql).unwrap().sql(), sql);
    assert_eq!(Action::parse("REMOVE a").unwrap().sql(), "REMOVE a");
}
