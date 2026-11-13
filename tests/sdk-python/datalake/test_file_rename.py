"""rename_file: within and across directories / filesystems, overwrite, conditions."""

from datetime import timedelta

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.filedatalake import DataLakeFileClient

from dlutil import assert_error, assert_error_in, assert_status, past, put_file, read_file, tree


def test_rename_file_within_filesystem(fs):
    f = put_file(fs, "oldfile", b"abc")
    new = f.rename_file(fs.file_system_name + "/newname")
    assert isinstance(new, DataLakeFileClient)
    assert new.path_name == "newname"
    assert read_file(new) == b"abc"
    assert not f.exists()
    assert tree(fs) == {"newname": False}


def test_rename_file_keeps_etag_and_size(fs):
    """Recorded: the rename response carries the same ETag as the last flush."""
    f = put_file(fs, "f", b"abc")
    before = f.get_file_properties()
    new = f.rename_file(fs.file_system_name + "/g")
    after = new.get_file_properties()
    assert after.etag == before.etag
    assert after.size == 3


def test_rename_file_into_other_existing_directory(fs):
    f = put_file(fs, "a/f.txt", b"abc")
    fs.create_directory("b")
    new = f.rename_file(fs.file_system_name + "/b/moved.txt")
    assert read_file(new) == b"abc"
    assert tree(fs) == {"a": True, "b": True, "b/moved.txt": False}


def test_rename_file_to_other_filesystem(fs, fs2):
    f = put_file(fs, "f", b"abc")
    new = f.rename_file(fs2.file_system_name + "/g")
    assert new.file_system_name == fs2.file_system_name
    assert read_file(new) == b"abc"
    assert not f.exists()
    assert tree(fs) == {}
    assert tree(fs2) == {"g": False}


def test_rename_file_accepts_leading_slash(fs):
    f = put_file(fs, "f", b"abc")
    new = f.rename_file("/" + fs.file_system_name + "/g")
    assert read_file(new) == b"abc"


def test_rename_file_over_existing_file_replaces_it(fs):
    """Recorded: rename onto an existing file succeeds; the destination now holds the source content."""
    existing = put_file(fs, "existingfile", b"a")
    f = put_file(fs, "f", b"abc")
    new = f.rename_file(fs.file_system_name + "/" + existing.path_name)
    assert read_file(new) == b"abc"
    assert not f.exists()
    assert tree(fs) == {"existingfile": False}


def test_rename_file_will_not_change_existing_directory(fs):
    """Recorded scenario: dir2/file3 renamed over dir1/file1; siblings untouched."""
    put_file(fs, "dir1/file1", b"file1")
    put_file(fs, "dir1/file2", b"file2")
    put_file(fs, "dir2/file3", b"file3")
    put_file(fs, "dir2/file4", b"file4")
    fs.get_file_client("dir2/file3").rename_file(fs.file_system_name + "/dir1/file1")
    assert read_file(fs.get_file_client("dir1/file1")) == b"file3"
    assert read_file(fs.get_file_client("dir1/file2")) == b"file2"
    assert read_file(fs.get_file_client("dir2/file4")) == b"file4"
    assert not fs.get_file_client("dir2/file3").exists()


def test_rename_file_with_special_characters_in_new_name(fs):
    f = put_file(fs, "oldfile", b"abc")
    new = f.rename_file(fs.file_system_name + "/?!@#$%^&*.?test")
    assert new.get_file_properties().name == "?!@#$%^&*.?test"
    assert read_file(new) == b"abc"


def test_rename_file_from_and_to_names_with_spaces(fs):
    f = put_file(fs, "old dir/old file.txt", b"abc")
    fs.create_directory("new dir")
    new = f.rename_file(fs.file_system_name + "/new dir/new file.txt")
    assert read_file(new) == b"abc"
    assert not f.exists()


def test_rename_file_twice(fs):
    f = put_file(fs, "a", b"abc")
    b = f.rename_file(fs.file_system_name + "/b")
    c = b.rename_file(fs.file_system_name + "/c")
    assert read_file(c) == b"abc"
    assert tree(fs) == {"c": False}


@pytest.mark.docs
def test_rename_file_preserves_metadata(fs):
    """Docs: without x-ms-properties on the rename request the existing properties are preserved."""
    f = fs.create_file("f", metadata={"k": "v"})
    new = f.rename_file(fs.file_system_name + "/g")
    assert new.get_file_properties().metadata == {"k": "v"}


def test_rename_file_new_dest_with_if_none_match_and_source_if_match(fs):
    f = put_file(fs, "f", b"abc")
    etag = f.get_file_properties().etag
    new = f.rename_file(
        fs.file_system_name + "/g",
        match_condition=MatchConditions.IfMissing,
        source_etag=etag,
        source_match_condition=MatchConditions.IfNotModified,
    )
    assert read_file(new) == b"abc"


def test_rename_file_source_if_unmodified_since_in_future_succeeds(fs):
    from dlutil import future

    f = put_file(fs, "f", b"abc")
    new = f.rename_file(fs.file_system_name + "/g", source_if_unmodified_since=future(60))
    assert new.exists()


@pytest.mark.docs
def test_rename_file_if_none_match_star_on_existing_destination_fails(fs):
    put_file(fs, "dest", b"old")
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.rename_file(fs.file_system_name + "/dest", match_condition=MatchConditions.IfMissing)
    # not recorded: create answers 409 PathAlreadyExists for If-None-Match:*; rename may answer 412
    # ConditionNotMet (plain HTTP semantics). Either is acceptable, success is not.
    assert_error_in(e, ("PathAlreadyExists", "ConditionNotMet"), e.value.status_code)
    assert e.value.status_code in (409, 412)
    assert read_file(fs.get_file_client("dest")) == b"old"
    assert f.exists()


@pytest.mark.docs
def test_rename_file_source_etag_mismatch_fails_412(fs):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.rename_file(
            fs.file_system_name + "/g",
            source_etag='"0xDEADBEEF"',
            source_match_condition=MatchConditions.IfNotModified,
        )
    # Microsoft's Data Lake error table: 412 SourceConditionNotMet
    assert_error(e, "SourceConditionNotMet", 412)
    assert f.exists()
    assert not fs.get_file_client("g").exists()


@pytest.mark.docs
def test_rename_file_source_if_unmodified_since_before_creation_fails_412(fs):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.rename_file(fs.file_system_name + "/g", source_if_unmodified_since=past(60))
    assert_error(e, "SourceConditionNotMet", 412)
    assert f.exists()


@pytest.mark.docs
def test_rename_missing_file_error_code(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").rename_file(fs.file_system_name + "/g")
    assert_error(e, "SourcePathNotFound", 404)


@pytest.mark.docs
def test_rename_file_into_missing_parent_error_code(fs):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(ResourceNotFoundError) as e:
        f.rename_file(fs.file_system_name + "/no/such/parent/g")
    assert_error(e, "RenameDestinationParentPathNotFound", 404)
    assert f.exists()


@pytest.mark.docs
def test_rename_file_into_missing_filesystem(fs, service, uniq):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(ResourceNotFoundError) as e:
        f.rename_file(uniq("nofs") + "/g")
    assert_status(e, 404)
    assert f.exists()


@pytest.mark.docs
def test_rename_file_onto_existing_directory_is_resource_type_mismatch(fs):
    """Docs: 409 InvalidSourceOrDestinationResourceType - source and destination types must be identical."""
    fs.create_directory("dest")
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.rename_file(fs.file_system_name + "/dest")
    assert_error(e, "InvalidSourceOrDestinationResourceType", 409)
    assert f.exists()
    assert fs.get_directory_client("dest").exists()


@pytest.mark.docs
def test_rename_directory_onto_existing_file_is_resource_type_mismatch(fs):
    put_file(fs, "dest", b"x")
    d = fs.create_directory("d")
    with pytest.raises(HttpResponseError) as e:
        d.rename_directory(fs.file_system_name + "/dest")
    assert_error(e, "InvalidSourceOrDestinationResourceType", 409)
    assert d.exists()


@pytest.mark.docs
def test_rename_directory_into_its_own_subdirectory_is_invalid_source_path(fs):
    """Docs: 409 InvalidRenameSourcePath - destination cannot be a subdirectory of the source."""
    d = fs.create_directory("a")
    with pytest.raises(HttpResponseError) as e:
        d.rename_directory(fs.file_system_name + "/a/inner")
    assert_error(e, "InvalidRenameSourcePath", 409)
    assert tree(fs) == {"a": True}
