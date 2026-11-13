"""Certificates: soft delete, recover, purge, backup and restore.

Sources:
  * Recorded: DELETE /certificates/{n} -> 200 with recoveryId, deletedDate, scheduledPurgeDate and
    the full bundle (id with version, kid, sid, x5t, cer, attributes, policy); GET
    /deletedcertificates/{n} answers 404 CertificateNotFound "Deleted Certificate not found: <n>"
    until the deleted object exists, then 200; GET /certificates/{n}/{version} after the delete -> 404
    CertificateNotFound; GET /deletedcertificates lists items with recoveryId, deletedDate,
    scheduledPurgeDate, id WITHOUT version, x5t, attributes, tags (no cer, kid or sid); POST
    /deletedcertificates/{n}/recover -> 200 bundle; DELETE /deletedcertificates/{n} -> 204;
    POST /certificates/{n}/backup -> {"value": ...}; POST /certificates/restore keeps the original
    version ids; restore over an existing certificate -> 409 code "Conflict", message "There was a
    conflict restoring the certificate '<url>'"; deleting a certificate that is still pending works.
  * From Azure docs / the secrets and keys tests (marked): the linked secret and key leave with the
    certificate and come back on recover; creating a certificate with a name in the deleted state is a
    409; restore into another vault; garbage backup is a client error; backups hold every version.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.keyvault.certificates import DeletedCertificate, KeyVaultCertificate

from kvutil import (
    POLL,
    begin_pending,
    create_cert,
    delete_and_wait,
    ec_policy,
    error_code,
    load_pfx_secret,
    public_numbers_equal,
    purge,
    recover_and_wait,
    retry_on_conflict,
    unique,
    x509_of,
)


def _utcnow():
    return datetime.now(timezone.utc)


# -- delete ---------------------------------------------------------------------------------
def test_begin_delete_certificate_poller_completes(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    poller = certs.begin_delete_certificate(name, _polling_interval=POLL)
    poller.wait()
    assert poller.done()
    assert poller.status() == "finished"


def test_begin_delete_certificate_returns_deleted_certificate(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy(), tags={"k": "v"})
    deleted = delete_and_wait(certs, name)
    assert isinstance(deleted, DeletedCertificate)
    assert deleted.name == name
    assert deleted.recovery_id == f"{certs.vault_url}/deletedcertificates/{name}"
    assert deleted.id == created.id
    assert deleted.key_id == created.key_id
    assert deleted.secret_id == created.secret_id
    assert deleted.cer == created.cer
    assert deleted.properties.tags == {"k": "v"}
    assert deleted.policy.subject == "CN=azemu-test"
    assert abs(deleted.deleted_on - _utcnow()) < timedelta(minutes=2)
    assert deleted.scheduled_purge_date > deleted.deleted_on


def test_get_certificate_after_delete_is_not_found(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_certificate(name)
    assert error_code(exc.value) == "CertificateNotFound"
    with pytest.raises(ResourceNotFoundError) as exc2:
        certs.get_certificate_version(name, created.properties.version)  # recorded in test_crud_operations
    assert error_code(exc2.value) == "CertificateNotFound"


def test_deleted_certificate_leaves_the_active_list(certs):
    keep, gone = unique("cert"), unique("cert")
    create_cert(certs, keep, ec_policy())
    create_cert(certs, gone, ec_policy())
    delete_and_wait(certs, gone)
    assert [p.name for p in certs.list_properties_of_certificates()] == [keep]


def test_delete_certificate_removes_every_version(certs):
    name = unique("cert")
    versions = [create_cert(certs, name, ec_policy()).properties.version for _ in range(3)]
    delete_and_wait(certs, name)
    assert list(certs.list_properties_of_certificate_versions(name)) == []
    for v in versions:
        with pytest.raises(ResourceNotFoundError):
            certs.get_certificate_version(name, v)


def test_delete_missing_certificate_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.begin_delete_certificate(unique("missing"))
    assert error_code(exc.value) == "CertificateNotFound"


def test_delete_certificate_twice_second_is_not_found(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    with pytest.raises(ResourceNotFoundError):
        certs.begin_delete_certificate(name)


def test_delete_disabled_certificate_succeeds(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy(), enabled=False)
    assert delete_and_wait(certs, name).name == name


def test_delete_pending_certificate_succeeds(certs):
    # Recorded (test_async_request_cancellation_and_deletion): a certificate whose create is
    # cancelled / pending is deleted like any other and shows up as deleted.
    name = unique("ext")
    begin_pending(certs, name)
    deleted = delete_and_wait(certs, name)
    assert deleted.name == name
    assert deleted.recovery_id == f"{certs.vault_url}/deletedcertificates/{name}"
    assert certs.get_deleted_certificate(name).name == name


# -- the linked secret and key -----------------------------------------------------------------
def test_delete_certificate_removes_its_secret_and_key(certs, secrets, keys):
    # From Azure docs: the secret and the key of a certificate cannot exist without it.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    with pytest.raises(ResourceNotFoundError) as s:
        secrets.get_secret(name)
    assert error_code(s.value) == "SecretNotFound"
    with pytest.raises(ResourceNotFoundError) as k:
        keys.get_key(name)
    assert error_code(k.value) == "KeyNotFound"
    assert list(secrets.list_properties_of_secrets()) == []
    assert list(keys.list_properties_of_keys()) == []


def test_recover_brings_back_the_secret_and_key(certs, secrets, keys):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    before, _, _ = load_pfx_secret(secrets.get_secret(name).value)
    delete_and_wait(certs, name)
    recover_and_wait(certs, name)
    secret = secrets.get_secret(name)
    assert secret.properties.version == created.properties.version
    assert secret.properties.managed is True
    after, _, _ = load_pfx_secret(secret.value)
    assert after.private_numbers() == before.private_numbers()
    key = keys.get_key(name)
    assert key.properties.version == created.properties.version
    assert key.properties.managed is True


# -- get deleted / list deleted ---------------------------------------------------------------------
def test_get_deleted_certificate(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    got = certs.get_deleted_certificate(name)
    assert isinstance(got, DeletedCertificate)
    assert got.name == name
    assert got.id == created.id  # recorded: full id with version
    assert got.cer == created.cer
    assert got.recovery_id == f"{certs.vault_url}/deletedcertificates/{name}"
    assert got.deleted_on is not None
    assert got.scheduled_purge_date > got.deleted_on


def test_get_deleted_certificate_that_is_not_deleted_is_not_found(certs):
    # Recorded: "Deleted Certificate not found: <name>" with code CertificateNotFound.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.get_deleted_certificate(name)
    assert error_code(exc.value) == "CertificateNotFound"
    assert f"Deleted Certificate not found: {name}" in exc.value.message


def test_get_deleted_certificate_unknown_name_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.get_deleted_certificate(unique("missing"))


def test_list_deleted_certificates_empty(certs):
    assert list(certs.list_deleted_certificates()) == []


def test_list_deleted_certificates_contains_only_deleted_ones(certs):
    live, dead1, dead2 = unique("cert"), unique("cert"), unique("cert")
    for n in (live, dead1, dead2):
        create_cert(certs, n, ec_policy())
    delete_and_wait(certs, dead1)
    delete_and_wait(certs, dead2)
    assert {d.name for d in certs.list_deleted_certificates()} == {dead1, dead2}


def test_list_deleted_certificates_items_have_recovery_id_dates_and_id_without_version(certs):
    # Recorded: list items carry recoveryId, deletedDate, scheduledPurgeDate and an id without version.
    name = unique("cert")
    create_cert(certs, name, ec_policy(), tags={"foo": "bar"})
    delete_and_wait(certs, name)
    (item,) = list(certs.list_deleted_certificates())
    assert isinstance(item, DeletedCertificate)
    assert item.name == name
    assert item.id == f"{certs.vault_url}/certificates/{name}"
    assert item.recovery_id == f"{certs.vault_url}/deletedcertificates/{name}"
    assert item.deleted_on is not None
    assert item.scheduled_purge_date > item.deleted_on
    assert item.properties.tags == {"foo": "bar"}
    assert item.properties.version is None
    assert item.cer is None


def test_list_deleted_certificates_pages_through_more_than_25(certs):
    # No page size is asserted: the recordings show that Key Vault chooses its own default page size.
    names = {unique("cert") for _ in range(27)}
    for n in names:
        create_cert(certs, n, ec_policy())
        delete_and_wait(certs, n)
    assert {d.name for d in certs.list_deleted_certificates()} == names


def test_list_active_certificates_pages_through_more_than_25(certs):
    names = {unique("cert") for _ in range(27)}
    for n in names:
        create_cert(certs, n, ec_policy())
    listed = [p.name for p in certs.list_properties_of_certificates()]
    assert len(listed) == 27
    assert set(listed) == names


def test_list_deleted_certificates_pages_with_max_page_size(certs):
    names = {unique("cert") for _ in range(5)}
    for n in names:
        create_cert(certs, n, ec_policy())
        delete_and_wait(certs, n)
    pages = [list(p) for p in certs.list_deleted_certificates(max_page_size=2).by_page()]
    assert [len(p) for p in pages] == [2, 2, 1]
    assert {d.name for page in pages for d in page} == names


# -- recover ----------------------------------------------------------------------------------------------
def test_recover_deleted_certificate_restores_certificate_policy_and_metadata(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy("CN=recover-me"), tags={"a": "1"})
    delete_and_wait(certs, name)
    recovered = recover_and_wait(certs, name)
    assert isinstance(recovered, KeyVaultCertificate)
    assert recovered.name == name
    assert recovered.id == created.id
    assert recovered.cer == created.cer
    got = certs.get_certificate(name)
    assert got.properties.tags == {"a": "1"}
    assert got.policy.subject == "CN=recover-me"
    assert got.id == created.id


def test_recover_deleted_certificate_poller_completes(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    poller = certs.begin_recover_deleted_certificate(name, _polling_interval=POLL)
    poller.wait()
    assert poller.done()
    assert poller.status() == "finished"


def test_recovered_certificate_leaves_the_deleted_list(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    recover_and_wait(certs, name)
    assert list(certs.list_deleted_certificates()) == []
    with pytest.raises(ResourceNotFoundError):
        certs.get_deleted_certificate(name)
    assert [p.name for p in certs.list_properties_of_certificates()] == [name]


def test_recover_brings_back_all_versions(certs):
    name = unique("cert")
    versions = {create_cert(certs, name, ec_policy()).properties.version for _ in range(3)}
    delete_and_wait(certs, name)
    recover_and_wait(certs, name)
    assert {p.version for p in certs.list_properties_of_certificate_versions(name)} == versions


def test_recover_certificate_that_was_never_deleted_is_not_found(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    with pytest.raises(ResourceNotFoundError):
        certs.begin_recover_deleted_certificate(name)


def test_recover_missing_certificate_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.begin_recover_deleted_certificate(unique("missing"))


def test_create_new_version_after_recover(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    recover_and_wait(certs, name)
    second = create_cert(certs, name, ec_policy())
    assert second.properties.version != first.properties.version
    assert len(list(certs.list_properties_of_certificate_versions(name))) == 2


# -- purge ------------------------------------------------------------------------------------------------------
def test_purge_deleted_certificate_removes_it_for_good(certs):
    # Recorded: DELETE /deletedcertificates/{n} -> 204.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    assert certs.purge_deleted_certificate(name) is None
    with pytest.raises(ResourceNotFoundError):
        certs.get_deleted_certificate(name)
    assert list(certs.list_deleted_certificates()) == []


def test_purged_certificate_cannot_be_recovered(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    purge(certs, name)
    with pytest.raises(ResourceNotFoundError):
        certs.begin_recover_deleted_certificate(name)


def test_name_can_be_reused_after_purge(certs):
    name = unique("cert")
    first = create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    purge(certs, name)
    second = retry_on_conflict(lambda: create_cert(certs, name, ec_policy()))
    assert second.properties.version != first.properties.version
    assert certs.get_certificate(name).id == second.id


def test_purge_active_certificate_does_not_delete_it(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    with pytest.raises(ResourceNotFoundError):
        certs.purge_deleted_certificate(name)
    assert certs.get_certificate(name).name == name


def test_purge_missing_certificate_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError):
        certs.purge_deleted_certificate(unique("missing"))


def test_purge_removes_the_linked_secret_and_key_for_good(certs, secrets, keys):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    purge(certs, name)
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret(name)
    with pytest.raises(ResourceNotFoundError):
        keys.get_key(name)


# -- name in the deleted state -------------------------------------------------------------------------------------
def test_create_certificate_while_name_is_deleted_is_conflict(certs):
    # From Azure docs (as for secrets and keys): a soft-deleted name cannot be reused until it is
    # purged or recovered.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    with pytest.raises(HttpResponseError) as exc:
        certs.begin_create_certificate(name, ec_policy())
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"


def test_import_certificate_while_name_is_deleted_is_conflict(certs):
    from kvutil import make_ec_self_signed, to_pfx

    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    key, cert = make_ec_self_signed()
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate(name, to_pfx(key, cert))
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"


def test_create_conflict_leaves_the_deleted_certificate_untouched(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    with pytest.raises(HttpResponseError):
        certs.begin_create_certificate(name, ec_policy())
    assert certs.get_deleted_certificate(name).id == created.id


# -- backup / restore ------------------------------------------------------------------------------------------------
def test_backup_certificate_returns_non_empty_bytes(certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    backup = certs.backup_certificate(name)
    assert isinstance(backup, bytes)
    assert len(backup) > 100


def test_backup_missing_certificate_is_not_found(certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        certs.backup_certificate(unique("missing"))
    assert error_code(exc.value) == "CertificateNotFound"


def test_restore_certificate_backup_round_trip_after_purge(certs, secrets, keys):
    # Recorded (test_backup_restore): backup, delete, purge, restore -> the same certificate bundle.
    name = unique("cert")
    created = create_cert(certs, name, ec_policy("CN=round-trip"), tags={"b": "1"})
    private, _, _ = load_pfx_secret(secrets.get_secret(name).value)
    backup = certs.backup_certificate(name)
    delete_and_wait(certs, name)
    purge(certs, name)
    restored = retry_on_conflict(lambda: certs.restore_certificate_backup(backup))
    assert isinstance(restored, KeyVaultCertificate)
    assert restored.id == created.id  # recorded: restore keeps the original version id
    assert restored.cer == created.cer
    assert restored.key_id == created.key_id
    assert restored.secret_id == created.secret_id
    got = certs.get_certificate(name)
    assert got.id == created.id
    assert got.properties.tags == {"b": "1"}
    assert got.policy.subject == "CN=round-trip"
    assert got.policy.issuer_name == "Self"
    assert got.properties.x509_thumbprint == created.properties.x509_thumbprint
    # the secret and the key are back with the same private key
    secret = secrets.get_secret(name)
    assert secret.properties.version == created.properties.version
    restored_key, restored_leaf, _ = load_pfx_secret(secret.value)
    assert restored_key.private_numbers() == private.private_numbers()
    assert public_numbers_equal(restored_key.public_key(), x509_of(created).public_key())
    assert keys.get_key(name).properties.version == created.properties.version


def test_restore_certificate_backup_restores_every_version(certs):
    # From Azure docs: a backup holds all versions of the certificate.
    name = unique("cert")
    versions = {create_cert(certs, name, ec_policy()).properties.version for _ in range(3)}
    backup = certs.backup_certificate(name)
    delete_and_wait(certs, name)
    purge(certs, name)
    retry_on_conflict(lambda: certs.restore_certificate_backup(backup))
    assert {p.version for p in certs.list_properties_of_certificate_versions(name)} == versions


def test_restore_certificate_backup_while_certificate_exists_is_conflict(certs):
    # Recorded: 409, code "Conflict", message "There was a conflict restoring the certificate '<url>'.
    # This can happen if either: a second certificate with the same name ...".
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    backup = certs.backup_certificate(name)
    with pytest.raises(ResourceExistsError) as exc:
        certs.restore_certificate_backup(backup)
    assert exc.value.status_code == 409
    assert error_code(exc.value) == "Conflict"
    assert "conflict restoring the certificate" in exc.value.message


def test_restore_conflict_leaves_the_existing_certificate_alone(certs):
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    backup = certs.backup_certificate(name)
    with pytest.raises(ResourceExistsError):
        certs.restore_certificate_backup(backup)
    assert certs.get_certificate(name).id == created.id
    assert len(list(certs.list_properties_of_certificate_versions(name))) == 1


def test_restore_while_the_certificate_is_deleted_but_not_purged_is_conflict(certs):
    # From Azure docs: the name is still taken by the deleted certificate.
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    backup = certs.backup_certificate(name)
    delete_and_wait(certs, name)
    with pytest.raises(HttpResponseError) as exc:
        certs.restore_certificate_backup(backup)
    assert exc.value.status_code == 409


def test_restore_garbage_backup_is_a_client_error(certs):
    with pytest.raises(HttpResponseError) as exc:
        certs.restore_certificate_backup(b"this is not a certificate backup")
    assert 400 <= exc.value.status_code < 500


def test_backup_restore_in_another_vault_is_independent(emu, certs, make_clients):
    # From Azure docs: a backup can be restored into another vault; the vaults stay separate.
    _s, _k, other = make_clients("othervault")
    name = unique("cert")
    created = create_cert(certs, name, ec_policy())
    backup = certs.backup_certificate(name)
    restored = other.restore_certificate_backup(backup)
    assert restored.name == name
    assert restored.cer == created.cer
    assert other.vault_url != certs.vault_url
    assert restored.id.startswith(other.vault_url)
    delete_and_wait(other, name)
    assert certs.get_certificate(name).id == created.id
