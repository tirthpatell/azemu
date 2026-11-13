"""Helpers shared by the Key Vault SDK conformance tests.

Everything the tests need to talk to the emulator the way a real application
talks to Azure Key Vault lives here: a fake credential (the emulator accepts any
bearer token), client factories with the right constructor arguments, polling
helpers that do not sleep for seconds, and a few crypto helpers.
"""

import base64
import time
import uuid

from azure.core.credentials import AccessToken, AccessTokenInfo
from azure.core.exceptions import HttpResponseError

DEFAULT_VAULT = "azemukv"
TOKEN = "azemu-test-token"

# The emulator should finish soft-delete / recover / create operations at once,
# so every poller in the suite is told to poll every 100 ms.
POLL = 0.1


# -- credentials ------------------------------------------------------------
class FakeCredential:
    """Sync credential. The emulator accepts any bearer token."""

    def get_token(self, *scopes, **kwargs):
        return AccessToken(TOKEN, int(time.time()) + 3600)

    def get_token_info(self, *scopes, options=None):
        return AccessTokenInfo(TOKEN, int(time.time()) + 3600)


class AsyncFakeCredential:
    """Async twin of FakeCredential for the azure.keyvault.*.aio clients."""

    async def get_token(self, *scopes, **kwargs):
        return AccessToken(TOKEN, int(time.time()) + 3600)

    async def get_token_info(self, *scopes, options=None):
        return AccessTokenInfo(TOKEN, int(time.time()) + 3600)

    async def close(self):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        pass


# -- names and urls ---------------------------------------------------------
def unique(prefix: str = "t") -> str:
    """A fresh object name. Key Vault names allow only 0-9, a-z, A-Z and '-'."""
    return f"{prefix}-{uuid.uuid4().hex[:12]}"


def vault_url(emu, vault: str = DEFAULT_VAULT) -> str:
    return emu.https_url("vault", vault)


def client_kwargs(emu) -> dict:
    """Keyword arguments every Key Vault client needs to reach the emulator.

    Only connection_verify: trust the emulator's local CA. The SDK's check of the challenge resource stays on (its
    default): for a host like myvault.vault.localhost:4577 the emulator's challenge names vault.localhost:4577, which
    passes it. `test_challenge_resource.py` has the test that shows the old switch still works.
    """
    kwargs = {}
    if emu.ca_file:
        kwargs["connection_verify"] = emu.ca_file
    return kwargs


def make_secret_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.secrets import SecretClient

    return SecretClient(vault_url(emu, vault), FakeCredential(), **client_kwargs(emu))


def make_key_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.keys import KeyClient

    return KeyClient(vault_url(emu, vault), FakeCredential(), **client_kwargs(emu))


def make_cert_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.certificates import CertificateClient

    return CertificateClient(vault_url(emu, vault), FakeCredential(), **client_kwargs(emu))


def make_async_secret_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.secrets.aio import SecretClient

    return SecretClient(vault_url(emu, vault), AsyncFakeCredential(), **client_kwargs(emu))


def make_async_key_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.keys.aio import KeyClient

    return KeyClient(vault_url(emu, vault), AsyncFakeCredential(), **client_kwargs(emu))


def make_async_cert_client(emu, vault: str = DEFAULT_VAULT):
    from azure.keyvault.certificates.aio import CertificateClient

    return CertificateClient(vault_url(emu, vault), AsyncFakeCredential(), **client_kwargs(emu))


def make_server_side_crypto_client(emu, key_id: str):
    """A CryptographyClient that always sends the operation to the service.

    CryptographyClient normally downloads the public key and does encrypt /
    verify / wrap locally. That would not test the emulator at all. The client
    only goes to the service when its local provider says it does not support
    the operation. Passing the key as an *id string* (not a KeyVaultKey) leaves
    the local provider as NoLocalCryptography, whose supports() is always False.
    Setting _initialized = True stops the client from fetching the key and
    replacing that provider with a real local one on first use.

    key_id must include the version: https://<vault>/keys/<name>/<version>.
    """
    from azure.keyvault.keys.crypto import CryptographyClient

    client = CryptographyClient(key_id, FakeCredential(), **client_kwargs(emu))
    client._initialized = True  # noqa: SLF001 - see docstring
    return client


def make_async_server_side_crypto_client(emu, key_id: str):
    from azure.keyvault.keys.crypto.aio import CryptographyClient

    client = CryptographyClient(key_id, AsyncFakeCredential(), **client_kwargs(emu))
    client._initialized = True  # noqa: SLF001
    return client


# -- polling helpers --------------------------------------------------------
def wait_poller(poller):
    """Wait for a delete/recover poller to finish and return its resource."""
    poller.wait()
    return poller.result()


def delete_and_wait(client, name):
    """begin_delete_* + wait, for secrets, keys and certificates alike."""
    if hasattr(client, "begin_delete_secret"):
        poller = client.begin_delete_secret(name, _polling_interval=POLL)
    elif hasattr(client, "begin_delete_key"):
        poller = client.begin_delete_key(name, _polling_interval=POLL)
    else:
        poller = client.begin_delete_certificate(name, _polling_interval=POLL)
    return wait_poller(poller)


def recover_and_wait(client, name):
    if hasattr(client, "begin_recover_deleted_secret"):
        poller = client.begin_recover_deleted_secret(name, _polling_interval=POLL)
    elif hasattr(client, "begin_recover_deleted_key"):
        poller = client.begin_recover_deleted_key(name, _polling_interval=POLL)
    else:
        poller = client.begin_recover_deleted_certificate(name, _polling_interval=POLL)
    return wait_poller(poller)


def purge(client, name):
    if hasattr(client, "purge_deleted_secret"):
        client.purge_deleted_secret(name)
    elif hasattr(client, "purge_deleted_key"):
        client.purge_deleted_key(name)
    else:
        client.purge_deleted_certificate(name)


def retry_on_conflict(fn, tries: int = 40, delay: float = 0.25):
    """Call fn() again while it answers 409 Conflict.

    Real Azure purges asynchronously: for a short time after a purge the name is
    still reported as "being deleted" (409 Conflict, innererror
    ObjectIsBeingDeleted) and cannot be reused. The emulator may finish at once,
    so on the emulator this loop normally runs fn() exactly once.
    """
    last = None
    for _ in range(tries):
        try:
            return fn()
        except HttpResponseError as e:
            if e.status_code != 409:
                raise
            last = e
            time.sleep(delay)
    raise last


# -- small encoding helpers -------------------------------------------------
def b64u(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def b64u_decode(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def int_from_bytes(data: bytes) -> int:
    return int.from_bytes(data, "big")


def error_code(exc) -> str | None:
    """The Key Vault error code (error.code in the JSON body) of an SDK exception."""
    err = getattr(exc, "error", None)
    return getattr(err, "code", None)


# -- local key material (cryptography package) -------------------------------
# Used to import keys, to verify what the service produced and to produce data
# the service must accept. Generating keys locally is much faster than asking
# the emulator for a fresh RSA key in every test.
def _be(n: int, length: int | None = None) -> bytes:
    length = length or max(1, (n.bit_length() + 7) // 8)
    return n.to_bytes(length, "big")


def generate_rsa_private_key(bits: int = 2048):
    from cryptography.hazmat.primitives.asymmetric import rsa

    return rsa.generate_private_key(public_exponent=65537, key_size=bits)


EC_CURVES = {
    "P-256": "SECP256R1",
    "P-384": "SECP384R1",
    "P-521": "SECP521R1",
    "P-256K": "SECP256K1",
}
EC_COORD_BYTES = {"P-256": 32, "P-384": 48, "P-521": 66, "P-256K": 32}


def generate_ec_private_key(crv: str):
    from cryptography.hazmat.primitives.asymmetric import ec

    return ec.generate_private_key(getattr(ec, EC_CURVES[crv])())


def rsa_jwk_kwargs(private_key, key_ops=None) -> dict:
    """Keyword arguments for azure.keyvault.keys.JsonWebKey from an RSA private key."""
    nums = private_key.private_numbers()
    pub = nums.public_numbers
    return {
        "kty": "RSA",
        "key_ops": key_ops,
        "n": _be(pub.n),
        "e": _be(pub.e),
        "d": _be(nums.d),
        "p": _be(nums.p),
        "q": _be(nums.q),
        "dp": _be(nums.dmp1),
        "dq": _be(nums.dmq1),
        "qi": _be(nums.iqmp),
    }


def ec_jwk_kwargs(private_key, crv: str, key_ops=None) -> dict:
    nums = private_key.private_numbers()
    pub = nums.public_numbers
    size = EC_COORD_BYTES[crv]
    return {
        "kty": "EC",
        "crv": crv,
        "key_ops": key_ops,
        "x": _be(pub.x, size),
        "y": _be(pub.y, size),
        "d": _be(nums.private_value, size),
    }


def rsa_public_key_from_jwk(jwk):
    """cryptography RSAPublicKey from the n/e of a downloaded JsonWebKey."""
    from cryptography.hazmat.primitives.asymmetric import rsa

    return rsa.RSAPublicNumbers(int_from_bytes(jwk.e), int_from_bytes(jwk.n)).public_key()


def ec_public_key_from_jwk(jwk):
    from cryptography.hazmat.primitives.asymmetric import ec

    curve = getattr(ec, EC_CURVES[str(getattr(jwk.crv, "value", jwk.crv))])()
    return ec.EllipticCurvePublicNumbers(int_from_bytes(jwk.x), int_from_bytes(jwk.y), curve).public_key()


def ecdsa_raw_to_der(signature: bytes) -> bytes:
    """Key Vault returns ECDSA signatures as r||s (RFC 7518); cryptography wants DER."""
    from cryptography.hazmat.primitives.asymmetric import utils

    half = len(signature) // 2
    return utils.encode_dss_signature(int_from_bytes(signature[:half]), int_from_bytes(signature[half:]))


def ecdsa_der_to_raw(signature: bytes, coord_bytes: int) -> bytes:
    from cryptography.hazmat.primitives.asymmetric import utils

    r, s = utils.decode_dss_signature(signature)
    return _be(r, coord_bytes) + _be(s, coord_bytes)


def hash_alg(bits: int):
    from cryptography.hazmat.primitives import hashes

    return {256: hashes.SHA256, 384: hashes.SHA384, 512: hashes.SHA512}[bits]()


def digest_of(data: bytes, bits: int) -> bytes:
    from cryptography.hazmat.primitives import hashes

    h = hashes.Hash(hash_alg(bits))
    h.update(data)
    return h.finalize()


# -- certificates ------------------------------------------------------------
def create_cert(certs, name, policy=None, **kwargs):
    """begin_create_certificate + wait for the result (a KeyVaultCertificate)."""
    from azure.keyvault.certificates import CertificatePolicy

    poller = certs.begin_create_certificate(
        name, policy or CertificatePolicy.get_default(), _polling_interval=POLL, **kwargs
    )
    return poller.result()


def ec_policy(subject="CN=azemu-test", **kwargs):
    """A fast self-signed policy: P-256 EC key (no slow RSA key generation)."""
    from azure.keyvault.certificates import CertificatePolicy

    kwargs.setdefault("key_type", "EC")
    kwargs.setdefault("key_curve_name", "P-256")
    return CertificatePolicy("Self", subject=subject, **kwargs)


def make_self_signed_cert(common_name="azemu-import", key=None, days=365):
    """(private_key, x509.Certificate) - a self-signed certificate made locally."""
    import datetime

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes
    from cryptography.x509.oid import NameOID

    key = key or generate_rsa_private_key(2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=days))
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .sign(key, hashes.SHA256())
    )
    return key, cert


def to_pfx(key, cert, password: str | None = None) -> bytes:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.serialization import pkcs12

    enc = (
        serialization.BestAvailableEncryption(password.encode())
        if password
        else serialization.NoEncryption()
    )
    return pkcs12.serialize_key_and_certificates(b"azemu", key, cert, None, enc)


def to_pem_bundle(key, cert) -> bytes:
    """PEM text with the PKCS#8 private key followed by the certificate."""
    from cryptography.hazmat.primitives import serialization

    return key.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption(),
    ) + cert.public_bytes(serialization.Encoding.PEM)


def load_pfx_secret(value: str, password: str | None = None):
    """Parse a certificate-backed secret value (base64 PKCS#12) -> (key, cert, extra_certs)."""
    from cryptography.hazmat.primitives.serialization import pkcs12

    return pkcs12.load_key_and_certificates(base64.b64decode(value), password.encode() if password else None)


def load_pem_secret(value: str):
    """Parse a certificate-backed PEM secret value -> (private_key, [certs])."""
    import re as _re

    from cryptography import x509
    from cryptography.hazmat.primitives import serialization

    data = value.encode()
    key_blocks = _re.findall(rb"-----BEGIN (?:[A-Z ]*)PRIVATE KEY-----.*?-----END (?:[A-Z ]*)PRIVATE KEY-----", data, _re.S)
    cert_blocks = _re.findall(rb"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", data, _re.S)
    assert len(key_blocks) == 1, "expected exactly one private key block"
    assert cert_blocks, "expected at least one certificate block"
    key = serialization.load_pem_private_key(key_blocks[0], None)
    return key, [x509.load_pem_x509_certificate(c) for c in cert_blocks]


def public_numbers_equal(a, b) -> bool:
    """Compare two cryptography public keys (RSA or EC) by their numbers."""
    return a.public_numbers() == b.public_numbers()


# -- certificates: X.509 inspection and a tiny CA ----------------------------
def x509_of(cert):
    """Parse the DER in KeyVaultCertificate.cer (or CertificateOperation-free bytes)."""
    from cryptography import x509

    der = cert if isinstance(cert, (bytes, bytearray)) else cert.cer
    return x509.load_der_x509_certificate(bytes(der))


def sha1_of(der: bytes) -> bytes:
    import hashlib

    return hashlib.sha1(der).digest()


def add_months(dt, months: int):
    """dt plus a number of calendar months (day clamped to the month length)."""
    import calendar
    import datetime

    total = dt.month - 1 + months
    year, month = dt.year + total // 12, total % 12 + 1
    day = min(dt.day, calendar.monthrange(year, month)[1])
    return dt.replace(year=year, month=month, day=day) + datetime.timedelta(0)


def subject_cn(cert) -> str:
    from cryptography.x509.oid import NameOID

    return cert.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value


def san_values(cert, kind):
    """The values of one SAN kind (x509.DNSName, x509.RFC822Name, x509.IPAddress,
    x509.UniformResourceIdentifier) of a certificate, as a list."""
    from cryptography import x509

    try:
        ext = cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
    except x509.ExtensionNotFound:
        return []
    return ext.get_values_for_type(kind)


UPN_OID = "1.3.6.1.4.1.311.20.2.3"


def san_upns(cert):
    """UPN otherName SANs (OID 1.3.6.1.4.1.311.20.2.3, a DER UTF8String) as str."""
    from cryptography import x509

    try:
        ext = cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
    except x509.ExtensionNotFound:
        return []
    out = []
    for name in ext:
        if isinstance(name, x509.OtherName) and name.type_id.dotted_string == UPN_OID:
            raw = name.value  # DER: 0x0c <len> <utf8>
            assert raw[0] == 0x0C, raw
            length, off = raw[1], 2
            if length & 0x80:
                n = length & 0x7F
                length, off = int.from_bytes(raw[2 : 2 + n], "big"), 2 + n
            out.append(raw[off : off + length].decode())
    return out


def unknown_policy(subject="CN=azemu-external", **kwargs):
    """A policy whose issuer Key Vault does not know: the create stays pending (CSR)."""
    from azure.keyvault.certificates import CertificatePolicy

    kwargs.setdefault("key_type", "EC")
    kwargs.setdefault("key_curve_name", "P-256")
    return CertificatePolicy("Unknown", subject=subject, **kwargs)


def begin_pending(certs, name, policy=None):
    """Start a create with an unknown issuer and return the CertificateOperation
    (the poller result is the operation itself in that case)."""
    poller = certs.begin_create_certificate(name, policy or unknown_policy(), _polling_interval=POLL)
    return poller.result()


def make_ca(common_name="azemu test CA", curve="P-256"):
    """(ca_key, ca_cert) - a self-signed CA made locally with cryptography."""
    import datetime

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes
    from cryptography.x509.oid import NameOID

    key = generate_ec_private_key(curve)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=30))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .sign(key, hashes.SHA256())
    )
    return key, cert


def sign_csr(csr_der: bytes, ca_key, ca_cert, days: int = 30):
    """Sign a DER certificate request with the CA; returns the leaf x509.Certificate."""
    import datetime

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes

    csr = x509.load_der_x509_csr(csr_der)
    assert csr.is_signature_valid
    now = datetime.datetime.now(datetime.timezone.utc)
    builder = (
        x509.CertificateBuilder()
        .subject_name(csr.subject)
        .issuer_name(ca_cert.subject)
        .public_key(csr.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=days))
    )
    for ext in csr.extensions:
        builder = builder.add_extension(ext.value, ext.critical)
    return builder.sign(ca_key, hashes.SHA256())


def der_of(cert) -> bytes:
    from cryptography.hazmat.primitives import serialization

    return cert.public_bytes(serialization.Encoding.DER)


def pem_of(cert) -> bytes:
    from cryptography.hazmat.primitives import serialization

    return cert.public_bytes(serialization.Encoding.PEM)


def pkcs12_secret(secrets, name):
    """Fetch the secret that backs a certificate and parse it as PKCS#12 (no password)."""
    secret = secrets.get_secret(name)
    return secret, load_pfx_secret(secret.value)


def make_ec_self_signed(curve="P-256", common_name="azemu-import-ec", days=365):
    return make_self_signed_cert(common_name, generate_ec_private_key(curve), days)
