"""Conditional requests (If-Match / If-None-Match / If-(Un)Modified-Since) on files and directories.

Recorded results: dfs 412 `ConditionNotMet` with a JSON body for create/flush/delete/setAccessControl.
HEAD/GET (blob endpoint) results follow the blob rules: 412 `ConditionNotMet`, 304 for If-None-Match /
If-Modified-Since.
"""

from datetime import timedelta

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceModifiedError

from dlutil import assert_error, assert_status, future, past, put_file, read_file


# -- create ------------------------------------------------------------------
def test_create_file_if_none_match_star_new_ok_existing_409(fs):
    f = fs.get_file_client("f")
    f.create_file(match_condition=MatchConditions.IfMissing)
    with pytest.raises(HttpResponseError) as e:
        f.create_file(match_condition=MatchConditions.IfMissing)
    assert_error(e, "PathAlreadyExists", 409)


def test_create_directory_if_none_match_star_new_ok_existing_409(fs):
    d = fs.get_directory_client("d")
    d.create_directory(match_condition=MatchConditions.IfMissing)
    with pytest.raises(HttpResponseError) as e:
        d.create_directory(match_condition=MatchConditions.IfMissing)
    assert_error(e, "PathAlreadyExists", 409)


@pytest.mark.docs
def test_create_file_if_match_current_etag_overwrites(fs):
    f = fs.get_file_client("f")
    etag = f.create_file()["etag"]
    f.create_file(etag=etag, match_condition=MatchConditions.IfNotModified)


@pytest.mark.docs
def test_create_file_if_match_stale_etag_is_412(fs):
    f = fs.get_file_client("f")
    f.create_file()
    with pytest.raises(HttpResponseError) as e:
        f.create_file(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)


@pytest.mark.docs
def test_create_file_if_unmodified_since_past_is_412(fs):
    f = fs.get_file_client("f")
    f.create_file()
    with pytest.raises(HttpResponseError) as e:
        f.create_file(if_unmodified_since=past(60))
    assert_error(e, "ConditionNotMet", 412)


# -- append / flush ----------------------------------------------------------
def test_flush_with_current_etag_succeeds_then_stale_etag_fails(fs):
    """Recorded scenario (flush_data_with_match_condition)."""
    f = fs.create_file("f")
    etag = f.get_file_properties().etag
    f.append_data(b"abc", 0, 3)
    f.flush_data(3, etag=etag, match_condition=MatchConditions.IfNotModified)
    f.append_data(b"abc", 3, 3)
    with pytest.raises(ResourceModifiedError) as e:
        f.flush_data(6, etag=etag, match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)
    assert f.get_file_properties().size == 3


def test_flush_if_none_match_star_on_existing_file_fails(fs):
    """Recorded: flush with If-None-Match: * on an existing file -> 412 ConditionNotMet."""
    f = put_file(fs, "f", b"abc")
    f.append_data(b"x" * 100, 0, 100)
    with pytest.raises(HttpResponseError) as e:
        f.flush_data(100, match_condition=MatchConditions.IfMissing)
    assert_error(e, "ConditionNotMet", 412)
    assert read_file(f) == b"abc"


@pytest.mark.docs
def test_flush_if_unmodified_since_past_is_412(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    with pytest.raises(HttpResponseError) as e:
        f.flush_data(3, if_unmodified_since=past(60))
    assert_error(e, "ConditionNotMet", 412)


@pytest.mark.docs
def test_flush_if_unmodified_since_future_succeeds(fs):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    f.flush_data(3, if_unmodified_since=future(60))
    assert f.get_file_properties().size == 3


# -- delete ------------------------------------------------------------------
def test_delete_file_if_unmodified_since_last_modified_succeeds(fs):
    """Recorded: if_unmodified_since == the file's last_modified deletes it."""
    f = put_file(fs, "f", b"abc")
    props = f.get_file_properties()
    f.delete_file(if_unmodified_since=props.last_modified)
    assert not f.exists()


def test_delete_directory_if_modified_since_last_modified_is_412(fs):
    d = fs.create_directory("d")
    props = d.get_directory_properties()
    with pytest.raises(HttpResponseError) as e:
        d.delete_directory(if_modified_since=props.last_modified)
    assert_error(e, "ConditionNotMet", 412)
    assert d.exists()


@pytest.mark.docs
def test_delete_file_if_modified_since_future_is_412(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.delete_file(if_modified_since=future(60))
    assert_error(e, "ConditionNotMet", 412)
    assert f.exists()


@pytest.mark.docs
def test_delete_file_if_match_stale_etag_is_412(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.delete_file(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)
    assert f.exists()


@pytest.mark.docs
def test_delete_file_if_match_current_etag_succeeds(fs):
    f = fs.create_file("f")
    etag = f.get_file_properties().etag
    f.delete_file(etag=etag, match_condition=MatchConditions.IfNotModified)
    assert not f.exists()


@pytest.mark.docs
def test_delete_directory_if_match_stale_etag_is_412(fs):
    d = fs.create_directory("d")
    with pytest.raises(HttpResponseError) as e:
        d.delete_directory(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)


# -- set / get access control ------------------------------------------------
def test_set_access_control_if_missing_on_existing_file_is_412(fs):
    """Recorded: setAccessControl with If-None-Match: * on an existing path -> 412 ConditionNotMet."""
    f = fs.create_file("f")
    with pytest.raises(ResourceModifiedError) as e:
        f.set_access_control(permissions="0777", match_condition=MatchConditions.IfMissing)
    assert_error(e, "ConditionNotMet", 412)


def test_set_access_control_with_current_etag_succeeds_on_directory(fs):
    d = fs.create_directory("d")
    etag = d.get_directory_properties().etag
    resp = d.set_access_control(permissions="0777", etag=etag, match_condition=MatchConditions.IfNotModified)
    assert resp is not None


def test_get_access_control_with_current_etag_succeeds(fs):
    d = fs.get_directory_client("d")
    d.create_directory(permissions="0777", umask="0000")
    etag = d.get_directory_properties().etag
    ac = d.get_access_control(etag=etag, match_condition=MatchConditions.IfNotModified)
    assert ac["permissions"] == "rwxrwxrwx"


@pytest.mark.docs
def test_get_access_control_with_stale_etag_is_412(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.get_access_control(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)
    assert_status(e, 412)


# -- get properties (blob endpoint) ------------------------------------------
@pytest.mark.docs
def test_get_file_properties_if_match_stale_etag_is_412(fs):
    f = fs.create_file("f")
    with pytest.raises(ResourceModifiedError) as e:
        f.get_file_properties(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)


@pytest.mark.docs
def test_get_file_properties_if_none_match_current_etag_is_304(fs):
    f = put_file(fs, "f", b"abc")
    etag = f.get_file_properties().etag
    # 304 carries x-ms-error-code: ConditionNotMet (recorded for tags), so the SDK raises ResourceModifiedError
    with pytest.raises(HttpResponseError) as e:
        f.get_file_properties(etag=etag, match_condition=MatchConditions.IfModified)
    assert_status(e, 304)


@pytest.mark.docs
def test_get_file_properties_if_modified_since_future_is_304(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.get_file_properties(if_modified_since=future(60))
    assert_status(e, 304)


@pytest.mark.docs
def test_get_file_properties_if_unmodified_since_past_is_412(fs):
    f = fs.create_file("f")
    with pytest.raises(ResourceModifiedError):
        f.get_file_properties(if_unmodified_since=past(60))


@pytest.mark.docs
def test_get_directory_properties_conditions(fs):
    d = fs.create_directory("d")
    etag = d.get_directory_properties().etag
    assert d.get_directory_properties(etag=etag, match_condition=MatchConditions.IfNotModified).name == "d"
    with pytest.raises(ResourceModifiedError):
        d.get_directory_properties(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified)


@pytest.mark.docs
def test_download_if_match_stale_etag_is_412(fs):
    f = put_file(fs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        f.download_file(etag='"0xDEADBEEF"', match_condition=MatchConditions.IfNotModified).readall()
    assert_error(e, "ConditionNotMet", 412)


@pytest.mark.docs
def test_download_if_match_current_etag_succeeds(fs):
    f = put_file(fs, "f", b"abc")
    etag = f.get_file_properties().etag
    assert f.download_file(etag=etag, match_condition=MatchConditions.IfNotModified).readall() == b"abc"


@pytest.mark.docs
def test_etag_changes_after_set_metadata_so_old_etag_fails(fs):
    f = fs.create_file("f")
    old = f.get_file_properties().etag
    f.set_metadata({"a": "1"})
    with pytest.raises(ResourceModifiedError):
        f.get_file_properties(etag=old, match_condition=MatchConditions.IfNotModified)
