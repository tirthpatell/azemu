"""Blob soft delete (enabled through Set Blob Service Properties) and Undelete."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.storage.blob import RetentionPolicy

from blobutil import assert_error


@pytest.fixture
def soft_delete(service):
    service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=3))
    yield
    try:
        service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=False))
    except Exception:
        pass


@pytest.fixture
def blob(container, blob_name, soft_delete):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"precious", metadata={"k": "v"})
    return b


def test_retention_policy_roundtrip(service, soft_delete):
    p = service.get_service_properties()["delete_retention_policy"]
    assert p.enabled is True
    assert p.days == 3


def test_disabling_soft_delete_roundtrip(service, soft_delete):
    service.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=False))
    assert service.get_service_properties()["delete_retention_policy"].enabled is False


def test_soft_deleted_blob_is_hidden_from_get_and_default_listing(blob, container):
    blob.delete_blob()
    with pytest.raises(ResourceNotFoundError) as ei:
        blob.get_blob_properties()
    assert_error(ei, "BlobNotFound", 404)
    assert list(container.list_blobs()) == []


@pytest.mark.not_on_azurite(reason="Azurite does not list soft-deleted blobs (include=deleted)")
def test_soft_deleted_blob_is_listed_with_include_deleted(blob, container):
    blob.delete_blob()
    (item,) = list(container.list_blobs(include=["deleted"]))
    assert item.name == blob.blob_name
    assert item.deleted is True
    assert item.deleted_time is not None
    assert item.remaining_retention_days is not None and 1 <= item.remaining_retention_days <= 3


@pytest.mark.not_on_azurite(reason="Azurite does not implement Undelete Blob (APINotImplemented)")
def test_undelete_restores_blob_content_and_metadata(blob, container):
    blob.delete_blob()
    blob.undelete_blob()
    assert blob.download_blob().readall() == b"precious"
    assert blob.get_blob_properties().metadata == {"k": "v"}
    assert [b.name for b in container.list_blobs()] == [blob.blob_name]


@pytest.mark.not_on_azurite(reason="Azurite does not implement Undelete Blob (APINotImplemented)")
def test_undelete_clears_deleted_flag_in_listing(blob, container):
    blob.delete_blob()
    blob.undelete_blob()
    (item,) = list(container.list_blobs(include=["deleted"]))
    assert not item.deleted


@pytest.mark.not_on_azurite(reason="Azurite does not implement Undelete Blob (APINotImplemented)")
def test_undelete_of_blob_that_is_not_deleted_succeeds(blob):
    blob.undelete_blob()
    assert blob.download_blob().readall() == b"precious"


@pytest.mark.not_on_azurite(reason="Azurite does not implement Undelete Blob (APINotImplemented)")
def test_undelete_of_never_existing_blob_is_not_found(container, blob_name, soft_delete):
    with pytest.raises(ResourceNotFoundError) as ei:
        container.get_blob_client(blob_name).undelete_blob()
    assert_error(ei, "BlobNotFound", 404)


def test_deleted_blob_name_can_be_reused(blob, container):
    blob.delete_blob()
    blob.upload_blob(b"new life")
    assert blob.download_blob().readall() == b"new life"


@pytest.mark.not_on_azurite(reason="Azurite does not implement Undelete Blob (APINotImplemented)")
def test_soft_deleted_snapshot_is_listed_and_can_be_restored(service, container, blob):
    snap = blob.create_snapshot()["snapshot"]
    snap_client = service.get_blob_client(container.container_name, blob.blob_name, snapshot=snap)
    snap_client.delete_blob()
    assert not snap_client.exists()
    listed = list(container.list_blobs(include=["deleted", "snapshots"]))
    assert any(i.snapshot == snap and i.deleted for i in listed)
    blob.undelete_blob()
    assert snap_client.exists()


@pytest.mark.not_on_azurite(reason="Azurite does not list soft-deleted blobs (include=deleted)")
def test_delete_with_include_snapshots_soft_deletes_everything(blob, container):
    blob.create_snapshot()
    blob.delete_blob(delete_snapshots="include")
    assert list(container.list_blobs(include=["snapshots"])) == []
    deleted = list(container.list_blobs(include=["deleted", "snapshots"]))
    assert len(deleted) == 2 and all(i.deleted for i in deleted)


@pytest.mark.not_on_azurite(reason="Azurite does not create a soft-deleted snapshot when a blob is overwritten")
def test_overwrite_keeps_soft_deleted_snapshot_of_old_content(service, container, blob):
    blob.upload_blob(b"replacement", overwrite=True)
    listed = list(container.list_blobs(include=["deleted", "snapshots"]))
    old = [i for i in listed if i.snapshot and i.deleted]
    assert len(old) == 1
    blob.undelete_blob()
    # Undelete promotes nothing by itself, but the old snapshot is readable again.
    snap = service.get_blob_client(container.container_name, blob.blob_name, snapshot=old[0].snapshot)
    assert snap.download_blob().readall() == b"precious"


def test_without_soft_delete_delete_is_permanent(service, container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"gone")
    b.delete_blob()
    assert list(container.list_blobs(include=["deleted"])) == []
