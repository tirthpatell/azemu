"""Certificates through azure.keyvault.certificates.aio (the async clients).

The same scenarios as the sync tests, so that the async pollers, pagers and error mapping are
covered. Expected values come from the recordings listed in the sync test files
(test_async twins of Microsoft's tests exist in the recordings: test_certificates_client_async.py).
"""

import base64

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.keyvault.certificates import (
    CertificateContact,
    CertificateOperation,
    CertificatePolicy,
    DeletedCertificate,
    KeyVaultCertificate,
)

from kvutil import (
    POLL,
    der_of,
    ec_policy,
    error_code,
    load_pfx_secret,
    make_ca,
    make_ec_self_signed,
    public_numbers_equal,
    sha1_of,
    sign_csr,
    to_pfx,
    unique,
    unknown_policy,
    x509_of,
)


async def _create(client, name, policy=None, **kwargs):
    # The async client has no begin_* methods: awaiting create_certificate polls until the result is there.
    return await client.create_certificate(name, policy or ec_policy(), _polling_interval=POLL, **kwargs)


async def test_async_create_certificate_default_policy(async_certs):
    name = unique("cert")
    cert = await async_certs.create_certificate(name, CertificatePolicy.get_default(), _polling_interval=POLL)
    assert isinstance(cert, KeyVaultCertificate)
    assert cert.name == name
    c = x509_of(cert)
    assert c.subject.rfc4514_string() == "CN=DefaultPolicy"
    assert c.issuer == c.subject
    c.verify_directly_issued_by(c)
    assert cert.properties.x509_thumbprint == sha1_of(cert.cer)
    assert cert.policy.issuer_name == "Self"
    op = await async_certs.get_certificate_operation(name)
    assert op.status.lower() == "completed"


async def test_async_get_certificate_and_version_and_policy(async_certs):
    name = unique("cert")
    first = await _create(async_certs, name)
    second = await _create(async_certs, name)
    assert (await async_certs.get_certificate(name)).id == second.id
    assert (await async_certs.get_certificate_version(name, first.properties.version)).cer == first.cer
    assert (await async_certs.get_certificate_policy(name)).subject == "CN=azemu-test"


async def test_async_update_properties_and_policy(async_certs):
    name = unique("cert")
    created = await _create(async_certs, name)
    updated = await async_certs.update_certificate_properties(name, tags={"tag1": "updated_value1"})
    assert updated.properties.tags == {"tag1": "updated_value1"}
    assert updated.id == created.id
    policy = await async_certs.update_certificate_policy(name, ec_policy(validity_in_months=24))
    assert policy.validity_in_months == 24


async def test_async_list_certificates_and_versions_with_paging(async_certs):
    names = {unique("cert") for _ in range(4)}
    for n in names:
        await _create(async_certs, n)
    listed = [p async for p in async_certs.list_properties_of_certificates(max_page_size=3)]
    assert {p.name for p in listed} == names
    pages = [len([i async for i in page]) async for page in async_certs.list_properties_of_certificates(max_page_size=3).by_page()]
    assert pages == [3, 1]
    one = next(iter(names))
    await _create(async_certs, one)
    versions = [p async for p in async_certs.list_properties_of_certificate_versions(one)]
    assert len(versions) == 2


async def test_async_missing_certificate_is_not_found(async_certs):
    with pytest.raises(ResourceNotFoundError) as exc:
        await async_certs.get_certificate(unique("missing"))
    assert error_code(exc.value) == "CertificateNotFound"


async def test_async_invalid_name_is_bad_parameter(async_certs):
    with pytest.raises(HttpResponseError) as exc:
        await async_certs.get_certificate("not_valid")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


async def test_async_import_pkcs12_and_secret_round_trip(async_certs, async_secrets):
    key, cert = make_ec_self_signed()
    name = unique("imp")
    got = await async_certs.import_certificate(name, to_pfx(key, cert, "1234"), password="1234")
    assert got.cer == der_of(cert)
    secret = await async_secrets.get_secret(name)
    private, leaf, _ = load_pfx_secret(secret.value)
    assert private.private_numbers() == key.private_numbers()
    assert public_numbers_equal(private.public_key(), cert.public_key())
    assert secret.properties.managed is True


async def test_async_unknown_issuer_and_merge(async_certs):
    name = unique("ext")
    op = await async_certs.create_certificate(name, unknown_policy(), _polling_interval=POLL)
    assert isinstance(op, CertificateOperation)
    assert op.status.lower() == "inprogress"
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    merged = await async_certs.merge_certificate(name, [base64.b64encode(der_of(signed))])
    assert merged.cer == der_of(signed)
    assert (await async_certs.get_certificate(name)).cer == der_of(signed)


async def test_async_cancel_and_delete_operation(async_certs):
    name = unique("ext")
    await async_certs.create_certificate(name, unknown_policy(), _polling_interval=POLL)
    cancelled = await async_certs.cancel_certificate_operation(name)
    assert cancelled.cancellation_requested is True
    await async_certs.delete_certificate_operation(name)
    with pytest.raises(ResourceNotFoundError) as exc:
        await async_certs.get_certificate_operation(name)
    assert error_code(exc.value) == "PendingCertificateNotFound"


async def test_async_delete_recover_purge(async_certs):
    name = unique("cert")
    created = await _create(async_certs, name)
    deleted = await async_certs.delete_certificate(name, _polling_interval=POLL)
    assert isinstance(deleted, DeletedCertificate)
    assert deleted.recovery_id == f"{async_certs.vault_url}/deletedcertificates/{name}"
    assert [d.name async for d in async_certs.list_deleted_certificates()] == [name]
    recovered = await async_certs.recover_deleted_certificate(name, _polling_interval=POLL)
    assert recovered.id == created.id
    await async_certs.delete_certificate(name, _polling_interval=POLL)
    await async_certs.purge_deleted_certificate(name)
    with pytest.raises(ResourceNotFoundError):
        await async_certs.get_deleted_certificate(name)


async def test_async_backup_and_restore(async_certs):
    name = unique("cert")
    created = await _create(async_certs, name)
    backup = await async_certs.backup_certificate(name)
    with pytest.raises(ResourceExistsError):
        await async_certs.restore_certificate_backup(backup)
    await async_certs.delete_certificate(name, _polling_interval=POLL)
    await async_certs.purge_deleted_certificate(name)
    restored = await async_certs.restore_certificate_backup(backup)
    assert restored.id == created.id
    assert restored.cer == created.cer


async def test_async_issuers_and_contacts(async_certs):
    name = unique("iss")
    issuer = await async_certs.create_issuer(name, "Test", account_id="keyvaultuser")
    assert issuer.account_id == "keyvaultuser"
    assert [i.name async for i in async_certs.list_properties_of_issuers()] == [name]
    updated = await async_certs.update_issuer(name, account_id="other")
    assert updated.account_id == "other"
    await async_certs.delete_issuer(name)
    with pytest.raises(ResourceNotFoundError) as exc:
        await async_certs.get_issuer(name)
    assert error_code(exc.value) == "CertificateIssuerNotFound"

    contacts = [CertificateContact(email="a@example.test", name="A", phone="1")]
    assert [c.email for c in await async_certs.set_contacts(contacts)] == ["a@example.test"]
    assert [c.email for c in await async_certs.get_contacts()] == ["a@example.test"]
    await async_certs.delete_contacts()
    with pytest.raises(ResourceNotFoundError) as exc2:
        await async_certs.get_contacts()
    assert error_code(exc2.value) == "ContactsNotFound"


async def test_async_linked_key_and_signature(async_certs, async_keys, emu):
    import hashlib

    from azure.keyvault.keys.crypto import SignatureAlgorithm
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric import ec

    from kvutil import ecdsa_raw_to_der, int_from_bytes, make_async_server_side_crypto_client

    name = unique("cert")
    cert = await _create(async_certs, name)
    key = await async_keys.get_key(name)
    pub = x509_of(cert).public_key().public_numbers()
    assert int_from_bytes(key.key.x) == pub.x
    assert key.properties.managed is True
    crypto = make_async_server_side_crypto_client(emu, key.id)
    try:
        sig = (await crypto.sign(SignatureAlgorithm.es256, hashlib.sha256(b"m").digest())).signature
    finally:
        await crypto.close()
    x509_of(cert).public_key().verify(ecdsa_raw_to_der(sig), b"m", ec.ECDSA(hashes.SHA256()))
