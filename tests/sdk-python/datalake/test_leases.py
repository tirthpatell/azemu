"""Leases on files and directories (acquire/renew/change/release/break through the blob endpoint,
enforcement on dfs writes).  Filesystem leases live in test_filesystems.py."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.filedatalake import DataLakeLeaseClient

from dlutil import assert_error, assert_error_in, assert_status, put_file, read_file

LEASE_A = "00000000-1111-2222-3333-444444444444"
LEASE_B = "99999999-8888-7777-6666-555555555555"


# -- file lease lifecycle ----------------------------------------------------
def test_file_acquire_infinite_lease(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    assert isinstance(lease, DataLakeLeaseClient)
    assert lease.id
    props = f.get_file_properties()
    assert props.lease.state == "leased"
    assert props.lease.status == "locked"
    assert props.lease.duration == "infinite"


def test_file_acquire_fixed_lease_with_proposed_id(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_duration=15, lease_id=LEASE_A)
    assert lease.id == LEASE_A
    props = f.get_file_properties()
    assert props.lease.duration == "fixed"
    assert props.lease.state == "leased"


def test_file_lease_release(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    lease.release()
    props = f.get_file_properties()
    assert props.lease.state == "available"
    assert props.lease.status == "unlocked"


def test_file_lease_renew_keeps_it_leased(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_duration=15)
    lease.renew()
    assert f.get_file_properties().lease.state == "leased"


def test_file_lease_change_id(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_id=LEASE_A)
    lease.change(LEASE_B)
    assert lease.id == LEASE_B
    # the new id is the one that works for writes
    f.append_data(b"abc", 0, 3, lease=LEASE_B)
    f.flush_data(3, lease=LEASE_B)
    assert read_file(f) == b"abc"


def test_file_lease_break_period_zero_breaks_immediately(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    remaining = lease.break_lease(lease_break_period=0)
    assert remaining == 0
    props = f.get_file_properties()
    assert props.lease.state == "broken"
    assert props.lease.status == "unlocked"


def test_file_lease_break_with_period_goes_to_breaking(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    remaining = lease.break_lease(lease_break_period=30)
    assert 0 <= remaining <= 30
    assert f.get_file_properties().lease.state == "breaking"


@pytest.mark.docs
def test_acquire_lease_on_leased_file_conflicts(fs):
    f = fs.create_file("f")
    f.acquire_lease(lease_id=LEASE_A)
    with pytest.raises(HttpResponseError) as e:
        f.acquire_lease(lease_id=LEASE_B)
    assert_error(e, "LeaseAlreadyPresent", 409)


@pytest.mark.docs
def test_acquire_lease_on_missing_file_is_404(fs):
    with pytest.raises(ResourceNotFoundError) as e:
        fs.get_file_client("nofile").acquire_lease()
    assert_error(e, "BlobNotFound", 404)


@pytest.mark.docs
def test_release_with_wrong_lease_id_conflicts(fs):
    f = fs.create_file("f")
    f.acquire_lease(lease_id=LEASE_A)
    wrong = DataLakeLeaseClient(f, lease_id=LEASE_B)
    with pytest.raises(HttpResponseError) as e:
        wrong.release()
    assert_status(e, 409)


# -- enforcement on writes ---------------------------------------------------
@pytest.mark.docs
def test_append_without_lease_id_on_leased_file_is_412_lease_id_missing(fs):
    f = fs.create_file("f")
    f.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        f.append_data(b"abc", 0, 3)
    assert_error(e, "LeaseIdMissing", 412)


@pytest.mark.docs
def test_append_with_wrong_lease_id_is_412(fs):
    f = fs.create_file("f")
    f.acquire_lease(lease_id=LEASE_A)
    with pytest.raises(HttpResponseError) as e:
        f.append_data(b"abc", 0, 3, lease=LEASE_B)
    # Microsoft's Data Lake error table: 412 LeaseIdMismatch
    assert_error_in(e, ("LeaseIdMismatch", "LeaseIdMismatchWithBlobOperation"), 412)


@pytest.mark.docs
def test_append_with_lease_id_on_unleased_file_is_412_lease_not_present(fs):
    f = fs.create_file("f")
    with pytest.raises(HttpResponseError) as e:
        f.append_data(b"abc", 0, 3, lease=LEASE_A)
    assert_error_in(e, ("LeaseNotPresent", "LeaseNotPresentWithBlobOperation"), 412)


def test_append_flush_with_lease_succeeds(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    f.append_data(b"abc", 0, 3, lease=lease)
    f.flush_data(3, lease=lease)
    assert read_file(f) == b"abc"


def test_overwrite_leased_file_with_lease_keeps_lease(fs):
    """Recorded: create_file(lease=...) on a leased file overwrites it and the lease persists."""
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_id=LEASE_A)
    f.create_file(lease=lease)
    assert f.get_file_properties().lease.state == "leased"


@pytest.mark.docs
def test_delete_leased_file_requires_lease(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        f.delete_file()
    assert_error(e, "LeaseIdMissing", 412)
    f.delete_file(lease=lease)
    assert not f.exists()


@pytest.mark.docs
def test_set_metadata_on_leased_file_with_wrong_lease_is_412(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease(lease_id=LEASE_A)
    with pytest.raises(HttpResponseError) as e:
        f.set_metadata({"a": "1"}, lease=LEASE_B)
    assert_status(e, 412)
    f.set_metadata({"a": "1"}, lease=lease)
    assert f.get_file_properties().metadata == {"a": "1"}


@pytest.mark.docs
def test_rename_leased_file_needs_source_lease(fs):
    f = put_file(fs, "f", b"abc")
    lease = f.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        f.rename_file(fs.file_system_name + "/g")
    assert_status(e, 412)
    new = f.rename_file(fs.file_system_name + "/g", source_lease=lease)
    assert read_file(new) == b"abc"


def test_broken_lease_allows_writes_without_id(fs):
    f = fs.create_file("f")
    lease = f.acquire_lease()
    lease.break_lease(lease_break_period=0)
    f.append_data(b"abc", 0, 3)
    f.flush_data(3)
    assert read_file(f) == b"abc"


# -- lease actions on append/flush (recorded) --------------------------------
def test_append_and_flush_lease_actions(fs):
    """Recorded: x-ms-lease-action acquire / auto-renew / release / acquire-release on append."""
    f = fs.create_file("f")
    data = b"Hello world"
    lease_id = "670d43d1-ecde-4ae9-9c37-d22d340e7719"
    f.append_data(data, 0, len(data), lease_action="acquire", lease_duration=30, lease=lease_id)
    lease = f.get_file_properties().lease
    assert (lease.state, lease.duration) == ("leased", "fixed")
    f.append_data(data, 0, len(data), lease_action="auto-renew", lease=lease_id)
    lease = f.get_file_properties().lease
    assert (lease.state, lease.duration) == ("leased", "fixed")
    f.append_data(data, 0, len(data), flush=True, lease_action="release", lease=lease_id)
    lease = f.get_file_properties().lease
    assert lease.state == "available"
    assert not lease.duration
    f.append_data(data, 0, len(data), flush=True, lease_action="acquire-release", lease=lease_id)
    lease = f.get_file_properties().lease
    assert lease.state == "available"
    assert not lease.duration


# -- directory leases --------------------------------------------------------
def test_directory_lease_lifecycle(fs):
    d = fs.create_directory("d")
    lease = d.acquire_lease(lease_duration=15, lease_id=LEASE_A)
    props = d.get_directory_properties()
    assert props.lease.state == "leased"
    assert props.lease.duration == "fixed"
    lease.renew()
    lease.change(LEASE_B)
    lease.release()
    assert d.get_directory_properties().lease.state == "available"


@pytest.mark.docs
def test_delete_leased_directory_requires_lease(fs):
    d = fs.create_directory("d")
    lease = d.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        d.delete_directory()
    assert_status(e, 412)
    d.delete_directory(lease=lease)
    assert not d.exists()


@pytest.mark.docs
def test_rename_leased_directory_with_source_lease(fs):
    d = fs.create_directory("d")
    lease = d.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        d.rename_directory(fs.file_system_name + "/e")
    assert_status(e, 412)
    new = d.rename_directory(fs.file_system_name + "/e", source_lease=lease)
    assert new.exists()
