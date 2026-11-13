"""Partition key value types, the "none" partition and hierarchical partition keys."""

import pytest
from azure.cosmos import PartitionKey
from azure.cosmos.exceptions import CosmosResourceNotFoundError
from azure.cosmos.partition_key import NonePartitionKeyValue, NullPartitionKeyValue

from cosmosutil import ids, run, uid


# ---- value types -----------------------------------------------------------
@pytest.mark.parametrize("value", ["text", "", 7, 7.5, -3, True, False, "unicode-é中"])
def test_partition_key_value_types_round_trip(container, value):
    container.create_item({"id": "a", "pk": value})
    assert container.read_item("a", value)["pk"] == value


@pytest.mark.not_on_vnext(reason="vnext treats number 5 and string five as the same partition key")
def test_string_and_number_partition_keys_are_different_partitions(container):
    container.create_item({"id": "a", "pk": 5, "n": 1})
    container.create_item({"id": "a", "pk": "5", "n": 2})
    assert container.read_item("a", 5)["n"] == 1
    assert container.read_item("a", "5")["n"] == 2


def test_bool_and_number_partition_keys_are_different_partitions(container):
    container.create_item({"id": "a", "pk": True})
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", 1)


def test_null_partition_key(container):
    container.create_item({"id": "a", "pk": None})
    assert container.read_item("a", None)["pk"] is None
    assert container.read_item("a", NullPartitionKeyValue)["pk"] is None


def test_null_partition_key_is_not_the_string_null(container):
    container.create_item({"id": "a", "pk": None})
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", "null")


def test_missing_partition_key_field_lands_in_none_partition(container):
    # Microsoft test_partition_key: documents without the key are read with NonePartitionKeyValue
    created = container.create_item({"id": "a", "v": 1})
    assert "pk" not in created
    assert container.read_item("a", NonePartitionKeyValue)["v"] == 1


def test_none_partition_documents_cannot_be_read_with_a_string_key(container):
    container.create_item({"id": "a"})
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", "anything")


def test_none_partition_replace_upsert_delete(container):
    container.create_item({"id": "a"})
    container.replace_item("a", {"id": "a", "v": 2})
    container.upsert_item({"id": "a", "v": 3})
    assert container.read_item("a", NonePartitionKeyValue)["v"] == 3
    container.delete_item("a", NonePartitionKeyValue)
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", NonePartitionKeyValue)


def test_none_partition_patch(container):
    container.create_item({"id": "a"})
    got = container.patch_item("a", NonePartitionKeyValue, [{"op": "add", "path": "/c", "value": "red"}])
    assert got["c"] == "red"


def test_query_none_partition_only_returns_documents_without_the_key(container):
    container.create_item({"id": "nokey"})
    container.create_item({"id": "withkey", "pk": "x"})
    got = run(container, "SELECT c.id FROM c", partition_key=NonePartitionKeyValue)
    assert ids(got) == ["nokey"]


def test_query_null_partition_returns_only_null_documents(container):
    container.create_item({"id": "n", "pk": None})
    container.create_item({"id": "s", "pk": "x"})
    got = run(container, "SELECT c.id FROM c", partition_key=NullPartitionKeyValue)
    assert ids(got) == ["n"]


def test_partition_key_on_nested_missing_path_is_none_partition(make_container):
    c = make_container("/address/state")
    c.create_item({"id": "a", "address": {"city": "x"}})
    assert c.read_item("a", NonePartitionKeyValue)["id"] == "a"


def test_partition_key_extracted_from_document_on_upsert(container):
    container.upsert_item({"id": "a", "pk": "x", "v": 1})
    container.upsert_item({"id": "a", "pk": "y", "v": 2})
    assert container.read_item("a", "x")["v"] == 1 and container.read_item("a", "y")["v"] == 2


def test_cross_partition_query_includes_null_and_none_partitions(container):
    container.create_item({"id": "n", "pk": None})
    container.create_item({"id": "u"})
    container.create_item({"id": "s", "pk": "x"})
    assert ids(run(container, "SELECT c.id FROM c")) == ["n", "s", "u"]


# ---- hierarchical partition keys --------------------------------------------
@pytest.fixture
def hier(make_container):
    c = make_container(PartitionKey(path=["/state", "/city", "/zip"], kind="MultiHash"))
    docs = [
        ("1", "WA", "Seattle", "98101"),
        ("2", "WA", "Seattle", "98102"),
        ("3", "WA", "Redmond", "98052"),
        ("4", "OR", "Portland", "97201"),
        ("5", "OR", "Portland", "97202"),
    ]
    for i, s, ci, z in docs:
        c.create_item({"id": i, "state": s, "city": ci, "zip": z})
    return c


def test_hierarchical_read_with_full_key(hier):
    assert hier.read_item("1", ["WA", "Seattle", "98101"])["id"] == "1"


def test_hierarchical_read_with_wrong_key_is_404(hier):
    with pytest.raises(CosmosResourceNotFoundError):
        hier.read_item("1", ["WA", "Seattle", "99999"])


def test_hierarchical_same_id_in_different_full_keys(hier):
    hier.create_item({"id": "1", "state": "WA", "city": "Seattle", "zip": "98102", "other": True})
    assert hier.read_item("1", ["WA", "Seattle", "98102"])["other"] is True
    assert "other" not in hier.read_item("1", ["WA", "Seattle", "98101"])


def test_hierarchical_replace_upsert_delete(hier):
    key = ["WA", "Seattle", "98101"]
    hier.replace_item("1", {"id": "1", "state": "WA", "city": "Seattle", "zip": "98101", "v": 1})
    hier.upsert_item({"id": "1", "state": "WA", "city": "Seattle", "zip": "98101", "v": 2})
    assert hier.read_item("1", key)["v"] == 2
    hier.delete_item("1", key)
    with pytest.raises(CosmosResourceNotFoundError):
        hier.read_item("1", key)


def test_hierarchical_patch(hier):
    got = hier.patch_item("1", ["WA", "Seattle", "98101"], [{"op": "add", "path": "/v", "value": 1}])
    assert got["v"] == 1


def test_hierarchical_query_with_full_key(hier):
    got = run(hier, "SELECT c.id FROM c", partition_key=["WA", "Seattle", "98101"])
    assert ids(got) == ["1"]


def test_hierarchical_query_with_one_level_prefix(hier):
    got = run(hier, "SELECT c.id FROM c", partition_key=["WA"])
    assert ids(got) == ["1", "2", "3"]


def test_hierarchical_query_with_two_level_prefix(hier):
    got = run(hier, "SELECT c.id FROM c", partition_key=["WA", "Seattle"])
    assert ids(got) == ["1", "2"]


def test_hierarchical_cross_partition_query(hier):
    assert ids(run(hier, "SELECT c.id FROM c")) == ["1", "2", "3", "4", "5"]


def test_hierarchical_cross_partition_aggregate_and_order(hier):
    assert run(hier, "SELECT VALUE COUNT(1) FROM c") == [5]
    assert run(hier, "SELECT VALUE c.zip FROM c ORDER BY c.zip") == ["97201", "97202", "98052", "98101", "98102"]


def test_hierarchical_prefix_query_with_filter(hier):
    got = run(hier, "SELECT c.id FROM c WHERE c.zip = '98102'", partition_key=["WA"])
    assert ids(got) == ["2"]


def test_hierarchical_two_levels(make_container):
    c = make_container(PartitionKey(path=["/tenant", "/user"], kind="MultiHash"))
    c.create_item({"id": "a", "tenant": "t1", "user": "u1"})
    c.create_item({"id": "b", "tenant": "t1", "user": "u2"})
    assert c.read_item("a", ["t1", "u1"])["id"] == "a"
    assert ids(run(c, "SELECT c.id FROM c", partition_key=["t1"])) == ["a", "b"]


def test_hierarchical_key_with_numbers_and_booleans(make_container):
    c = make_container(PartitionKey(path=["/a", "/b"], kind="MultiHash"))
    c.create_item({"id": "x", "a": 1, "b": True})
    assert c.read_item("x", [1, True])["id"] == "x"
