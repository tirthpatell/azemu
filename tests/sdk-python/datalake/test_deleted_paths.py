"""Soft delete of paths (needs delete retention enabled through the blob service properties).

Recorded on a soft-delete account: dfs DELETE answers 200 with `x-ms-delete-type-permanent: false` and
`x-ms-deletion-id`; deleted paths are listed through the blob endpoint (`comp=list&showonly=deleted`);
undelete is a blob `comp=undelete` call.  Without retention the delete is permanent
(`x-ms-delete-type-permanent: true`, no deletion id).
"""

import pytest
from azure.storage.filedatalake import RetentionPolicy

from dlutil import put_file, tree


@pytest.fixture
def soft_fs(service, fs):
    service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=1))
    return fs


def test_delete_without_retention_is_permanent(fs, raw):
    fs.create_file("f")
    r = raw.dfs("DELETE", f"{fs.file_system_name}/f")
    assert r.status_code == 200
    assert r.headers.get("x-ms-delete-type-permanent") == "true"
    assert list(fs.list_deleted_paths()) == []


def test_delete_with_retention_returns_deletion_id(soft_fs):
    f = soft_fs.create_file("f")
    resp = f.delete_file()
    assert resp["deletion_id"]


def test_delete_with_retention_header_is_not_permanent(soft_fs, raw):
    soft_fs.create_file("f")
    r = raw.dfs("DELETE", f"{soft_fs.file_system_name}/f")
    assert r.status_code == 200
    assert r.headers["x-ms-delete-type-permanent"] == "false"
    assert r.headers["x-ms-deletion-id"]


def test_list_deleted_paths(soft_fs):
    """Recorded scenario: 2 files, 2 directories and 2 files in dir3 deleted -> 6 deleted paths."""
    fs = soft_fs
    file0 = fs.create_file("file0")
    file1 = fs.create_file("file1")
    dir1 = fs.create_directory("dir1")
    dir2 = fs.create_directory("dir2")
    dir3 = fs.create_directory("dir3")
    in_dir3 = dir3.create_file("file_in_dir3")
    in_sub = dir3.create_file("subdir/file_in_subdir")
    for c in (file0, file1):
        c.delete_file()
    dir1.delete_directory()
    dir2.delete_directory()
    in_dir3.delete_file()
    in_sub.delete_file()
    deleted = list(fs.list_deleted_paths())
    assert len(deleted) == 6
    dir3_paths = list(fs.list_deleted_paths(path_prefix="dir3/"))
    assert [p.name for p in dir3_paths] == ["dir3/file_in_dir3", "dir3/subdir/file_in_subdir"]
    assert all(p.deletion_id for p in dir3_paths)
    assert all(p.deleted_time is not None for p in dir3_paths)
    assert all(p.remaining_retention_days is not None for p in dir3_paths)


def test_list_deleted_paths_paging(soft_fs):
    fs = soft_fs
    for i in range(6):
        fs.create_file(f"f{i}").delete_file()
    gen1 = fs.list_deleted_paths(results_per_page=2).by_page()
    page1 = list(next(gen1))
    gen2 = fs.list_deleted_paths(results_per_page=4).by_page(continuation_token=gen1.continuation_token)
    page2 = list(next(gen2))
    assert len(page1) == 2
    assert len(page2) == 4
    assert {p.name for p in page1}.isdisjoint({p.name for p in page2})


def test_deleted_paths_do_not_show_in_get_paths(soft_fs):
    put_file(soft_fs, "keep", b"1")
    put_file(soft_fs, "gone", b"2").delete_file()
    assert tree(soft_fs) == {"keep": False}


def test_undelete_file_with_deletion_id(soft_fs):
    """Recorded (private SDK API `_undelete_path`, blob `comp=undelete`)."""
    f = put_file(soft_fs, "dir10/file", b"abc")
    resp = f.delete_file()
    assert not f.exists()
    restored = soft_fs._undelete_path("dir10/file", resp["deletion_id"])
    assert restored.get_file_properties() is not None


def test_undelete_directory_with_deletion_id(soft_fs):
    d = soft_fs.create_directory("dir10")
    resp = d.delete_directory()
    restored = soft_fs._undelete_path("dir10", resp["deletion_id"])
    assert restored.get_directory_properties() is not None
