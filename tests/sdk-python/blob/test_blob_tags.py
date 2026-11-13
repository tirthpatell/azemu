"""Blob index tags: set/get, tag count, find by tags, tag conditions."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.core import MatchConditions

from blobutil import assert_error


@pytest.fixture
def tag_value(uniq):
    """A tag value no other test uses, so queries only find this test's blobs."""
    return uniq("v")


def test_upload_with_tags_then_get_tags(container, blob_name, tag_value):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"project": tag_value, "env": "test"})
    assert b.get_blob_tags() == {"project": tag_value, "env": "test"}


def test_blob_without_tags_has_empty_tag_set(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    assert b.get_blob_tags() == {}


def test_set_blob_tags_replaces_all_tags(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"a": "1", "b": "2"})
    b.set_blob_tags({"c": "3"})
    assert b.get_blob_tags() == {"c": "3"}


def test_set_empty_tags_clears_them(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"a": "1"})
    b.set_blob_tags({})
    assert b.get_blob_tags() == {}


def test_set_tags_does_not_change_etag_or_content(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"data")
    etag = b.get_blob_properties().etag
    b.set_blob_tags({"a": "1"})
    assert b.get_blob_properties().etag == etag
    assert b.download_blob().readall() == b"data"


def test_tag_count_in_blob_properties(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"a": "1", "b": "2", "c": "3"})
    assert b.get_blob_properties().tag_count == 3


def test_tag_count_in_download_properties(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"a": "1", "b": "2"})
    assert b.download_blob().properties.tag_count == 2


def test_tags_with_allowed_special_characters(container, blob_name):
    tags = {"key with space": "value with space", "a.b-c_d/e": "x:y=z+1"}
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags=tags)
    assert b.get_blob_tags() == tags


def test_ten_tags_are_allowed(container, blob_name):
    tags = {f"k{i}": f"v{i}" for i in range(10)}
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags=tags)
    assert b.get_blob_tags() == tags


def test_eleven_tags_are_rejected(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    with pytest.raises(HttpResponseError) as ei:
        b.set_blob_tags({f"k{i}": "v" for i in range(11)})
    assert ei.value.status_code == 400


def test_get_tags_of_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).get_blob_tags()
    assert_error(ei, "BlobNotFound", 404)


def test_set_tags_on_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).set_blob_tags({"a": "b"})
    assert_error(ei, "BlobNotFound", 404)


def test_list_blobs_include_tags(container, blob_name):
    container.upload_blob(blob_name, b"x", tags={"a": "1"})
    (item,) = list(container.list_blobs(include=["tags"]))
    assert item.tags == {"a": "1"}


def test_commit_block_list_with_tags(container, blob_name):
    from azure.storage.blob import BlobBlock

    b = container.get_blob_client(blob_name)
    b.stage_block("blk00001", b"x")
    b.commit_block_list([BlobBlock("blk00001")], tags={"via": "blocks"})
    assert b.get_blob_tags() == {"via": "blocks"}


# --- find blobs by tags ----------------------------------------------------


def test_find_blobs_by_tags_in_container(container, uniq, tag_value):
    hit = container.get_blob_client(uniq("hit"))
    hit.upload_blob(b"x", tags={"project": tag_value})
    container.upload_blob(uniq("miss"), b"x", tags={"project": "other-" + tag_value})
    found = list(container.find_blobs_by_tags(f"\"project\"='{tag_value}'"))
    assert [f.name for f in found] == [hit.blob_name]
    assert found[0].tags == {"project": tag_value}


def test_find_blobs_by_tags_at_service_level_spans_containers(service, make_container, uniq, tag_value):
    c1, c2 = make_container(), make_container()
    n1, n2 = uniq("a"), uniq("b")
    c1.upload_blob(n1, b"x", tags={"shared": tag_value})
    c2.upload_blob(n2, b"x", tags={"shared": tag_value})
    found = {(f.container_name, f.name) for f in service.find_blobs_by_tags(f"\"shared\"='{tag_value}'")}
    assert found == {(c1.container_name, n1), (c2.container_name, n2)}


def test_find_blobs_by_tags_and_condition(container, uniq, tag_value):
    both = uniq("both")
    container.upload_blob(both, b"x", tags={"a": tag_value, "b": "2"})
    container.upload_blob(uniq("only-a"), b"x", tags={"a": tag_value})
    found = [f.name for f in container.find_blobs_by_tags(f"\"a\"='{tag_value}' AND \"b\"='2'")]
    assert found == [both]


def test_find_blobs_by_tags_with_container_filter(service, container, uniq, tag_value):
    n = uniq("n")
    container.upload_blob(n, b"x", tags={"k": tag_value})
    q = f"@container='{container.container_name}' AND \"k\"='{tag_value}'"
    assert [f.name for f in service.find_blobs_by_tags(q)] == [n]


def test_find_blobs_by_tags_no_match(container):
    assert list(container.find_blobs_by_tags("\"nothing\"='matches-this-zzz'")) == []


def test_find_blobs_by_tags_reflects_tag_update(container, blob_name, tag_value):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"k": "old"})
    b.set_blob_tags({"k": tag_value})
    assert [f.name for f in container.find_blobs_by_tags(f"\"k\"='{tag_value}'")] == [blob_name]
    assert list(container.find_blobs_by_tags("\"k\"='old'")) == []


def test_find_blobs_by_tags_ignores_deleted_blobs(container, blob_name, tag_value):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"k": tag_value})
    b.delete_blob()
    assert list(container.find_blobs_by_tags(f"\"k\"='{tag_value}'")) == []


def test_find_blobs_by_tags_paging(container, uniq, tag_value):
    names = sorted(uniq("n") for _ in range(3))
    for n in names:
        container.upload_blob(n, b"x", tags={"k": tag_value})
    pages = list(container.find_blobs_by_tags(f"\"k\"='{tag_value}'", results_per_page=2).by_page())
    assert [len(list(p)) for p in pages] == [2, 1]


def test_find_blobs_by_tags_range_operators(container, uniq, tag_value):
    n = uniq("n")
    container.upload_blob(n, b"x", tags={"rank": "m", "grp": tag_value})
    q = f"\"grp\"='{tag_value}' AND \"rank\">='a' AND \"rank\"<'z'"
    assert [f.name for f in container.find_blobs_by_tags(q)] == [n]


# --- tag conditions (x-ms-if-tags) -----------------------------------------


def test_if_tags_match_condition_true_allows_write(container, blob_name, tag_value):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": tag_value})
    b.set_blob_metadata({"a": "b"}, if_tags_match_condition=f"\"state\"='{tag_value}'")
    assert b.get_blob_properties().metadata == {"a": "b"}


def test_if_tags_match_condition_false_rejects_set_metadata(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    with pytest.raises(HttpResponseError) as ei:
        b.set_blob_metadata({"a": "b"}, if_tags_match_condition="\"state\"='closed'")
    assert_error(ei, "ConditionNotMet", 412)
    assert not b.get_blob_properties().metadata


def test_if_tags_match_condition_false_rejects_delete(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    with pytest.raises(HttpResponseError) as ei:
        b.delete_blob(if_tags_match_condition="\"state\"='closed'")
    assert_error(ei, "ConditionNotMet", 412)
    assert b.exists()


def test_if_tags_match_condition_false_rejects_overwrite(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    with pytest.raises(HttpResponseError) as ei:
        b.upload_blob(b"y", overwrite=True, if_tags_match_condition="\"state\"='closed'")
    assert_error(ei, "ConditionNotMet", 412)
    assert b.download_blob().readall() == b"x"


@pytest.mark.not_on_azurite(reason="Azurite ignores x-ms-if-tags on Set Blob Tags")
def test_if_tags_match_condition_false_rejects_set_tags(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    with pytest.raises(HttpResponseError) as ei:
        b.set_blob_tags({"state": "closed"}, if_tags_match_condition="\"state\"='closed'")
    assert_error(ei, "ConditionNotMet", 412)
    assert b.get_blob_tags() == {"state": "open"}


def test_if_tags_match_condition_on_download(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    assert b.download_blob(if_tags_match_condition="\"state\"='open'").readall() == b"x"
    with pytest.raises(HttpResponseError) as ei:
        b.download_blob(if_tags_match_condition="\"state\"='closed'")
    assert ei.value.status_code == 412


def test_if_tags_match_condition_on_get_properties(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", tags={"state": "open"})
    with pytest.raises(HttpResponseError) as ei:
        b.get_blob_properties(if_tags_match_condition="\"state\"='closed'")
    assert ei.value.status_code == 412
