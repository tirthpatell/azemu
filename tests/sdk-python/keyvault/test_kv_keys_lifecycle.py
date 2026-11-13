"""Keys: soft delete, recover, purge, backup and restore.

Recorded: DELETE /keys/<n> -> 200 with recoveryId, deletedDate, scheduledPurgeDate and
the key's JWK; GET /deletedkeys/<n> -> 404 KeyNotFound ("Deleted Key not found: <n>")
until the delete is complete; DELETE /deletedkeys/<n> -> 204; restore keeps the kid;
restore over an existing key -> 409 Conflict ("conflict restoring the key").
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.keys import DeletedKey, KeyProperties, KeyVaultKey
from azure.keyvault.keys.crypto import SignatureAlgorithm

from kvutil import (
    POLL,
    delete_and_wait,
    digest_of,
    ec_public_key_from_jwk,
    ecdsa_raw_to_der,
    error_code,
    hash_alg,
    purge,
    recover_and_wait,
    retry_on_conflict,
    unique,
)


# -- delete ------------------------------------------------------------------
def test_begin_delete_key_poller_completes(keys):
    name = unique("k")
    keys.create_ec_key(name)
    poller = keys.begin_delete_key(name, _polling_interval=POLL)
    poller.wait()
    assert poller.done()
    assert poller.status() == "finished"


def test_begin_delete_key_returns_deleted_key_with_dates_and_recovery_id(keys):
    name = unique("k")
    created = keys.create_ec_key(name, tags={"a": "b"})
    deleted = delete_and_wait(keys, name)
    assert isinstance(deleted, DeletedKey)
    assert deleted.name == name
    assert deleted.recovery_id == f"{keys.vault_url}/deletedkeys/{name}"
    assert deleted.id == created.id
    assert deleted.key.x == created.key.x  # recorded: the delete response includes the JWK
    assert abs(deleted.deleted_date - datetime.now(timezone.utc)) < timedelta(minutes=2)
    assert deleted.scheduled_purge_date > deleted.deleted_date
    assert deleted.properties.tags == {"a": "b"}


def test_get_key_after_delete_is_not_found(keys):
    name = unique("k")
    keys.create_ec_key(name)
    delete_and_wait(keys, name)
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_key(name)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "KeyNotFound"


def test_delete_missing_key_is_key_not_found(keys):
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.begin_delete_key(unique("missing"), _polling_interval=POLL)
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "KeyNotFound"


def test_delete_key_removes_every_version(keys):
    name = unique("k")
    for _ in range(3):
        keys.create_ec_key(name)
    delete_and_wait(keys, name)
    assert list(keys.list_properties_of_key_versions(name)) == []
    assert list(keys.list_properties_of_keys()) == []


def test_deleted_key_is_not_in_active_list(keys):
    keep, gone = unique("keep"), unique("gone")
    keys.create_ec_key(keep)
    keys.create_ec_key(gone)
    delete_and_wait(keys, gone)
    assert [p.name for p in keys.list_properties_of_keys()] == [keep]


# -- deleted keys ------------------------------------------------------------
def test_get_deleted_key(keys):
    name = unique("k")
    created = keys.create_rsa_key(name)
    delete_and_wait(keys, name)
    deleted = keys.get_deleted_key(name)
    assert isinstance(deleted, DeletedKey)
    assert deleted.name == name
    assert deleted.recovery_id == f"{keys.vault_url}/deletedkeys/{name}"
    assert deleted.id == created.id
    assert deleted.key.n == created.key.n
    assert deleted.properties.vault_url == keys.vault_url


def test_get_deleted_key_for_active_key_is_not_found(keys):
    name = unique("k")
    keys.create_ec_key(name)
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_deleted_key(name)
    assert error_code(exc.value) == "KeyNotFound"
    assert "Deleted Key not found" in exc.value.message


def test_list_deleted_keys_empty(keys):
    assert list(keys.list_deleted_keys()) == []


def test_list_deleted_keys_only_deleted_ones_with_recovery_ids(keys):
    keep, g1, g2 = unique("keep"), unique("gone"), unique("gone")
    for n in (keep, g1, g2):
        keys.create_ec_key(n)
    delete_and_wait(keys, g1)
    delete_and_wait(keys, g2)
    listed = list(keys.list_deleted_keys())
    assert sorted(d.name for d in listed) == sorted([g1, g2])
    for d in listed:
        assert isinstance(d, DeletedKey)
        assert d.recovery_id == f"{keys.vault_url}/deletedkeys/{d.name}"
        assert d.deleted_date is not None and d.scheduled_purge_date is not None
        assert isinstance(d.properties, KeyProperties)


def test_list_deleted_keys_pages_through_more_than_25(keys):
    names = {unique("gone") for _ in range(27)}
    for n in names:
        keys.create_ec_key(n)
    for n in names:
        delete_and_wait(keys, n)
    assert {d.name for d in keys.list_deleted_keys()} == names


# -- recover -----------------------------------------------------------------
def test_recover_deleted_key_restores_kid_and_material(keys):
    name = unique("k")
    created = keys.create_rsa_key(name, tags={"t": "1"})
    delete_and_wait(keys, name)
    recovered = recover_and_wait(keys, name)
    assert isinstance(recovered, KeyVaultKey)
    assert recovered.id == created.id
    fetched = keys.get_key(name)
    assert fetched.id == created.id
    assert fetched.key.n == created.key.n
    assert fetched.properties.tags == {"t": "1"}
    assert list(keys.list_deleted_keys()) == []


def test_recover_brings_back_all_versions(keys):
    name = unique("k")
    versions = {keys.create_ec_key(name).properties.version for _ in range(3)}
    delete_and_wait(keys, name)
    recover_and_wait(keys, name)
    assert {p.version for p in keys.list_properties_of_key_versions(name)} == versions


def test_recover_key_that_was_not_deleted_is_not_found(keys):
    name = unique("k")
    keys.create_ec_key(name)
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.begin_recover_deleted_key(name, _polling_interval=POLL)
    assert error_code(exc.value) == "KeyNotFound"


def test_recovered_key_still_signs_with_the_original_private_key(keys, server_crypto):
    name = unique("k")
    created = keys.create_ec_key(name, curve="P-256")
    delete_and_wait(keys, name)
    recovered = recover_and_wait(keys, name)
    digest = digest_of(b"after recover", 256)
    sig = server_crypto(recovered.id).sign(SignatureAlgorithm.es256, digest).signature
    from cryptography.hazmat.primitives.asymmetric import ec, utils

    ec_public_key_from_jwk(created.key).verify(
        ecdsa_raw_to_der(sig), digest, ec.ECDSA(utils.Prehashed(hash_alg(256)))
    )


# -- purge -------------------------------------------------------------------
def test_purge_deleted_key_removes_it_for_good(keys):
    name = unique("k")
    keys.create_ec_key(name)
    delete_and_wait(keys, name)
    assert keys.purge_deleted_key(name) is None
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.get_deleted_key(name)
    assert error_code(exc.value) == "KeyNotFound"
    assert list(keys.list_deleted_keys()) == []


def test_purged_key_cannot_be_recovered(keys):
    name = unique("k")
    keys.create_ec_key(name)
    delete_and_wait(keys, name)
    purge(keys, name)
    with pytest.raises(ResourceNotFoundError):
        keys.begin_recover_deleted_key(name, _polling_interval=POLL)


def test_key_name_can_be_reused_after_purge(keys):
    name = unique("k")
    old = keys.create_ec_key(name)
    delete_and_wait(keys, name)
    purge(keys, name)
    fresh = retry_on_conflict(lambda: keys.create_ec_key(name))
    assert fresh.properties.version != old.properties.version
    assert len(list(keys.list_properties_of_key_versions(name))) == 1


def test_purge_active_key_is_not_found_and_keeps_the_key(keys):
    name = unique("k")
    keys.create_ec_key(name)
    with pytest.raises(ResourceNotFoundError):
        keys.purge_deleted_key(name)
    assert keys.get_key(name).name == name


# -- name in the deleted state -----------------------------------------------
def test_create_key_while_name_is_deleted_is_conflict(keys):
    # From Azure docs (and the "being deleted" 409 in the recordings): a soft-deleted
    # name cannot be reused until it is purged or recovered.
    name = unique("k")
    keys.create_ec_key(name)
    delete_and_wait(keys, name)
    with pytest.raises(HttpResponseError) as exc:
        keys.create_ec_key(name)
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"


# -- backup / restore --------------------------------------------------------
def test_backup_key_returns_non_empty_bytes(keys):
    name = unique("k")
    keys.create_ec_key(name)
    backup = keys.backup_key(name)
    assert isinstance(backup, bytes) and len(backup) > 0


def test_backup_missing_key_is_not_found(keys):
    with pytest.raises(ResourceNotFoundError) as exc:
        keys.backup_key(unique("missing"))
    assert error_code(exc.value) == "KeyNotFound"


def test_restore_key_backup_round_trip_keeps_kid_and_public_material(keys):
    name = unique("k")
    created = keys.create_rsa_key(name, tags={"b": "1"}, key_operations=["sign", "verify"])
    backup = keys.backup_key(name)
    delete_and_wait(keys, name)
    purge(keys, name)
    restored = retry_on_conflict(lambda: keys.restore_key_backup(backup))
    assert isinstance(restored, KeyVaultKey)
    assert restored.id == created.id  # recorded: restore keeps the original kid
    assert restored.key.n == created.key.n
    assert restored.key.e == created.key.e
    fetched = keys.get_key(name)
    assert fetched.id == created.id
    assert fetched.properties.tags == {"b": "1"}
    assert {str(getattr(o, "value", o)) for o in fetched.key_operations} == {"sign", "verify"}


def test_restore_key_backup_keeps_the_private_key(keys, server_crypto):
    name = unique("k")
    created = keys.create_ec_key(name, curve="P-256")
    backup = keys.backup_key(name)
    delete_and_wait(keys, name)
    purge(keys, name)
    restored = retry_on_conflict(lambda: keys.restore_key_backup(backup))
    digest = digest_of(b"after restore", 256)
    sig = server_crypto(restored.id).sign(SignatureAlgorithm.es256, digest).signature
    from cryptography.hazmat.primitives.asymmetric import ec, utils

    ec_public_key_from_jwk(created.key).verify(
        ecdsa_raw_to_der(sig), digest, ec.ECDSA(utils.Prehashed(hash_alg(256)))
    )


def test_restore_key_backup_restores_every_version(keys):
    name = unique("k")
    versions = {keys.create_ec_key(name).properties.version for _ in range(3)}
    backup = keys.backup_key(name)
    delete_and_wait(keys, name)
    purge(keys, name)
    retry_on_conflict(lambda: keys.restore_key_backup(backup))
    assert {p.version for p in keys.list_properties_of_key_versions(name)} == versions


def test_restore_key_backup_while_key_exists_is_conflict(keys):
    name = unique("k")
    keys.create_ec_key(name)
    backup = keys.backup_key(name)
    with pytest.raises(HttpResponseError) as exc:
        keys.restore_key_backup(backup)
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"
    assert "conflict restoring the key" in exc.value.message


def test_restore_garbage_key_backup_is_a_client_error(keys):
    with pytest.raises(HttpResponseError) as exc:
        keys.restore_key_backup(b"this is not a key vault backup")
    assert 400 <= exc.value.status_code < 500
