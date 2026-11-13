"""FileSystemClient: create / delete / exists / properties / metadata / access policy / leases.

All of these go to the BLOB endpoint of the account (a filesystem is a container),
so error codes are the container ones (ContainerNotFound, ContainerAlreadyExists).
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.storage.filedatalake import (
    AccessPolicy,
    DataLakeLeaseClient,
    FileSystemClient,
    FileSystemSasPermissions,
    PublicAccess,
)

from dlutil import assert_error, put_file


# -- create ------------------------------------------------------------------
def test_create_file_system_returns_properties(service, uniq):
    fsc = service.get_file_system_client(uniq("fs"))
    created = fsc.create_file_system()
    assert created["etag"]
    assert created["last_modified"] is not None


def test_create_file_system_via_service_client_returns_client(service, uniq):
    fsc = service.create_file_system(uniq("fs"))
    assert isinstance(fsc, FileSystemClient)
    assert fsc.get_file_system_properties().name == fsc.file_system_name


def test_create_file_system_with_trailing_slash_in_name(service, uniq):
    """Recorded: PUT /<name>/?restype=container -> 201."""
    name = uniq("fs")
    fsc = service.get_file_system_client(name + "/")
    assert fsc.create_file_system()
    assert service.get_file_system_client(name).exists()


def test_create_file_system_with_metadata(service, uniq):
    fsc = service.get_file_system_client(uniq("fs"))
    fsc.create_file_system(metadata={"hello": "world", "number": "42"})
    props = fsc.get_file_system_properties()
    assert props.metadata == {"hello": "world", "number": "42"}


def test_create_file_system_with_public_access(service, uniq):
    fsc = service.get_file_system_client(uniq("fs"))
    fsc.create_file_system(public_access=PublicAccess.FILESYSTEM)
    assert fsc.get_file_system_properties().public_access == PublicAccess.FILESYSTEM


def test_create_file_system_default_has_no_public_access(fs):
    assert fs.get_file_system_properties().public_access is None


def test_create_existing_file_system_raises(fs):
    with pytest.raises(ResourceExistsError) as e:
        fs.create_file_system()
    assert_error(e, "ContainerAlreadyExists", 409)


@pytest.mark.docs
def test_create_file_system_with_invalid_name_is_400(service):
    from azure.core.exceptions import HttpResponseError

    with pytest.raises(HttpResponseError) as e:
        service.get_file_system_client("Invalid_Name").create_file_system()
    assert_error(e, "InvalidResourceName", 400)


# -- exists / properties -----------------------------------------------------
def test_file_system_exists_true_and_false(service, uniq):
    fsc = service.get_file_system_client(uniq("fs"))
    assert not fsc.exists()
    fsc.create_file_system()
    assert fsc.exists()


def test_get_file_system_properties_fields(fs):
    props = fs.get_file_system_properties()
    assert props.name == fs.file_system_name
    assert props.etag
    assert props.last_modified is not None
    assert props.has_immutability_policy is not None
    assert props.has_legal_hold is not None
    assert props.lease.state == "available"
    assert props.lease.status == "unlocked"


def test_get_properties_of_missing_file_system_raises(service, uniq):
    with pytest.raises(ResourceNotFoundError) as e:
        service.get_file_system_client(uniq("fs")).get_file_system_properties()
    assert_error(e, "ContainerNotFound", 404)


# -- metadata ----------------------------------------------------------------
def test_set_file_system_metadata_returns_etag_and_last_modified(fs):
    resp = fs.set_file_system_metadata({"hello": "world", "number": "42"})
    assert resp["etag"]
    assert resp["last_modified"] is not None
    assert fs.get_file_system_properties().metadata == {"hello": "world", "number": "42"}


def test_set_file_system_metadata_replaces_previous(fs):
    fs.set_file_system_metadata({"a": "1", "b": "2"})
    fs.set_file_system_metadata({"c": "3"})
    assert fs.get_file_system_properties().metadata == {"c": "3"}


def test_set_file_system_metadata_empty_clears(fs):
    fs.set_file_system_metadata({"a": "1"})
    fs.set_file_system_metadata({})
    assert not fs.get_file_system_properties().metadata


def test_set_file_system_metadata_changes_etag(fs):
    before = fs.get_file_system_properties().etag
    fs.set_file_system_metadata({"a": "1"})
    assert fs.get_file_system_properties().etag != before


def test_set_metadata_on_missing_file_system_raises(service, uniq):
    with pytest.raises(ResourceNotFoundError) as e:
        service.get_file_system_client(uniq("fs")).set_file_system_metadata({"a": "1"})
    assert_error(e, "ContainerNotFound", 404)


# -- delete ------------------------------------------------------------------
def test_delete_file_system_returns_none(service, uniq):
    fsc = service.create_file_system(uniq("fs"))
    assert fsc.delete_file_system() is None
    assert not fsc.exists()


def test_delete_missing_file_system_raises_not_found(service):
    """Recorded: DELETE /fakeclient?restype=container -> 404 ContainerNotFound (XML)."""
    with pytest.raises(ResourceNotFoundError) as e:
        service.get_file_system_client("fakeclient").delete_file_system()
    assert_error(e, "ContainerNotFound", 404)


def test_operations_on_deleted_file_system_raise(service, uniq):
    fsc = service.create_file_system(uniq("fs"))
    fsc.delete_file_system()
    with pytest.raises(ResourceNotFoundError):
        fsc.get_file_system_properties()
    with pytest.raises(ResourceNotFoundError) as e:
        fsc.create_file("a.txt")
    assert e.value.status_code == 404


@pytest.mark.docs
def test_create_file_in_missing_file_system_error_code(service, uniq):
    with pytest.raises(ResourceNotFoundError) as e:
        service.get_file_system_client(uniq("fs")).create_file("a.txt")
    assert_error(e, "FilesystemNotFound", 404)


@pytest.mark.docs
def test_get_paths_on_missing_file_system_error_code(service, uniq):
    with pytest.raises(ResourceNotFoundError) as e:
        list(service.get_file_system_client(uniq("fs")).get_paths())
    assert_error(e, "FilesystemNotFound", 404)


# -- access policy / public access ------------------------------------------
def _policy(**kw):
    return AccessPolicy(
        permission=FileSystemSasPermissions(read=True),
        start=datetime.now(timezone.utc),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
        **kw,
    )


def test_set_file_system_access_policy_with_public_access(fs):
    resp = fs.set_file_system_access_policy({"testid": _policy()}, public_access=PublicAccess.FILESYSTEM)
    assert resp["etag"]
    assert resp["last_modified"] is not None
    acl = fs.get_file_system_access_policy()
    assert acl["public_access"] == PublicAccess.FILESYSTEM
    assert len(acl["signed_identifiers"]) == 1
    assert acl["signed_identifiers"][0].id == "testid"


def test_set_file_system_access_policy_without_public_access_resets_it(fs):
    fs.set_file_system_access_policy({"testid": _policy()}, public_access=PublicAccess.FILESYSTEM)
    fs.set_file_system_access_policy({"testid": _policy(), "test2": _policy()})
    acl = fs.get_file_system_access_policy()
    assert acl["public_access"] is None
    assert sorted(i.id for i in acl["signed_identifiers"]) == ["test2", "testid"]


def test_access_policy_permission_round_trips(fs):
    fs.set_file_system_access_policy({"p": _policy()})
    ident = fs.get_file_system_access_policy()["signed_identifiers"][0]
    assert "r" in str(ident.access_policy.permission)


def test_clear_access_policy(fs):
    fs.set_file_system_access_policy({"testid": _policy()})
    fs.set_file_system_access_policy({})
    assert fs.get_file_system_access_policy()["signed_identifiers"] == []


def test_new_file_system_has_empty_access_policy(fs):
    acl = fs.get_file_system_access_policy()
    assert acl["public_access"] is None
    assert acl["signed_identifiers"] == []


# -- leases on a filesystem --------------------------------------------------
def test_file_system_lease_acquire_state(fs):
    lease = fs.acquire_lease(lease_duration=15)
    assert isinstance(lease, DataLakeLeaseClient)
    assert lease.id
    props = fs.get_file_system_properties()
    assert props.lease.state == "leased"
    assert props.lease.status == "locked"
    assert props.lease.duration == "fixed"


def test_file_system_lease_infinite(fs):
    fs.acquire_lease()
    assert fs.get_file_system_properties().lease.duration == "infinite"


def test_file_system_lease_with_proposed_id(fs):
    lease_id = "4cf4e284-f6a8-4540-b53e-c3469af032dc"
    lease = fs.acquire_lease(lease_id=lease_id)
    assert lease.id == lease_id


def test_file_system_lease_release_makes_it_available(fs):
    lease = fs.acquire_lease()
    lease.release()
    assert fs.get_file_system_properties().lease.state == "available"


def test_file_system_lease_renew_and_change(fs):
    lease = fs.acquire_lease(lease_duration=15)
    lease.renew()
    new_id = "11111111-2222-3333-4444-555555555555"
    lease.change(new_id)
    assert lease.id == new_id
    assert fs.get_file_system_properties().lease.state == "leased"


def test_file_system_lease_break(fs):
    lease = fs.acquire_lease()
    remaining = lease.break_lease(lease_break_period=0)
    assert remaining == 0
    assert fs.get_file_system_properties().lease.state == "broken"


@pytest.mark.docs
def test_delete_leased_file_system_requires_lease_id(fs):
    lease = fs.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        fs.delete_file_system()
    assert_error(e, "LeaseIdMissing", 412)
    fs.delete_file_system(lease=lease)
    assert not fs.exists()


@pytest.mark.docs
def test_acquire_lease_twice_conflicts(fs):
    fs.acquire_lease()
    with pytest.raises(HttpResponseError) as e:
        fs.acquire_lease()
    assert_error(e, "LeaseAlreadyPresent", 409)


@pytest.mark.docs
def test_set_metadata_with_wrong_lease_id_fails_with_right_one_succeeds(fs):
    lease = fs.acquire_lease()
    # Set Container Metadata: when a lease id is sent, it must be the active lease (412 otherwise).
    with pytest.raises(HttpResponseError) as e:
        fs.set_file_system_metadata({"a": "1"}, lease="00000000-0000-0000-0000-000000000000")
    assert e.value.status_code == 412
    fs.set_file_system_metadata({"a": "1"}, lease=lease)
    assert fs.get_file_system_properties().metadata == {"a": "1"}


# -- conditional delete ------------------------------------------------------
def test_delete_file_system_if_unmodified_since_past_fails(fs):
    props = fs.get_file_system_properties()
    with pytest.raises(HttpResponseError) as e:
        fs.delete_file_system(if_unmodified_since=props.last_modified - timedelta(hours=1))
    assert_error(e, "ConditionNotMet", 412)
    assert fs.exists()


def test_delete_file_system_if_unmodified_since_future_succeeds(fs):
    fs.delete_file_system(if_unmodified_since=datetime.now(timezone.utc) + timedelta(hours=1))
    assert not fs.exists()
