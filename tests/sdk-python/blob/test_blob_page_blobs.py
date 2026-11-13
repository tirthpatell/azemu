"""Page blobs."""

from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import BlobSasPermissions, BlobType, generate_blob_sas

from blobutil import assert_error

PAGE = 512


@pytest.fixture
def pb(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.create_page_blob(size=4 * PAGE)
    return b


def test_create_page_blob_properties(pb):
    p = pb.get_blob_properties()
    assert p.blob_type == BlobType.PAGEBLOB
    assert p.size == 4 * PAGE
    assert p.page_blob_sequence_number == 0


def test_create_page_blob_with_sequence_number(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.create_page_blob(size=PAGE, sequence_number=7)
    assert b.get_blob_properties().page_blob_sequence_number == 7


def test_new_page_blob_reads_as_zeros(pb):
    assert pb.download_blob().readall() == b"\x00" * (4 * PAGE)


def test_new_page_blob_has_no_page_ranges(pb):
    pages, cleared = pb.get_page_ranges()
    assert pages == [] and cleared == []


def test_upload_page_and_read_back(pb):
    data = b"A" * PAGE
    res = pb.upload_page(data, offset=PAGE, length=PAGE)
    assert res["etag"].startswith('"')
    assert pb.download_blob(offset=PAGE, length=PAGE).readall() == data
    assert pb.download_blob(offset=0, length=PAGE).readall() == b"\x00" * PAGE


def test_upload_multiple_pages_at_once(pb):
    data = b"B" * (2 * PAGE)
    pb.upload_page(data, offset=0, length=2 * PAGE)
    assert pb.download_blob().readall() == data + b"\x00" * (2 * PAGE)


def test_get_page_ranges_reports_written_ranges(pb):
    pb.upload_page(b"A" * PAGE, offset=0, length=PAGE)
    pb.upload_page(b"B" * PAGE, offset=3 * PAGE, length=PAGE)
    pages, cleared = pb.get_page_ranges()
    assert pages == [{"start": 0, "end": PAGE - 1}, {"start": 3 * PAGE, "end": 4 * PAGE - 1}]
    assert cleared == []


def test_clear_page_removes_range_and_zeroes_data(pb):
    pb.upload_page(b"A" * (2 * PAGE), offset=0, length=2 * PAGE)
    pb.clear_page(offset=0, length=PAGE)
    pages, _ = pb.get_page_ranges()
    assert pages == [{"start": PAGE, "end": 2 * PAGE - 1}]
    assert pb.download_blob(offset=0, length=PAGE).readall() == b"\x00" * PAGE
    assert pb.download_blob(offset=PAGE, length=PAGE).readall() == b"A" * PAGE
    assert pb.get_blob_properties().size == 4 * PAGE


def test_get_page_ranges_in_a_window(pb):
    pb.upload_page(b"A" * PAGE, offset=0, length=PAGE)
    pb.upload_page(b"B" * PAGE, offset=3 * PAGE, length=PAGE)
    pages, _ = pb.get_page_ranges(offset=2 * PAGE, length=2 * PAGE)
    assert pages == [{"start": 3 * PAGE, "end": 4 * PAGE - 1}]


def test_resize_page_blob_grows(pb):
    pb.resize_blob(8 * PAGE)
    assert pb.get_blob_properties().size == 8 * PAGE
    assert pb.download_blob(offset=4 * PAGE).readall() == b"\x00" * (4 * PAGE)


def test_resize_page_blob_shrinks_and_drops_pages(pb):
    pb.upload_page(b"A" * PAGE, offset=3 * PAGE, length=PAGE)
    pb.resize_blob(2 * PAGE)
    assert pb.get_blob_properties().size == 2 * PAGE
    pages, _ = pb.get_page_ranges()
    assert pages == []


def test_resize_to_unaligned_size_is_rejected(pb):
    with pytest.raises(HttpResponseError) as ei:
        pb.resize_blob(1000)
    assert ei.value.status_code == 400


def test_create_page_blob_with_unaligned_size_is_rejected(container, blob_name):
    with pytest.raises(HttpResponseError) as ei:
        container.get_blob_client(blob_name).create_page_blob(size=1000)
    assert ei.value.status_code == 400


@pytest.mark.parametrize(
    "action,value,expected",
    [("update", 10, 10), ("max", 3, 3), ("increment", None, 1)],
)
def test_set_sequence_number(pb, action, value, expected):
    pb.set_sequence_number(action, value)
    assert pb.get_blob_properties().page_blob_sequence_number == expected


def test_sequence_number_max_keeps_the_larger_value(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.create_page_blob(size=PAGE, sequence_number=9)
    b.set_sequence_number("max", 4)
    assert b.get_blob_properties().page_blob_sequence_number == 9


def test_sequence_number_condition_not_met(pb):
    pb.set_sequence_number("update", 5)
    with pytest.raises(HttpResponseError) as ei:
        pb.upload_page(b"A" * PAGE, offset=0, length=PAGE, if_sequence_number_lt=5)
    assert_error(ei, "SequenceNumberConditionNotMet", 412)


def test_sequence_number_condition_met(pb):
    pb.set_sequence_number("update", 5)
    pb.upload_page(b"A" * PAGE, offset=0, length=PAGE, if_sequence_number_eq=5)
    assert pb.download_blob(offset=0, length=PAGE).readall() == b"A" * PAGE


@pytest.mark.not_on_azurite(reason="Azurite sends the error code 'Requested Range Not Satisfiable' instead of InvalidPageRange")
def test_upload_page_beyond_blob_size_is_invalid_range(pb):
    with pytest.raises(HttpResponseError) as ei:
        pb.upload_page(b"A" * PAGE, offset=4 * PAGE, length=PAGE)
    assert_error(ei, "InvalidPageRange", 416)


def test_upload_page_to_block_blob_is_invalid_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x" * PAGE)
    with pytest.raises(HttpResponseError) as ei:
        b.upload_page(b"A" * PAGE, offset=0, length=PAGE)
    assert_error(ei, "InvalidBlobType", 409)


def test_upload_page_to_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).upload_page(b"A" * PAGE, offset=0, length=PAGE)
    assert_error(ei, "BlobNotFound", 404)


def test_page_alignment_is_enforced_by_the_service(pb, account_name, account_key, container):
    """The SDK checks alignment itself, so send an unaligned range over raw HTTP."""
    sas = generate_blob_sas(
        account_name,
        container.container_name,
        pb.blob_name,
        account_key=account_key,
        permission=BlobSasPermissions(write=True, read=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )
    r = requests.put(
        f"{pb.url}?comp=page&{sas}",
        data=b"x" * 100,
        headers={"x-ms-page-write": "update", "x-ms-range": "bytes=0-99"},
    )
    assert r.status_code in (400, 416)
    assert "<Error>" in r.text and "<Code>" in r.text
    assert pb.get_page_ranges()[0] == []


def test_upload_blob_with_page_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    data = b"Z" * (2 * PAGE)
    b.upload_blob(data, blob_type=BlobType.PAGEBLOB)
    assert b.get_blob_properties().blob_type == BlobType.PAGEBLOB
    assert b.download_blob().readall() == data


def test_get_page_ranges_on_block_blob_is_invalid_blob_type(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x")
    with pytest.raises(HttpResponseError) as ei:
        b.get_page_ranges()
    assert_error(ei, "InvalidBlobType", 409)
