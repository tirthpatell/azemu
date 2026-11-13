"""patch_item (partial document update)."""

import pytest
from azure.core import MatchConditions
from azure.cosmos.exceptions import (
    CosmosAccessConditionFailedError,
    CosmosHttpResponseError,
    CosmosResourceNotFoundError,
)

DOC = {
    "id": "d1",
    "pk": "k",
    "city": "Seattle",
    "port": 9000,
    "flag": True,
    "tags": ["a", "b"],
    "nested": {"x": 1, "y": {"z": 2}},
    "remove_me": 1,
    "move_me": "yes",
}


@pytest.fixture
def item(container):
    container.create_item(dict(DOC))
    return container


def patch(c, ops, **kw):
    return c.patch_item("d1", "k", ops, **kw)


def test_add_new_property(item):
    got = patch(item, [{"op": "add", "path": "/color", "value": "red"}])
    assert got["color"] == "red"
    assert item.read_item("d1", "k")["color"] == "red"


def test_add_replaces_existing_property(item):
    assert patch(item, [{"op": "add", "path": "/city", "value": "Redmond"}])["city"] == "Redmond"


def test_add_nested_property(item):
    got = patch(item, [{"op": "add", "path": "/nested/w", "value": 5}])
    assert got["nested"] == {"x": 1, "y": {"z": 2}, "w": 5}


def test_add_inserts_into_array_at_index(item):
    got = patch(item, [{"op": "add", "path": "/tags/1", "value": "mid"}])
    assert got["tags"] == ["a", "mid", "b"]


def test_add_appends_to_array_with_dash(item):
    got = patch(item, [{"op": "add", "path": "/tags/-", "value": "end"}])
    assert got["tags"] == ["a", "b", "end"]


def test_set_creates_or_replaces(item):
    got = patch(item, [{"op": "set", "path": "/city", "value": "Tacoma"}, {"op": "set", "path": "/new", "value": 1}])
    assert got["city"] == "Tacoma" and got["new"] == 1


def test_set_array_element(item):
    assert patch(item, [{"op": "set", "path": "/tags/0", "value": "first"}])["tags"] == ["first", "b"]


def test_replace_existing_property(item):
    assert patch(item, [{"op": "replace", "path": "/city", "value": "Redmond"}])["city"] == "Redmond"


def test_replace_missing_property_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "replace", "path": "/nope", "value": 1}])
    assert e.value.status_code == 400


def test_remove_property(item):
    got = patch(item, [{"op": "remove", "path": "/remove_me"}])
    assert "remove_me" not in got
    assert "remove_me" not in item.read_item("d1", "k")


def test_remove_array_element(item):
    assert patch(item, [{"op": "remove", "path": "/tags/0"}])["tags"] == ["b"]


def test_remove_missing_property_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "remove", "path": "/nope"}])
    assert e.value.status_code == 400


def test_incr_adds_to_number(item):
    assert patch(item, [{"op": "incr", "path": "/port", "value": 5}])["port"] == 9005
    assert patch(item, [{"op": "incr", "path": "/port", "value": -10}])["port"] == 8995


def test_incr_with_float(item):
    assert patch(item, [{"op": "incr", "path": "/port", "value": 0.5}])["port"] == 9000.5


def test_incr_on_non_number_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "incr", "path": "/city", "value": 1}])
    assert e.value.status_code == 400


def test_move_property(item):
    got = patch(item, [{"op": "move", "from": "/move_me", "path": "/moved"}])
    assert got["moved"] == "yes" and "move_me" not in got


def test_multiple_operations_apply_in_order(item):
    got = patch(
        item,
        [
            {"op": "add", "path": "/favorite", "value": "red"},
            {"op": "remove", "path": "/remove_me"},
            {"op": "replace", "path": "/city", "value": "Redmond"},
            {"op": "set", "path": "/flag", "value": False},
            {"op": "incr", "path": "/port", "value": 5},
            {"op": "move", "from": "/move_me", "path": "/moved"},
        ],
    )
    assert got["favorite"] == "red" and "remove_me" not in got
    assert got["city"] == "Redmond" and got["flag"] is False and got["port"] == 9005
    assert got["moved"] == "yes" and "move_me" not in got


def test_patch_changes_etag_and_keeps_other_properties(item):
    before = item.read_item("d1", "k")
    got = patch(item, [{"op": "set", "path": "/city", "value": "X"}])
    assert got["_etag"] != before["_etag"]
    assert got["nested"] == DOC["nested"] and got["tags"] == DOC["tags"]


def test_patch_missing_item_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.patch_item("nope", "k", [{"op": "add", "path": "/a", "value": 1}])


def test_patch_wrong_partition_key_is_404(item):
    with pytest.raises(CosmosResourceNotFoundError):
        item.patch_item("d1", "other", [{"op": "add", "path": "/a", "value": 1}])


def test_filter_predicate_true_applies_patch(item):
    got = patch(item, [{"op": "set", "path": "/city", "value": "Y"}], filter_predicate="from c where c.port = 9000")
    assert got["city"] == "Y"


def test_filter_predicate_false_is_412(item):
    with pytest.raises(CosmosAccessConditionFailedError) as e:
        patch(item, [{"op": "set", "path": "/city", "value": "Y"}], filter_predicate="from c where c.port = 1")
    assert e.value.status_code == 412
    assert item.read_item("d1", "k")["city"] == "Seattle"


def test_patch_with_stale_etag_is_412(item):
    old = item.read_item("d1", "k")["_etag"]
    patch(item, [{"op": "set", "path": "/city", "value": "A"}])
    with pytest.raises(CosmosAccessConditionFailedError):
        patch(item, [{"op": "set", "path": "/city", "value": "B"}], etag=old, match_condition=MatchConditions.IfNotModified)


@pytest.mark.not_on_vnext(reason="vnext accepts a patch path without a leading slash")
def test_invalid_path_without_leading_slash_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "add", "path": "color", "value": "red"}])
    assert e.value.status_code == 400


@pytest.mark.not_on_vnext(reason="vnext answers 404 instead of 400")
def test_patching_the_partition_key_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "set", "path": "/pk", "value": "other"}])
    assert e.value.status_code == 400


@pytest.mark.not_on_vnext(reason="vnext answers 404 instead of 400")
def test_patching_the_id_is_400(item):
    with pytest.raises(CosmosHttpResponseError) as e:
        patch(item, [{"op": "set", "path": "/id", "value": "other"}])
    assert e.value.status_code == 400


def test_failed_operation_leaves_document_unchanged(item):
    with pytest.raises(CosmosHttpResponseError):
        patch(item, [{"op": "set", "path": "/city", "value": "Z"}, {"op": "remove", "path": "/nope"}])
    assert item.read_item("d1", "k")["city"] == "Seattle"
