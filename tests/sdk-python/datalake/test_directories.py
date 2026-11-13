"""DataLakeDirectoryClient: create / exists / properties / delete / rename / sub-directory helpers."""

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.filedatalake import ContentSettings, DataLakeDirectoryClient, DataLakeFileClient

from dlutil import (
    DEFAULT_DIR_ACL,
    DEFAULT_DIR_PERMISSIONS,
    SUPERUSER,
    assert_error,
    assert_error_in,
    assert_status,
    put_file,
    read_file,
    tree,
)


# -- create ------------------------------------------------------------------
def test_create_directory_returns_etag_and_last_modified(fs):
    created = fs.get_directory_client("d1").create_directory()
    assert created["etag"]
    assert created["last_modified"] is not None


def test_file_system_create_directory_returns_client(fs):
    d = fs.create_directory("d1")
    assert isinstance(d, DataLakeDirectoryClient)
    assert d.path_name == "d1"
    assert d.exists()


def test_create_nested_directory_creates_parents(fs):
    fs.create_directory("a/b/c")
    assert tree(fs) == {"a": True, "a/b": True, "a/b/c": True}
    for p in ("a", "a/b", "a/b/c"):
        assert fs.get_directory_client(p).exists()


def test_create_directory_with_metadata(fs):
    d = fs.get_directory_client("d1")
    d.create_directory(metadata={"hello": "world", "number": "42"})
    props = d.get_directory_properties()
    assert props.metadata["hello"] == "world"
    assert props.metadata["number"] == "42"


def test_create_directory_with_content_settings_is_accepted(fs):
    created = fs.get_directory_client("d1").create_directory(
        content_settings=ContentSettings(content_language="spanish", content_disposition="inline")
    )
    assert created["etag"]


def test_create_directory_if_none_match_star_on_new_path(fs):
    created = fs.get_directory_client("d1").create_directory(match_condition=MatchConditions.IfMissing)
    assert created["etag"]


def test_create_directory_if_none_match_star_on_existing_path_fails(fs):
    d = fs.create_directory("d1")
    with pytest.raises(ResourceExistsError) as e:
        d.create_directory(match_condition=MatchConditions.IfMissing)
    assert_error(e, "PathAlreadyExists", 409)


def test_create_directory_with_proposed_lease(fs):
    d = fs.get_directory_client("d1")
    d.create_directory(lease_id="4cf4e284-f6a8-4540-b53e-c3469af032dc", lease_duration=15)
    props = d.get_directory_properties()
    assert props.lease.state == "leased"
    assert props.lease.status == "locked"


def test_create_sub_directory_with_proposed_lease(fs):
    d = fs.create_directory("d1")
    sub = d.create_sub_directory("sub", lease_id="4cf4e284-f6a8-4540-b53e-c3469af032dc", lease_duration=15)
    assert sub.get_directory_properties().lease.state == "leased"


@pytest.mark.docs
def test_create_directory_where_file_exists_is_path_conflict(fs):
    fs.create_file("x")
    with pytest.raises(HttpResponseError) as e:
        fs.get_directory_client("x").create_directory()
    assert_error(e, "PathConflict", 409)


@pytest.mark.docs
def test_create_file_where_directory_exists_is_path_conflict(fs):
    fs.create_directory("x")
    with pytest.raises(HttpResponseError) as e:
        fs.get_file_client("x").create_file()
    assert_error(e, "PathConflict", 409)


@pytest.mark.docs
def test_create_file_below_a_file_conflicts(fs):
    fs.create_file("x")
    with pytest.raises(HttpResponseError) as e:
        fs.get_file_client("x/y").create_file()
    assert e.value.status_code == 409


# -- exists / properties -----------------------------------------------------
def test_directory_exists_true_and_false(fs):
    d = fs.get_directory_client("d1")
    assert not d.exists()
    d.create_directory()
    assert d.exists()


def test_directory_exists_is_true_for_implicit_parent(fs):
    fs.create_file("p/q/f.txt")
    assert fs.get_directory_client("p").exists()
    assert fs.get_directory_client("p/q").exists()


def test_directory_properties_fields(fs):
    d = fs.get_directory_client("d1")
    created = d.create_directory(metadata={"hello": "world"})
    props = d.get_directory_properties()
    assert props.name == "d1"
    assert props.etag.strip('"') == created["etag"].strip('"')
    assert props.last_modified is not None
    assert props.creation_time is not None
    assert props.deleted is False
    assert props.lease.state == "available"
    assert props.lease.status == "unlocked"


def test_directory_properties_owner_group_permissions_acl(fs):
    d = fs.create_directory("d1")
    props = d.get_directory_properties()
    assert props.owner == SUPERUSER
    assert props.group == SUPERUSER
    assert props.permissions == DEFAULT_DIR_PERMISSIONS
    assert props.acl == DEFAULT_DIR_ACL


def test_directory_properties_expose_hdi_isfolder_metadata(fs):
    d = fs.create_directory("d1")
    assert d.get_directory_properties().metadata["hdi_isfolder"] == "true"


def test_file_properties_do_not_have_hdi_isfolder(fs):
    f = fs.create_file("f1")
    assert "hdi_isfolder" not in (f.get_file_properties().metadata or {})


def test_get_properties_of_missing_directory_raises(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_directory_client("missing").get_directory_properties()
    assert_error(e, "BlobNotFound", 404)


# -- delete ------------------------------------------------------------------
def test_delete_empty_directory(fs):
    d = fs.create_directory("d1")
    d.delete_directory()
    assert not d.exists()
    assert tree(fs) == {}


def test_delete_directory_removes_children_recursively(fs):
    put_file(fs, "d1/a/b/f.txt", b"x")
    put_file(fs, "d1/g.txt", b"y")
    put_file(fs, "keep.txt", b"z")
    fs.get_directory_client("d1").delete_directory()
    assert tree(fs) == {"keep.txt": False}
    assert not fs.get_file_client("d1/a/b/f.txt").exists()


def test_file_system_delete_directory(fs):
    fs.create_directory("d1/d2")
    fs.delete_directory("d1")
    assert tree(fs) == {}


def test_delete_missing_directory_raises_not_found(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_directory_client("nodir").delete_directory()
    assert_status(e, 404)


@pytest.mark.docs
def test_delete_missing_directory_error_code(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_directory_client("nodir").delete_directory()
    assert_error(e, "PathNotFound", 404)


def test_delete_directory_with_if_modified_since_equal_to_last_modified_fails(fs):
    """Recorded: If-Modified-Since == Last-Modified -> 412 ConditionNotMet (JSON body)."""
    d = fs.create_directory("d1")
    props = d.get_directory_properties()
    with pytest.raises(HttpResponseError) as e:
        d.delete_directory(if_modified_since=props.last_modified)
    assert_error(e, "ConditionNotMet", 412)
    assert d.exists()


# -- sub-directory helpers ---------------------------------------------------
def test_create_and_delete_sub_directory(fs):
    d = fs.create_directory("d1")
    sub = d.create_sub_directory("subdir")
    assert isinstance(sub, DataLakeDirectoryClient)
    assert sub.path_name == "d1/subdir"
    assert fs.get_directory_client("d1/subdir").get_directory_properties()
    d.delete_sub_directory("subdir")
    with pytest.raises(ResourceNotFoundError):
        fs.get_directory_client("d1/subdir").get_directory_properties()


def test_get_sub_directory_client(fs):
    d = fs.create_directory("d1")
    d.create_sub_directory("s")
    sub = d.get_sub_directory_client("s")
    assert sub.path_name == "d1/s"
    assert sub.exists()


def test_directory_create_file_and_get_file_client(fs):
    d = fs.create_directory("d1")
    f = d.create_file("a.txt")
    assert isinstance(f, DataLakeFileClient)
    assert f.path_name == "d1/a.txt"
    assert d.get_file_client("a.txt").exists()
    assert not d.get_file_client("b.txt").exists()


def test_create_file_in_directory_creates_nested_subdirectories(fs):
    d = fs.create_directory("d1")
    d.create_file("x/y/z.txt")
    assert tree(fs) == {"d1": True, "d1/x": True, "d1/x/y": True, "d1/x/y/z.txt": False}


# -- rename ------------------------------------------------------------------
def test_rename_directory_within_filesystem(fs):
    d = fs.create_directory("olddir")
    new = d.rename_directory(fs.file_system_name + "/newname")
    assert isinstance(new, DataLakeDirectoryClient)
    assert new.path_name == "newname"
    assert new.get_directory_properties().name == "newname"
    assert not d.exists()
    assert tree(fs) == {"newname": True}


def test_rename_directory_keeps_etag_of_source(fs):
    """Recorded: the rename response carries the source's ETag."""
    d = fs.create_directory("olddir")
    before = d.get_directory_properties().etag
    new = d.rename_directory(fs.file_system_name + "/newname")
    assert new.get_directory_properties().etag == before


def test_rename_directory_moves_children(fs):
    put_file(fs, "src/a.txt", b"aaa")
    put_file(fs, "src/sub/b.txt", b"bbbb")
    fs.get_directory_client("src").rename_directory(fs.file_system_name + "/dst")
    assert tree(fs) == {"dst": True, "dst/a.txt": False, "dst/sub": True, "dst/sub/b.txt": False}
    assert read_file(fs.get_file_client("dst/sub/b.txt")) == b"bbbb"
    assert not fs.get_file_client("src/a.txt").exists()


def test_rename_directory_to_nested_destination_in_existing_parent(fs):
    fs.create_directory("src")
    fs.create_directory("parent")
    fs.get_directory_client("src").rename_directory(fs.file_system_name + "/parent/moved")
    assert tree(fs) == {"parent": True, "parent/moved": True}


def test_rename_directory_accepts_leading_slash(fs):
    d = fs.create_directory("src")
    new = d.rename_directory("/" + fs.file_system_name + "/dst")
    assert new.path_name == "dst"
    assert new.exists()


def test_rename_directory_to_other_filesystem(fs, fs2):
    d = fs.create_directory("old dir")
    put_file(fs, "old dir/oldfile", b"abc")
    fs2.create_directory("new name/sub dir")
    new = d.rename_directory("/" + fs2.file_system_name + "/new name/sub dir")
    assert new.file_system_name == fs2.file_system_name
    assert new.get_directory_properties() is not None
    assert new.get_file_client("oldfile").get_file_properties() is not None
    assert read_file(new.get_file_client("oldfile")) == b"abc"
    assert not d.exists()
    assert tree(fs) == {}


def test_rename_directory_to_nonexisting_directory_in_other_filesystem(fs, fs2):
    d = fs.create_directory("source")
    d.create_sub_directory("subdir")
    new = d.rename_directory(fs2.file_system_name + "/nonexisting")
    assert new.get_directory_properties() is not None
    assert tree(fs2) == {"nonexisting": True, "nonexisting/subdir": True}
    assert tree(fs) == {}


def test_rename_directory_onto_existing_empty_directory_replaces_it(fs):
    """Recorded: rename over an existing directory succeeds (201) and the source is gone."""
    fs.create_directory("dest")
    src = fs.create_directory("source")
    src.create_sub_directory("subdir")
    new = src.rename_directory(fs.file_system_name + "/dest")
    assert new.exists()
    assert tree(fs) == {"dest": True, "dest/subdir": True}


def test_rename_directory_with_if_none_match_and_source_if_match(fs, fs2):
    """Recorded: dest If-None-Match:* + source If-Match:<etag> on a fresh destination -> success."""
    src = fs.create_directory("source")
    src.create_sub_directory("subdir")
    etag = src.get_directory_properties().etag
    new = src.rename_directory(
        fs2.file_system_name + "/nonexistingdir",
        match_condition=MatchConditions.IfMissing,
        source_etag=etag,
        source_match_condition=MatchConditions.IfNotModified,
    )
    assert new.exists()
    assert not src.exists()


@pytest.mark.docs
def test_rename_directory_if_none_match_star_on_existing_destination_fails(fs):
    src = fs.create_directory("source")
    fs.create_directory("dest")
    with pytest.raises(HttpResponseError) as e:
        src.rename_directory(fs.file_system_name + "/dest", match_condition=MatchConditions.IfMissing)
    # not recorded: create answers 409 PathAlreadyExists for If-None-Match:*; rename may answer 412
    # ConditionNotMet (plain HTTP semantics). Either is acceptable, success is not.
    assert_error_in(e, ("PathAlreadyExists", "ConditionNotMet"), e.value.status_code)
    assert e.value.status_code in (409, 412)
    assert src.exists()


@pytest.mark.docs
def test_rename_directory_source_etag_mismatch_fails_412(fs):
    src = fs.create_directory("source")
    with pytest.raises(HttpResponseError) as e:
        src.rename_directory(
            fs.file_system_name + "/dest",
            source_etag='"0xDEADBEEF"',
            source_match_condition=MatchConditions.IfNotModified,
        )
    # Microsoft's Data Lake error table: 412 SourceConditionNotMet
    assert_error(e, "SourceConditionNotMet", 412)
    assert src.exists()


@pytest.mark.docs
def test_rename_missing_directory_error_code(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_directory_client("nothere").rename_directory(fs.file_system_name + "/dest")
    assert_error(e, "SourcePathNotFound", 404)


@pytest.mark.docs
def test_rename_directory_into_missing_parent_error_code(fs):
    src = fs.create_directory("source")
    with pytest.raises(ResourceNotFoundError) as e:
        src.rename_directory(fs.file_system_name + "/no/such/parent/dest")
    assert_error(e, "RenameDestinationParentPathNotFound", 404)
    assert src.exists()


def test_rename_directory_with_special_characters(fs):
    d = fs.create_directory("olddir")
    new = d.rename_directory(fs.file_system_name + "/?!@#$%^&*.?test")
    assert new.get_directory_properties().name == "?!@#$%^&*.?test"


def test_rename_directory_with_spaces_and_unicode(fs):
    d = fs.create_directory("old dir")
    new = d.rename_directory(fs.file_system_name + "/caf\u00e9 \u30c7\u30fc\u30bf")
    assert new.get_directory_properties().name == "caf\u00e9 \u30c7\u30fc\u30bf"
    assert not d.exists()


@pytest.mark.docs
def test_directory_unicode_and_space_names_round_trip(fs):
    name = "café データ/sub dir+1"
    fs.create_directory(name)
    assert tree(fs)[name] is True
    assert fs.get_directory_client(name).exists()
