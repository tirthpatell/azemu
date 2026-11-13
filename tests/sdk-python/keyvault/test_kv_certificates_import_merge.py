"""Certificates: import_certificate and merge_certificate.

Sources:
  * Recorded (test_import_certificate_*, test_list, test_merge_certificate):
      - POST /certificates/{n}/import with {"value": base64 PFX, "pwd": ..., "policy": ...} -> 200
        and a bundle whose attributes nbf / exp are the certificate's notBefore / notAfter, and
        whose policy is derived from the imported certificate: key_props (exportable true, kty,
        key_size, reuse_key false), secret_props contentType application/x-pkcs12, subject from the
        certificate, validity_months from the certificate. Without a policy the issuer is
        "Unknown" with an EmailContacts action at 80 %; with a policy (issuer Self) the issuer
        is the policy's.
      - merge: create with issuer Unknown, read the CSR (CertificateOperation.csr, DER), sign it with
        your own CA, POST .../pending/merge with {"x5c": [...]} -> 201 and a bundle whose cer is the
        merged certificate and whose attributes nbf / exp are the merged certificate's. Microsoft's
        test passes the base64 text of the DER as the bytes of the list item.
  * From Azure docs / assumed (marked): PEM import, EC imports, error answers for garbage,
    wrong password and certificates without private key, merge errors, the secret of an imported
    certificate being a passwordless PFX, merge with a raw DER item.
"""

import base64

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.certificates import CertificateContentType, CertificatePolicy, KeyType, KeyVaultCertificate
from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa

from kvutil import (
    begin_pending,
    der_of,
    error_code,
    generate_ec_private_key,
    load_pem_secret,
    load_pfx_secret,
    make_ca,
    make_ec_self_signed,
    make_self_signed_cert,
    pem_of,
    public_numbers_equal,
    sha1_of,
    sign_csr,
    to_pem_bundle,
    to_pfx,
    unique,
    unknown_policy,
    x509_of,
)

PASSWORD = "1234"


def _pem_policy(**kwargs):
    kwargs.setdefault("subject", "CN=imported")
    return CertificatePolicy("Self", content_type=CertificateContentType.pem, **kwargs)


@pytest.fixture(scope="module")
def rsa_material():
    """One RSA key and self-signed certificate shared by the tests of this module (key generation is slow)."""
    return make_self_signed_cert("azemu-import-rsa")


@pytest.fixture(scope="module")
def ec_material():
    return make_ec_self_signed("P-256", "azemu-import-ec")


# -- import: PKCS12 ----------------------------------------------------------------------
def test_import_pkcs12_without_password_returns_the_certificate(certs, rsa_material):
    key, cert = rsa_material
    name = unique("imp")
    got = certs.import_certificate(name, to_pfx(key, cert))
    assert isinstance(got, KeyVaultCertificate)
    assert got.name == name
    assert got.cer == der_of(cert)
    assert got.policy is not None  # recorded (test_import_certificate_not_password_encoded_no_policy)


def test_import_pkcs12_with_password(certs, rsa_material):
    key, cert = rsa_material
    got = certs.import_certificate(unique("imp"), to_pfx(key, cert, PASSWORD), password=PASSWORD)
    assert got.cer == der_of(cert)
    assert got.policy is not None  # recorded (test_import_certificate_password_encoded_no_policy)


def test_import_returns_ids_thumbprint_and_certificate_dates(certs, rsa_material):
    key, cert = rsa_material
    name = unique("imp")
    got = certs.import_certificate(name, to_pfx(key, cert))
    version = got.properties.version
    assert got.id == f"{certs.vault_url}/certificates/{name}/{version}"
    assert got.key_id == f"{certs.vault_url}/keys/{name}/{version}"
    assert got.secret_id == f"{certs.vault_url}/secrets/{name}/{version}"
    assert got.properties.x509_thumbprint == sha1_of(der_of(cert))
    # Recorded: nbf / exp of the bundle are the imported certificate's validity.
    assert got.properties.not_before == cert.not_valid_before_utc.replace(microsecond=0)
    assert got.properties.expires_on == cert.not_valid_after_utc.replace(microsecond=0)
    assert got.properties.enabled is True


def test_import_without_policy_gets_unknown_issuer_and_pkcs12_policy(certs, rsa_material):
    # Recorded: policy of an import without policy: issuer Unknown, RSA 2048, exportable,
    # not reusable, PKCS12.
    key, cert = rsa_material
    got = certs.import_certificate(unique("imp"), to_pfx(key, cert))
    p = got.policy
    assert p.issuer_name == "Unknown"
    assert p.key_type == KeyType.rsa
    assert p.key_size == 2048
    assert p.exportable is True
    assert p.reuse_key is False
    assert p.content_type == CertificateContentType.pkcs12


def test_import_without_policy_lifetime_action_is_email_contacts_at_80_percent(certs, rsa_material):
    # Recorded (both test_import_certificate_*_no_policy recordings): an imported certificate without
    # a policy gets issuer Unknown, so its lifetime action is EmailContacts at 80 % (not AutoRenew).
    from azure.keyvault.certificates import CertificatePolicyAction

    key, cert = rsa_material
    p = certs.import_certificate(unique("imp"), to_pfx(key, cert)).policy
    assert [(a.action, a.lifetime_percentage) for a in p.lifetime_actions] == [
        (CertificatePolicyAction.email_contacts, 80)
    ]


def test_import_policy_subject_comes_from_the_certificate(certs, rsa_material):
    # Recorded: the policy returned by an import carries the certificate's subject, not the one sent.
    key, cert = rsa_material
    got = certs.import_certificate(
        unique("imp"), to_pfx(key, cert), policy=CertificatePolicy("Self", subject="CN=ignored", content_type="application/x-pkcs12")
    )
    assert got.policy.subject == "CN=azemu-import-rsa"


def test_import_with_policy_uses_the_issuer_of_the_policy(certs, rsa_material):
    # Recorded (test_list): import with an issuer-Self policy -> the policy has issuer Self.
    key, cert = rsa_material
    policy = CertificatePolicy(
        "Self",
        subject="CN=DefaultPolicy",
        exportable=True,
        key_type="RSA",
        key_size=2048,
        reuse_key=False,
        content_type="application/x-pkcs12",
        validity_in_months=12,
        key_usage=["digitalSignature", "keyEncipherment"],
    )
    got = certs.import_certificate(unique("imp"), to_pfx(key, cert, PASSWORD), password=PASSWORD, policy=policy)
    assert got.policy.issuer_name == "Self"
    assert got.policy.content_type == CertificateContentType.pkcs12


def test_import_with_tags_and_disabled(certs, rsa_material):
    key, cert = rsa_material
    name = unique("imp")
    got = certs.import_certificate(name, to_pfx(key, cert), enabled=False, tags={"src": "test"})
    assert got.properties.enabled is False
    assert got.properties.tags == {"src": "test"}
    assert certs.get_certificate(name).properties.tags == {"src": "test"}


def test_imported_certificate_can_be_read_back_and_listed(certs, rsa_material):
    key, cert = rsa_material
    name = unique("imp")
    certs.import_certificate(name, to_pfx(key, cert))
    got = certs.get_certificate(name)
    assert got.cer == der_of(cert)
    assert [p.name for p in certs.list_properties_of_certificates()] == [name]


def test_import_ec_pkcs12(certs, ec_material):
    key, cert = ec_material
    got = certs.import_certificate(unique("imp"), to_pfx(key, cert))
    assert got.cer == der_of(cert)
    assert got.policy.key_type == KeyType.ec


def test_import_chain_pkcs12_certificate_is_the_leaf(certs):
    # From Azure docs: the certificate of the bundle is the leaf; the chain is kept in the secret.
    from cryptography.hazmat.primitives.serialization import pkcs12

    ca_key, ca_cert = make_ca()
    leaf_key = generate_ec_private_key("P-256")
    _, leaf_self = make_self_signed_cert("leaf", leaf_key)
    leaf = _issue(leaf_self, leaf_key, ca_key, ca_cert)
    pfx = pkcs12.serialize_key_and_certificates(b"chain", leaf_key, leaf, [ca_cert], serialization.NoEncryption())
    got = certs.import_certificate(unique("imp"), pfx)
    assert got.cer == der_of(leaf)


def _issue(template_cert, key, ca_key, ca_cert):
    import datetime

    from cryptography.hazmat.primitives import hashes

    now = datetime.datetime.now(datetime.timezone.utc)
    return (
        x509.CertificateBuilder()
        .subject_name(template_cert.subject)
        .issuer_name(ca_cert.subject)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=10))
        .sign(ca_key, hashes.SHA256())
    )


# -- import: PEM ------------------------------------------------------------------------------
def test_import_pem_with_pem_policy(certs, rsa_material):
    # SDK docs: a PEM import needs a policy with content type application/x-pem-file.
    key, cert = rsa_material
    name = unique("imp")
    got = certs.import_certificate(name, to_pem_bundle(key, cert), policy=_pem_policy())
    assert got.cer == der_of(cert)
    assert got.policy.content_type == CertificateContentType.pem
    assert got.properties.x509_thumbprint == sha1_of(der_of(cert))


def test_import_pem_ec(certs, ec_material):
    key, cert = ec_material
    got = certs.import_certificate(unique("imp"), to_pem_bundle(key, cert), policy=_pem_policy())
    assert got.cer == der_of(cert)
    assert got.policy.key_type == KeyType.ec


def test_import_pem_with_certificate_before_the_key(certs, ec_material):
    # From Azure docs: the PEM file holds the key and the certificates; their order is not fixed.
    key, cert = ec_material
    data = pem_of(cert) + key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    )
    got = certs.import_certificate(unique("imp"), data, policy=_pem_policy())
    assert got.cer == der_of(cert)


# -- the secret and key of an imported certificate ---------------------------------------------------
def test_imported_pkcs12_secret_returns_a_usable_private_key(certs, secrets, rsa_material):
    key, cert = rsa_material
    name = unique("imp")
    certs.import_certificate(name, to_pfx(key, cert))
    secret = secrets.get_secret(name)
    got_key, got_cert, _ = load_pfx_secret(secret.value)
    assert isinstance(got_key, rsa.RSAPrivateKey)
    assert public_numbers_equal(got_key.public_key(), key.public_key())
    assert got_key.private_numbers() == key.private_numbers()
    assert der_of(got_cert) == der_of(cert)
    assert secret.properties.content_type == "application/x-pkcs12"
    assert secret.properties.managed is True


def test_imported_password_pkcs12_secret_has_no_password(certs, secrets, ec_material):
    # From Azure docs: Key Vault stores the key material; the secret is a PFX without password.
    key, cert = ec_material
    name = unique("imp")
    certs.import_certificate(name, to_pfx(key, cert, PASSWORD), password=PASSWORD)
    got_key, got_cert, _ = load_pfx_secret(secrets.get_secret(name).value)
    assert isinstance(got_key, ec.EllipticCurvePrivateKey)
    assert got_key.private_numbers() == key.private_numbers()
    assert der_of(got_cert) == der_of(cert)


def test_imported_pem_secret_returns_pem_with_key_and_certificate(certs, secrets, ec_material):
    key, cert = ec_material
    name = unique("imp")
    certs.import_certificate(name, to_pem_bundle(key, cert), policy=_pem_policy())
    secret = secrets.get_secret(name)
    assert secret.properties.content_type == "application/x-pem-file"
    got_key, got_certs = load_pem_secret(secret.value)
    assert got_key.private_numbers() == key.private_numbers()
    assert der_of(got_certs[0]) == der_of(cert)


def test_imported_certificate_key_is_the_public_key_of_the_certificate(certs, keys, rsa_material):
    from kvutil import int_from_bytes

    key, cert = rsa_material
    name = unique("imp")
    imported = certs.import_certificate(name, to_pfx(key, cert))
    jwk = keys.get_key(name)
    pub = cert.public_key().public_numbers()
    assert int_from_bytes(jwk.key.n) == pub.n
    assert int_from_bytes(jwk.key.e) == pub.e
    assert jwk.properties.managed is True
    assert jwk.id == imported.key_id


def test_import_same_name_again_creates_a_new_version(certs, ec_material):
    key, cert = ec_material
    name = unique("imp")
    first = certs.import_certificate(name, to_pfx(key, cert))
    second = certs.import_certificate(name, to_pfx(key, cert))
    assert first.properties.version != second.properties.version
    assert certs.get_certificate(name).properties.version == second.properties.version
    assert certs.get_certificate_version(name, first.properties.version).cer == der_of(cert)


# -- import: errors -------------------------------------------------------------------------------------
def test_import_garbage_bytes_is_bad_request(certs):
    # No recording of a failed import; 400 BadParameter is Key Vault's answer for an unusable certificate (assumed).
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate(unique("imp"), b"this is not a certificate at all")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_import_wrong_password_is_bad_request(certs, rsa_material):
    key, cert = rsa_material
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate(unique("imp"), to_pfx(key, cert, PASSWORD), password="not-the-password")
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_import_encrypted_pkcs12_without_password_is_bad_request(certs, rsa_material):
    key, cert = rsa_material
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate(unique("imp"), to_pfx(key, cert, PASSWORD))
    assert exc.value.status_code == 400


def test_import_certificate_without_private_key_is_bad_request(certs, rsa_material):
    # From Azure docs: the certificate to import must contain the private key.
    _key, cert = rsa_material
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate(unique("imp"), pem_of(cert), policy=_pem_policy())
    assert exc.value.status_code == 400


def test_failed_import_leaves_nothing_behind(certs):
    name = unique("imp")
    with pytest.raises(HttpResponseError):
        certs.import_certificate(name, b"garbage")
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate(name)


def test_import_with_invalid_name_is_bad_parameter(certs, ec_material):
    key, cert = ec_material
    with pytest.raises(HttpResponseError) as exc:
        certs.import_certificate("not_valid", to_pfx(key, cert))
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


# -- merge -----------------------------------------------------------------------------------------------
def _merge_item(cert) -> bytes:
    """The list item Microsoft's test passes: the base64 text of the DER certificate."""
    return base64.b64encode(der_of(cert))


def test_merge_signed_certificate_completes_the_pending_certificate(certs):
    name = unique("ext")
    op = begin_pending(certs, name, unknown_policy("CN=merge-me"))
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    merged = certs.merge_certificate(name, [_merge_item(signed)])
    assert isinstance(merged, KeyVaultCertificate)
    assert merged.name == name
    assert merged.cer == der_of(signed)  # recorded: the bundle's cer is the merged certificate
    assert merged.properties.x509_thumbprint == sha1_of(der_of(signed))
    assert merged.properties.not_before == signed.not_valid_before_utc.replace(microsecond=0)
    assert merged.properties.expires_on == signed.not_valid_after_utc.replace(microsecond=0)
    assert merged.id == f"{certs.vault_url}/certificates/{name}/{merged.properties.version}"
    assert merged.policy.issuer_name == "Unknown"


def test_merged_certificate_can_be_read_with_get_certificate(certs):
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    merged = certs.merge_certificate(name, [_merge_item(signed)])
    got = certs.get_certificate(name)
    assert got.cer == der_of(signed)
    assert got.id == merged.id
    x = x509_of(got)
    assert x.issuer == ca_cert.subject  # signed by the test CA, not self-signed
    x.verify_directly_issued_by(ca_cert)


def test_merged_certificate_operation_is_completed(certs):
    # Assumed (the recordings do not read the operation after the merge): the pending operation completes.
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    certs.merge_certificate(name, [_merge_item(sign_csr(op.csr, ca_key, ca_cert))])
    assert certs.get_certificate_operation(name).status.lower() == "completed"


def test_merged_certificate_secret_holds_the_pending_private_key(certs, secrets):
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    certs.merge_certificate(name, [_merge_item(signed)])
    secret = secrets.get_secret(name)
    key, leaf, _ = load_pfx_secret(secret.value)
    assert der_of(leaf) == der_of(signed)
    assert public_numbers_equal(key.public_key(), signed.public_key())
    assert secret.properties.managed is True


def test_merged_certificate_key_is_the_csr_key(certs, keys):
    from kvutil import int_from_bytes

    name = unique("ext")
    op = begin_pending(certs, name, unknown_policy("CN=ec-merge"))
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    certs.merge_certificate(name, [_merge_item(signed)])
    jwk = keys.get_key(name)
    pub = x509.load_der_x509_csr(op.csr).public_key().public_numbers()
    assert int_from_bytes(jwk.key.x) == pub.x
    assert int_from_bytes(jwk.key.y) == pub.y


def test_merge_with_rsa_pending_key(certs):
    name = unique("ext")
    policy = CertificatePolicy("Unknown", subject="CN=rsa-merge", key_type="RSA", key_size=2048)
    op = begin_pending(certs, name, policy)
    csr = x509.load_der_x509_csr(op.csr)
    assert isinstance(csr.public_key(), rsa.RSAPublicKey)
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    assert certs.merge_certificate(name, [_merge_item(signed)]).cer == der_of(signed)


def test_merge_chain_leaf_first(certs):
    # From Azure docs: x5c is the certificate or the chain, leaf first; the bundle's cer is the leaf.
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    merged = certs.merge_certificate(name, [_merge_item(signed), _merge_item(ca_cert)])
    assert merged.cer == der_of(signed)


def test_merge_with_raw_der_item(certs):
    # Assumed: a raw DER certificate as the list item is accepted as well as its base64 text.
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    signed = sign_csr(op.csr, ca_key, ca_cert)
    assert certs.merge_certificate(name, [der_of(signed)]).cer == der_of(signed)


def test_merge_with_tags(certs):
    name = unique("ext")
    op = begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    merged = certs.merge_certificate(name, [_merge_item(sign_csr(op.csr, ca_key, ca_cert))], tags={"ca": "test"})
    assert merged.properties.tags == {"ca": "test"}


def test_merge_certificate_for_a_different_key_is_rejected(certs):
    # Assumed (no recording): the merged certificate's public key must be the pending key; Key Vault answers 400.
    name = unique("ext")
    begin_pending(certs, name)
    ca_key, ca_cert = make_ca()
    other_key = generate_ec_private_key("P-256")
    _, foreign = make_self_signed_cert("foreign", other_key)
    with pytest.raises(HttpResponseError) as exc:
        certs.merge_certificate(name, [_merge_item(foreign)])
    assert exc.value.status_code == 400
    # nothing was merged: the certificate is still pending (no `cer`)
    assert certs.get_certificate(name).cer is None
    assert certs.get_certificate_operation(name).status.lower() == "inprogress"


def test_merge_garbage_is_rejected(certs):
    name = unique("ext")
    begin_pending(certs, name)
    with pytest.raises(HttpResponseError) as exc:
        certs.merge_certificate(name, [b"not a certificate"])
    assert exc.value.status_code == 400


def test_merge_for_a_name_without_pending_operation_is_an_error(certs):
    ca_key, ca_cert = make_ca()
    _, some = make_self_signed_cert("some", generate_ec_private_key("P-256"))
    with pytest.raises(HttpResponseError) as exc:
        certs.merge_certificate(unique("missing"), [_merge_item(some)])
    assert exc.value.status_code == 404
