"""Item CRUD basics."""

import pytest
from azure.core import MatchConditions
from azure.cosmos.exceptions import (
    CosmosAccessConditionFailedError,
    CosmosResourceExistsError,
    CosmosResourceNotFoundError,
)

from cosmosutil import uid


def test_create_item_has_system_properties(container):
    item = container.create_item({"id": "a", "pk": "x", "n": 1})
    for key in ("_rid", "_self", "_etag", "_attachments", "_ts"):
        assert key in item
    assert item["n"] == 1


def test_read_item_round_trip(container):
    container.create_item({"id": "a", "pk": "x", "n": 1})
    assert container.read_item("a", partition_key="x")["n"] == 1


def test_duplicate_id_same_partition_is_409(container):
    container.create_item({"id": "a", "pk": "x"})
    with pytest.raises(CosmosResourceExistsError) as e:
        container.create_item({"id": "a", "pk": "x"})
    assert e.value.status_code == 409


def test_same_id_in_different_partitions_is_allowed(container):
    container.create_item({"id": "a", "pk": "x", "v": 1})
    container.create_item({"id": "a", "pk": "y", "v": 2})
    assert container.read_item("a", "x")["v"] == 1
    assert container.read_item("a", "y")["v"] == 2


def test_read_missing_item_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("nope", partition_key="x")


def test_read_item_with_wrong_partition_key_is_404(container):
    container.create_item({"id": "a", "pk": "x"})
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", partition_key="other")


def test_replace_missing_item_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.replace_item("nope", {"id": "nope", "pk": "x"})


def test_delete_missing_item_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.delete_item("nope", partition_key="x")


def test_upsert_creates_then_replaces(container):
    container.upsert_item({"id": "a", "pk": "x", "v": 1})
    container.upsert_item({"id": "a", "pk": "x", "v": 2})
    assert container.read_item("a", "x")["v"] == 2


def test_replace_removes_properties_not_in_body(container):
    container.create_item({"id": "a", "pk": "x", "old": 1})
    container.replace_item("a", {"id": "a", "pk": "x", "new": 2})
    got = container.read_item("a", "x")
    assert "old" not in got and got["new"] == 2


def test_delete_item_then_read_is_404(container):
    container.create_item({"id": "a", "pk": "x"})
    container.delete_item("a", partition_key="x")
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", "x")


def test_replace_with_stale_etag_is_412(container):
    item = container.create_item({"id": "a", "pk": "x", "v": 1})
    container.replace_item("a", {"id": "a", "pk": "x", "v": 2})
    with pytest.raises(CosmosAccessConditionFailedError) as e:
        container.replace_item(
            "a", {"id": "a", "pk": "x", "v": 3}, etag=item["_etag"], match_condition=MatchConditions.IfNotModified
        )
    assert e.value.status_code == 412


def test_automatic_id_generation(container):
    item = container.create_item({"pk": "x"}, enable_automatic_id_generation=True)
    assert item["id"]


@pytest.mark.parametrize("bad", ["a/b", "a\\b", "a?b", "a#b"])
def test_item_id_with_illegal_character_is_rejected_by_sdk(container, bad):
    with pytest.raises(ValueError):
        container.create_item({"id": bad, "pk": "x"})


def test_read_all_items(container):
    ids = [uid("i") for _ in range(3)]
    for i in ids:
        container.create_item({"id": i, "pk": "x"})
    assert sorted(d["id"] for d in container.read_all_items()) == sorted(ids)


# ---- etags and conditional requests ----------------------------------------
def test_etag_is_a_quoted_string_and_changes_on_replace(container):
    created = container.create_item({"id": "a", "pk": "x", "v": 1})
    assert created["_etag"].startswith('"') and created["_etag"].endswith('"')
    replaced = container.replace_item("a", {"id": "a", "pk": "x", "v": 2})
    assert replaced["_etag"] != created["_etag"]


def test_rid_and_self_stay_stable_across_replace(container):
    created = container.create_item({"id": "a", "pk": "x"})
    replaced = container.replace_item("a", {"id": "a", "pk": "x", "v": 1})
    assert replaced["_rid"] == created["_rid"] and replaced["_self"] == created["_self"]


def test_ts_is_integer_seconds_close_to_now(container):
    import time

    created = container.create_item({"id": "a", "pk": "x"})
    assert isinstance(created["_ts"], int)
    assert abs(created["_ts"] - time.time()) < 300


def test_read_returns_same_etag_as_create(container):
    created = container.create_item({"id": "a", "pk": "x"})
    assert container.read_item("a", "x")["_etag"] == created["_etag"]


def test_replace_with_current_etag_succeeds(container):
    created = container.create_item({"id": "a", "pk": "x", "v": 1})
    got = container.replace_item(
        "a", {"id": "a", "pk": "x", "v": 2}, etag=created["_etag"], match_condition=MatchConditions.IfNotModified
    )
    assert got["v"] == 2


@pytest.mark.not_on_vnext(reason="vnext rejects If-Match: *")
def test_replace_with_wildcard_etag_succeeds(container):
    container.create_item({"id": "a", "pk": "x", "v": 1})
    got = container.replace_item("a", {"id": "a", "pk": "x", "v": 2}, etag="*", match_condition=MatchConditions.IfNotModified)
    assert got["v"] == 2


def test_upsert_with_stale_etag_is_412(container):
    created = container.create_item({"id": "a", "pk": "x"})
    container.replace_item("a", {"id": "a", "pk": "x", "v": 1})
    with pytest.raises(CosmosAccessConditionFailedError):
        container.upsert_item({"id": "a", "pk": "x", "v": 2}, etag=created["_etag"], match_condition=MatchConditions.IfNotModified)


def test_delete_with_stale_etag_is_412_and_keeps_item(container):
    created = container.create_item({"id": "a", "pk": "x"})
    container.replace_item("a", {"id": "a", "pk": "x", "v": 1})
    with pytest.raises(CosmosAccessConditionFailedError) as e:
        container.delete_item("a", "x", etag=created["_etag"], match_condition=MatchConditions.IfNotModified)
    assert e.value.status_code == 412
    assert container.read_item("a", "x")["v"] == 1


def test_delete_with_current_etag_succeeds(container):
    created = container.create_item({"id": "a", "pk": "x"})
    container.delete_item("a", "x", etag=created["_etag"], match_condition=MatchConditions.IfNotModified)
    with pytest.raises(CosmosResourceNotFoundError):
        container.read_item("a", "x")


def test_read_if_modified_with_unchanged_etag_returns_empty_result(container):
    # 304 Not Modified: the SDK surfaces it as an empty result, not an exception
    created = container.create_item({"id": "a", "pk": "x"})
    got = container.read_item("a", "x", etag=created["_etag"], match_condition=MatchConditions.IfModified)
    assert dict(got) == {}


def test_read_if_modified_with_stale_etag_returns_item(container):
    created = container.create_item({"id": "a", "pk": "x", "v": 1})
    container.replace_item("a", {"id": "a", "pk": "x", "v": 2})
    got = container.read_item("a", "x", etag=created["_etag"], match_condition=MatchConditions.IfModified)
    assert got["v"] == 2


# ---- response headers ------------------------------------------------------
def test_write_response_has_request_charge_and_activity_id(container):
    item = container.create_item({"id": "a", "pk": "x"})
    h = item.get_response_headers()
    assert float(h["x-ms-request-charge"]) > 0
    assert h["x-ms-activity-id"]


def test_read_response_headers_include_etag(container):
    created = container.create_item({"id": "a", "pk": "x"})
    h = container.read_item("a", "x").get_response_headers()
    assert h["etag"] == created["_etag"]


def test_response_hook_is_called_for_each_operation(container):
    seen = []
    container.create_item({"id": "a", "pk": "x"}, response_hook=lambda h, b: seen.append(("create", dict(b))))
    container.read_item("a", "x", response_hook=lambda h, b: seen.append(("read", dict(b))))
    assert [s[0] for s in seen] == ["create", "read"]
    assert seen[1][1]["id"] == "a"


# ---- size limits -----------------------------------------------------------
def test_item_of_one_megabyte_is_accepted(container):
    container.create_item({"id": "big", "pk": "x", "blob": "a" * 1_000_000})
    assert len(container.read_item("big", "x")["blob"]) == 1_000_000


def test_item_over_two_megabytes_is_413(container):
    from azure.cosmos.exceptions import CosmosHttpResponseError

    with pytest.raises(CosmosHttpResponseError) as e:
        container.create_item({"id": "huge", "pk": "x", "blob": "a" * 2_600_000})
    assert e.value.status_code == 413


def test_replace_in_wrong_container_partition_is_not_found(container):
    container.create_item({"id": "a", "pk": "x"})
    with pytest.raises(CosmosResourceNotFoundError):
        container.replace_item("a", {"id": "a", "pk": "y"})


def test_create_item_without_id_and_without_auto_generation_fails_in_sdk(container):
    with pytest.raises(Exception):
        container.create_item({"pk": "x"})


def test_read_all_items_of_empty_container(container):
    assert list(container.read_all_items()) == []


def test_read_all_items_spans_partitions(container):
    for i in range(6):
        container.create_item({"id": f"i{i}", "pk": f"p{i % 3}"})
    assert sorted(d["id"] for d in container.read_all_items()) == [f"i{i}" for i in range(6)]


def test_read_all_items_pages(container):
    for i in range(7):
        container.create_item({"id": f"i{i}", "pk": "x"})
    pages = [list(p) for p in container.read_all_items(max_item_count=3).by_page()]
    assert [len(p) for p in pages] == [3, 3, 1]


def test_create_item_returns_a_copy_with_all_submitted_properties(container):
    body = {"id": "a", "pk": "x", "n": 1, "s": "t", "arr": [1, 2], "obj": {"k": "v"}}
    got = container.create_item(dict(body))
    for k, v in body.items():
        assert got[k] == v
