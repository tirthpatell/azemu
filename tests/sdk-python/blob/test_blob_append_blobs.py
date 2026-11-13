"""Append blobs."""

import hashlib
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import BlobSasPermissions, BlobType, generate_blob_sas

from blobutil import assert_error


@pytest.fixture
def ab(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.create_append_blob()
    return b


def test_create_append_blob_is_empty(ab):
    p = ab.get_blob_properties()
    assert p.blob_type == BlobType.APPENDBLOB
    assert p.size == 0
    assert p.append_blob_committed_block_count == 0
    assert ab.download_blob().readall() == b""


def test_create_append_blob_returns_etag(container, blob_name):
    res = container.get_blob_client(blob_name).create_append_blob()
    assert res["etag"].startswith('"')
    assert res["last_modified"] is not None


def test_append_block_reports_offset_and_count(ab):
    r1 = ab.append_block(b"hello ")
    r2 = ab.append_block(b"world")
    assert (int(r1["blob_append_offset"]), r1["blob_committed_block_count"]) == (0, 1)
    assert (int(r2["blob_append_offset"]), r2["blob_committed_block_count"]) == (6, 2)


def test_appended_blocks_concatenate(ab):
    for part in (b"one,", b"two,", b"three"):
        ab.append_block(part)
    assert ab.download_blob().readall() == b"one,two,three"
    p = ab.get_blob_properties()
    assert p.size == 13
    assert p.append_blob_committed_block_count == 3


def test_append_changes_etag_and_last_modified(ab):
    before = ab.get_blob_properties()
    ab.append_block(b"x")
    after = ab.get_blob_properties()
    assert after.etag != before.etag
    assert after.last_modified >= before.last_modified


def test_append_block_validate_content(ab):
    res = ab.append_block(b"checked", validate_content=True)
    assert int(res["blob_append_offset"]) == 0
    assert ab.download_blob().readall() == b"checked"


def test_append_position_condition_match(ab):
    ab.append_block(b"1234")
    ab.append_block(b"5678", appendpos_condition=4)
    assert ab.download_blob().readall() == b"12345678"


def test_append_position_condition_mismatch(ab):
    ab.append_block(b"1234")
    with pytest.raises(HttpResponseError) as ei:
        ab.append_block(b"5678", appendpos_condition=0)
    assert_error(ei, "AppendPositionConditionNotMet", 412)
    assert ab.get_blob_properties().size == 4


def test_max_size_condition_allows_fit(ab):
    ab.append_block(b"1234", maxsize_condition=8)
    ab.append_block(b"5678", maxsize_condition=8)
    assert ab.get_blob_properties().size == 8


def test_max_size_condition_exceeded(ab):
    ab.append_block(b"1234", maxsize_condition=6)
    with pytest.raises(HttpResponseError) as ei:
        ab.append_block(b"5678", maxsize_condition=6)
    assert_error(ei, "MaxBlobSizeConditionNotMet", 412)


def test_append_to_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).append_block(b"x")
    assert_error(ei, "BlobNotFound", 404)


def test_append_to_block_blob_is_invalid_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"block blob")
    with pytest.raises(HttpResponseError) as ei:
        b.append_block(b"x")
    assert_error(ei, "InvalidBlobType", 409)


def test_create_append_blob_again_resets_content(ab):
    ab.append_block(b"old data")
    ab.create_append_blob()
    assert ab.get_blob_properties().size == 0


def test_create_append_blob_with_metadata_and_settings(container, blob_name):
    from azure.storage.blob import ContentSettings

    b = container.get_blob_client(blob_name)
    b.create_append_blob(content_settings=ContentSettings(content_type="text/csv"), metadata={"k": "v"})
    p = b.get_blob_properties()
    assert p.content_settings.content_type == "text/csv"
    assert p.metadata == {"k": "v"}


def test_upload_blob_with_append_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"streamed in", blob_type=BlobType.APPENDBLOB)
    assert b.get_blob_properties().blob_type == BlobType.APPENDBLOB
    assert b.download_blob().readall() == b"streamed in"


def test_range_download_from_append_blob(ab):
    ab.append_block(b"0123456789")
    assert ab.download_blob(offset=3, length=4).readall() == b"3456"


def test_append_blob_listing_reports_type(container, ab):
    (item,) = list(container.list_blobs())
    assert item.blob_type == "AppendBlob"


@pytest.mark.not_on_azurite(reason="Azurite does not implement Append Block From URL (APINotImplemented)")
def test_append_block_from_url(container, ab, account_name, account_key, uniq):
    src = container.get_blob_client(uniq("src"))
    src.upload_blob(b"ABCDEFGHIJ")
    sas = generate_blob_sas(
        account_name,
        container.container_name,
        src.blob_name,
        account_key=account_key,
        permission=BlobSasPermissions(read=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )
    ab.append_block_from_url(f"{src.url}?{sas}", source_offset=2, source_length=5)
    assert ab.download_blob().readall() == b"CDEFG"


def test_seal_append_blob_blocks_further_appends(ab):
    ab.append_block(b"final")
    ab.seal_append_blob()
    assert ab.get_blob_properties().is_append_blob_sealed is True
    with pytest.raises(HttpResponseError) as ei:
        ab.append_block(b"more")
    assert_error(ei, "BlobIsSealed", 409)
    assert ab.download_blob().readall() == b"final"
