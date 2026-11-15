"""Answers of a real vault that the emulator now gives too (tried against Azure in November 2026; the texts and codes below
are what Azure sent). These suites have no Azure mode: the checks were made once with raw requests."""

import pytest
from azure.core.exceptions import HttpResponseError
from azure.keyvault.certificates import CertificatePolicy
from azure.keyvault.keys.crypto import EncryptionAlgorithm, SignatureAlgorithm

from kvutil import error_code, make_server_side_crypto_client, unique


def test_a_secret_over_25600_characters_is_refused(secrets):
    secrets.set_secret(unique("sec"), "x" * 25_600)
    with pytest.raises(HttpResponseError) as exc:
        secrets.set_secret(unique("sec"), "x" * 25_601)
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"
    assert "Secret is beyond the maximum permitted length of 25600 characters." in exc.value.message


def test_restore_of_a_backup_that_is_not_one(secrets, keys, certs):
    for restore in (secrets.restore_secret_backup, keys.restore_key_backup, certs.restore_certificate_backup):
        with pytest.raises(HttpResponseError) as exc:
            restore(b"hello")
        assert exc.value.status_code == 400
        assert error_code(exc.value) == "Malformed backup blob"
        assert "Backup blob is corrupt." in exc.value.message


def test_key_sizes_and_types_the_vault_refuses(keys):
    with pytest.raises(HttpResponseError) as exc:
        keys.create_rsa_key(unique("k"), size=1024)
    assert "Unsupported key size: 1024. Valid values for created keys are 2048, 3072, 4096." in exc.value.message
    with pytest.raises(HttpResponseError) as exc:
        keys.create_key(unique("k"), "oct", size=256)
    assert exc.value.status_code == 400
    assert "Software oct key type is not supported. Use oct-HSM." in exc.value.message


def test_key_operation_errors(emu, keys):
    name = unique("k")
    rsa = keys.create_rsa_key(name)
    crypto = make_server_side_crypto_client(emu, rsa.id)
    with pytest.raises(HttpResponseError) as exc:
        crypto.encrypt(EncryptionAlgorithm.rsa_oaep, b"x" * 300)
    assert exc.value.status_code == 400
    assert "Bad Length." in exc.value.message
    with pytest.raises(HttpResponseError) as exc:
        crypto.decrypt(EncryptionAlgorithm.rsa_oaep, b"y" * 256)
    assert "Error occurred while decoding OAEP padding." in exc.value.message
    with pytest.raises(HttpResponseError) as exc:
        crypto.sign(SignatureAlgorithm.rs256, b"z" * 10)
    assert "Invalid length of 'value': 10 bytes. RS256 requires 32 bytes, encoded with base64url." in exc.value.message
    with pytest.raises(HttpResponseError) as exc:
        crypto.sign(SignatureAlgorithm.es256, b"z" * 32)
    assert "Key and signing algorithm are incompatible." in exc.value.message
    assert "can only be used with a key of type 'EC' or 'EC-HSM'." in exc.value.message
    crypto.close()


def test_a_pending_certificate_as_the_service_shows_it(certs):
    name = unique("cert")
    policy = CertificatePolicy(issuer_name="Unknown", subject="CN=pending-test")
    certs.begin_create_certificate(name, policy)  # not waited for: it stays pending until a certificate is merged
    # the bundle is disabled and has no versions (tried in November 2026)
    cert = certs.get_certificate(name)
    assert cert.properties.enabled is False
    assert list(certs.list_properties_of_certificate_versions(name)) == []
    # a second request while the first is pending is refused
    with pytest.raises(HttpResponseError) as exc:
        certs.begin_create_certificate(name, policy)
    assert exc.value.status_code == 409
    assert "while a pending key vault certificate's status is inProgress" in exc.value.message
    # a cancel leaves the request in progress, with the flag set
    op = certs.get_certificate_operation(name)
    assert (op.status, op.cancellation_requested) == ("inProgress", False)
    op = certs.cancel_certificate_operation(name)
    assert (op.status, op.cancellation_requested) == ("inProgress", True)
    # deleting the request leaves the certificate (still without a version)
    certs.delete_certificate_operation(name)
    assert certs.get_certificate(name).properties.enabled is False
