"""Entity group transactions (submit_transaction): atomicity, limits, etag conditions."""

import uuid
from datetime import datetime, timezone

import pytest
from azure.core import MatchConditions
from azure.data.tables import EdmType, EntityProperty, TableTransactionError, UpdateMode


def e(rk, pk="p", **props):
    return {"PartitionKey": pk, "RowKey": rk, **props}


def row_keys(table):
    return [x["RowKey"] for x in table.list_entities()]


# -- success paths ------------------------------------------------------------------


def test_transaction_creates_several_entities(table):
    results = table.submit_transaction([("create", e("1", v=1)), ("create", e("2", v=2)), ("create", e("3", v=3))])
    assert len(results) == 3
    assert row_keys(table) == ["1", "2", "3"]
    assert table.get_entity("p", "2")["v"] == 2


def test_transaction_results_have_etags_in_operation_order(table):
    results = table.submit_transaction([("create", e("1")), ("create", e("2"))])
    assert table.get_entity("p", "1").metadata["etag"] == results[0]["etag"]
    assert table.get_entity("p", "2").metadata["etag"] == results[1]["etag"]


def test_transaction_with_a_single_operation(table):
    results = table.submit_transaction([("create", e("1"))])
    assert len(results) == 1
    assert row_keys(table) == ["1"]


def test_transaction_mixes_create_update_upsert_and_delete(table):
    table.create_entity(e("upd", v=1))
    table.create_entity(e("del", v=1))
    table.create_entity(e("ups", v=1))
    table.submit_transaction([
        ("create", e("new", v=1)),
        ("update", e("upd", v=2), {"mode": UpdateMode.MERGE}),
        ("upsert", e("ups", v=2), {"mode": UpdateMode.REPLACE}),
        ("upsert", e("ups2", v=1)),
        ("delete", e("del")),
    ])
    assert row_keys(table) == ["new", "upd", "ups", "ups2"]
    assert table.get_entity("p", "upd")["v"] == 2
    assert table.get_entity("p", "ups")["v"] == 2


def test_transaction_update_merge_keeps_properties_and_replace_drops_them(table):
    table.create_entity(e("m", a=1, b=2))
    table.create_entity(e("r", a=1, b=2))
    table.submit_transaction([
        ("update", e("m", b=20), {"mode": UpdateMode.MERGE}),
        ("update", e("r", b=20), {"mode": UpdateMode.REPLACE}),
    ])
    assert (table.get_entity("p", "m")["a"], table.get_entity("p", "m")["b"]) == (1, 20)
    assert "a" not in table.get_entity("p", "r")


def test_transaction_update_default_mode_is_merge(table):
    table.create_entity(e("1", a=1, b=2))
    table.submit_transaction([("update", e("1", b=20))])
    got = table.get_entity("p", "1")
    assert (got["a"], got["b"]) == (1, 20)


def test_transaction_upsert_creates_and_overwrites(table):
    table.create_entity(e("old", v=1))
    table.submit_transaction([("upsert", e("old", v=2)), ("upsert", e("fresh", v=1))])
    assert table.get_entity("p", "old")["v"] == 2
    assert table.get_entity("p", "fresh")["v"] == 1


def test_transaction_delete_removes_entities(table):
    for rk in "abc":
        table.create_entity(e(rk))
    table.submit_transaction([("delete", e("a")), ("delete", e("c"))])
    assert row_keys(table) == ["b"]


def test_transaction_of_exactly_100_operations_is_accepted(table):
    results = table.submit_transaction([("create", e(f"{i:03d}")) for i in range(100)])
    assert len(results) == 100
    assert len(row_keys(table)) == 100


def test_transaction_with_special_character_keys(table):
    pk = "O'Brien & \"co\" 100%"
    table.submit_transaction([("create", e("a b", pk=pk)), ("create", e("c'd", pk=pk))])
    assert table.get_entity(pk, "c'd")["RowKey"] == "c'd"


def test_transaction_can_carry_all_property_types(table):
    g = uuid.uuid4()
    when = datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    table.submit_transaction([("create", e("t", s="x", i=1, f=1.5, b=True, g=g, d=when, raw=b"\x01",
                                           big=EntityProperty(1 << 40, EdmType.INT64)))])
    got = table.get_entity("p", "t")
    assert (got["s"], got["i"], got["f"], got["b"], got["g"], got["d"], got["raw"]) == ("x", 1, 1.5, True, g, when, b"\x01")
    assert got["big"].value == 1 << 40


def test_transaction_operations_can_be_a_generator(table):
    table.submit_transaction(("create", e(str(i))) for i in range(3))
    assert row_keys(table) == ["0", "1", "2"]


# -- atomicity -----------------------------------------------------------------------------


def test_duplicate_create_fails_with_entity_already_exists_and_its_index(table):
    table.create_entity(e("taken"))
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e("a")), ("create", e("b")), ("create", e("taken"))])
    assert exc.value.status_code == 409
    assert exc.value.error_code == "EntityAlreadyExists"
    assert exc.value.index == 2


def test_failed_transaction_applies_nothing(table):
    table.create_entity(e("taken"))
    with pytest.raises(TableTransactionError):
        table.submit_transaction([("create", e("a")), ("create", e("b")), ("create", e("taken"))])
    assert row_keys(table) == ["taken"]


def test_failed_transaction_does_not_apply_earlier_updates_or_deletes(table):
    table.create_entity(e("keep", v=1))
    table.create_entity(e("gone", v=1))
    before = table.get_entity("p", "keep").metadata["etag"]
    with pytest.raises(TableTransactionError):
        table.submit_transaction([
            ("update", e("keep", v=2)),
            ("delete", e("gone")),
            ("update", e("missing", v=1)),  # fails, so the two ops above must be undone
        ])
    assert table.get_entity("p", "keep")["v"] == 1
    assert table.get_entity("p", "keep").metadata["etag"] == before
    assert table.get_entity("p", "gone")["v"] == 1


def test_update_of_missing_entity_fails_the_transaction_with_404(table):
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e("a")), ("update", e("missing", v=1))])
    assert exc.value.status_code == 404
    assert exc.value.error_code == "ResourceNotFound"
    assert row_keys(table) == []


def test_delete_of_missing_entity_fails_the_transaction_with_404(table):
    table.create_entity(e("exists"))
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("delete", e("exists")), ("delete", e("missing"))])
    assert exc.value.status_code == 404
    assert exc.value.error_code == "ResourceNotFound"
    assert row_keys(table) == ["exists"]


def test_transaction_on_missing_table_is_table_not_found(service, table_name):
    client = service.get_table_client(table_name)
    with pytest.raises(TableTransactionError) as exc:
        client.submit_transaction([("create", e("a"))])
    assert exc.value.status_code == 404
    assert exc.value.error_code == "TableNotFound"


def test_invalid_key_character_fails_the_whole_transaction(table):
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e("ok")), ("create", e("bad/key"))])
    assert exc.value.status_code == 400
    assert row_keys(table) == []


# -- rules about what a transaction may contain ---------------------------------------------------


def test_operations_with_different_partition_keys_are_rejected_by_the_sdk(table):
    with pytest.raises(ValueError, match="Partition Keys"):
        table.submit_transaction([("create", e("a", pk="p1")), ("create", e("a", pk="p2"))])
    assert row_keys(table) == []


def test_same_entity_twice_in_a_transaction_is_invalid_duplicate_row(table):
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e("a")), ("upsert", e("a", v=2))])
    assert exc.value.status_code == 400
    assert exc.value.error_code == "InvalidDuplicateRow"
    assert exc.value.index == 1
    assert row_keys(table) == []


def test_creating_the_same_entity_twice_in_a_transaction_is_invalid_duplicate_row(table):
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e("a")), ("create", e("a"))])
    assert exc.value.error_code == "InvalidDuplicateRow"


def test_update_and_delete_of_the_same_entity_is_invalid_duplicate_row(table):
    table.create_entity(e("a"))
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("update", e("a", v=1)), ("delete", e("a"))])
    assert exc.value.error_code == "InvalidDuplicateRow"
    assert table.get_entity("p", "a")["RowKey"] == "a"


@pytest.mark.not_on_azurite(reason="Azurite answers a 101-operation transaction with a malformed response")
def test_more_than_100_operations_is_rejected_and_applies_nothing(table):
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("create", e(f"{i:03d}")) for i in range(101)])
    assert exc.value.status_code == 400
    assert exc.value.error_code == "InvalidInput"
    assert "100" in exc.value.message
    assert row_keys(table) == []


# -- etag conditions inside a transaction ---------------------------------------------------------------


def test_update_with_matching_etag_inside_transaction(table):
    created = table.create_entity(e("a", v=1))
    table.submit_transaction([("update", e("a", v=2), {"etag": created["etag"], "match_condition": MatchConditions.IfNotModified})])
    assert table.get_entity("p", "a")["v"] == 2


def test_update_with_stale_etag_fails_the_transaction_with_412(table):
    created = table.create_entity(e("a", v=1))
    table.update_entity(e("a", v=2))
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([
            ("create", e("b")),
            ("update", e("a", v=3), {"etag": created["etag"], "match_condition": MatchConditions.IfNotModified}),
        ])
    assert exc.value.status_code == 412
    assert exc.value.error_code == "UpdateConditionNotSatisfied"
    assert row_keys(table) == ["a"]
    assert table.get_entity("p", "a")["v"] == 2


def test_delete_with_stale_etag_fails_the_transaction_with_412(table):
    created = table.create_entity(e("a", v=1))
    table.update_entity(e("a", v=2))
    with pytest.raises(TableTransactionError) as exc:
        table.submit_transaction([("delete", e("a"), {"etag": created["etag"], "match_condition": MatchConditions.IfNotModified})])
    assert exc.value.status_code == 412
    assert exc.value.error_code == "UpdateConditionNotSatisfied"
    assert table.get_entity("p", "a")["v"] == 2


def test_delete_with_matching_etag_inside_transaction(table):
    created = table.create_entity(e("a"))
    table.submit_transaction([("delete", e("a"), {"etag": created["etag"], "match_condition": MatchConditions.IfNotModified})])
    assert row_keys(table) == []


def test_transaction_updates_change_the_etag(table):
    created = table.create_entity(e("a", v=1))
    results = table.submit_transaction([("update", e("a", v=2))])
    assert results[0]["etag"] != created["etag"]
    assert table.get_entity("p", "a").metadata["etag"] == results[0]["etag"]


def test_transaction_failure_leaves_etags_untouched(table):
    created = table.create_entity(e("a", v=1))
    with pytest.raises(TableTransactionError):
        table.submit_transaction([("update", e("a", v=2)), ("update", e("missing", v=1))])
    assert table.get_entity("p", "a").metadata["etag"] == created["etag"]
