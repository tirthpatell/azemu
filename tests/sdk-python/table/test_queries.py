"""query_entities / list_entities: filters, literals, parameters, select, paging, ordering."""

import uuid
from datetime import datetime, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import EdmType, EntityProperty, TableTransactionError

from tableutil import code

U1 = uuid.UUID("c9da6455-213d-42c9-9a79-3e9149a57833")
U2 = uuid.UUID("c8da6455-213e-42d9-9b79-3f9149a57833")


def utc(*args):
    return datetime(*args, tzinfo=timezone.utc)


PEOPLE = [
    # pk, rk, props
    ("eu", "001", dict(name="Alice", age=30, score=1.5, active=True, born=utc(2000, 1, 1), uid=U1,
                       blob=b"a", big=EntityProperty(10_000_000_000, EdmType.INT64), note="n1")),
    ("eu", "002", dict(name="bob", age=25, score=2.5, active=False, born=utc(1995, 6, 15, 12, 30), uid=U2,
                       blob=b"bin", big=EntityProperty(5, EdmType.INT64))),
    ("eu", "010", dict(name="Carol", age=35, score=-0.5, active=True, born=utc(1990, 12, 31, 23, 59, 59), uid=U1,
                       blob=b"\x00\xff", big=EntityProperty(-7, EdmType.INT64), note="n3")),
    ("us", "001", dict(name="Dave", age=40, score=10.0, active=False, born=utc(1985, 3, 3), uid=U2,
                       blob=b"a", big=EntityProperty(2**40, EdmType.INT64))),
    ("us", "002", dict(name="O'Brien", age=22, score=0.0, active=True, born=utc(2005, 7, 7), uid=U1,
                       blob=b"", big=EntityProperty(0, EdmType.INT64), note="n5")),
    ("asia", "100", dict(name="eve", age=30, score=3.25, active=True, born=utc(2000, 1, 1, 0, 0, 0, 500000), uid=U2,
                         blob=b"bin", big=EntityProperty(5, EdmType.INT64), note="")),
]


@pytest.fixture
def people(table):
    for pk, rk, props in PEOPLE:
        table.create_entity({"PartitionKey": pk, "RowKey": rk, **props})
    return table


def ids(entities):
    return [f"{e['PartitionKey']}/{e['RowKey']}" for e in entities]


def q(table, flt=None, **kw):
    return ids(table.query_entities(flt, **kw)) if flt is not None else ids(table.list_entities(**kw))


def qs(table, flt, **kw):
    """Sorted ids, for filters whose result order we do not care about."""
    return sorted(q(table, flt, **kw))


# -- ordering and listing --------------------------------------------------------


def test_list_entities_returns_everything_ordered_by_partition_then_row_key(people):
    assert q(people) == ["asia/100", "eu/001", "eu/002", "eu/010", "us/001", "us/002"]


def test_query_entities_with_filter_keeps_key_order(people):
    assert q(people, "age ge 22") == ["asia/100", "eu/001", "eu/002", "eu/010", "us/001", "us/002"]


def test_results_are_ordered_by_ordinal_string_comparison_not_numeric(table):
    for rk in ["9", "10", "1", "100", "B", "a", "_", "Z"]:
        table.create_entity({"PartitionKey": "p", "RowKey": rk})
    assert [e["RowKey"] for e in table.list_entities()] == ["1", "10", "100", "9", "B", "Z", "_", "a"]


def test_results_ignore_insertion_order(table):
    for pk, rk in [("b", "2"), ("a", "9"), ("b", "1"), ("a", "1")]:
        table.create_entity({"PartitionKey": pk, "RowKey": rk})
    assert q(table) == ["a/1", "a/9", "b/1", "b/2"]


def test_non_ascii_keys_sort_by_utf16_code_unit_after_ascii(table):
    for rk in ["z", "é", "a", "Z"]:
        table.create_entity({"PartitionKey": "p", "RowKey": rk})
    assert [e["RowKey"] for e in table.list_entities()] == ["Z", "a", "z", "é"]


def test_query_of_empty_table_returns_nothing(table):
    assert q(table, "PartitionKey eq 'x'") == []
    assert q(table) == []


def test_query_of_missing_table_raises_table_not_found(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        list(service.get_table_client(table_name).query_entities("PartitionKey eq 'x'"))
    assert exc.value.status_code == 404
    assert code(exc.value) == "TableNotFound"


def test_list_entities_of_missing_table_raises_table_not_found(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        list(service.get_table_client(table_name).list_entities())
    assert code(exc.value) == "TableNotFound"


# -- comparison operators ----------------------------------------------------------


def test_eq_on_string(people):
    assert qs(people, "name eq 'Alice'") == ["eu/001"]


def test_string_comparison_is_case_sensitive(people):
    assert qs(people, "name eq 'alice'") == []
    assert qs(people, "name eq 'ALICE'") == []


def test_ne_on_string(people):
    assert qs(people, "name ne 'Alice'") == ["asia/100", "eu/002", "eu/010", "us/001", "us/002"]


@pytest.mark.parametrize(
    "op,expected",
    [
        ("eq", ["asia/100", "eu/001"]),
        ("ne", ["eu/002", "eu/010", "us/001", "us/002"]),
        ("gt", ["eu/010", "us/001"]),
        ("ge", ["asia/100", "eu/001", "eu/010", "us/001"]),
        ("lt", ["eu/002", "us/002"]),
        ("le", ["asia/100", "eu/001", "eu/002", "us/002"]),
    ],
)
def test_comparison_operators_on_int32(people, op, expected):
    assert qs(people, f"age {op} 30") == expected


def test_string_gt_and_lt_use_ordinal_order_so_uppercase_sorts_first(people):
    # 'A' < 'C' < 'D' < "O" < 'a'... < 'b' < 'e'
    assert qs(people, "name lt 'a'") == ["eu/001", "eu/010", "us/001", "us/002"]
    assert qs(people, "name ge 'a'") == ["asia/100", "eu/002"]


def test_string_comparison_is_by_ordinal_prefix_rules(people):
    assert qs(people, "name gt 'Alic' and name lt 'Alicf'") == ["eu/001"]
    assert qs(people, "name gt 'Alice'") != ["eu/001"]


def test_string_comparison_with_empty_string(people):
    assert qs(people, "note eq ''") == ["asia/100"]
    assert qs(people, "name gt ''") == ["asia/100", "eu/001", "eu/002", "eu/010", "us/001", "us/002"]


@pytest.mark.parametrize("flag,expected", [("true", ["asia/100", "eu/001", "eu/010", "us/002"]),
                                          ("false", ["eu/002", "us/001"])])
def test_eq_on_bool(people, flag, expected):
    assert qs(people, f"active eq {flag}") == expected


def test_ne_on_bool(people):
    assert qs(people, "active ne true") == ["eu/002", "us/001"]


def test_double_comparisons(people):
    assert qs(people, "score eq 2.5") == ["eu/002"]
    assert qs(people, "score lt 1.5") == ["eu/010", "us/002"]
    assert qs(people, "score ge 3.25") == ["asia/100", "us/001"]


def test_double_literal_without_fraction_digits_matches_double_property(people):
    assert qs(people, "score eq 10.0") == ["us/001"]
    assert qs(people, "score eq 0.0") == ["us/002"]


def test_negative_number_literals(people):
    assert qs(people, "score lt 0.0") == ["eu/010"]
    assert qs(people, "age gt -5") == ["asia/100", "eu/001", "eu/002", "eu/010", "us/001", "us/002"]


# -- boolean combinators ----------------------------------------------------------------


def test_and(people):
    assert qs(people, "PartitionKey eq 'eu' and age gt 28") == ["eu/001", "eu/010"]


def test_or(people):
    assert qs(people, "name eq 'Alice' or name eq 'Dave'") == ["eu/001", "us/001"]


def test_not_with_parentheses(people):
    assert qs(people, "not (age gt 30)") == ["asia/100", "eu/001", "eu/002", "us/002"]


def test_not_on_bool_comparison(people):
    assert qs(people, "not (active eq true)") == ["eu/002", "us/001"]


def test_and_binds_tighter_than_or(people):
    # a or (b and c)
    assert qs(people, "name eq 'Dave' or PartitionKey eq 'eu' and age eq 25") == ["eu/002", "us/001"]


def test_parentheses_override_precedence(people):
    # (a or b) and c
    assert qs(people, "(name eq 'Dave' or PartitionKey eq 'eu') and age eq 25") == ["eu/002"]


def test_nested_parentheses_and_not(people):
    flt = "not (PartitionKey eq 'eu' and (age lt 30 or age gt 30))"
    assert qs(people, flt) == ["asia/100", "eu/001", "us/001", "us/002"]


# -- key ranges ----------------------------------------------------------------------------


def test_partition_and_row_key_range(people):
    assert q(people, "PartitionKey eq 'eu' and RowKey ge '002'") == ["eu/002", "eu/010"]
    assert q(people, "PartitionKey eq 'eu' and RowKey gt '001' and RowKey lt '010'") == ["eu/002"]


def test_partition_key_range_across_partitions(people):
    assert q(people, "PartitionKey ge 'e' and PartitionKey lt 'v'") == ["eu/001", "eu/002", "eu/010", "us/001", "us/002"]


def test_row_key_prefix_scan_using_a_range(table):
    for rk in ["user-1", "user-2", "users", "usex", "usd", "user"]:
        table.create_entity({"PartitionKey": "p", "RowKey": rk})
    got = [e["RowKey"] for e in table.query_entities("PartitionKey eq 'p' and RowKey ge 'user' and RowKey lt 'uses'")]
    assert got == ["user", "user-1", "user-2", "users"]


def test_row_key_comparison_is_string_comparison_not_numeric(people):
    # "001" < "1" as strings, so nothing in partition eu is greater than "1".
    assert q(people, "PartitionKey eq 'eu' and RowKey gt '1'") == []
    assert q(people, "PartitionKey eq 'eu' and RowKey lt '1'") == ["eu/001", "eu/002", "eu/010"]


# -- typed literals -----------------------------------------------------------------------------


def test_int64_literal_with_l_suffix(people):
    assert qs(people, "big eq 10000000000L") == ["eu/001"]
    assert qs(people, "big eq 1099511627776L") == ["us/001"]


def test_int64_comparisons(people):
    assert qs(people, "big gt 5L") == ["eu/001", "us/001"]
    assert qs(people, "big le 0L") == ["eu/010", "us/002"]


def test_int64_negative_literal(people):
    assert qs(people, "big eq -7L") == ["eu/010"]


def test_datetime_literal_eq(people):
    assert qs(people, "born eq datetime'2000-01-01T00:00:00Z'") == ["eu/001"]


def test_datetime_literal_with_microseconds(people):
    assert qs(people, "born eq datetime'2000-01-01T00:00:00.500000Z'") == ["asia/100"]


def test_datetime_literal_ranges(people):
    assert qs(people, "born ge datetime'2000-01-01T00:00:00Z'") == ["asia/100", "eu/001", "us/002"]
    assert qs(people, "born lt datetime'1990-01-01T00:00:00Z'") == ["us/001"]
    got = qs(people, "born gt datetime'1990-01-01T00:00:00Z' and born lt datetime'2000-01-01T00:00:00Z'")
    assert got == ["eu/002", "eu/010"]


def test_guid_literal(people):
    assert qs(people, f"uid eq guid'{U1}'") == ["eu/001", "eu/010", "us/002"]
    assert qs(people, f"uid ne guid'{U1}'") == ["asia/100", "eu/002", "us/001"]


def test_binary_literal_with_x_prefix(people):
    assert qs(people, "blob eq X'61'") == ["eu/001", "us/001"]
    assert qs(people, "blob eq X'62696e'") == ["asia/100", "eu/002"]


def test_binary_literal_with_binary_prefix(people):
    assert qs(people, "blob eq binary'61'") == ["eu/001", "us/001"]


def test_binary_literal_with_non_text_bytes(people):
    assert qs(people, "blob eq X'00ff'") == ["eu/010"]


def test_timestamp_system_property_can_be_filtered(people):
    assert len(qs(people, "Timestamp ge datetime'2000-01-01T00:00:00Z'")) == 6
    assert qs(people, "Timestamp lt datetime'2000-01-01T00:00:00Z'") == []


# -- strings with quotes and unicode ---------------------------------------------------------------


def test_quote_escaped_string_literal(people):
    assert qs(people, "name eq 'O''Brien'") == ["us/002"]


def test_unescaped_quote_in_literal_is_a_syntax_error(people):
    with pytest.raises(HttpResponseError) as exc:
        qs(people, "name eq 'O'Brien'")
    assert exc.value.status_code == 400
    assert code(exc.value) == "InvalidInput"


def test_filter_cannot_be_injected_through_an_escaped_quote(people):
    hostile = "x' or name ne 'x"
    escaped = hostile.replace("'", "''")
    assert qs(people, f"name eq '{escaped}'") == []


def test_unicode_in_filter_literal(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "1", "name": "héllo 你好"})
    table.create_entity({"PartitionKey": "p", "RowKey": "2", "name": "hello"})
    assert q(table, "name eq 'héllo 你好'") == ["p/1"]


def test_literal_with_percent_and_plus_and_space_matches_exactly(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "1", "s": "a+b c%20d&e=f"})
    table.create_entity({"PartitionKey": "p", "RowKey": "2", "s": "a b c d"})
    assert q(table, "s eq 'a+b c%20d&e=f'") == ["p/1"]


# -- properties that some entities lack --------------------------------------------------------------


def test_eq_on_property_some_entities_lack_returns_only_those_that_have_it(people):
    assert qs(people, "note eq 'n1'") == ["eu/001"]


def test_ge_on_property_some_entities_lack_excludes_those_without_it(people):
    assert qs(people, "note ge 'n'") == ["eu/001", "eu/010", "us/002"]


def test_filter_on_property_nobody_has_returns_nothing(people):
    assert qs(people, "ghost eq 'x'") == []


def test_or_with_missing_property_still_returns_the_other_branch(people):
    assert qs(people, "ghost eq 'x' or PartitionKey eq 'us'") == ["us/001", "us/002"]


# -- parameters ---------------------------------------------------------------------------------------


def test_parameter_string(people):
    assert qs(people, "name eq @n", parameters={"n": "Alice"}) == ["eu/001"]


def test_parameter_string_with_quote_is_escaped_for_you(people):
    assert qs(people, "name eq @n", parameters={"n": "O'Brien"}) == ["us/002"]


def test_parameter_injection_attempt_is_just_a_string(people):
    assert qs(people, "name eq @n", parameters={"n": "x' or name ne 'x"}) == []


def test_parameter_int(people):
    assert qs(people, "age gt @a", parameters={"a": 30}) == ["eu/010", "us/001"]


def test_parameter_large_int_becomes_int64_literal(people):
    assert qs(people, "big eq @b", parameters={"b": 10_000_000_000}) == ["eu/001"]


def test_parameter_float(people):
    assert qs(people, "score eq @s", parameters={"s": 2.5}) == ["eu/002"]


def test_parameter_bool(people):
    assert qs(people, "active eq @a", parameters={"a": False}) == ["eu/002", "us/001"]


def test_parameter_datetime(people):
    assert qs(people, "born eq @d", parameters={"d": utc(2000, 1, 1)}) == ["eu/001"]


def test_parameter_uuid(people):
    assert qs(people, "uid eq @u", parameters={"u": U1}) == ["eu/001", "eu/010", "us/002"]


def test_parameter_bytes(people):
    assert qs(people, "blob eq @b", parameters={"b": b"bin"}) == ["asia/100", "eu/002"]


def test_several_parameters_in_one_filter(people):
    flt = "PartitionKey eq @pk and age ge @lo and active eq @on"
    assert qs(people, flt, parameters={"pk": "eu", "lo": 30, "on": True}) == ["eu/001", "eu/010"]


# -- select -----------------------------------------------------------------------------------------------


def test_select_returns_only_the_requested_properties(people):
    got = list(people.query_entities("PartitionKey eq 'eu' and RowKey eq '001'", select=["name", "age"]))
    assert got == [{"name": "Alice", "age": 30}]


def test_select_with_keys(people):
    got = list(people.query_entities("PartitionKey eq 'us' and RowKey eq '001'", select=["PartitionKey", "RowKey", "name"]))
    assert got == [{"PartitionKey": "us", "RowKey": "001", "name": "Dave"}]


def test_select_on_list_entities(people):
    got = [dict(e) for e in people.list_entities(select=["age"])]
    assert sorted(e["age"] for e in got) == [22, 25, 30, 30, 35, 40]
    assert all(set(e) == {"age"} for e in got)


def test_select_keeps_type_information(people):
    got = list(people.query_entities("RowKey eq '010'", select=["uid", "blob", "born", "big"]))[0]
    assert got["uid"] == U1
    assert got["blob"] == b"\x00\xff"
    assert got["born"] == utc(1990, 12, 31, 23, 59, 59)
    assert got["big"].value == -7


def test_select_does_not_affect_filtering_on_other_properties(people):
    got = sorted(e["name"] for e in people.query_entities("age eq 30", select=["name"]))
    assert got == ["Alice", "eve"]


# -- paging ------------------------------------------------------------------------------------------------


def test_results_per_page_splits_results_into_pages(people):
    pages = [ids(p) for p in people.list_entities(results_per_page=4).by_page()]
    assert pages == [["asia/100", "eu/001", "eu/002", "eu/010"], ["us/001", "us/002"]]


def test_results_per_page_exact_multiple_has_no_empty_trailing_page(people):
    pages = [ids(p) for p in people.list_entities(results_per_page=3).by_page()]
    assert pages == [["asia/100", "eu/001", "eu/002"], ["eu/010", "us/001", "us/002"]]


def test_results_per_page_larger_than_result_is_one_page(people):
    pages = [ids(p) for p in people.list_entities(results_per_page=50).by_page()]
    assert len(pages) == 1
    assert len(pages[0]) == 6


def test_iterating_with_results_per_page_still_yields_everything_in_order(people):
    assert q(people, results_per_page=1) == ["asia/100", "eu/001", "eu/002", "eu/010", "us/001", "us/002"]


def test_query_entities_results_per_page_with_filter(people):
    pages = [ids(p) for p in people.query_entities("PartitionKey ne 'asia'", results_per_page=2).by_page()]
    assert pages == [["eu/001", "eu/002"], ["eu/010", "us/001"], ["us/002"]]


def test_page_boundary_can_fall_between_partitions(people):
    first = next(people.list_entities(results_per_page=1).by_page())
    assert ids(first) == ["asia/100"]


def test_continuation_token_resumes_after_the_last_returned_entity(people):
    pager = people.list_entities(results_per_page=2).by_page()
    assert ids(next(pager)) == ["asia/100", "eu/001"]
    token = pager.continuation_token
    assert token
    resumed = people.list_entities(results_per_page=2).by_page(continuation_token=token)
    assert ids(next(resumed)) == ["eu/002", "eu/010"]


def test_continuation_token_has_a_next_partition_key_and_row_key(people):
    pager = people.list_entities(results_per_page=2).by_page()
    next(pager)
    token = pager.continuation_token
    assert set(token) >= {"PartitionKey", "RowKey"}
    assert token["PartitionKey"] and token["RowKey"]


def test_last_page_has_no_continuation_token(people):
    pager = people.list_entities(results_per_page=6).by_page()
    next(pager)
    assert not pager.continuation_token


def test_paging_with_a_filter_resumes_in_the_filtered_set(people):
    flt = "active eq true"
    pager = people.query_entities(flt, results_per_page=2).by_page()
    assert ids(next(pager)) == ["asia/100", "eu/001"]
    resumed = people.query_entities(flt, results_per_page=2).by_page(continuation_token=pager.continuation_token)
    assert [ids(p) for p in resumed] == [["eu/010", "us/002"]]


def test_a_filter_that_matches_nothing_gives_no_entities_even_when_paging(people):
    pages = [list(p) for p in people.query_entities("name eq 'nobody'", results_per_page=2).by_page()]
    assert [e for page in pages for e in page] == []


def test_default_page_holds_at_most_1000_entities(table):
    # Real Azure caps a page at 1000 entities and hands back a continuation token.
    for start in range(0, 1001, 100):
        ops = [("create", {"PartitionKey": "p", "RowKey": f"{i:05d}"}) for i in range(start, min(start + 100, 1001))]
        table.submit_transaction(ops)
    pages = [ids(p) for p in table.list_entities().by_page()]
    assert [len(p) for p in pages] == [1000, 1]
    assert pages[1] == ["p/01000"]


# -- invalid filters -----------------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "bad",
    [
        "aaa bbb ccc",
        "name eq",
        "(name eq 'x'",
        "name eq 'x')",
        "name equals 'x'",
        "eq 'x'",
        "name eq 'x' and",
        "and name eq 'x'",
    ],
)
def test_invalid_filter_is_400_invalid_input(people, bad):
    with pytest.raises(HttpResponseError) as exc:
        list(people.query_entities(bad))
    assert exc.value.status_code == 400
    assert code(exc.value) == "InvalidInput"


@pytest.mark.not_on_azurite(reason="Azurite accepts an unterminated string literal instead of answering 400")
def test_unterminated_string_literal_is_400_invalid_input(people):
    with pytest.raises(HttpResponseError) as exc:
        list(people.query_entities("name eq 'unterminated"))
    assert exc.value.status_code == 400
    assert code(exc.value) == "InvalidInput"


def test_invalid_filter_on_empty_table_is_still_400(table):
    with pytest.raises(HttpResponseError) as exc:
        list(table.query_entities("aaa bbb ccc"))
    assert exc.value.status_code == 400
