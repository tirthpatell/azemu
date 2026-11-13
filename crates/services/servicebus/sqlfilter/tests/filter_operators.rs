mod common;
use common::*;

fn orders() -> MapProperties {
    props().user("price", 20).user("color", "red").user("qty", 10)
}

// comparisons
t!(eq_long, "price = 20", orders(), true);
t!(eq_long_false, "price = 21", orders(), false);
t!(eq_double_eq_sign, "price == 20", orders(), true);
t!(ne_angle, "price <> 21", orders(), true);
t!(ne_bang, "price != 20", orders(), false);
t!(lt, "price < 21", orders(), true);
t!(lt_false, "price < 20", orders(), false);
t!(le, "price <= 20", orders(), true);
t!(gt, "price > 19", orders(), true);
t!(gt_false, "price > 20", orders(), false);
t!(ge, "price >= 20", orders(), true);
t!(string_eq, "color = 'red'", orders(), true);
t!(string_ordering, "color < 'sed' AND color > 'blue'", orders(), true);
t!(bool_eq, "flag = TRUE", props().user("flag", true), true);
t!(bool_ne, "flag <> TRUE", props().user("flag", true), false);
t!(bool_lt_unknown, "flag < TRUE", props().user("flag", false), false);
t!(bare_bool_property, "flag", props().user("flag", true), true);
t!(bare_bool_property_false, "flag", props().user("flag", false), false);
t!(bare_non_bool_property, "color", orders(), false);
t!(true_literal, "TRUE", props(), true);
t!(false_literal, "FALSE", props(), false);
t!(one_equals_one, "1=1", props(), true);
t!(one_equals_two, "1=2", props(), false);

// logic and precedence
t!(and_both, "price > 10 AND color = 'red'", orders(), true);
t!(and_one_false, "price > 10 AND color = 'blue'", orders(), false);
t!(or_one_true, "price > 100 OR color = 'red'", orders(), true);
t!(or_none, "price > 100 OR color = 'blue'", orders(), false);
t!(not_pred, "NOT price > 100", orders(), true);
t!(not_parens, "NOT (price = 1)", orders(), true);
t!(not_not, "NOT NOT price = 20", orders(), true);
// AND binds tighter than OR: TRUE OR (FALSE AND FALSE) = TRUE, while
// (TRUE OR FALSE) AND FALSE = FALSE.
t!(prec_and_over_or, "TRUE OR FALSE AND FALSE", props(), true);
t!(prec_parens_override, "(TRUE OR FALSE) AND FALSE", props(), false);
t!(prec_or_then_and, "FALSE AND FALSE OR TRUE", props(), true);
// NOT binds tighter than AND: (NOT TRUE) AND FALSE vs NOT (TRUE AND FALSE)
t!(prec_not_over_and, "NOT TRUE AND FALSE", props(), false);
t!(prec_not_over_or, "NOT FALSE OR FALSE", props(), true);
t!(keywords_case_insensitive, "price > 10 and Color = 'red' Or not price = 20", orders(), true);

// arithmetic
t!(arith_add, "price + 5 = 25", orders(), true);
t!(arith_sub, "price - 5 = 15", orders(), true);
t!(arith_mul, "price * 2 > 30", orders(), true);
t!(arith_div_int, "price / 3 = 6", orders(), true);
t!(arith_div_double, "price / 8.0 = 2.5", orders(), true);
t!(arith_mod, "price % 3 = 2", orders(), true);
t!(arith_precedence, "2 + 3 * 4 = 14", props(), true);
t!(arith_parens, "(2 + 3) * 4 = 20", props(), true);
t!(arith_left_assoc, "10 - 4 - 3 = 3", props(), true);
t!(arith_unary_minus, "-price = -20", orders(), true);
t!(arith_unary_minus_expr, "-(2 + 3) = -5", props(), true);
t!(arith_double_minus, "- -3 = 3", props(), true);
t!(arith_unary_plus, "+price = 20", orders(), true);
t!(arith_binary_minus_negative, "5 - -2 = 7", props(), true);
t!(arith_property_both_sides, "price * qty = 200", orders(), true);
t!(arith_min_literal, "-9223372036854775808 < 0", props(), true);
t!(arith_wraps, "9223372036854775807 + 1 < 0", props(), true);
t!(arith_mixed_promotes, "1 + 0.5 = 1.5", props(), true);
t!(arith_string_concat, "'a' + 'b' = 'ab'", props(), true);

// IN / LIKE / IS NULL / EXISTS
t!(in_hit, "color IN ('red', 'blue')", orders(), true);
t!(in_miss, "color IN ('green', 'blue')", orders(), false);
t!(not_in_hit, "color NOT IN ('red', 'blue')", orders(), false);
t!(not_in_miss, "color NOT IN ('green')", orders(), true);
t!(in_numbers_promote, "price IN (1, 20.0)", orders(), true);
t!(in_no_space, "color IN('red')", orders(), true);
t!(in_expressions, "price IN (10 * 2, 7)", orders(), true);
t!(in_wrong_type_no_match, "price IN ('20')", orders(), false);
t!(like_prefix, "color LIKE 're%'", orders(), true);
t!(like_not_like, "color NOT LIKE 're%'", orders(), false);
t!(like_single_char, "color LIKE 'r_d'", orders(), true);
t!(is_null_missing, "nothing IS NULL", orders(), true);
t!(is_null_present, "color IS NULL", orders(), false);
t!(is_not_null_present, "color IS NOT NULL", orders(), true);
t!(is_not_null_missing, "nothing IS NOT NULL", orders(), false);
t!(is_null_null_value, "x IS NULL", props().user("x", Value::Null), true);
t!(exists_true, "EXISTS(price)", orders(), true);
t!(exists_false, "EXISTS(nothing)", orders(), false);
t!(exists_scoped, "EXISTS(user.price)", orders(), true);
t!(exists_sys, "EXISTS(sys.Label)", props().sys("Label", "x"), true);
t!(exists_sys_unset, "exists(sys.Label)", props(), false);
t!(not_exists, "NOT EXISTS(nothing)", orders(), true);

// literals
t!(lit_exponent, "x = 101.5E5", props().user("x", 10150000.0), true);
t!(lit_neg_exponent, "x = 0.5E-2", props().user("x", 0.005), true);
t!(lit_exponent_no_dot, "x = 2e3", props().user("x", 2000.0), true);
t!(lit_exponent_plus, "x = 1E+2", props().user("x", 100.0), true);
t!(lit_leading_dot, "x = .5", props().user("x", 0.5), true);
t!(lit_decimal, "x = 1894.1204", props().user("x", 1894.1204), true);
t!(lit_two_point_zero_is_double, "x = 2.0", props().user("x", 2), true);
t!(lit_string_escape, "n = 'O''Brien'", props().user("n", "O'Brien"), true);
t!(lit_empty_string, "n = ''", props().user("n", ""), true);
t!(lit_unicode_string, "n = 'caf\u{e9} \u{1F600}'", props().user("n", "caf\u{e9} \u{1F600}"), true);
t!(lit_null_compare_unknown, "x = NULL", props().user("x", 1), false);
t!(lit_null_ne_unknown, "x <> NULL", props().user("x", 1), false);
t!(lit_bool_case, "x = true AND y = False", props().user("x", true).user("y", false), true);
