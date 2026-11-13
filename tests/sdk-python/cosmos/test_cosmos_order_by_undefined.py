"""ORDER BY and documents that lack the sort property.

Azure serves ORDER BY from the index. Microsoft Learn, "Indexing policies": "If an explicitly indexed
path doesn't exist in an item, a value is added to the index to indicate that the path is undefined"
(the same for composite indexes). So:
  * default policy (`/*`), one field: documents without the field are left out;
  * the field indexed explicitly (`/n/?`), or several fields (composite index): they are returned,
    undefined sorting before null, booleans, numbers and strings (ascending), last when descending.
"""

import pytest
from azure.cosmos import PartitionKey

from cosmosutil import idlist

DOCS = [
    {"id": "undef", "pk": "p"},
    {"id": "null", "pk": "p", "n": None},
    {"id": "false", "pk": "q", "n": False},
    {"id": "true", "pk": "q", "n": True},
    {"id": "one", "pk": "p", "n": 1},
    {"id": "two", "pk": "q", "n": 2},
    {"id": "str", "pk": "p", "n": "a"},
]
EXPLICIT = {
    "indexingMode": "consistent",
    "includedPaths": [{"path": "/n/?"}, {"path": "/pk/?"}],
    "excludedPaths": [{"path": "/*"}],
}
ASC = ["undef", "null", "false", "true", "one", "two", "str"]


def load(make_container, policy=None):
    kw = {"indexing_policy": policy} if policy else {}
    c = make_container("/pk", **kw)
    for d in DOCS:
        c.create_item(d)
    return c


def q(c, sql, **kw):
    if "partition_key" not in kw:
        kw["enable_cross_partition_query"] = True
    return idlist(c.query_items(sql, **kw))


def test_default_policy_leaves_out_documents_without_the_field(make_container):
    c = load(make_container)
    assert q(c, "SELECT * FROM c ORDER BY c.n") == ASC[1:]
    assert q(c, "SELECT * FROM c ORDER BY c.n DESC") == ASC[1:][::-1]


def test_default_policy_single_partition(make_container):
    c = load(make_container)
    assert q(c, "SELECT * FROM c ORDER BY c.n", partition_key="p") == ["null", "one", "str"]


def test_explicitly_indexed_path_returns_them_first_ascending(make_container):
    c = load(make_container, EXPLICIT)
    assert q(c, "SELECT * FROM c ORDER BY c.n") == ASC
    assert q(c, "SELECT * FROM c ORDER BY c.n ASC") == ASC


def test_explicitly_indexed_path_returns_them_last_descending(make_container):
    c = load(make_container, EXPLICIT)
    assert q(c, "SELECT * FROM c ORDER BY c.n DESC") == ASC[::-1]


def test_explicitly_indexed_path_single_partition_and_paging(make_container):
    c = load(make_container, EXPLICIT)
    assert q(c, "SELECT * FROM c ORDER BY c.n", partition_key="p") == ["undef", "null", "one", "str"]
    pages = list(c.query_items("SELECT * FROM c ORDER BY c.n", enable_cross_partition_query=True, max_item_count=2).by_page())
    assert [idlist(p) for p in pages] == [ASC[0:2], ASC[2:4], ASC[4:6], ASC[6:]]


def test_several_fields_use_a_composite_index_and_return_documents_without_a_field(make_container):
    policy = {
        "indexingMode": "consistent",
        "includedPaths": [{"path": "/*"}],
        "excludedPaths": [],
        "compositeIndexes": [[{"path": "/n", "order": "ascending"}, {"path": "/id", "order": "ascending"}]],
    }
    c = load(make_container, policy)
    assert q(c, "SELECT * FROM c ORDER BY c.n, c.id") == ASC


def test_several_fields_without_a_composite_index_are_refused(make_container):
    from azure.cosmos.exceptions import CosmosHttpResponseError

    c = load(make_container)
    with pytest.raises(CosmosHttpResponseError) as e:
        q(c, "SELECT * FROM c ORDER BY c.n, c.id")
    assert e.value.status_code == 400


def test_nested_path_and_projection(make_container):
    c = make_container("/pk", indexing_policy={
        "indexingMode": "consistent",
        "includedPaths": [{"path": "/a/b/?"}],
        "excludedPaths": [{"path": "/*"}],
    })
    c.create_item({"id": "x", "pk": "p"})
    c.create_item({"id": "y", "pk": "p", "a": {"b": 2}})
    c.create_item({"id": "z", "pk": "p", "a": {"b": 1}})
    assert q(c, "SELECT * FROM c ORDER BY c.a.b", partition_key="p") == ["x", "z", "y"]
    assert list(c.query_items("SELECT VALUE c.id FROM c ORDER BY c.a.b DESC", partition_key="p")) == ["y", "z", "x"]
