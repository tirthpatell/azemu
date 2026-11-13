"""Copy Blob, Put Blob From URL, Put Block From URL."""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import BlobBlock, BlobSasPermissions, ContentSettings, generate_blob_sas

from blobutil import assert_error


@pytest.fixture
def src(container, uniq):
    b = container.get_blob_client(uniq("src"))
    b.upload_blob(
        b"source content 0123456789",
        metadata={"origin": "src"},
        content_settings=ContentSettings(content_type="text/plain"),
    )
    return b


@pytest.fixture
def src_sas_url(src, account_name, account_key):
    sas = generate_blob_sas(
        account_name,
        src.container_name,
        src.blob_name,
        account_key=account_key,
        permission=BlobSasPermissions(read=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )
    return f"{src.url}?{sas}"


def test_start_copy_within_account_succeeds_immediately(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    res = dest.start_copy_from_url(src.url)
    assert res["copy_status"] == "success"
    assert res["copy_id"]
    assert res["etag"].startswith('"')


def test_copied_blob_has_source_content(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.start_copy_from_url(src.url)
    assert dest.download_blob().readall() == b"source content 0123456789"


def test_copy_properties_fields(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    res = dest.start_copy_from_url(src.url)
    c = dest.get_blob_properties().copy
    assert c.status == "success"
    assert c.id == res["copy_id"]
    assert c.source.split("?")[0].endswith(f"/{src.container_name}/{src.blob_name}")
    assert c.progress == "25/25"
    assert c.completion_time is not None


def test_copy_carries_source_metadata_and_content_type_by_default(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.start_copy_from_url(src.url)
    p = dest.get_blob_properties()
    assert p.metadata == {"origin": "src"}
    assert p.content_settings.content_type == "text/plain"


def test_copy_with_metadata_replaces_source_metadata(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.start_copy_from_url(src.url, metadata={"fresh": "yes"})
    assert dest.get_blob_properties().metadata == {"fresh": "yes"}


def test_copy_over_existing_blob_overwrites_it(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.upload_blob(b"previous")
    dest.start_copy_from_url(src.url)
    assert dest.download_blob().readall() == b"source content 0123456789"


def test_copy_does_not_change_source(container, src, uniq):
    before = src.get_blob_properties().etag
    container.get_blob_client(uniq("dst")).start_copy_from_url(src.url)
    assert src.get_blob_properties().etag == before


def test_copy_to_other_container(service, container, src, uniq):
    other = service.create_container(uniq("c"))
    dest = other.get_blob_client("copied")
    dest.start_copy_from_url(src.url)
    assert dest.download_blob().readall() == b"source content 0123456789"


def test_copy_missing_source_is_not_found(container, uniq):
    missing = container.get_blob_client(uniq("missing"))
    dest = container.get_blob_client(uniq("dst"))
    with pytest.raises(ResourceNotFoundError) as ei:
        dest.start_copy_from_url(missing.url)
    assert ei.value.status_code == 404


def test_copy_with_if_none_match_star_fails_when_destination_exists(container, src, uniq):
    from azure.core import MatchConditions

    dest = container.get_blob_client(uniq("dst"))
    dest.upload_blob(b"already here")
    with pytest.raises(HttpResponseError) as ei:
        dest.start_copy_from_url(src.url, match_condition=MatchConditions.IfMissing)
    assert ei.value.status_code in (409, 412)
    assert dest.download_blob().readall() == b"already here"


def test_sync_copy_with_requires_sync(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    res = dest.start_copy_from_url(src_sas_url, requires_sync=True)
    assert res["copy_status"] == "success"
    assert dest.download_blob().readall() == b"source content 0123456789"


def test_abort_copy_on_finished_copy_is_an_error(container, src, uniq):
    dest = container.get_blob_client(uniq("dst"))
    res = dest.start_copy_from_url(src.url)
    with pytest.raises(HttpResponseError) as ei:
        dest.abort_copy(res["copy_id"])
    assert_error(ei, "NoPendingCopyOperation", 409)


def test_copy_properties_absent_on_plain_blob(src):
    assert src.get_blob_properties().copy.status is None


def test_list_blobs_include_copy(container, src, uniq):
    name = uniq("dst")
    container.get_blob_client(name).start_copy_from_url(src.url)
    (item,) = list(container.list_blobs(name_starts_with=name, include=["copy"]))
    assert item.copy.status == "success"


# --- Put Blob From URL -----------------------------------------------------


@pytest.mark.not_on_azurite(reason="Azurite does not implement Put Blob From URL")
def test_upload_blob_from_url(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.upload_blob_from_url(src_sas_url)
    assert dest.download_blob().readall() == b"source content 0123456789"


@pytest.mark.not_on_azurite(reason="Azurite does not implement Put Blob From URL")
def test_upload_blob_from_url_overwrite_false_conflicts(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.upload_blob(b"exists")
    with pytest.raises(HttpResponseError) as ei:
        dest.upload_blob_from_url(src_sas_url, overwrite=False)
    assert_error(ei, "BlobAlreadyExists", 409)


@pytest.mark.not_on_azurite(reason="Azurite does not implement Put Blob From URL")
def test_upload_blob_from_url_overwrite_true_replaces(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.upload_blob(b"exists")
    dest.upload_blob_from_url(src_sas_url, overwrite=True)
    assert dest.download_blob().readall() == b"source content 0123456789"


# --- Put Block From URL ----------------------------------------------------


def test_stage_block_from_url_range_and_commit(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.stage_block_from_url("blk00001", src_sas_url, source_offset=0, source_length=7)
    dest.stage_block_from_url("blk00002", src_sas_url, source_offset=7, source_length=7)
    dest.commit_block_list([BlobBlock("blk00001"), BlobBlock("blk00002")])
    assert dest.download_blob().readall() == b"source content"


def test_stage_block_from_url_whole_source(container, src_sas_url, uniq):
    dest = container.get_blob_client(uniq("dst"))
    dest.stage_block_from_url("blk00001", src_sas_url)
    dest.commit_block_list([BlobBlock("blk00001")])
    assert dest.download_blob().readall() == b"source content 0123456789"
