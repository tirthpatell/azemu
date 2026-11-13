"""Blob properties, content settings, metadata, HTTP headers and access tiers."""

import hashlib

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import BlobType, ContentSettings, StandardBlobTier

from blobutil import assert_error


@pytest.fixture
def blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"0123456789")
    return b


def test_get_properties_basic_fields(blob, blob_name, container):
    p = blob.get_blob_properties()
    assert p.name == blob_name
    assert p.container == container.container_name
    assert p.size == 10
    assert p.blob_type == BlobType.BLOCKBLOB
    assert p.etag.startswith('"') and p.etag.endswith('"')
    assert p.last_modified is not None
    assert p.creation_time is not None
    assert p.server_encrypted is True
    assert p.lease.state == "available" and p.lease.status == "unlocked"
    assert p.deleted is False or p.deleted is None


def test_default_content_type_is_octet_stream(blob):
    assert blob.get_blob_properties().content_settings.content_type == "application/octet-stream"


def test_default_access_tier_is_hot_and_inferred(blob):
    p = blob.get_blob_properties()
    assert p.blob_tier == "Hot"
    assert p.blob_tier_inferred is True


def test_single_shot_upload_stores_md5_of_content(blob):
    md5 = blob.get_blob_properties().content_settings.content_md5
    assert bytes(md5) == hashlib.md5(b"0123456789").digest()


def test_properties_of_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).get_blob_properties()
    assert_error(ei, "BlobNotFound", 404)


def test_properties_of_blob_in_missing_container(service, uniq):
    with pytest.raises(ResourceNotFoundError) as ei:
        service.get_blob_client(uniq("c"), "x").get_blob_properties()
    assert_error(ei, "ContainerNotFound", 404)


def test_blob_exists(container, blob_name):
    b = container.get_blob_client(blob_name)
    assert b.exists() is False
    b.upload_blob(b"x")
    assert b.exists() is True


def test_last_modified_and_etag_change_on_rewrite(blob):
    before = blob.get_blob_properties()
    blob.upload_blob(b"changed", overwrite=True)
    after = blob.get_blob_properties()
    assert after.etag != before.etag
    assert after.last_modified >= before.last_modified


def test_creation_time_not_after_last_modified(blob):
    p = blob.get_blob_properties()
    assert p.creation_time <= p.last_modified


# --- content settings ------------------------------------------------------


FULL_SETTINGS = dict(
    content_type="text/plain; charset=utf-8",
    content_encoding="x-custom",
    content_language="en-US",
    content_disposition='attachment; filename="a.txt"',
    cache_control="max-age=3600, private",
)


def test_upload_with_content_settings_roundtrip(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"data", content_settings=ContentSettings(**FULL_SETTINGS))
    cs = b.get_blob_properties().content_settings
    for k, v in FULL_SETTINGS.items():
        assert getattr(cs, k) == v, k


def test_download_properties_carry_content_settings(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"data", content_settings=ContentSettings(**FULL_SETTINGS))
    cs = b.download_blob().properties.content_settings
    for k, v in FULL_SETTINGS.items():
        assert getattr(cs, k) == v, k


def test_upload_with_explicit_content_md5_is_stored(container, blob_name):
    data = b"data"
    md5 = hashlib.md5(data).digest()
    b = container.get_blob_client(blob_name)
    b.upload_blob(data, content_settings=ContentSettings(content_md5=bytearray(md5)))
    assert bytes(b.get_blob_properties().content_settings.content_md5) == md5


def test_upload_with_wrong_content_md5_is_md5_mismatch(container, blob_name):
    # Real Azure validates x-ms-blob-content-md5 on Put Blob against the body:
    # recording test_block_blob...test_upload_blob_content_md5, entry 4 -> 400 Md5Mismatch.
    wrong = hashlib.md5(b"anything-else").digest()
    b = container.get_blob_client(blob_name)
    with pytest.raises(HttpResponseError) as ei:
        b.upload_blob(b"data", content_settings=ContentSettings(content_md5=bytearray(wrong)))
    assert_error(ei, "Md5Mismatch", 400)
    assert not b.exists()


def test_set_http_headers_changes_content_settings(blob):
    blob.set_http_headers(ContentSettings(content_type="image/png", cache_control="no-cache"))
    cs = blob.get_blob_properties().content_settings
    assert cs.content_type == "image/png"
    assert cs.cache_control == "no-cache"


def test_set_http_headers_clears_unspecified_headers(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"data", content_settings=ContentSettings(**FULL_SETTINGS))
    b.set_http_headers(ContentSettings(content_language="fr"))
    cs = b.get_blob_properties().content_settings
    assert cs.content_language == "fr"
    assert cs.content_encoding is None
    assert cs.content_disposition is None
    assert cs.cache_control is None


def test_set_http_headers_keeps_content_and_metadata(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"keep me", metadata={"k": "v"})
    b.set_http_headers(ContentSettings(content_type="text/plain"))
    assert b.download_blob().readall() == b"keep me"
    assert b.get_blob_properties().metadata == {"k": "v"}


def test_set_http_headers_changes_etag(blob):
    before = blob.get_blob_properties().etag
    res = blob.set_http_headers(ContentSettings(content_type="text/plain"))
    assert res["etag"] != before


def test_set_http_headers_on_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).set_http_headers(ContentSettings(content_type="x/y"))
    assert_error(ei, "BlobNotFound", 404)


# --- metadata --------------------------------------------------------------


def test_upload_with_metadata(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", metadata={"Alpha": "1", "beta": "two words"})
    md = {k.lower(): v for k, v in b.get_blob_properties().metadata.items()}
    assert md == {"alpha": "1", "beta": "two words"}


def test_set_blob_metadata_replaces_everything(blob):
    blob.set_blob_metadata({"a": "1", "b": "2"})
    blob.set_blob_metadata({"c": "3"})
    assert blob.get_blob_properties().metadata == {"c": "3"}


def test_set_empty_metadata_clears_it(blob):
    blob.set_blob_metadata({"a": "1"})
    blob.set_blob_metadata({})
    assert not blob.get_blob_properties().metadata


def test_set_metadata_returns_new_etag_and_keeps_content(blob):
    before = blob.get_blob_properties().etag
    res = blob.set_blob_metadata({"a": "1"})
    assert res["etag"] != before
    assert blob.download_blob().readall() == b"0123456789"


def test_download_returns_metadata(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", metadata={"k": "v"})
    assert b.download_blob().properties.metadata == {"k": "v"}


def test_set_metadata_on_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).set_blob_metadata({"a": "b"})
    assert_error(ei, "BlobNotFound", 404)


def test_metadata_name_with_dash_is_invalid(container, blob_name):
    with pytest.raises(HttpResponseError) as ei:
        container.get_blob_client(blob_name).upload_blob(b"x", metadata={"bad-name": "v"})
    assert_error(ei, "InvalidMetadata", 400)


# --- access tiers ----------------------------------------------------------


@pytest.mark.parametrize("tier", [StandardBlobTier.COOL, StandardBlobTier.ARCHIVE])
def test_set_standard_blob_tier(blob, tier):
    blob.set_standard_blob_tier(tier)
    p = blob.get_blob_properties()
    assert p.blob_tier == tier.value
    assert p.blob_tier_inferred is False


def test_set_tier_hot_after_cool(blob):
    blob.set_standard_blob_tier(StandardBlobTier.COOL)
    blob.set_standard_blob_tier(StandardBlobTier.HOT)
    assert blob.get_blob_properties().blob_tier == "Hot"


def test_set_cold_tier(blob):
    blob.set_standard_blob_tier("Cold")
    p = blob.get_blob_properties()
    assert p.blob_tier == "Cold"
    assert p.blob_tier_inferred is False


def test_upload_with_standard_blob_tier(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", standard_blob_tier=StandardBlobTier.COOL)
    assert b.get_blob_properties().blob_tier == "Cool"


def test_tier_is_reported_in_listing(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"x", standard_blob_tier=StandardBlobTier.COOL)
    (item,) = list(container.list_blobs(name_starts_with=blob_name))
    assert item.blob_tier == "Cool"


def test_download_archived_blob_is_rejected(blob):
    blob.set_standard_blob_tier(StandardBlobTier.ARCHIVE)
    with pytest.raises(HttpResponseError) as ei:
        blob.download_blob().readall()
    assert_error(ei, "BlobArchived", 409)


def test_set_tier_on_missing_blob(container, blob_name):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).set_standard_blob_tier("Cool")
    assert_error(ei, "BlobNotFound", 404)


def test_set_invalid_tier_name_is_rejected(blob):
    with pytest.raises(HttpResponseError) as ei:
        blob.set_standard_blob_tier("Lukewarm")
    assert ei.value.status_code == 400
