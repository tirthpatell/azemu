"""Blob versioning.

Versioning is an account setting on real Azure and cannot be switched on through
the data plane. It is off until someone turns it on, and so it is in azemu: an account that
appears on first use keeps no versions. The tests marked `versioning` describe an
account that has versioning ON; they run in the runner's second pass, which starts the emulator
with AZEMU_BLOB_VERSIONING=1 (see ../autocreate_gate.py; through ARM it is switched on per
account, see ../arm/test_blob_service_properties.py). The tests marked `no_versioning` describe
the default. All are skipped on Azurite (no versioning support).
"""

from urllib.parse import quote

import pytest
from azure.core.exceptions import ResourceNotFoundError

from blobutil import assert_error

pytestmark = pytest.mark.not_on_azurite(reason="Azurite has no blob versioning")
needs_versioning = pytest.mark.versioning


@pytest.fixture
def blob(container, blob_name):
    return container.get_blob_client(blob_name)


def _version(service, container, blob, vid):
    return service.get_blob_client(container.container_name, blob.blob_name, version_id=vid)


@needs_versioning
def test_upload_returns_a_version_id(blob):
    res = blob.upload_blob(b"v1")
    assert res["version_id"]


@needs_versioning
def test_overwrite_creates_a_new_version_id(blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    v2 = blob.upload_blob(b"v2", overwrite=True)["version_id"]
    assert v1 != v2


@needs_versioning
def test_properties_report_version_and_current_flag(blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    p = blob.get_blob_properties()
    assert p.version_id == v1
    assert p.is_current_version is True


@needs_versioning
def test_old_version_is_readable_after_overwrite(service, container, blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    blob.upload_blob(b"v2", overwrite=True)
    assert _version(service, container, blob, v1).download_blob().readall() == b"v1"
    assert blob.download_blob().readall() == b"v2"


@needs_versioning
def test_list_blobs_include_versions(container, blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    v2 = blob.upload_blob(b"v2", overwrite=True)["version_id"]
    items = list(container.list_blobs(include=["versions"]))
    assert {i.version_id for i in items} == {v1, v2}
    assert {i.version_id for i in items if i.is_current_version} == {v2}


def test_list_blobs_without_versions_shows_one_entry(container, blob):
    blob.upload_blob(b"v1")
    blob.upload_blob(b"v2", overwrite=True)
    assert len(list(container.list_blobs())) == 1


@needs_versioning
def test_deleting_blob_keeps_previous_versions(service, container, blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    blob.delete_blob()
    with pytest.raises(ResourceNotFoundError) as ei:
        blob.get_blob_properties()
    assert_error(ei, "BlobNotFound", 404)
    assert _version(service, container, blob, v1).download_blob().readall() == b"v1"
    assert [i.version_id for i in container.list_blobs(include=["versions"])] == [v1]


@needs_versioning
def test_delete_a_specific_version(service, container, blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    blob.upload_blob(b"v2", overwrite=True)
    blob.delete_blob(version_id=v1)
    assert not _version(service, container, blob, v1).exists()
    assert blob.download_blob().readall() == b"v2"


@needs_versioning
def test_copy_old_version_over_current_restores_it(service, container, blob):
    v1 = blob.upload_blob(b"v1")["version_id"]
    v2 = blob.upload_blob(b"v2", overwrite=True)["version_id"]
    # BlobClient.url never carries the version id (only the snapshot), so the
    # source URL has to be built by hand.
    source = f"{blob.url}?versionid={quote(v1, safe='')}"
    assert "versionid=" not in _version(service, container, blob, v1).url
    blob.start_copy_from_url(source)
    assert blob.download_blob().readall() == b"v1"
    # The restore is a new version: v1 and v2 are kept, current is a third one.
    current = blob.get_blob_properties().version_id
    assert current not in (v1, v2)
    assert {i.version_id for i in container.list_blobs(include=["versions"])} == {v1, v2, current}


def test_unknown_version_id_is_not_found(service, container, blob):
    blob.upload_blob(b"v1")
    with pytest.raises(ResourceNotFoundError) as ei:
        _version(service, container, blob, "2020-01-01T00:00:00.0000000Z").download_blob()
    assert ei.value.status_code == 404


@pytest.mark.no_versioning
def test_an_account_without_versioning_keeps_no_versions(service, container, blob):
    """The default, as on Azure: no version id, an overwrite replaces the data, a delete is final."""
    res = blob.upload_blob(b"v1")
    assert res.get("version_id") is None
    res = blob.upload_blob(b"v2", overwrite=True)
    assert res.get("version_id") is None
    p = blob.get_blob_properties()
    assert p.version_id is None and p.is_current_version is None
    assert [(i.name, i.version_id) for i in container.list_blobs(include=["versions"])] == [(blob.blob_name, None)]
    blob.delete_blob()
    assert list(container.list_blobs(include=["versions"])) == []
    with pytest.raises(ResourceNotFoundError) as ei:
        blob.get_blob_properties()
    assert_error(ei, "BlobNotFound", 404)
