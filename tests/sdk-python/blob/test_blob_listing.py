"""Listing blobs: prefix, delimiter, paging, include options, odd names."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.storage.blob import BlobPrefix

from blobutil import assert_error


def _put(container, *names, data=b"x"):
    for n in names:
        container.upload_blob(n, data)


def test_list_empty_container(container):
    assert list(container.list_blobs()) == []


def test_list_blobs_returns_all_names_in_binary_order(container):
    names = ["b", "a", "B", "_x", "1", "é", "a/b"]
    _put(container, *names)
    assert [b.name for b in container.list_blobs()] == sorted(names, key=lambda n: n.encode("utf-8"))


def test_list_blob_names(container):
    _put(container, "one", "two")
    assert list(container.list_blob_names()) == ["one", "two"]


def test_list_blobs_prefix(container):
    _put(container, "logs/a", "logs/b", "data/a", "logsx")
    assert [b.name for b in container.list_blobs(name_starts_with="logs/")] == ["logs/a", "logs/b"]
    assert [b.name for b in container.list_blobs(name_starts_with="logs")] == ["logs/a", "logs/b", "logsx"]


def test_list_blobs_prefix_without_match(container):
    _put(container, "a")
    assert list(container.list_blobs(name_starts_with="zzz")) == []


def test_list_blob_item_fields(container, blob_name):
    container.upload_blob(blob_name, b"12345")
    (item,) = list(container.list_blobs())
    assert item.name == blob_name
    assert item.size == 5
    assert item.blob_type == "BlockBlob"
    # The XML listing carries the ETag without quotes; HEAD/GET carry it quoted.
    assert item.etag.strip('"') == container.get_blob_client(blob_name).get_blob_properties().etag.strip('"')
    assert item.last_modified is not None
    assert item.creation_time is not None
    assert item.content_settings.content_type == "application/octet-stream"
    assert item.lease.state == "available"
    assert item.deleted is None or item.deleted is False
    assert item.container == container.container_name


def test_list_blobs_metadata_only_when_requested(container, blob_name):
    container.upload_blob(blob_name, b"x", metadata={"k": "v"})
    (plain,) = list(container.list_blobs())
    (with_md,) = list(container.list_blobs(include=["metadata"]))
    assert not plain.metadata
    assert with_md.metadata == {"k": "v"}


def test_list_blobs_does_not_show_uncommitted_blobs_by_default(container, blob_name):
    container.get_blob_client(blob_name).stage_block("blk00001", b"pending")
    assert list(container.list_blobs()) == []


def test_list_blobs_include_uncommitted(container, blob_name):
    container.get_blob_client(blob_name).stage_block("blk00001", b"pending")
    assert [b.name for b in container.list_blobs(include=["uncommittedblobs"])] == [blob_name]


def test_list_paging_by_page(container):
    names = [f"p{i:02d}" for i in range(5)]
    _put(container, *names)
    pages = [[b.name for b in p] for p in container.list_blobs(results_per_page=2).by_page()]
    assert pages == [names[0:2], names[2:4], names[4:5]]


def test_list_continuation_token(container):
    names = [f"p{i:02d}" for i in range(5)]
    _put(container, *names)
    pager = container.list_blobs(results_per_page=2).by_page()
    first = [b.name for b in next(pager)]
    token = pager.continuation_token
    assert token
    rest = [b.name for p in container.list_blobs(results_per_page=2).by_page(continuation_token=token) for b in p]
    assert first + rest == names


def test_list_paging_with_exact_multiple_has_no_extra_page(container):
    _put(container, "a", "b", "c", "d")
    pages = [[b.name for b in p] for p in container.list_blobs(results_per_page=2).by_page()]
    assert pages == [["a", "b"], ["c", "d"]]


def test_walk_blobs_with_delimiter_yields_prefixes_and_blobs(container):
    _put(container, "root.txt", "dir1/a", "dir1/b", "dir2/sub/c")
    items = list(container.walk_blobs(delimiter="/"))
    prefixes = [i.name for i in items if isinstance(i, BlobPrefix)]
    blobs = [i.name for i in items if not isinstance(i, BlobPrefix)]
    assert prefixes == ["dir1/", "dir2/"]
    assert blobs == ["root.txt"]


def test_walk_blobs_into_a_virtual_directory(container):
    _put(container, "dir1/a", "dir1/b", "dir1/sub/c", "dir2/x")
    items = list(container.walk_blobs(name_starts_with="dir1/", delimiter="/"))
    assert [i.name for i in items if isinstance(i, BlobPrefix)] == ["dir1/sub/"]
    assert [i.name for i in items if not isinstance(i, BlobPrefix)] == ["dir1/a", "dir1/b"]


def test_walk_blobs_recurses_through_prefix_objects(container):
    _put(container, "a/b/c.txt", "a/d.txt")
    found = []

    def walk(it):
        for i in it:
            if isinstance(i, BlobPrefix):
                walk(i)
            else:
                found.append(i.name)

    walk(container.walk_blobs(delimiter="/"))
    assert sorted(found) == ["a/b/c.txt", "a/d.txt"]


def test_walk_blobs_without_directories_yields_only_blobs(container):
    _put(container, "a", "b")
    items = list(container.walk_blobs(delimiter="/"))
    assert [i.name for i in items] == ["a", "b"]
    assert not any(isinstance(i, BlobPrefix) for i in items)


def test_walk_blobs_with_custom_delimiter(container):
    _put(container, "x-1", "x-2", "y")
    items = list(container.walk_blobs(delimiter="-"))
    assert [i.name for i in items if isinstance(i, BlobPrefix)] == ["x-"]


def test_walk_blobs_paging(container):
    _put(container, "d1/a", "d2/a", "d3/a", "top")
    pages = [[i.name for i in p] for p in container.walk_blobs(delimiter="/", results_per_page=2).by_page()]
    assert [n for p in pages for n in p] == ["d1/", "d2/", "d3/", "top"]
    assert len(pages) == 2


def test_list_blobs_with_delimiter_style_names_flat(container):
    _put(container, "a/b/c", "a/b/d", "a/e")
    assert [b.name for b in container.list_blobs()] == ["a/b/c", "a/b/d", "a/e"]


@pytest.mark.parametrize(
    "name",
    [
        "a b",
        "  leading and trailing  ",
        "é",
        "日本語/ファイル.txt",
        "emoji-😀",
        "hash#tag",
        "question?mark",
        "percent%20literal",
        "100%",
        "plus+sign",
        "amp&equals=semi;colon",
        "dir//double//slash",
        "a/b/c/d/e",
        "-_.~!$'()*,@:",
        "[brackets]{braces}",
        "quote\"s",
    ],
)
def test_blob_names_roundtrip(container, name):
    data = name.encode("utf-8")
    container.upload_blob(name, data)
    b = container.get_blob_client(name)
    assert b.download_blob().readall() == data
    assert b.get_blob_properties().name == name
    assert [x.name for x in container.list_blobs()] == [name]
    b.delete_blob()
    assert not b.exists()


@pytest.mark.parametrize("name", ["tab\tinside", "line\nbreak", "nul\x00byte", "bell\x07"])
def test_control_characters_in_blob_names_are_refused(container, name):
    # Microsoft's naming page: "control characters (0x00 to 0x1F ...) are also not allowed" in a blob name; the service answers
    # 400 InvalidResourceName
    from azure.core.exceptions import HttpResponseError

    with pytest.raises(HttpResponseError) as ei:
        container.upload_blob(name, b"x")
    assert ei.value.status_code == 400
    assert ei.value.error_code == "InvalidResourceName"
    assert list(container.list_blobs()) == []


def test_long_blob_name(container):
    name = "n" * 1000
    container.upload_blob(name, b"x")
    assert [b.name for b in container.list_blobs()] == [name]


def test_names_that_differ_only_by_case_are_distinct(container):
    _put(container, "Case", "case")
    assert [b.name for b in container.list_blobs()] == ["Case", "case"]


def test_list_blobs_in_missing_container(service, uniq):
    with pytest.raises(ResourceNotFoundError) as ei:
        list(service.get_container_client(uniq("c")).list_blobs())
    assert_error(ei, "ContainerNotFound", 404)
