"""Secrets: soft delete, recover, purge, backup and restore.

Delete / recover pollers are given a 100 ms polling interval (kvutil.POLL).
Recorded facts: DELETE /secrets/<n> -> 200 with recoveryId, deletedDate and
scheduledPurgeDate; GET /deletedsecrets/<n> -> 404 SecretNotFound until the
deleted object exists; DELETE /deletedsecrets/<n> -> 204; POST /secrets/restore
keeps the original version id; restore over an existing name -> 409 Conflict.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.secrets import DeletedSecret, SecretProperties

from kvutil import (
    POLL,
    delete_and_wait,
    error_code,
    purge,
    recover_and_wait,
    retry_on_conflict,
    unique,
)


# -- delete ------------------------------------------------------------------
def test_begin_delete_secret_poller_completes(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    poller = secrets.begin_delete_secret(name, _polling_interval=POLL)
    poller.wait()
    assert poller.done()
    assert poller.status() == "finished"


def test_begin_delete_secret_returns_deleted_secret_with_dates_and_recovery_id(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v", content_type="text/plain", tags={"k": "v"})
    deleted = delete_and_wait(secrets, name)
    assert isinstance(deleted, DeletedSecret)
    assert deleted.name == name
    assert deleted.recovery_id == f"{secrets.vault_url}/deletedsecrets/{name}"
    assert deleted.id == created.id
    now = datetime.now(timezone.utc)
    assert abs(deleted.deleted_date - now) < timedelta(minutes=2)
    assert deleted.scheduled_purge_date > deleted.deleted_date
    assert deleted.scheduled_purge_date - deleted.deleted_date <= timedelta(days=91)
    assert deleted.properties.content_type == "text/plain"
    assert deleted.properties.tags == {"k": "v"}


def test_get_secret_after_delete_is_not_found(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_secret(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"


def test_get_specific_version_after_delete_is_not_found(secrets):
    name = unique("sec")
    version = secrets.set_secret(name, "v").properties.version
    delete_and_wait(secrets, name)
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret(name, version)


def test_delete_missing_secret_is_secret_not_found(secrets):
    # Assumed from the shape of the other 404s; no recording of DELETE on a missing secret.
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.begin_delete_secret(unique("missing"), _polling_interval=POLL)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"


def test_delete_secret_twice_second_is_not_found(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    with pytest.raises(ResourceNotFoundError):
        secrets.begin_delete_secret(name, _polling_interval=POLL)


def test_delete_secret_with_several_versions_removes_all_of_them(secrets):
    name = unique("sec")
    for i in range(3):
        secrets.set_secret(name, f"v{i}")
    delete_and_wait(secrets, name)
    assert list(secrets.list_properties_of_secret_versions(name)) == []


def test_delete_disabled_secret_succeeds(secrets):
    # Recorded: deleting a disabled secret works (the SDK's own CRUD test does it).
    name = unique("sec")
    secrets.set_secret(name, "v", enabled=False)
    deleted = delete_and_wait(secrets, name)
    assert deleted.name == name


# -- deleted secrets ---------------------------------------------------------
def test_get_deleted_secret(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "v", tags={"a": "b"})
    delete_and_wait(secrets, name)
    deleted = secrets.get_deleted_secret(name)
    assert isinstance(deleted, DeletedSecret)
    assert deleted.name == name
    assert deleted.recovery_id == f"{secrets.vault_url}/deletedsecrets/{name}"
    assert deleted.properties.vault_url == secrets.vault_url
    assert deleted.id == created.id
    assert deleted.properties.tags == {"a": "b"}
    assert deleted.deleted_date is not None
    assert deleted.scheduled_purge_date is not None


def test_get_deleted_secret_that_is_not_deleted_is_not_found(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_deleted_secret(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "SecretNotFound"
    assert "Deleted Secret not found" in exc.value.message


def test_get_deleted_secret_unknown_name_is_not_found(secrets):
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_deleted_secret(unique("missing"))
    assert error_code(exc.value) == "SecretNotFound"


def test_list_deleted_secrets_empty(secrets):
    assert list(secrets.list_deleted_secrets()) == []


def test_list_deleted_secrets_contains_only_deleted_ones(secrets):
    keep, gone1, gone2 = unique("keep"), unique("gone"), unique("gone")
    for n in (keep, gone1, gone2):
        secrets.set_secret(n, "v")
    delete_and_wait(secrets, gone1)
    delete_and_wait(secrets, gone2)
    listed = list(secrets.list_deleted_secrets())
    assert sorted(d.name for d in listed) == sorted([gone1, gone2])
    assert all(isinstance(d, DeletedSecret) for d in listed)


def test_list_deleted_secrets_items_have_recovery_id_and_dates(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    (d,) = list(secrets.list_deleted_secrets())
    assert d.recovery_id == f"{secrets.vault_url}/deletedsecrets/{name}"
    assert d.deleted_date is not None
    assert d.scheduled_purge_date is not None
    assert d.properties.vault_url == secrets.vault_url
    assert not hasattr(d, "value") or d.value is None


def test_list_deleted_secrets_pages_through_more_than_25(secrets):
    names = {unique("gone") for _ in range(28)}
    for n in names:
        secrets.set_secret(n, "v")
    for n in names:
        delete_and_wait(secrets, n)
    listed = {d.name for d in secrets.list_deleted_secrets()}
    assert listed == names


# -- recover -----------------------------------------------------------------
def test_recover_deleted_secret_restores_value_and_metadata(secrets):
    name = unique("sec")
    created = secrets.set_secret(name, "precious", content_type="text/plain", tags={"t": "1"})
    delete_and_wait(secrets, name)
    recovered = recover_and_wait(secrets, name)
    assert isinstance(recovered, SecretProperties)
    assert recovered.name == name
    assert recovered.version == created.properties.version
    fetched = secrets.get_secret(name)
    assert fetched.value == "precious"
    assert fetched.properties.content_type == "text/plain"
    assert fetched.properties.tags == {"t": "1"}
    assert fetched.properties.version == created.properties.version


def test_recover_deleted_secret_poller_completes(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    poller = secrets.begin_recover_deleted_secret(name, _polling_interval=POLL)
    poller.wait()
    assert poller.done()


def test_recovered_secret_leaves_the_deleted_list(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    recover_and_wait(secrets, name)
    assert list(secrets.list_deleted_secrets()) == []
    assert [p.name for p in secrets.list_properties_of_secrets()] == [name]


def test_recover_brings_back_all_versions(secrets):
    name = unique("sec")
    versions = {secrets.set_secret(name, f"v{i}").properties.version for i in range(3)}
    delete_and_wait(secrets, name)
    recover_and_wait(secrets, name)
    assert {p.version for p in secrets.list_properties_of_secret_versions(name)} == versions


def test_recover_secret_that_was_never_deleted_is_not_found(secrets):
    # Assumed: same SecretNotFound the service uses for every missing deleted secret.
    name = unique("sec")
    secrets.set_secret(name, "v")
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.begin_recover_deleted_secret(name, _polling_interval=POLL)
    assert error_code(exc.value) == "SecretNotFound"


def test_set_secret_can_continue_after_recover(secrets):
    name = unique("sec")
    secrets.set_secret(name, "one")
    delete_and_wait(secrets, name)
    recover_and_wait(secrets, name)
    secrets.set_secret(name, "two")
    assert secrets.get_secret(name).value == "two"
    assert len(list(secrets.list_properties_of_secret_versions(name))) == 2


# -- purge -------------------------------------------------------------------
def test_purge_deleted_secret_removes_it_for_good(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    assert secrets.purge_deleted_secret(name) is None
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.get_deleted_secret(name)
    assert error_code(exc.value) == "SecretNotFound"
    assert name not in [d.name for d in secrets.list_deleted_secrets()]


def test_purged_secret_cannot_be_recovered(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    purge(secrets, name)
    with pytest.raises(ResourceNotFoundError):
        secrets.begin_recover_deleted_secret(name, _polling_interval=POLL)


def test_name_can_be_reused_after_purge(secrets):
    name = unique("sec")
    secrets.set_secret(name, "old")
    delete_and_wait(secrets, name)
    purge(secrets, name)
    fresh = retry_on_conflict(lambda: secrets.set_secret(name, "new"))
    assert fresh.value == "new"
    assert len(list(secrets.list_properties_of_secret_versions(name))) == 1


def test_purge_active_secret_does_not_delete_it(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    with pytest.raises(ResourceNotFoundError):
        secrets.purge_deleted_secret(name)
    assert secrets.get_secret(name).value == "v"


# -- name in the deleted state -----------------------------------------------
def test_set_secret_while_name_is_deleted_is_conflict(secrets):
    # From Azure docs: a soft-deleted name cannot be reused; the secret can only
    # be recovered or purged. Not in the recordings; only status and code are asserted.
    name = unique("sec")
    secrets.set_secret(name, "v")
    delete_and_wait(secrets, name)
    with pytest.raises(HttpResponseError) as exc:
        secrets.set_secret(name, "again")
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"


def test_set_secret_conflict_leaves_the_deleted_secret_untouched(secrets):
    name = unique("sec")
    secrets.set_secret(name, "orig")
    delete_and_wait(secrets, name)
    with pytest.raises(HttpResponseError):
        secrets.set_secret(name, "again")
    recover_and_wait(secrets, name)
    assert secrets.get_secret(name).value == "orig"


# -- backup / restore --------------------------------------------------------
def test_backup_secret_returns_non_empty_bytes(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    backup = secrets.backup_secret(name)
    assert isinstance(backup, bytes)
    assert len(backup) > 0


def test_backup_missing_secret_is_not_found(secrets):
    # Assumed: SecretNotFound like every other missing-secret error.
    with pytest.raises(ResourceNotFoundError) as exc:
        secrets.backup_secret(unique("missing"))
    assert error_code(exc.value) == "SecretNotFound"


def test_restore_secret_backup_round_trip_keeps_value_version_and_metadata(secrets):
    name = unique("sec")
    exp = datetime(2045, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
    created = secrets.set_secret(name, "backed-up", content_type="text/plain", tags={"k": "v"}, expires_on=exp)
    backup = secrets.backup_secret(name)
    delete_and_wait(secrets, name)
    purge(secrets, name)
    restored = retry_on_conflict(lambda: secrets.restore_secret_backup(backup))
    assert isinstance(restored, SecretProperties)
    assert restored.name == name
    assert restored.version == created.properties.version  # recorded: restore keeps the version id
    fetched = secrets.get_secret(name)
    assert fetched.value == "backed-up"
    assert fetched.properties.version == created.properties.version
    assert fetched.properties.content_type == "text/plain"
    assert fetched.properties.tags == {"k": "v"}
    assert fetched.properties.expires_on == exp


def test_restore_secret_backup_restores_every_version(secrets):
    name = unique("sec")
    versions = {secrets.set_secret(name, f"v{i}").properties.version for i in range(3)}
    backup = secrets.backup_secret(name)
    delete_and_wait(secrets, name)
    purge(secrets, name)
    retry_on_conflict(lambda: secrets.restore_secret_backup(backup))
    assert {p.version for p in secrets.list_properties_of_secret_versions(name)} == versions
    assert secrets.get_secret(name).value == "v2"


def test_restore_secret_backup_while_secret_exists_is_conflict(secrets):
    name = unique("sec")
    secrets.set_secret(name, "v")
    backup = secrets.backup_secret(name)
    with pytest.raises(HttpResponseError) as exc:
        secrets.restore_secret_backup(backup)
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"
    assert "conflict restoring the secret" in exc.value.message


def test_restore_conflict_leaves_the_existing_secret_alone(secrets):
    name = unique("sec")
    secrets.set_secret(name, "orig")
    backup = secrets.backup_secret(name)
    secrets.set_secret(name, "newer")
    with pytest.raises(HttpResponseError):
        secrets.restore_secret_backup(backup)
    assert secrets.get_secret(name).value == "newer"


def test_restore_garbage_backup_is_a_client_error(secrets):
    # Real Azure answers 400; the exact code is not in the recordings.
    with pytest.raises(HttpResponseError) as exc:
        secrets.restore_secret_backup(b"this is not a key vault backup")
    assert 400 <= exc.value.status_code < 500


def test_backup_restore_in_another_vault_is_independent(emu, secrets):
    # A backup taken from one vault restores into a different vault (same region
    # and subscription in real Azure; the emulator has one "region").
    from kvutil import make_secret_client

    other = make_secret_client(emu, "otherkv")
    try:
        name = unique("sec")
        secrets.set_secret(name, "portable")
        backup = secrets.backup_secret(name)
        restored = other.restore_secret_backup(backup)
        assert restored.vault_url == other.vault_url
        assert other.get_secret(name).value == "portable"
    finally:
        other.close()
