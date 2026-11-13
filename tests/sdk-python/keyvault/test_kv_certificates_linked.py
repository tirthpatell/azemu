"""Certificates: the secret and the key that Key Vault creates for every certificate.

Sources:
  * Recorded: secret and key listings contain the certificate's name as an item with
    "managed": true; the secret item has contentType application/x-pkcs12 and nbf / exp
    equal to the certificate's (nbf = created - 600 s); the certificate bundle's sid / kid
    carry the same version as the certificate id; GET /secrets/{name}?outContentType=
    application/x-pem-file exists and answers 400 for a plain secret with
    "Only PFX to PEM transformation is supported".
  * From Azure docs (marked in the tests): the secret value of a PKCS12 certificate is the
    base64 PFX without password, of a PEM certificate the PEM text with the private key and the
    certificate; the key is the public JWK of the certificate key; disabling a certificate
    disables its secret; keys of certificates can sign.
  * Left out: what a non-exportable certificate's secret returns (not in the recordings, and
    the docs are not precise enough to assert on).
"""

import base64
import hashlib
import re

import pytest
from azure.core.exceptions import HttpResponseError
from azure.keyvault.certificates import CertificateContentType, CertificatePolicy, KeyType
from azure.keyvault.keys import KeyType as KvKeyType
from azure.keyvault.keys.crypto import SignatureAlgorithm
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa

from kvutil import (
    create_cert,
    ec_policy,
    ecdsa_raw_to_der,
    error_code,
    int_from_bytes,
    load_pem_secret,
    load_pfx_secret,
    public_numbers_equal,
    unique,
    x509_of,
)


def _rsa_policy(content_type=CertificateContentType.pkcs12, size=2048):
    return CertificatePolicy(
        "Self", subject="CN=linked-rsa", key_type=KeyType.rsa, key_size=size, content_type=content_type
    )


def _ec_policy(content_type=CertificateContentType.pkcs12, curve="P-256"):
    return ec_policy("CN=linked-ec", key_curve_name=curve, content_type=content_type)


# -- the secret: PKCS12 -----------------------------------------------------------
def test_secret_of_pkcs12_certificate_holds_private_key_and_certificate(certs, secrets):
    # From Azure docs: the secret is the base64 PFX (no password) of the certificate.
    name = unique("cert")
    cert = create_cert(certs, name, _rsa_policy())
    secret = secrets.get_secret(name)
    key, leaf, _extra = load_pfx_secret(secret.value)
    assert isinstance(key, rsa.RSAPrivateKey)
    assert leaf.public_bytes(serialization.Encoding.DER) == cert.cer


def test_secret_private_key_matches_certificate_public_key(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _rsa_policy())
    key, _leaf, _ = load_pfx_secret(secrets.get_secret(name).value)
    assert public_numbers_equal(key.public_key(), x509_of(cert).public_key())


def test_secret_of_ec_certificate_has_ec_private_key(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    key, leaf, _ = load_pfx_secret(secrets.get_secret(name).value)
    assert isinstance(key, ec.EllipticCurvePrivateKey)
    assert isinstance(key.curve, ec.SECP256R1)
    assert public_numbers_equal(key.public_key(), x509_of(cert).public_key())


def test_secret_properties_content_type_and_managed(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    secret = secrets.get_secret(name)
    assert secret.properties.content_type == "application/x-pkcs12"  # recorded in secret listings
    assert secret.properties.managed is True  # recorded in secret listings
    assert secret.id == cert.secret_id
    assert secret.name == name


def test_secret_has_certificate_version_and_dates(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    secret = secrets.get_secret(name)
    assert secret.properties.version == cert.properties.version
    assert secret.properties.vault_url == certs.vault_url
    assert secret.properties.enabled is True
    # Recorded in the managed secret items: nbf / exp are the certificate's.
    assert secret.properties.not_before == cert.properties.not_before
    assert secret.properties.expires_on == cert.properties.expires_on


def test_secret_key_id_points_to_the_certificate_key(certs, secrets):
    # From Azure docs: the secret bundle of a certificate carries kid, the key of the certificate.
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    assert secrets.get_secret(name).properties.key_id == cert.key_id


def test_secret_by_version_returns_the_same_value(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    latest = secrets.get_secret(name)
    by_version = secrets.get_secret(name, cert.properties.version)
    assert by_version.value == latest.value
    assert by_version.id == latest.id


def test_secret_value_is_valid_base64(certs, secrets):
    name = unique("cert")
    create_cert(certs, name, _ec_policy())
    value = secrets.get_secret(name).value
    assert re.fullmatch(r"[A-Za-z0-9+/]+={0,2}", value)
    assert base64.b64decode(value, validate=True)


def test_secret_as_pem_via_out_content_type(certs, secrets):
    # Recorded: outContentType=application/x-pem-file is a real query parameter and only
    # PFX -> PEM is supported. The PEM result itself is from Azure docs.
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    secret = secrets.get_secret(name, out_content_type="application/x-pem-file")
    key, leafs = load_pem_secret(secret.value)
    assert leafs[0].public_bytes(serialization.Encoding.DER) == cert.cer
    assert public_numbers_equal(key.public_key(), x509_of(cert).public_key())


# -- the secret: PEM ----------------------------------------------------------------
def test_secret_of_pem_certificate_is_pem_with_key_and_certificate(certs, secrets):
    # From Azure docs: policy content type application/x-pem-file -> PEM text secret.
    name = unique("cert")
    cert = create_cert(certs, name, _rsa_policy(CertificateContentType.pem))
    secret = secrets.get_secret(name)
    assert secret.value.lstrip().startswith("-----BEGIN")
    key, leafs = load_pem_secret(secret.value)
    assert isinstance(key, rsa.RSAPrivateKey)
    assert leafs[0].public_bytes(serialization.Encoding.DER) == cert.cer
    assert public_numbers_equal(key.public_key(), x509_of(cert).public_key())


def test_secret_of_pem_certificate_content_type(certs, secrets):
    name = unique("cert")
    create_cert(certs, name, _ec_policy(CertificateContentType.pem))
    secret = secrets.get_secret(name)
    assert secret.properties.content_type == "application/x-pem-file"
    assert secret.properties.managed is True


def test_secret_of_ec_pem_certificate(certs, secrets):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy(CertificateContentType.pem, "P-384"))
    key, leafs = load_pem_secret(secrets.get_secret(name).value)
    assert isinstance(key, ec.EllipticCurvePrivateKey)
    assert isinstance(key.curve, ec.SECP384R1)
    assert public_numbers_equal(key.public_key(), x509_of(cert).public_key())


# -- the key ------------------------------------------------------------------------
def test_key_of_rsa_certificate_is_the_public_key_of_the_certificate(certs, keys):
    name = unique("cert")
    cert = create_cert(certs, name, _rsa_policy())
    key = keys.get_key(name)
    pub = x509_of(cert).public_key().public_numbers()
    assert key.key_type == KvKeyType.rsa
    assert int_from_bytes(key.key.n) == pub.n
    assert int_from_bytes(key.key.e) == pub.e
    assert key.key.d is None  # only the public part is returned
    assert key.id == cert.key_id
    assert key.name == name


def test_key_of_ec_certificate_is_the_public_key_of_the_certificate(certs, keys):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy(curve="P-384"))
    key = keys.get_key(name)
    pub = x509_of(cert).public_key().public_numbers()
    assert key.key_type == KvKeyType.ec
    assert str(getattr(key.key.crv, "value", key.key.crv)) == "P-384"
    assert int_from_bytes(key.key.x) == pub.x
    assert int_from_bytes(key.key.y) == pub.y
    assert key.key.d is None


def test_key_is_managed_and_has_the_certificate_version(certs, keys):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    key = keys.get_key(name)
    assert key.properties.managed is True  # recorded in key listings
    assert key.properties.version == cert.properties.version
    assert key.properties.vault_url == certs.vault_url
    assert key.properties.enabled is True
    assert key.properties.not_before == cert.properties.not_before  # recorded: nbf / exp as the certificate
    assert key.properties.expires_on == cert.properties.expires_on


# -- signing with the certificate key ---------------------------------------------------
def test_sign_with_rsa_certificate_key_verifies_against_the_certificate(certs, keys, server_crypto):
    # From Azure docs: the key of a certificate can sign (policy key usage digitalSignature).
    name = unique("cert")
    cert = create_cert(certs, name, _rsa_policy())
    crypto = server_crypto(keys.get_key(name).id)
    digest = hashlib.sha256(b"message to sign").digest()
    result = crypto.sign(SignatureAlgorithm.rs256, digest)
    x509_of(cert).public_key().verify(result.signature, b"message to sign", padding.PKCS1v15(), hashes.SHA256())


def test_sign_with_ec_certificate_key_verifies_against_the_certificate(certs, keys, server_crypto):
    name = unique("cert")
    cert = create_cert(certs, name, _ec_policy())
    crypto = server_crypto(keys.get_key(name).id)
    digest = hashlib.sha256(b"message to sign").digest()
    result = crypto.sign(SignatureAlgorithm.es256, digest)
    x509_of(cert).public_key().verify(
        ecdsa_raw_to_der(result.signature), b"message to sign", ec.ECDSA(hashes.SHA256())
    )


def test_certificate_key_in_the_key_vault_matches_private_key_in_the_secret(certs, secrets, keys, server_crypto):
    # A signature made by Key Vault verifies with the public key of the private key in the PFX.
    name = unique("cert")
    create_cert(certs, name, _ec_policy())
    private, _leaf, _ = load_pfx_secret(secrets.get_secret(name).value)
    crypto = server_crypto(keys.get_key(name).id)
    digest = hashlib.sha256(b"abc").digest()
    sig = crypto.sign(SignatureAlgorithm.es256, digest).signature
    private.public_key().verify(ecdsa_raw_to_der(sig), b"abc", ec.ECDSA(hashes.SHA256()))


# -- versions --------------------------------------------------------------------------
def test_new_certificate_version_gets_new_secret_and_key_versions(certs, secrets, keys):
    name = unique("cert")
    first = create_cert(certs, name, _ec_policy())
    second = create_cert(certs, name, _ec_policy())
    assert first.properties.version != second.properties.version
    assert secrets.get_secret(name).properties.version == second.properties.version
    assert keys.get_key(name).properties.version == second.properties.version
    old = secrets.get_secret(name, first.properties.version)
    key, leaf, _ = load_pfx_secret(old.value)
    assert leaf.public_bytes(serialization.Encoding.DER) == first.cer
    assert public_numbers_equal(key.public_key(), x509_of(first).public_key())
    assert keys.get_key(name, first.properties.version).properties.version == first.properties.version


# -- disabled certificate ---------------------------------------------------------------
def test_disabling_the_certificate_disables_the_secret(certs, secrets):
    # From Azure docs: "enabled" says whether the certificate data can be retrieved as a secret.
    # The error is the one recorded for disabled secrets: 403, code "Forbidden",
    # message "Operation get is not allowed on a disabled secret."
    name = unique("cert")
    create_cert(certs, name, _ec_policy())
    certs.update_certificate_properties(name, enabled=False)
    with pytest.raises(HttpResponseError) as exc:
        secrets.get_secret(name)
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"
    assert "disabled secret" in exc.value.message


def test_re_enabling_the_certificate_makes_the_secret_readable_again(certs, secrets):
    name = unique("cert")
    create_cert(certs, name, _ec_policy())
    certs.update_certificate_properties(name, enabled=False)
    certs.update_certificate_properties(name, enabled=True)
    assert secrets.get_secret(name).properties.managed is True
