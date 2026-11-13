"""PartitionKey / RowKey values: URL encoding, quotes, empties, forbidden characters."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import UpdateMode

from tableutil import code

# Keys that are legal but awkward in URLs, OData literals, or both.
AWKWARD_KEYS = [
    "with space",
    "  leading and trailing  ",
    "O'Brien",
    "a''b",
    "100%",
    "%41%20",
    "a+b",
    "a&b=c;d,e",
    'say "hi"',
    "(x) [y] {z} @ : |",
    "héllo 你好 🙂",
]


@pytest.mark.parametrize("key", AWKWARD_KEYS)
def test_awkward_partition_key_survives_create_get_update_delete(table, key):
    table.create_entity({"PartitionKey": key, "RowKey": "r", "v": 1})
    got = table.get_entity(key, "r")
    assert got["PartitionKey"] == key
    table.update_entity({"PartitionKey": key, "RowKey": "r", "v": 2})
    table.upsert_entity({"PartitionKey": key, "RowKey": "r", "w": 3}, mode=UpdateMode.MERGE)
    got = table.get_entity(key, "r")
    assert (got["v"], got["w"]) == (2, 3)
    table.delete_entity(key, "r")
    with pytest.raises(ResourceNotFoundError):
        table.get_entity(key, "r")


@pytest.mark.parametrize("key", AWKWARD_KEYS)
def test_awkward_row_key_survives_create_get_update_delete(table, key):
    table.create_entity({"PartitionKey": "p", "RowKey": key, "v": 1})
    got = table.get_entity("p", key)
    assert got["RowKey"] == key
    table.update_entity({"PartitionKey": "p", "RowKey": key, "v": 2}, mode=UpdateMode.REPLACE)
    assert table.get_entity("p", key)["v"] == 2
    table.delete_entity("p", key)
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", key)


@pytest.mark.parametrize("key", AWKWARD_KEYS)
def test_awkward_keys_can_be_found_with_an_escaped_filter(table, key):
    table.create_entity({"PartitionKey": key, "RowKey": key, "v": 1})
    table.create_entity({"PartitionKey": "other", "RowKey": "other", "v": 2})
    escaped = key.replace("'", "''")
    got = list(table.query_entities(f"PartitionKey eq '{escaped}' and RowKey eq '{escaped}'"))
    assert [(e["PartitionKey"], e["RowKey"]) for e in got] == [(key, key)]


def test_keys_differing_only_in_case_are_different_entities(table):
    table.create_entity({"PartitionKey": "Key", "RowKey": "r", "v": 1})
    table.create_entity({"PartitionKey": "key", "RowKey": "r", "v": 2})
    assert table.get_entity("Key", "r")["v"] == 1
    assert table.get_entity("key", "r")["v"] == 2


def test_keys_that_differ_only_in_trailing_space_are_different_entities(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    table.create_entity({"PartitionKey": "p", "RowKey": "r ", "v": 2})
    assert table.get_entity("p", "r")["v"] == 1
    assert table.get_entity("p", "r ")["v"] == 2


def test_single_quote_key_is_not_found_under_its_unescaped_neighbour(table):
    table.create_entity({"PartitionKey": "O'Brien", "RowKey": "r"})
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("OBrien", "r")


# -- empty keys -----------------------------------------------------------------------


def test_empty_partition_key_is_allowed(table):
    table.create_entity({"PartitionKey": "", "RowKey": "r", "v": 1})
    got = table.get_entity("", "r")
    assert got["PartitionKey"] == ""
    assert got["v"] == 1


def test_empty_row_key_is_allowed(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "", "v": 1})
    got = table.get_entity("p", "")
    assert got["RowKey"] == ""
    assert got["v"] == 1


def test_both_keys_empty_is_allowed(table):
    table.create_entity({"PartitionKey": "", "RowKey": "", "v": 1})
    assert table.get_entity("", "")["v"] == 1


def test_empty_keys_can_be_updated_and_deleted(table):
    table.create_entity({"PartitionKey": "", "RowKey": "", "v": 1})
    table.update_entity({"PartitionKey": "", "RowKey": "", "v": 2})
    assert table.get_entity("", "")["v"] == 2
    table.delete_entity("", "")
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("", "")


def test_empty_keys_sort_before_everything_else(table):
    table.create_entity({"PartitionKey": "a", "RowKey": "r"})
    table.create_entity({"PartitionKey": "", "RowKey": "r"})
    table.create_entity({"PartitionKey": "a", "RowKey": ""})
    got = [(e["PartitionKey"], e["RowKey"]) for e in table.list_entities()]
    assert got == [("", "r"), ("a", ""), ("a", "r")]


def test_empty_partition_key_is_found_by_filter(table):
    table.create_entity({"PartitionKey": "", "RowKey": "r"})
    table.create_entity({"PartitionKey": "x", "RowKey": "r"})
    got = list(table.query_entities("PartitionKey eq ''"))
    assert [e["PartitionKey"] for e in got] == [""]


# -- forbidden characters -------------------------------------------------------------


FORBIDDEN = ["a/b", "a\\b", "a#b", "a?b", "a\tb"]


@pytest.mark.parametrize("key", FORBIDDEN, ids=lambda k: repr(k))
def test_forbidden_character_in_partition_key_is_rejected(table, key):
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({"PartitionKey": key, "RowKey": "r"})
    assert exc.value.status_code == 400
    assert code(exc.value) in ("InvalidInput", "OutOfRangeInput")


@pytest.mark.parametrize("key", FORBIDDEN, ids=lambda k: repr(k))
def test_forbidden_character_in_row_key_is_rejected(table, key):
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({"PartitionKey": "p", "RowKey": key})
    assert exc.value.status_code == 400
    assert code(exc.value) in ("InvalidInput", "OutOfRangeInput")


def test_rejected_key_does_not_create_an_entity(table):
    with pytest.raises(HttpResponseError):
        table.create_entity({"PartitionKey": "a/b", "RowKey": "r"})
    assert list(table.list_entities()) == []


def test_key_of_512_characters_is_allowed(table):
    key = "k" * 512
    table.create_entity({"PartitionKey": key, "RowKey": key})
    assert table.get_entity(key, key)["PartitionKey"] == key


def test_key_of_more_than_512_characters_is_rejected(table):
    # Keys are limited to 1 KiB of UTF-16, i.e. 512 characters.
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({"PartitionKey": "k" * 513, "RowKey": "r"})
    assert exc.value.status_code == 400
