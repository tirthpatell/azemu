"""Two requests that only the Java SDK sends.

The Java `createRsaKey` always sends `"public_exponent": 0` when the option is not set; the service must treat 0
as "not given" (default 65537) and answer 200.
The Java `importKey(name, JsonWebKey)` sends `"key_ops": []` for a key that has no operations; such a key must
get the default operations of its type (otherwise decrypt answers 403 at once). The recordings of the Python SDK
always send explicit operations, so there is no recording of Azure for an empty list; the rule is the one the Java
SDK's own live tests depend on (they import a key without operations and use it).
"""

import pytest
from azure.core.exceptions import HttpResponseError
from azure.keyvault.keys import JsonWebKey, KeyType
from azure.keyvault.keys.crypto import EncryptionAlgorithm
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding

from kvutil import error_code, generate_rsa_private_key, int_from_bytes, rsa_jwk_kwargs, unique

ALL_RSA_OPS = {"encrypt", "decrypt", "sign", "verify", "wrapKey", "unwrapKey"}


def _ops(key):
    return {str(getattr(o, "value", o)) for o in key.key_operations}


def test_create_rsa_key_with_public_exponent_zero_uses_the_default(keys):
    key = keys.create_rsa_key(unique("k1"), size=2048, public_exponent=0)
    assert key.key_type == KeyType.rsa
    assert int_from_bytes(key.key.e) == 65537


@pytest.mark.parametrize("exponent", [None, 65537])
def test_create_rsa_key_with_default_or_explicit_exponent(keys, exponent):
    key = keys.create_rsa_key(unique("k1"), size=2048, public_exponent=exponent)
    assert int_from_bytes(key.key.e) == 65537


def test_create_rsa_key_with_an_even_exponent_is_still_refused(keys):
    with pytest.raises(HttpResponseError) as e:
        keys.create_rsa_key(unique("k1"), size=2048, public_exponent=4)
    assert e.value.status_code == 400 and error_code(e.value) == "BadParameter"


def test_import_key_with_empty_key_ops_gets_the_default_operations(keys):
    name = unique("k2")
    jwk = JsonWebKey(**rsa_jwk_kwargs(generate_rsa_private_key(2048), key_ops=[]))
    imported = keys.import_key(name, jwk)
    assert _ops(imported) == ALL_RSA_OPS
    assert _ops(keys.get_key(name)) == ALL_RSA_OPS


def test_import_key_with_empty_key_ops_can_decrypt_on_the_service(keys, server_crypto):
    name = unique("k2")
    private = generate_rsa_private_key(2048)
    imported = keys.import_key(name, JsonWebKey(**rsa_jwk_kwargs(private, key_ops=[])))
    cipher = private.public_key().encrypt(
        b"hello java", padding.OAEP(padding.MGF1(hashes.SHA256()), hashes.SHA256(), None)
    )
    out = server_crypto(imported.id).decrypt(EncryptionAlgorithm.rsa_oaep_256, cipher)
    assert out.plaintext == b"hello java"


def test_import_key_with_explicit_key_ops_keeps_only_those(keys):
    name = unique("k2")
    jwk = JsonWebKey(**rsa_jwk_kwargs(generate_rsa_private_key(2048), key_ops=["sign", "verify"]))
    assert _ops(keys.import_key(name, jwk)) == {"sign", "verify"}


def test_create_key_with_empty_key_ops_gets_the_default_operations(keys):
    key = keys.create_rsa_key(unique("k2"), size=2048, key_operations=[])
    assert _ops(key) == ALL_RSA_OPS
