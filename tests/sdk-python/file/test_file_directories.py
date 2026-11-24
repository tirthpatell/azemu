"""Directories: create, properties, metadata, SMB attributes and times, listing, rename, delete, names.

Written from the REST reference and azure-storage-file-share 12.27.1; the answers that were checked against Azure in November
2026 say so.
"""

from datetime import datetime, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.fileshare import NTFSAttributes

import fileutil
from fileutil import expect


def test_create_directory_and_read_it_back(work):
    sub = work.create_subdirectory("sub", metadata={"k": "v"})
    props = sub.get_directory_properties()
    assert props.metadata == {"k": "v"}
    assert props.etag.startswith('"0x') and props.last_modified
    assert props.file_attributes == "Directory"
    assert props.file_id and props.parent_id == work.get_directory_properties().file_id
    assert props.creation_time and props.last_write_time and props.change_time
    assert props.permission_key and "*" in props.permission_key


def test_create_twice_is_a_conflict(work):
    work.create_subdirectory("twice")
    with pytest.raises(ResourceExistsError) as e:
        work.create_subdirectory("twice")
    expect(e, 409, "ResourceAlreadyExists")


def test_parent_must_exist(work):
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_subdirectory_client("nope/child").create_directory()
    expect(e, 404, "ParentNotFound")


def test_missing_directory(work):
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_subdirectory_client("nope").get_directory_properties()
    expect(e, 404, "ResourceNotFound")


def test_names_are_case_insensitive_and_keep_their_case(work):
    work.create_subdirectory("MixedCase")
    work.get_subdirectory_client("mixedcase").get_directory_properties()
    assert [d.name for d in work.list_directories_and_files()] == ["MixedCase"]
    with pytest.raises(ResourceExistsError):
        work.create_subdirectory("MIXEDCASE")


@pytest.mark.parametrize("name", ["a:b", "a*b", "a?b", "a<b", "a>b", "a|b", 'a"b', "CON", "aux", "NUL", "com1", "x" * 256])
def test_names_that_are_not_allowed(work, name):
    # checked against Azure in November 2026
    with pytest.raises(HttpResponseError) as e:
        work.get_subdirectory_client(name).create_directory()
    expect(e, 400, "InvalidResourceName")


@pytest.mark.parametrize("name", ["con.txt", "a b", "$a", "ünï", "a#b", "a%b", "a;b", "a+b", "x" * 255])
def test_names_that_are_allowed(work, name):
    work.get_subdirectory_client(name).create_directory()
    assert [d.name for d in work.list_directories_and_files(name_starts_with=name[:3])] == [name]


def test_delete_needs_an_empty_directory(work):
    sub = work.create_subdirectory("full")
    sub.upload_file("f.txt", b"x")
    with pytest.raises(HttpResponseError) as e:
        work.delete_subdirectory("full")
    expect(e, 409, "DirectoryNotEmpty")
    sub.delete_file("f.txt")
    work.delete_subdirectory("full")
    with pytest.raises(ResourceNotFoundError) as e:
        sub.get_directory_properties()
    expect(e, 404, "ResourceNotFound")


def test_set_directory_metadata_replaces(work):
    work.set_directory_metadata({"a": "1"})
    assert work.get_directory_properties().metadata == {"a": "1"}
    before = work.get_directory_properties().etag
    work.set_directory_metadata({})
    props = work.get_directory_properties()
    assert props.metadata == {} and props.etag != before


def test_directory_attributes_and_times(work):
    created = datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    written = datetime(2021, 6, 7, 8, 9, 10, tzinfo=timezone.utc)
    sub = work.get_subdirectory_client("smb")
    sub.create_directory(file_attributes=NTFSAttributes(hidden=True, directory=True), file_creation_time=created, file_last_write_time=written)
    props = sub.get_directory_properties()
    assert props.file_attributes == "Hidden | Directory"
    assert fileutil.as_utc(props.creation_time) == created
    assert fileutil.as_utc(props.last_write_time) == written
    # Set Properties keeps what it is told to keep (preserve is the SDK's default) and changes the rest
    sub.set_http_headers(file_attributes="Directory", file_creation_time="preserve", file_last_write_time="preserve")
    again = sub.get_directory_properties()
    assert again.file_attributes == "Directory"
    assert fileutil.as_utc(again.creation_time) == created
    assert again.etag != props.etag


def test_a_directory_needs_its_own_attribute(work):
    # checked against Azure in November 2026: Hidden alone is refused for a directory, None means Directory
    with pytest.raises(HttpResponseError) as e:
        work.get_subdirectory_client("a").create_directory(file_attributes="Hidden")
    expect(e, 400, "InvalidHeaderValue")
    sub = work.get_subdirectory_client("b")
    sub.create_directory(file_attributes="None")
    assert sub.get_directory_properties().file_attributes == "Directory"


def test_listing_in_name_order_without_regard_to_case(work):
    work.create_subdirectory("sub2")
    work.create_subdirectory("Sub1")
    work.create_subdirectory("sub3")
    for n in ("b.txt", "a.txt", "C.txt"):
        work.upload_file(n, b"12345")
    listed = list(work.list_directories_and_files())
    # the SDK puts the directories of a page before its files; each group is in name order, upper case or not
    assert [(i.name, i.is_directory) for i in listed] == [
        ("Sub1", True), ("sub2", True), ("sub3", True), ("a.txt", False), ("b.txt", False), ("C.txt", False)]
    assert [i.size for i in listed if not i.is_directory] == [5, 5, 5]
    assert all(i.file_id for i in listed)


def test_listing_prefix_is_not_case_sensitive(work):
    work.create_subdirectory("Alpha")
    work.upload_file("alpine.txt", b"x")
    work.upload_file("beta.txt", b"x")
    assert sorted(i.name for i in work.list_directories_and_files(name_starts_with="al")) == ["Alpha", "alpine.txt"]


def test_listing_pages(work):
    for i in range(5):
        work.upload_file(f"f{i}.txt", b"x")
    pages = list(work.list_directories_and_files(results_per_page=2).by_page())
    assert [[i.name for i in p] for p in pages] == [["f0.txt", "f1.txt"], ["f2.txt", "f3.txt"], ["f4.txt"]]


def test_listing_with_extended_info(work):
    work.upload_file("f.txt", b"abc")
    work.create_subdirectory("d")
    items = {i.name: i for i in work.list_directories_and_files(include=["timestamps", "etag", "attributes"])}
    f, d = items["f.txt"], items["d"]
    assert f.etag.startswith('"0x') and f.file_attributes == "Archive" and f.creation_time and f.last_write_time
    assert d.file_attributes == "Directory" and d.etag


def test_listing_the_root_of_a_share(shared, work):
    names = [i.name for i in shared.list_directories_and_files()]
    assert work.directory_path in names


def test_rename_directory(work):
    src = work.create_subdirectory("old")
    src.upload_file("inside.txt", b"data")
    moved = src.rename_directory(f"{work.directory_path}/new")
    assert moved.directory_path.endswith("/new")
    assert [i.name for i in work.list_directories_and_files()] == ["new"]
    assert work.get_subdirectory_client("new").get_file_client("inside.txt").download_file().readall() == b"data"
    with pytest.raises(ResourceNotFoundError):
        src.get_directory_properties()


def test_rename_over_an_existing_directory_fails(work):
    a = work.create_subdirectory("a")
    work.create_subdirectory("b")
    with pytest.raises(HttpResponseError) as e:
        a.rename_directory(f"{work.directory_path}/b")
    expect(e, 409, "ResourceAlreadyExists")


def test_handles_of_a_share_nobody_mounted(work):
    assert list(work.list_handles(recursive=True)) == []
    assert work.close_all_handles(recursive=True) == {"closed_handles_count": 0, "failed_handles_count": 0}


def test_file_where_a_directory_is_wanted(work):
    work.upload_file("plain.txt", b"x")
    with pytest.raises(ResourceNotFoundError) as e:
        work.get_subdirectory_client("plain.txt").get_directory_properties()
    expect(e, 404, "ResourceNotFound")
    with pytest.raises(HttpResponseError) as e:
        work.get_subdirectory_client("plain.txt").create_directory()
    expect(e, 409, "ResourceTypeMismatch")
