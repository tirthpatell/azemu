"""get_paths from filesystem and directory clients (dfs `GET /fs?resource=filesystem`)."""

from datetime import datetime, timezone

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.storage.filedatalake import PathProperties

from dlutil import (
    DEFAULT_DIR_PERMISSIONS,
    DEFAULT_FILE_PERMISSIONS,
    SUPERUSER,
    assert_error,
    names,
    put_file,
    tree,
)


def test_get_paths_on_empty_file_system_is_empty(fs):
    assert list(fs.get_paths()) == []


def test_get_paths_yields_path_properties(fs):
    fs.create_directory("d")
    paths = list(fs.get_paths())
    assert len(paths) == 1
    assert isinstance(paths[0], PathProperties)


def test_get_paths_directory_fields(fs):
    created = fs.create_directory("dir1")
    [p] = list(fs.get_paths())
    assert p.name == "dir1"
    assert p.is_directory is True
    assert p.content_length == 0
    assert isinstance(p.last_modified, datetime)
    assert isinstance(p.creation_time, datetime)
    assert p.etag
    assert p.etag.strip('"') == created.get_directory_properties().etag.strip('"')
    assert p.owner == SUPERUSER
    assert p.group == SUPERUSER
    assert p.permissions == DEFAULT_DIR_PERMISSIONS
    assert p.expiry_time is None


def test_get_paths_file_fields(fs):
    f = put_file(fs, "file1.txt", b"hello")
    [p] = list(fs.get_paths())
    assert p.name == "file1.txt"
    assert p.is_directory is False
    assert p.content_length == 5
    assert isinstance(p.last_modified, datetime)
    assert isinstance(p.creation_time, datetime)
    assert p.owner == SUPERUSER
    assert p.group == SUPERUSER
    assert p.permissions == DEFAULT_FILE_PERMISSIONS
    props = f.get_file_properties()
    assert p.etag.strip('"') == props.etag.strip('"')
    # the path listing parses the RFC 1123 date without a timezone
    assert p.last_modified.replace(tzinfo=None) == props.last_modified.replace(tzinfo=None)


def test_get_paths_no_expiry_is_none(fs):
    fs.create_file("file1")
    [p] = list(fs.get_paths(upn=True))
    assert p.expiry_time is None


def test_get_paths_with_expiry_matches_properties(fs):
    from datetime import timedelta

    f = fs.create_file("file1")
    expiry = datetime.now(timezone.utc) + timedelta(days=1)
    f.set_file_expiry("Absolute", expires_on=expiry)
    [p] = list(fs.get_paths(upn=True))
    props = f.get_file_properties()
    assert abs(p.expiry_time - props.expiry_time) <= timedelta(seconds=1)
    assert abs(p.creation_time - props.creation_time) <= timedelta(seconds=1)


def test_get_paths_recursive_default_and_order(fs):
    for i in range(3):
        d = fs.create_directory(f"dir1{i}")
        sub = d.create_sub_directory("subdir")
        sub.create_sub_directory("subsub")
        sub.create_file("file")
    got = [p.name for p in fs.get_paths(recursive=True)]
    assert len(got) == 12
    # depth-first, parents before children (recorded for alphanumeric names)
    assert got[:5] == ["dir10", "dir10/subdir", "dir10/subdir/file", "dir10/subdir/subsub", "dir11"]
    assert got == sorted(got)


def test_get_paths_non_recursive_lists_only_children(fs):
    fs.create_directory("a/b/c")
    put_file(fs, "a/b/f.txt", b"x")
    put_file(fs, "top.txt", b"x")
    assert tree(fs, recursive=False) == {"a": True, "top.txt": False}


def test_get_paths_recursive_includes_implicit_parents(fs):
    fs.create_file("dir1/dir2/file")
    got = list(fs.get_paths(recursive=True))
    assert [p.name for p in got] == ["dir1", "dir1/dir2", "dir1/dir2/file"]
    assert [p.is_directory for p in got] == [True, True, False]


def test_get_paths_create_directory_nested_creates_parents(fs):
    fs.create_directory("dir1/dir2")
    got = list(fs.get_paths(recursive=False, upn=True))
    assert [p.name for p in got] == ["dir1"]
    assert tree(fs) == {"dir1": True, "dir1/dir2": True}


def test_get_paths_all_files(fs):
    for i in range(6):
        fs.create_file(f"file{i}")
    got = list(fs.get_paths(upn=True))
    assert [p.name for p in got] == [f"file{i}" for i in range(6)]
    assert not any(p.is_directory for p in got)


def test_get_paths_with_path_prefix_directory(fs):
    for d in ("keep", "other"):
        put_file(fs, f"{d}/f1", b"1")
        put_file(fs, f"{d}/sub/f2", b"22")
    got = tree(fs, path="keep")
    assert got == {"keep/f1": False, "keep/sub": True, "keep/sub/f2": False}


def test_get_paths_with_path_non_recursive(fs):
    put_file(fs, "keep/f1", b"1")
    put_file(fs, "keep/sub/f2", b"22")
    assert tree(fs, path="keep", recursive=False) == {"keep/f1": False, "keep/sub": True}


def test_get_paths_nested_path_content_length(fs):
    for i in range(3):
        sub = fs.create_directory(f"dir1{i}").create_sub_directory("subdir")
        sub.create_sub_directory("subsub")
        f = sub.create_file("file")
        f.append_data(b"abced", 0, 5)
        f.flush_data(5)
    paths = list(next(fs.get_paths(path="dir10/subdir", max_results=2).by_page()))
    assert len(paths) == 2
    assert paths[0].name == "dir10/subdir/file"
    assert paths[0].content_length == 5


@pytest.mark.docs
def test_get_paths_missing_directory_is_path_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        list(fs.get_paths(path="no/such/dir"))
    assert_error(e, "PathNotFound", 404)


def test_get_paths_max_results_pages_and_continuation(fs):
    for i in range(6):
        fs.create_directory(f"dir1{i}")
    gen1 = fs.get_paths(max_results=2, upn=True).by_page()
    page1 = list(next(gen1))
    token = gen1.continuation_token
    assert token
    gen2 = fs.get_paths(max_results=4, upn=True).by_page(continuation_token=token)
    page2 = list(next(gen2))
    assert [p.name for p in page1] == ["dir10", "dir11"]
    assert len(page2) == 4
    assert page2[0].name == "dir12"


def test_get_paths_pages_exactly_cover_listing(fs):
    """Recorded: 6 dirs + 6 files with max_results=6 -> two pages of 6, then nothing more."""
    for i in range(6):
        fs.create_directory(f"dir1{i}")
    for i in range(6):
        fs.create_file(f"file{i}")
    gen = fs.get_paths(max_results=6, upn=True).by_page()
    paths1 = list(next(gen))
    paths2 = list(next(gen))
    assert len(paths1) == 6
    assert len(paths2) == 6
    assert names(paths1).isdisjoint(names(paths2))
    # Recorded: no third page. (Azure sometimes ends with an empty page after a full one, so an empty
    # trailing page is tolerated; a non-empty third page is not.)
    assert [p for page in gen for p in page] == []


def test_get_paths_max_results_larger_than_listing_single_page(fs):
    for i in range(3):
        fs.create_file(f"f{i}")
    pages = list(fs.get_paths(max_results=100).by_page())
    assert [len(list(p)) for p in pages] == [3]


def test_get_paths_paged_union_equals_full_listing(fs):
    for i in range(4):
        put_file(fs, f"d{i}/sub/f", b"x")
    full = [p.name for p in fs.get_paths()]
    paged = [p.name for page in fs.get_paths(max_results=3).by_page() for p in page]
    assert paged == full
    assert len(full) == 12


def test_get_paths_start_from(fs):
    for i in range(6):
        d = fs.create_directory(f"dir{i}")
        sub = d.create_sub_directory("subdir")
        sub.create_sub_directory("subsub")
        sub.create_file("file")
    got = list(fs.get_paths(recursive=True, max_results=2, start_from="dir3"))
    # dir3 .. dir5, four entries each (recorded: 12)
    assert len(got) == 12
    assert got[0].name == "dir3"


def test_directory_client_get_paths(fs):
    d1 = fs.get_directory_client("dirA1")
    d1.get_file_client("file0").create_file()
    d1.get_file_client("file1").create_file()
    d2 = fs.get_directory_client("dirA2")
    d2.get_file_client("file2").create_file()
    got = list(d1.get_paths())
    assert [p["name"] for p in got] == ["dirA1/file0", "dirA1/file1"]


def test_directory_client_get_paths_start_from_is_relative_to_directory(fs):
    """Recorded: start_from="subdir1/file4" under directory2 -> 6 entries starting at dir2/subdir1/file4."""
    d2 = fs.get_directory_client("dirB2")
    d2.get_file_client("file2").create_file()
    s1 = d2.create_sub_directory("subdir1")
    for n in ("file3", "file4", "file5"):
        s1.get_file_client(n).create_file()
    s2 = d2.create_sub_directory("subdir2")
    for n in ("file6", "file7", "file8"):
        s2.get_file_client(n).create_file()
    got = list(d2.get_paths(recursive=True, start_from="subdir1/file4", max_results=2))
    assert len(got) == 6
    assert got[0]["name"] == "dirB2/subdir1/file4"
    assert got[1]["name"] == "dirB2/subdir1/file5"


def test_directory_client_get_paths_non_recursive(fs):
    put_file(fs, "d/f1", b"1")
    put_file(fs, "d/sub/f2", b"2")
    got = {p.name for p in fs.get_directory_client("d").get_paths(recursive=False)}
    assert got == {"d/f1", "d/sub"}


def test_get_paths_after_delete_and_rename(fs):
    put_file(fs, "a/f", b"x")
    put_file(fs, "b/f", b"x")
    fs.get_directory_client("a").rename_directory(fs.file_system_name + "/c")
    fs.delete_directory("b")
    assert tree(fs) == {"c": True, "c/f": False}


def test_get_paths_reflect_size_after_overwrite(fs):
    f = put_file(fs, "f", b"1234567890")
    f.upload_data(b"abc", overwrite=True)
    [p] = list(fs.get_paths())
    assert p.content_length == 3
