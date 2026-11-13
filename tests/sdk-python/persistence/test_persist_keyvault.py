"""Key Vault state across restarts, through the official SDKs: secrets with versions and deleted items, keys with
their private material (a signature made before the restart verifies after it, and RS256 is deterministic: the same
private key gives the same signature), certificates with their pending requests, issuers and contacts."""

import hashlib
import pathlib
import sys

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "keyvault"))
import kvutil  # noqa: E402

from azure.core.exceptions import ResourceNotFoundError  # noqa: E402
from azure.keyvault.certificates import AdministratorContact, CertificateContact, CertificatePolicy  # noqa: E402
from azure.keyvault.keys import KeyCurveName, KeyType  # noqa: E402
from azure.keyvault.keys.crypto import EncryptionAlgorithm, SignatureAlgorithm  # noqa: E402


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_vault_state_and_private_keys_survive(persistent, how):
    p = persistent()
    emu = p.emu
    secrets = kvutil.make_secret_client(emu)
    keys = kvutil.make_key_client(emu)
    certs = kvutil.make_cert_client(emu)

    # secrets
    secrets.set_secret("db-password", "first", content_type="text/plain", tags={"env": "dev"})
    s2 = secrets.set_secret("db-password", "second", tags={"env": "prod"})
    secrets.set_secret("off", "disabled", enabled=False)
    secrets.set_secret("deleted-one", "bye")
    secrets.begin_delete_secret("deleted-one").wait()
    deleted = secrets.get_deleted_secret("deleted-one")

    # keys
    rsa = keys.create_rsa_key("rsa-key", size=2048)
    ec = keys.create_ec_key("ec-key", curve=KeyCurveName.p_384)
    digest = hashlib.sha256(b"message").digest()
    crypto = kvutil.make_server_side_crypto_client(emu, rsa.id)
    sig = crypto.sign(SignatureAlgorithm.rs256, digest).signature
    cipher = crypto.encrypt(EncryptionAlgorithm.rsa_oaep_256, b"secret text").ciphertext
    ec_crypto = kvutil.make_server_side_crypto_client(emu, ec.id)
    ec_sig = ec_crypto.sign(SignatureAlgorithm.es384, hashlib.sha384(b"ec message").digest()).signature
    keys.rotate_key("rsa-key")
    versions_before = [k.version for k in keys.list_properties_of_key_versions("rsa-key")]

    # certificates: self-signed, and one waiting for an unknown issuer
    kvutil.create_cert(certs, "web", kvutil.ec_policy("CN=web.example.com"))
    cert_before = certs.get_certificate("web")
    cert_secret_before = secrets.get_secret("web").value
    certs.begin_create_certificate("ext", CertificatePolicy("Unknown", subject="CN=ext"), _polling_interval=0.1)
    certs.create_issuer("iss1", "Test", account_id="u", password="secret", admin_contacts=[AdministratorContact(first_name="J", email="j@x.com")])
    certs.set_contacts([CertificateContact(email="a@x.com", name="A", phone="1")])

    # another vault
    kvutil.make_secret_client(emu, "other").set_secret("s", "in the other vault")

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()
    emu = p.emu

    secrets = kvutil.make_secret_client(emu)
    keys = kvutil.make_key_client(emu)
    certs = kvutil.make_cert_client(emu)
    assert secrets.get_secret("db-password").value == "second"
    assert secrets.get_secret("db-password").properties.version == s2.properties.version
    assert secrets.get_secret("db-password").properties.tags == {"env": "prod"}
    assert [x.version for x in secrets.list_properties_of_secret_versions("db-password")] and len(list(secrets.list_properties_of_secret_versions("db-password"))) == 2
    assert [x.enabled for x in secrets.list_properties_of_secrets() if x.name == "off"] == [False]
    assert secrets.get_deleted_secret("deleted-one").scheduled_purge_date == deleted.scheduled_purge_date
    secrets.begin_recover_deleted_secret("deleted-one").wait()
    assert secrets.get_secret("deleted-one").value == "bye"
    assert kvutil.make_secret_client(emu, "other").get_secret("s").value == "in the other vault"

    rsa_now = keys.get_key("rsa-key")
    assert [k.version for k in keys.list_properties_of_key_versions("rsa-key")] == versions_before
    crypto = kvutil.make_server_side_crypto_client(emu, keys.get_key("rsa-key", version=rsa.properties.version).id)
    assert crypto.sign(SignatureAlgorithm.rs256, digest).signature == sig, "the same private key gives the same signature"
    assert crypto.decrypt(EncryptionAlgorithm.rsa_oaep_256, cipher).plaintext == b"secret text"
    ec_crypto = kvutil.make_server_side_crypto_client(emu, keys.get_key("ec-key").id)
    assert ec_crypto.verify(SignatureAlgorithm.es384, hashlib.sha384(b"ec message").digest(), ec_sig).is_valid
    assert rsa_now.key.n != b""

    cert_after = certs.get_certificate("web")
    assert cert_after.cer == cert_before.cer and cert_after.properties.version == cert_before.properties.version
    assert secrets.get_secret("web").value == cert_secret_before, "the PFX with the private key is the same"
    assert certs.get_certificate_operation("ext").status == "inProgress"
    assert certs.get_issuer("iss1").provider == "Test"
    assert [c.email for c in certs.get_contacts()] == ["a@x.com"]
    with pytest.raises(ResourceNotFoundError):
        secrets.get_secret("never-existed")


def test_the_key_file_is_private(persistent):
    import os
    import stat

    p = persistent()
    kvutil.make_secret_client(p.emu).set_secret("s", "v")
    p.stop()
    d = p.data_dir / "keyvault"
    assert stat.S_IMODE(os.stat(d).st_mode) == 0o700
    assert stat.S_IMODE(os.stat(d / "keyvault.state").st_mode) == 0o600
