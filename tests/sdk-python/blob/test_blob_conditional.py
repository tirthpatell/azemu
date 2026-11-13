"""Conditional requests: If-Match / If-None-Match / If-Modified-Since / If-Unmodified-Since."""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceModifiedError

from blobutil import assert_error

WRONG_ETAG = '"0xDEADBEEF"'


# A failed read condition (If-None-Match with the current ETag, If-Modified-Since
# with a date after the last modification) on Get Blob / Get Blob Properties is
# answered by real Azure with 304 AND the header `x-ms-error-code: ConditionNotMet`
# (see the recordings test_get_blob_with_if_none_match_fail,
# test_get_blob_properties_with_if_modified_fail, ...). Because of that header the
# SDK's process_storage_error turns the 304 into ResourceModifiedError (it only
# looks at the error code); the SDK never raises ResourceNotModifiedError here.


def _old():
    return datetime.now(timezone.utc) - timedelta(days=1)


def _future():
    return datetime.now(timezone.utc) + timedelta(days=1)


@pytest.fixture
def blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"original")
    return b


# --- If-Match --------------------------------------------------------------


def test_upload_if_match_correct_etag_succeeds(blob):
    etag = blob.get_blob_properties().etag
    blob.upload_blob(b"updated", overwrite=True, etag=etag, match_condition=MatchConditions.IfNotModified)
    assert blob.download_blob().readall() == b"updated"


def test_upload_if_match_wrong_etag_fails_with_condition_not_met(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.upload_blob(b"updated", overwrite=True, etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified)
    assert_error(ei, "ConditionNotMet", 412)
    assert blob.download_blob().readall() == b"original"


def test_upload_if_match_on_missing_blob_fails(container, blob_name):
    with pytest.raises(HttpResponseError) as ei:
        container.get_blob_client(blob_name).upload_blob(
            b"x", overwrite=True, etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified
        )
    assert ei.value.status_code == 412


def test_download_if_match_wrong_etag_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.download_blob(etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified)
    assert_error(ei, "ConditionNotMet", 412)


def test_download_if_match_correct_etag_succeeds(blob):
    etag = blob.get_blob_properties().etag
    assert blob.download_blob(etag=etag, match_condition=MatchConditions.IfNotModified).readall() == b"original"


def test_etag_condition_fails_after_blob_was_modified(blob):
    etag = blob.get_blob_properties().etag
    blob.set_blob_metadata({"touched": "yes"})
    with pytest.raises(ResourceModifiedError) as ei:
        blob.set_blob_metadata({"again": "no"}, etag=etag, match_condition=MatchConditions.IfNotModified)
    assert_error(ei, "ConditionNotMet", 412)


def test_delete_if_match_wrong_etag_fails_and_keeps_blob(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.delete_blob(etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified)
    assert_error(ei, "ConditionNotMet", 412)
    assert blob.exists()


def test_delete_if_match_correct_etag_deletes(blob):
    blob.delete_blob(etag=blob.get_blob_properties().etag, match_condition=MatchConditions.IfNotModified)
    assert not blob.exists()


def test_set_metadata_if_match_wrong_etag_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.set_blob_metadata({"a": "b"}, etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified)
    assert_error(ei, "ConditionNotMet", 412)


def test_get_properties_if_match_wrong_etag_fails(blob):
    with pytest.raises(HttpResponseError) as ei:
        blob.get_blob_properties(etag=WRONG_ETAG, match_condition=MatchConditions.IfNotModified)
    assert ei.value.status_code == 412


# --- If-None-Match ---------------------------------------------------------


def test_upload_if_none_match_star_on_new_blob_succeeds(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", overwrite=True, match_condition=MatchConditions.IfMissing)
    assert b.download_blob().readall() == b"x"


def test_upload_if_none_match_star_on_existing_blob_fails(blob):
    with pytest.raises(HttpResponseError) as ei:
        blob.upload_blob(b"x", overwrite=True, match_condition=MatchConditions.IfMissing)
    assert ei.value.status_code in (409, 412)  # Azure: 409 BlobAlreadyExists
    assert ei.value.error_code in ("BlobAlreadyExists", "ConditionNotMet")
    assert blob.download_blob().readall() == b"original"


def test_upload_if_match_star_on_existing_blob_succeeds(blob):
    blob.upload_blob(b"x", overwrite=True, match_condition=MatchConditions.IfPresent)
    assert blob.download_blob().readall() == b"x"


def test_upload_if_match_star_on_missing_blob_fails(container, blob_name):
    with pytest.raises(HttpResponseError) as ei:
        container.get_blob_client(blob_name).upload_blob(b"x", overwrite=True, match_condition=MatchConditions.IfPresent)
    assert ei.value.status_code == 412


def test_download_if_none_match_same_etag_is_not_modified(blob):
    etag = blob.get_blob_properties().etag
    with pytest.raises(ResourceModifiedError) as ei:
        blob.download_blob(etag=etag, match_condition=MatchConditions.IfModified)
    assert_error(ei, "ConditionNotMet", 304)


def test_download_if_none_match_other_etag_returns_content(blob):
    assert blob.download_blob(etag=WRONG_ETAG, match_condition=MatchConditions.IfModified).readall() == b"original"


def test_get_properties_if_none_match_same_etag_is_not_modified(blob):
    etag = blob.get_blob_properties().etag
    with pytest.raises(ResourceModifiedError) as ei:
        blob.get_blob_properties(etag=etag, match_condition=MatchConditions.IfModified)
    assert_error(ei, "ConditionNotMet", 304)


# --- If-Modified-Since / If-Unmodified-Since -------------------------------


def test_download_if_modified_since_old_date_returns_content(blob):
    assert blob.download_blob(if_modified_since=_old()).readall() == b"original"


def test_download_if_modified_since_future_date_is_not_modified(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.download_blob(if_modified_since=_future())
    assert_error(ei, "ConditionNotMet", 304)


def test_download_if_unmodified_since_old_date_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.download_blob(if_unmodified_since=_old())
    assert_error(ei, "ConditionNotMet", 412)


def test_download_if_unmodified_since_future_date_succeeds(blob):
    assert blob.download_blob(if_unmodified_since=_future()).readall() == b"original"


def test_upload_if_unmodified_since_old_date_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.upload_blob(b"new", overwrite=True, if_unmodified_since=_old())
    assert_error(ei, "ConditionNotMet", 412)


def test_upload_if_modified_since_future_date_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.upload_blob(b"new", overwrite=True, if_modified_since=_future())
    assert_error(ei, "ConditionNotMet", 412)


def test_delete_if_unmodified_since_old_date_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.delete_blob(if_unmodified_since=_old())
    assert_error(ei, "ConditionNotMet", 412)
    assert blob.exists()


def test_set_metadata_if_modified_since_future_date_fails(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.set_blob_metadata({"a": "b"}, if_modified_since=_future())
    assert_error(ei, "ConditionNotMet", 412)


def test_conditional_delete_container_if_unmodified_since_old_fails(container):
    with pytest.raises(ResourceModifiedError) as ei:
        container.delete_container(if_unmodified_since=_old())
    assert_error(ei, "ConditionNotMet", 412)
    assert container.exists()


def test_get_properties_if_modified_since_future_date_is_not_modified(blob):
    with pytest.raises(ResourceModifiedError) as ei:
        blob.get_blob_properties(if_modified_since=_future())
    assert_error(ei, "ConditionNotMet", 304)
