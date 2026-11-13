"""Container and blob leases. Only lease behaviour that fits in a couple of seconds."""

import uuid

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.blob import BlobLeaseClient

from blobutil import assert_error, assert_error_in


def _id():
    return str(uuid.uuid4())


@pytest.fixture
def blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    b.upload_blob(b"leased content")
    return b


# --- container leases ------------------------------------------------------


def test_container_acquire_infinite_lease(container):
    lease = container.acquire_lease(lease_duration=-1)
    assert lease.id
    p = container.get_container_properties()
    assert (p.lease.state, p.lease.status, p.lease.duration) == ("leased", "locked", "infinite")


def test_container_acquire_fixed_lease_with_proposed_id(container):
    lid = _id()
    lease = container.acquire_lease(lease_duration=15, lease_id=lid)
    assert lease.id == lid
    p = container.get_container_properties()
    assert (p.lease.state, p.lease.duration) == ("leased", "fixed")


def test_container_release_returns_to_available(container):
    lease = container.acquire_lease()
    lease.release()
    p = container.get_container_properties()
    assert (p.lease.state, p.lease.status) == ("available", "unlocked")


def test_container_renew_keeps_lease_id(container):
    lease = container.acquire_lease(lease_duration=15)
    lease.renew()
    assert container.get_container_properties().lease.state == "leased"


def test_container_change_lease_id(container):
    lease = container.acquire_lease()
    new_id = _id()
    lease.change(new_id)
    assert lease.id == new_id
    lease.release()  # releasing with the new id works
    assert container.get_container_properties().lease.state == "available"


def test_container_break_infinite_lease_immediately(container):
    lease = container.acquire_lease()
    remaining = lease.break_lease(lease_break_period=0)
    assert remaining == 0
    p = container.get_container_properties()
    assert (p.lease.state, p.lease.status) == ("broken", "unlocked")


def test_container_break_with_period_enters_breaking_state(container):
    lease = container.acquire_lease(lease_duration=15)
    remaining = lease.break_lease(lease_break_period=5)
    assert 0 < remaining <= 5
    assert container.get_container_properties().lease.state == "breaking"


def test_container_acquire_with_other_id_while_leased_conflicts(container):
    container.acquire_lease(lease_id=_id())
    with pytest.raises(ResourceExistsError) as ei:
        container.acquire_lease(lease_id=_id())
    assert_error(ei, "LeaseAlreadyPresent", 409)


def test_container_acquire_after_break_succeeds(container):
    container.acquire_lease().break_lease(lease_break_period=0)
    lease = container.acquire_lease()
    assert lease.id


def test_container_delete_without_lease_id_while_leased_fails(container):
    container.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        container.delete_container()
    assert_error(ei, "LeaseIdMissing", 412)


def test_container_delete_with_wrong_lease_id_fails(container):
    container.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        container.delete_container(lease=_id())
    assert_error(ei, "LeaseIdMismatchWithContainerOperation", 412)


def test_container_delete_with_lease_id_succeeds(service, container):
    lease = container.acquire_lease()
    container.delete_container(lease=lease)
    assert container.exists() is False


def test_container_delete_with_lease_id_when_not_leased_fails(container):
    with pytest.raises(HttpResponseError) as ei:
        container.delete_container(lease=_id())
    assert_error(ei, "LeaseNotPresentWithContainerOperation", 412)


def test_container_release_with_wrong_id_fails(container):
    container.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        BlobLeaseClient(container, lease_id=_id()).release()
    assert_error(ei, "LeaseIdMismatchWithLeaseOperation", 409)


def test_container_renew_without_lease_fails(container):
    with pytest.raises(HttpResponseError) as ei:
        BlobLeaseClient(container, lease_id=_id()).renew()
    # Azure: 409, LeaseNotPresentWithLeaseOperation ("no lease on the container").
    assert_error_in(ei, ("LeaseNotPresentWithLeaseOperation", "LeaseIdMismatchWithLeaseOperation"), 409)


def test_container_break_without_lease_fails(container):
    with pytest.raises(HttpResponseError) as ei:
        BlobLeaseClient(container).break_lease()
    assert_error(ei, "LeaseNotPresentWithLeaseOperation", 409)


def test_container_get_properties_with_lease_id(container):
    lease = container.acquire_lease()
    assert container.get_container_properties(lease=lease).lease.state == "leased"


def test_container_lease_on_missing_container(service, uniq):
    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        c.acquire_lease()
    assert_error(ei, "ContainerNotFound", 404)


# --- blob leases -----------------------------------------------------------


def test_blob_acquire_infinite_lease_updates_properties(blob):
    lease = blob.acquire_lease(lease_duration=-1)
    p = blob.get_blob_properties()
    assert lease.id
    assert (p.lease.state, p.lease.status, p.lease.duration) == ("leased", "locked", "infinite")


def test_blob_acquire_fixed_lease(blob):
    lid = _id()
    lease = blob.acquire_lease(lease_duration=15, lease_id=lid)
    assert lease.id == lid
    assert blob.get_blob_properties().lease.duration == "fixed"


def test_blob_release_lease(blob):
    lease = blob.acquire_lease()
    lease.release()
    p = blob.get_blob_properties()
    assert (p.lease.state, p.lease.status) == ("available", "unlocked")


def test_blob_renew_lease(blob):
    lease = blob.acquire_lease(lease_duration=15)
    lease.renew()
    assert blob.get_blob_properties().lease.state == "leased"


def test_blob_change_lease(blob):
    lease = blob.acquire_lease()
    new_id = _id()
    lease.change(new_id)
    assert lease.id == new_id
    blob.upload_blob(b"new", overwrite=True, lease=new_id)
    assert blob.download_blob().readall() == b"new"


def test_blob_break_lease_immediately(blob):
    lease = blob.acquire_lease()
    assert lease.break_lease(lease_break_period=0) == 0
    assert blob.get_blob_properties().lease.state == "broken"


def test_blob_break_lease_with_period_returns_remaining_seconds(blob):
    lease = blob.acquire_lease(lease_duration=15)
    remaining = lease.break_lease(lease_break_period=3)
    assert 0 < remaining <= 3
    assert blob.get_blob_properties().lease.state == "breaking"


def test_blob_acquire_twice_with_different_ids_conflicts(blob):
    blob.acquire_lease(lease_id=_id())
    with pytest.raises(ResourceExistsError) as ei:
        blob.acquire_lease(lease_id=_id())
    assert_error(ei, "LeaseAlreadyPresent", 409)


def test_blob_acquire_twice_with_same_id_is_idempotent(blob):
    lid = _id()
    blob.acquire_lease(lease_id=lid)
    again = blob.acquire_lease(lease_id=lid)
    assert again.id == lid


def test_blob_overwrite_without_lease_id_while_leased_fails(blob):
    blob.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        blob.upload_blob(b"other", overwrite=True)
    assert_error(ei, "LeaseIdMissing", 412)


def test_blob_overwrite_with_wrong_lease_id_fails(blob):
    blob.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        blob.upload_blob(b"other", overwrite=True, lease=_id())
    assert_error(ei, "LeaseIdMismatchWithBlobOperation", 412)


def test_blob_overwrite_with_correct_lease_id_succeeds(blob):
    lease = blob.acquire_lease()
    blob.upload_blob(b"other", overwrite=True, lease=lease)
    assert blob.download_blob().readall() == b"other"
    assert blob.get_blob_properties().lease.state == "leased"  # overwrite keeps the lease


def test_blob_delete_without_lease_id_fails(blob):
    blob.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        blob.delete_blob()
    assert_error(ei, "LeaseIdMissing", 412)


def test_blob_delete_with_lease_id_succeeds(blob):
    lease = blob.acquire_lease()
    blob.delete_blob(lease=lease)
    assert blob.exists() is False


def test_blob_set_metadata_without_lease_id_fails(blob):
    blob.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        blob.set_blob_metadata({"a": "b"})
    assert_error(ei, "LeaseIdMissing", 412)


def test_blob_set_metadata_with_lease_id_succeeds(blob):
    lease = blob.acquire_lease()
    blob.set_blob_metadata({"a": "b"}, lease=lease)
    assert blob.get_blob_properties().metadata == {"a": "b"}


def test_blob_read_does_not_need_lease_id(blob):
    blob.acquire_lease()
    assert blob.download_blob().readall() == b"leased content"
    assert blob.get_blob_properties().size == len(b"leased content")


def test_blob_operation_with_lease_id_when_not_leased_fails(blob):
    with pytest.raises(HttpResponseError) as ei:
        blob.upload_blob(b"x", overwrite=True, lease=_id())
    assert_error(ei, "LeaseNotPresentWithBlobOperation", 412)


def test_blob_release_with_wrong_id_fails(blob):
    blob.acquire_lease()
    with pytest.raises(HttpResponseError) as ei:
        BlobLeaseClient(blob, lease_id=_id()).release()
    assert_error(ei, "LeaseIdMismatchWithLeaseOperation", 409)


def test_blob_renew_released_lease_fails(blob):
    lease = blob.acquire_lease()
    old_id = lease.id
    lease.release()
    with pytest.raises(HttpResponseError) as ei:
        BlobLeaseClient(blob, lease_id=old_id).renew()
    assert_error_in(ei, ("LeaseNotPresentWithLeaseOperation", "LeaseIdMismatchWithLeaseOperation"), 409)


def test_blob_lease_on_missing_blob(container, blob_name):
    b = container.get_blob_client(blob_name)
    with pytest.raises(ResourceNotFoundError) as ei:
        b.acquire_lease()
    assert_error(ei, "BlobNotFound", 404)


def test_blob_lease_on_missing_container_blob(service, uniq):
    b = service.get_blob_client(uniq("c"), "x")
    with pytest.raises(ResourceNotFoundError) as ei:
        b.acquire_lease()
    assert_error(ei, "ContainerNotFound", 404)
