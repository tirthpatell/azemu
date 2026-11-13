"""Keys: cryptographic operations performed BY THE SERVICE.

CryptographyClient does encrypt / verify / wrap locally whenever it can download the
public key, which would never reach the emulator. kvutil.make_server_side_crypto_client
(the `server_crypto` fixture) builds the client from a key *id string* and sets
`_initialized = True`, which keeps the local provider as NoLocalCryptography
(`supports()` is always False), so every operation is an HTTP call to the vault.

Every result is cross-checked with the `cryptography` package:
  * a signature made by the service must verify locally with the downloaded public key
  * data encrypted / wrapped locally with the public key must decrypt / unwrap on the service
  * a signature verified by the service must be rejected for a tampered digest

Most tests import a locally generated key (fast) instead of asking the emulator for a
fresh RSA key; a few use service-generated keys to prove generation and the
operations agree. Error codes for crypto failures (403 / 400 cases) are not in the
recordings and come from Azure docs: disabled key or operation not in key_ops -> 403
Forbidden; wrong algorithm for the key type or wrong digest size -> 400 BadParameter.
"""

import os

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.keys import JsonWebKey
from azure.keyvault.keys.crypto import (
    EncryptionAlgorithm,
    KeyWrapAlgorithm,
    SignatureAlgorithm,
)
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec, padding, utils

from kvutil import (
    EC_COORD_BYTES,
    digest_of,
    ec_jwk_kwargs,
    ec_public_key_from_jwk,
    ecdsa_der_to_raw,
    ecdsa_raw_to_der,
    error_code,
    generate_ec_private_key,
    generate_rsa_private_key,
    hash_alg,
    rsa_jwk_kwargs,
    rsa_public_key_from_jwk,
    unique,
)

# -- algorithm tables ----------------------------------------------------------
RSA_ENCRYPT = [
    (EncryptionAlgorithm.rsa1_5, lambda: padding.PKCS1v15()),
    (EncryptionAlgorithm.rsa_oaep, lambda: padding.OAEP(padding.MGF1(hashes.SHA1()), hashes.SHA1(), None)),
    (EncryptionAlgorithm.rsa_oaep_256, lambda: padding.OAEP(padding.MGF1(hashes.SHA256()), hashes.SHA256(), None)),
]
RSA_WRAP = [
    (KeyWrapAlgorithm.rsa1_5, lambda: padding.PKCS1v15()),
    (KeyWrapAlgorithm.rsa_oaep, lambda: padding.OAEP(padding.MGF1(hashes.SHA1()), hashes.SHA1(), None)),
    (KeyWrapAlgorithm.rsa_oaep_256, lambda: padding.OAEP(padding.MGF1(hashes.SHA256()), hashes.SHA256(), None)),
]
RSA_SIGN = [
    (SignatureAlgorithm.rs256, 256, "pkcs1"),
    (SignatureAlgorithm.rs384, 384, "pkcs1"),
    (SignatureAlgorithm.rs512, 512, "pkcs1"),
    (SignatureAlgorithm.ps256, 256, "pss"),
    (SignatureAlgorithm.ps384, 384, "pss"),
    (SignatureAlgorithm.ps512, 512, "pss"),
]
EC_SIGN = [
    (SignatureAlgorithm.es256, "P-256", 256),
    (SignatureAlgorithm.es384, "P-384", 384),
    (SignatureAlgorithm.es512, "P-521", 512),
    (SignatureAlgorithm.es256_k, "P-256K", 256),
]


def _ids(table):
    return [str(getattr(row[0], "value", row[0])) for row in table]


def _rsa_pad(kind, bits):
    if kind == "pkcs1":
        return padding.PKCS1v15()
    return padding.PSS(mgf=padding.MGF1(hash_alg(bits)), salt_length=bits // 8)  # salt = digest length


def _rsa_local_sign(private, alg_kind, bits, digest):
    return private.sign(digest, _rsa_pad(alg_kind, bits), utils.Prehashed(hash_alg(bits)))


def _rsa_local_verify(public, alg_kind, bits, digest, signature):
    public.verify(signature, digest, _rsa_pad(alg_kind, bits), utils.Prehashed(hash_alg(bits)))


# -- fixtures ------------------------------------------------------------------
@pytest.fixture(scope="session")
def rsa_private():
    return generate_rsa_private_key(2048)


@pytest.fixture(scope="session")
def ec_privates():
    return {crv: generate_ec_private_key(crv) for crv in EC_COORD_BYTES}


@pytest.fixture
def rsa_key(keys, rsa_private):
    """An RSA key imported into the vault with every operation allowed."""
    ops = ["encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"]
    return keys.import_key(unique("rsa"), JsonWebKey(**rsa_jwk_kwargs(rsa_private, key_ops=ops)))


@pytest.fixture
def ec_key(keys, ec_privates):
    """Factory: ec_key(curve) -> KeyVaultKey imported into the vault."""

    def factory(curve):
        return keys.import_key(
            unique("ec"), JsonWebKey(**ec_jwk_kwargs(ec_privates[curve], curve, key_ops=["sign", "verify"]))
        )

    return factory


# ==============================================================================
# RSA encrypt / decrypt
# ==============================================================================
@pytest.mark.parametrize("alg,_pad", RSA_ENCRYPT, ids=_ids(RSA_ENCRYPT))
def test_rsa_encrypt_then_decrypt_on_the_service(server_crypto, rsa_key, alg, _pad):
    client = server_crypto(rsa_key.id)
    plaintext = os.urandom(32)
    enc = client.encrypt(alg, plaintext)
    assert enc.key_id == rsa_key.id
    assert enc.algorithm == alg
    assert len(enc.ciphertext) == 256
    assert enc.ciphertext != plaintext
    dec = client.decrypt(alg, enc.ciphertext)
    assert dec.plaintext == plaintext
    assert dec.key_id == rsa_key.id


@pytest.mark.parametrize("alg,pad", RSA_ENCRYPT, ids=_ids(RSA_ENCRYPT))
def test_rsa_service_ciphertext_decrypts_with_local_private_key(server_crypto, rsa_key, rsa_private, alg, pad):
    plaintext = os.urandom(32)
    ciphertext = server_crypto(rsa_key.id).encrypt(alg, plaintext).ciphertext
    assert rsa_private.decrypt(ciphertext, pad()) == plaintext


@pytest.mark.parametrize("alg,pad", RSA_ENCRYPT, ids=_ids(RSA_ENCRYPT))
def test_rsa_locally_encrypted_data_decrypts_on_the_service(server_crypto, keys, rsa_key, alg, pad):
    plaintext = os.urandom(32)
    public = rsa_public_key_from_jwk(keys.get_key(rsa_key.name).key)  # the downloaded public key
    ciphertext = public.encrypt(plaintext, pad())
    assert server_crypto(rsa_key.id).decrypt(alg, ciphertext).plaintext == plaintext


def test_rsa_encrypt_is_randomised(server_crypto, rsa_key):
    client = server_crypto(rsa_key.id)
    alg = EncryptionAlgorithm.rsa_oaep_256
    assert client.encrypt(alg, b"same").ciphertext != client.encrypt(alg, b"same").ciphertext


def test_rsa_decrypt_of_garbage_is_a_client_error(server_crypto, rsa_key):
    # Real Azure rejects ciphertext that does not decrypt with 400; code not recorded.
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).decrypt(EncryptionAlgorithm.rsa_oaep, os.urandom(256))
    assert 400 <= exc.value.status_code < 500


# ==============================================================================
# RSA wrap / unwrap
# ==============================================================================
@pytest.mark.parametrize("alg,_pad", RSA_WRAP, ids=_ids(RSA_WRAP))
def test_rsa_wrap_then_unwrap_on_the_service(server_crypto, rsa_key, alg, _pad):
    client = server_crypto(rsa_key.id)
    cek = os.urandom(32)
    wrapped = client.wrap_key(alg, cek)
    assert wrapped.key_id == rsa_key.id
    assert wrapped.algorithm == alg
    assert len(wrapped.encrypted_key) == 256
    unwrapped = client.unwrap_key(alg, wrapped.encrypted_key)
    assert unwrapped.key == cek
    assert unwrapped.key_id == rsa_key.id


@pytest.mark.parametrize("alg,pad", RSA_WRAP, ids=_ids(RSA_WRAP))
def test_rsa_service_wrapped_key_unwraps_with_local_private_key(server_crypto, rsa_key, rsa_private, alg, pad):
    cek = os.urandom(32)
    wrapped = server_crypto(rsa_key.id).wrap_key(alg, cek).encrypted_key
    assert rsa_private.decrypt(wrapped, pad()) == cek


@pytest.mark.parametrize("alg,pad", RSA_WRAP, ids=_ids(RSA_WRAP))
def test_rsa_locally_wrapped_key_unwraps_on_the_service(server_crypto, keys, rsa_key, alg, pad):
    cek = os.urandom(32)
    public = rsa_public_key_from_jwk(keys.get_key(rsa_key.name).key)
    wrapped = public.encrypt(cek, pad())
    assert server_crypto(rsa_key.id).unwrap_key(alg, wrapped).key == cek


# ==============================================================================
# RSA sign / verify
# ==============================================================================
@pytest.mark.parametrize("alg,bits,kind", RSA_SIGN, ids=_ids(RSA_SIGN))
def test_rsa_sign_then_verify_on_the_service(server_crypto, rsa_key, alg, bits, kind):
    client = server_crypto(rsa_key.id)
    digest = digest_of(b"message to sign", bits)
    signed = client.sign(alg, digest)
    assert signed.key_id == rsa_key.id
    assert signed.algorithm == alg
    assert len(signed.signature) == 256
    verified = client.verify(alg, digest, signed.signature)
    assert verified.is_valid is True
    assert verified.key_id == rsa_key.id


@pytest.mark.parametrize("alg,bits,kind", RSA_SIGN, ids=_ids(RSA_SIGN))
def test_rsa_service_signature_verifies_with_downloaded_public_key(server_crypto, keys, rsa_key, alg, bits, kind):
    digest = digest_of(b"message to sign", bits)
    signature = server_crypto(rsa_key.id).sign(alg, digest).signature
    public = rsa_public_key_from_jwk(keys.get_key(rsa_key.name).key)
    _rsa_local_verify(public, kind, bits, digest, signature)  # raises InvalidSignature on failure


@pytest.mark.parametrize("alg,bits,kind", RSA_SIGN, ids=_ids(RSA_SIGN))
def test_rsa_locally_made_signature_verifies_on_the_service(server_crypto, rsa_key, rsa_private, alg, bits, kind):
    digest = digest_of(b"message to sign", bits)
    signature = _rsa_local_sign(rsa_private, kind, bits, digest)
    assert server_crypto(rsa_key.id).verify(alg, digest, signature).is_valid is True


@pytest.mark.parametrize("alg,bits,kind", RSA_SIGN, ids=_ids(RSA_SIGN))
def test_rsa_service_verify_rejects_a_tampered_digest(server_crypto, rsa_key, alg, bits, kind):
    client = server_crypto(rsa_key.id)
    digest = digest_of(b"message to sign", bits)
    signature = client.sign(alg, digest).signature
    other = digest_of(b"a different message", bits)
    assert client.verify(alg, other, signature).is_valid is False


def test_rsa_service_verify_rejects_a_tampered_signature(server_crypto, rsa_key):
    client = server_crypto(rsa_key.id)
    alg = SignatureAlgorithm.rs256
    digest = digest_of(b"message", 256)
    signature = bytearray(client.sign(alg, digest).signature)
    signature[10] ^= 0xFF
    assert client.verify(alg, digest, bytes(signature)).is_valid is False


def test_rsa_pkcs1_signature_is_deterministic_and_pss_is_not(server_crypto, rsa_key):
    client = server_crypto(rsa_key.id)
    d = digest_of(b"m", 256)
    assert client.sign(SignatureAlgorithm.rs256, d).signature == client.sign(SignatureAlgorithm.rs256, d).signature
    assert client.sign(SignatureAlgorithm.ps256, d).signature != client.sign(SignatureAlgorithm.ps256, d).signature


def test_rsa_signature_of_one_algorithm_does_not_verify_as_another(server_crypto, rsa_key):
    client = server_crypto(rsa_key.id)
    digest = digest_of(b"m", 256)
    signature = client.sign(SignatureAlgorithm.rs256, digest).signature
    assert client.verify(SignatureAlgorithm.ps256, digest, signature).is_valid is False


# ==============================================================================
# EC sign / verify
# ==============================================================================
@pytest.mark.parametrize("alg,curve,bits", EC_SIGN, ids=_ids(EC_SIGN))
def test_ec_sign_then_verify_on_the_service(server_crypto, ec_key, alg, curve, bits):
    key = ec_key(curve)
    client = server_crypto(key.id)
    digest = digest_of(b"ec message", bits)
    signed = client.sign(alg, digest)
    assert signed.key_id == key.id
    assert signed.algorithm == alg
    assert len(signed.signature) == 2 * EC_COORD_BYTES[curve]  # raw r||s, not DER
    assert client.verify(alg, digest, signed.signature).is_valid is True


@pytest.mark.parametrize("alg,curve,bits", EC_SIGN, ids=_ids(EC_SIGN))
def test_ec_service_signature_verifies_with_downloaded_public_key(server_crypto, keys, ec_key, alg, curve, bits):
    key = ec_key(curve)
    digest = digest_of(b"ec message", bits)
    signature = server_crypto(key.id).sign(alg, digest).signature
    public = ec_public_key_from_jwk(keys.get_key(key.name).key)
    public.verify(ecdsa_raw_to_der(signature), digest, ec.ECDSA(utils.Prehashed(hash_alg(bits))))


@pytest.mark.parametrize("alg,curve,bits", EC_SIGN, ids=_ids(EC_SIGN))
def test_ec_locally_made_signature_verifies_on_the_service(server_crypto, ec_key, ec_privates, alg, curve, bits):
    key = ec_key(curve)
    digest = digest_of(b"ec message", bits)
    der = ec_privates[curve].sign(digest, ec.ECDSA(utils.Prehashed(hash_alg(bits))))
    raw = ecdsa_der_to_raw(der, EC_COORD_BYTES[curve])
    assert server_crypto(key.id).verify(alg, digest, raw).is_valid is True


@pytest.mark.parametrize("alg,curve,bits", EC_SIGN, ids=_ids(EC_SIGN))
def test_ec_service_verify_rejects_a_tampered_digest(server_crypto, ec_key, alg, curve, bits):
    key = ec_key(curve)
    client = server_crypto(key.id)
    signature = client.sign(alg, digest_of(b"ec message", bits)).signature
    assert client.verify(alg, digest_of(b"tampered", bits), signature).is_valid is False


# ==============================================================================
# Service-generated keys (not imported)
# ==============================================================================
def test_service_generated_rsa_key_signs_and_decrypts_consistently_with_its_public_key(keys, server_crypto):
    key = keys.create_rsa_key(unique("gen"), size=2048)
    client = server_crypto(key.id)
    public = rsa_public_key_from_jwk(keys.get_key(key.name).key)

    digest = digest_of(b"generated", 256)
    sig = client.sign(SignatureAlgorithm.rs256, digest).signature
    _rsa_local_verify(public, "pkcs1", 256, digest, sig)

    plaintext = os.urandom(32)
    ciphertext = public.encrypt(plaintext, RSA_ENCRYPT[2][1]())
    assert client.decrypt(EncryptionAlgorithm.rsa_oaep_256, ciphertext).plaintext == plaintext


@pytest.mark.parametrize("alg,curve,bits", EC_SIGN, ids=_ids(EC_SIGN))
def test_service_generated_ec_key_signs_with_its_published_public_key(keys, server_crypto, alg, curve, bits):
    key = keys.create_ec_key(unique("gen"), curve=curve)
    digest = digest_of(b"generated", bits)
    signature = server_crypto(key.id).sign(alg, digest).signature
    ec_public_key_from_jwk(key.key).verify(
        ecdsa_raw_to_der(signature), digest, ec.ECDSA(utils.Prehashed(hash_alg(bits)))
    )


def test_each_version_of_a_key_signs_with_its_own_private_key(keys, server_crypto):
    name = unique("gen")
    v1 = keys.create_ec_key(name, curve="P-256")
    v2 = keys.create_ec_key(name, curve="P-256")
    digest = digest_of(b"versions", 256)
    sig1 = server_crypto(v1.id).sign(SignatureAlgorithm.es256, digest).signature
    sig2 = server_crypto(v2.id).sign(SignatureAlgorithm.es256, digest).signature
    pub1, pub2 = ec_public_key_from_jwk(v1.key), ec_public_key_from_jwk(v2.key)
    alg = ec.ECDSA(utils.Prehashed(hashes.SHA256()))
    pub1.verify(ecdsa_raw_to_der(sig1), digest, alg)
    pub2.verify(ecdsa_raw_to_der(sig2), digest, alg)
    from cryptography.exceptions import InvalidSignature

    with pytest.raises(InvalidSignature):
        pub1.verify(ecdsa_raw_to_der(sig2), digest, alg)


# ==============================================================================
# Errors
# ==============================================================================
def test_crypto_on_missing_key_is_key_not_found(emu, server_crypto):
    from kvutil import vault_url

    key_id = f"{vault_url(emu)}/keys/{unique('missing')}/{'0' * 32}"
    with pytest.raises(ResourceNotFoundError) as exc:
        server_crypto(key_id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert exc.value.status_code == 404
    assert error_code(exc.value) == "KeyNotFound"


def test_crypto_on_missing_key_version_is_key_not_found(keys, server_crypto):
    key = keys.create_ec_key(unique("k"))
    bad = f"{keys.vault_url}/keys/{key.name}/{'0' * 32}"
    with pytest.raises(ResourceNotFoundError) as exc:
        server_crypto(bad).sign(SignatureAlgorithm.es256, digest_of(b"x", 256))
    assert error_code(exc.value) == "KeyNotFound"


def test_ec_algorithm_on_rsa_key_is_bad_parameter(server_crypto, rsa_key):
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).sign(SignatureAlgorithm.es256, digest_of(b"x", 256))
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_rsa_algorithm_on_ec_key_is_bad_parameter(server_crypto, ec_key):
    key = ec_key("P-256")
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(key.id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert exc.value.status_code == 400
    assert error_code(exc.value) == "BadParameter"


def test_digest_of_the_wrong_length_for_the_algorithm_is_rejected(server_crypto, rsa_key):
    # Assumed: RS256 needs a 32-byte digest; the service answers 400 (code not recorded).
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).sign(SignatureAlgorithm.rs256, os.urandom(20))
    assert exc.value.status_code == 400


def test_sign_with_disabled_key_is_forbidden(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, enabled=False)
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"


def test_encrypt_with_disabled_key_is_forbidden(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, enabled=False)
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).encrypt(EncryptionAlgorithm.rsa_oaep, b"x")
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"


def test_operation_works_again_after_the_key_is_re_enabled(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, enabled=False)
    keys.update_key_properties(rsa_key.name, enabled=True)
    result = server_crypto(rsa_key.id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert len(result.signature) == 256


def test_encrypt_when_key_ops_do_not_include_encrypt_is_forbidden(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, key_operations=["sign", "verify"])
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).encrypt(EncryptionAlgorithm.rsa_oaep, b"x")
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"


def test_sign_when_key_ops_do_not_include_sign_is_forbidden(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, key_operations=["encrypt", "decrypt"])
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"


def test_wrap_when_key_ops_do_not_include_wrap_key_is_forbidden(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, key_operations=["sign", "verify"])
    with pytest.raises(HttpResponseError) as exc:
        server_crypto(rsa_key.id).wrap_key(KeyWrapAlgorithm.rsa_oaep, os.urandom(32))
    assert exc.value.status_code == 403
    assert error_code(exc.value) == "Forbidden"


def test_permitted_operation_still_works_when_key_ops_are_limited(keys, server_crypto, rsa_key):
    keys.update_key_properties(rsa_key.name, key_operations=["sign", "verify"])
    client = server_crypto(rsa_key.id)
    digest = digest_of(b"x", 256)
    sig = client.sign(SignatureAlgorithm.rs256, digest).signature
    assert client.verify(SignatureAlgorithm.rs256, digest, sig).is_valid is True


def test_crypto_after_key_is_deleted_is_key_not_found(keys, server_crypto, rsa_key):
    from kvutil import delete_and_wait

    delete_and_wait(keys, rsa_key.name)
    with pytest.raises(ResourceNotFoundError) as exc:
        server_crypto(rsa_key.id).sign(SignatureAlgorithm.rs256, digest_of(b"x", 256))
    assert error_code(exc.value) == "KeyNotFound"


def test_server_crypto_client_really_sends_requests_to_the_service(emu, keys, rsa_key):
    # Guard for the whole file: prove the helper keeps the work on the server by checking
    # that the client's local provider cannot handle any operation.
    from kvutil import make_server_side_crypto_client
    from azure.keyvault.keys import KeyOperation

    client = make_server_side_crypto_client(emu, rsa_key.id)
    try:
        for op, alg in [
            (KeyOperation.encrypt, EncryptionAlgorithm.rsa_oaep),
            (KeyOperation.verify, SignatureAlgorithm.rs256),
            (KeyOperation.wrap_key, KeyWrapAlgorithm.rsa_oaep),
        ]:
            assert client._local_provider.supports(op, alg) is False
        assert client._initialized is True
        assert client._key is None  # the public key was never downloaded
    finally:
        client.close()
