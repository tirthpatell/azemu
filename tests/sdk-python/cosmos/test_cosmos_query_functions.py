"""Queries: built-in string, type, math, array and other functions."""

import math

import pytest

from cosmosdata import PRODUCTS
from cosmosutil import ids, run


def want(pred):
    return sorted(d["id"] for d in PRODUCTS if pred(d))


def scalar(container, expr, where="c.id = 'p01'"):
    got = run(container, f"SELECT VALUE {expr} FROM c WHERE {where}")
    assert len(got) == 1, got
    return got[0]


# ---- string functions ------------------------------------------------------
def test_contains_is_case_sensitive_by_default(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE CONTAINS(c.name, 'Cook')"))
    assert got == want(lambda d: "Cook" in d["name"])
    assert run(products, "SELECT c.id FROM c WHERE CONTAINS(c.name, 'cook')") == []


def test_contains_ignore_case(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE CONTAINS(c.name, 'cook', true)"))
    assert got == want(lambda d: "cook" in d["name"].lower())


def test_startswith(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE STARTSWITH(c.name, 'P')"))
    assert got == want(lambda d: d["name"].startswith("P"))


def test_startswith_ignore_case(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE STARTSWITH(c.name, 'p', true)"))
    assert got == want(lambda d: d["name"].lower().startswith("p"))


def test_endswith(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE ENDSWITH(c.name, 'er')"))
    assert got == want(lambda d: d["name"].endswith("er"))


def test_upper_and_lower(products):
    assert scalar(products, "UPPER(c.name)") == "RUST IN ACTION"
    assert scalar(products, "LOWER(c.name)") == "rust in action"


def test_length(products):
    assert scalar(products, "LENGTH(c.name)") == len("Rust in Action")


def test_substring_is_zero_based(products):
    assert scalar(products, "SUBSTRING(c.name, 0, 4)") == "Rust"
    assert scalar(products, "SUBSTRING(c.name, 5, 2)") == "in"


def test_concat(products):
    assert scalar(products, "CONCAT(c.id, '-', c.category)") == "p01-books"


def test_replace(products):
    assert scalar(products, "REPLACE(c.name, ' ', '_')") == "Rust_in_Action"


def test_index_of(products):
    assert scalar(products, "INDEX_OF(c.name, 'in')") == 5
    assert scalar(products, "INDEX_OF(c.name, 'zzz')") == -1


def test_left_and_right(products):
    assert scalar(products, "LEFT(c.name, 4)") == "Rust"
    assert scalar(products, "RIGHT(c.name, 6)") == "Action"


def test_trim_functions(products):
    assert scalar(products, "TRIM('  x  ')") == "x"
    assert scalar(products, "LTRIM('  x  ')") == "x  "
    assert scalar(products, "RTRIM('  x  ')") == "  x"


def test_reverse(products):
    assert scalar(products, "REVERSE('abc')") == "cba"


def test_regexmatch(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE RegexMatch(c.name, '^P.*k$')"))
    assert got == want(lambda d: d["name"].startswith("P") and d["name"].endswith("k"))


def test_regexmatch_with_ignore_case_modifier(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE RegexMatch(c.name, '^rust', 'i')"))
    assert got == ["p01"]


def test_stringequals(products):
    assert ids(run(products, "SELECT c.id FROM c WHERE StringEquals(c.name, 'shovel', true)")) == ["p14"]
    assert run(products, "SELECT c.id FROM c WHERE StringEquals(c.name, 'shovel')") == []


def test_tostring_and_stringtonumber(products):
    assert scalar(products, "ToString(c.stock)") == "12"
    assert scalar(products, "StringToNumber('42')") == 42


# ---- type checking functions -----------------------------------------------
def test_is_defined(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE IS_DEFINED(c.brand)"))
    assert got == want(lambda d: "brand" in d)
    got = ids(run(products, "SELECT c.id FROM c WHERE NOT IS_DEFINED(c.rating)"))
    assert got == want(lambda d: "rating" not in d)


def test_is_defined_is_true_for_null(products):
    assert "p03" in ids(run(products, "SELECT c.id FROM c WHERE IS_DEFINED(c.rating)"))


def test_is_null(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE IS_NULL(c.rating)"))
    assert got == want(lambda d: "rating" in d and d["rating"] is None)


def test_is_string_and_is_number(products):
    assert len(run(products, "SELECT c.id FROM c WHERE IS_STRING(c.name)")) == 24
    assert ids(run(products, "SELECT c.id FROM c WHERE IS_NUMBER(c.rating)")) == want(
        lambda d: isinstance(d.get("rating"), (int, float))
    )
    assert run(products, "SELECT c.id FROM c WHERE IS_NUMBER(c.name)") == []


def test_is_array_and_is_object(products):
    assert len(run(products, "SELECT c.id FROM c WHERE IS_ARRAY(c.tags)")) == 24
    assert run(products, "SELECT c.id FROM c WHERE IS_ARRAY(c.specs)") == []
    assert ids(run(products, "SELECT c.id FROM c WHERE IS_OBJECT(c.specs)")) == want(lambda d: "specs" in d)


def test_is_bool(products):
    assert len(run(products, "SELECT c.id FROM c WHERE IS_BOOL(c.discontinued)")) == 24
    assert run(products, "SELECT c.id FROM c WHERE IS_BOOL(c.stock)") == []


def test_is_primitive(products):
    assert scalar(products, "IS_PRIMITIVE(c.name)") is True
    assert scalar(products, "IS_PRIMITIVE(c.tags)") is False


# ---- math functions --------------------------------------------------------
def test_abs_ceiling_floor_round_trunc(products):
    assert scalar(products, "ABS(-5)") == 5
    assert scalar(products, "CEILING(c.price)") == 40
    assert scalar(products, "FLOOR(c.price)") == 39
    assert scalar(products, "ROUND(c.price)") == 40
    assert scalar(products, "TRUNC(c.price)") == 39


def test_sqrt_power_sign(products):
    assert scalar(products, "SQRT(16)") == 4
    assert scalar(products, "POWER(2, 10)") == 1024
    assert scalar(products, "SIGN(-3)") == -1
    assert scalar(products, "SIGN(0)") == 0


def test_pi_exp_log(products):
    assert scalar(products, "PI()") == pytest.approx(math.pi)
    assert scalar(products, "EXP(0)") == 1
    assert scalar(products, "LOG(1)") == 0
    assert scalar(products, "LOG10(1000)") == pytest.approx(3)


def test_trigonometry(products):
    assert scalar(products, "SIN(0)") == 0
    assert scalar(products, "COS(0)") == 1
    assert scalar(products, "DEGREES(PI())") == pytest.approx(180)


# ---- array functions -------------------------------------------------------
def test_array_contains(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.tags, 'programming')"))
    assert got == want(lambda d: "programming" in d["tags"])


def test_array_contains_object_needs_full_match_by_default(products):
    assert run(products, "SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.reviews, {'user': 'ann'})") == []
    got = ids(run(products, "SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.reviews, {'user': 'ann', 'stars': 5})"))
    assert got == ["p01", "p07"]


def test_array_contains_partial_match(products):
    got = ids(run(products, "SELECT c.id FROM c WHERE ARRAY_CONTAINS(c.reviews, {'user': 'ann'}, true)"))
    assert got == ["p01", "p04", "p07"]


def test_array_length(products):
    assert scalar(products, "ARRAY_LENGTH(c.tags)", "c.id = 'p06'") == 3
    assert scalar(products, "ARRAY_LENGTH(c.tags)", "c.id = 'p05'") == 0


def test_array_slice(products):
    assert scalar(products, "ARRAY_SLICE(c.tags, 1)", "c.id = 'p06'") == ["databases", "sql"]
    assert scalar(products, "ARRAY_SLICE(c.tags, 0, 2)", "c.id = 'p06'") == ["programming", "databases"]
    assert scalar(products, "ARRAY_SLICE(c.tags, -1)", "c.id = 'p06'") == ["sql"]


def test_array_concat(products):
    assert scalar(products, "ARRAY_CONCAT(c.tags, ['x', 'y'])") == ["programming", "rust", "x", "y"]


def test_set_functions(products):
    assert sorted(scalar(products, "SetIntersect(c.tags, ['rust', 'go'])")) == ["rust"]
    assert sorted(scalar(products, "SetUnion(c.tags, ['rust', 'go'])")) == ["go", "programming", "rust"]


# ---- other -----------------------------------------------------------------
def test_function_names_are_case_insensitive(products):
    assert scalar(products, "upper(c.name)") == "RUST IN ACTION"
    assert scalar(products, "Array_Length(c.tags)") == 2


def test_nested_function_calls(products):
    assert scalar(products, "UPPER(LEFT(c.name, 4))") == "RUST"
    assert scalar(products, "LENGTH(CONCAT(c.id, c.id))") == 6


def test_function_on_missing_property_filters_row_out(products):
    # UPPER(undefined) is undefined, never equal to anything
    got = ids(run(products, "SELECT c.id FROM c WHERE UPPER(c.brand) = 'ACME'"))
    assert got == want(lambda d: d.get("brand") == "Acme")
