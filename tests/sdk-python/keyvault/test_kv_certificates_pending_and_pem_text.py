"""A pending certificate can be read; a PEM bundle can be imported as plain text.

Both come from the JavaScript SDK (`@azure/keyvault-certificates`): its create poller reads
`GET /certificates/{name}` before the operation, and `importCertificate` sends a PEM bundle as text
in `value` (no `policy`, so the content type must come from the value). Not recorded; see keyvault.md.
"""

import requests
from azure.core.exceptions import ResourceNotFoundError
import pytest
from cryptography.hazmat.primitives import serialization

import kvutil
from kvutil import begin_pending, make_self_signed_cert, unique, vault_url


def test_a_pending_certificate_can_be_read_without_cer(certs, emu):
    name = unique("pend")
    begin_pending(certs, name)
    cert = certs.get_certificate(name)
    assert cert.name == name
    assert cert.cer is None and cert.properties.x509_thumbprint is None
    assert cert.policy.issuer_name == "Unknown"
    version = cert.properties.version
    assert len(version) == 32
    assert certs.get_certificate_version(name, version).properties.version == version
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate_version(name, "f" * 32)
    # the plain list leaves it out, include_pending pending shows it
    assert name not in [c.name for c in certs.list_properties_of_certificates()]
    assert name in [c.name for c in certs.list_properties_of_certificates(include_pending=True)]
    # the operation still works and a delete removes the pending certificate
    assert certs.get_certificate_operation(name).status == "inProgress"
    certs.begin_delete_certificate(name, _polling_interval=kvutil.POLL).result()
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate(name)


def test_a_pem_bundle_as_plain_text_is_imported(certs, emu, secrets):
    key, cert = make_self_signed_cert("azemu-pem-text")
    pem = key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    ) + cert.public_bytes(serialization.Encoding.PEM)
    name = unique("pemtext")
    r = requests.post(
        f"{vault_url(emu)}/certificates/{name}/import?api-version=7.6",
        json={"value": pem.decode()},
        headers={"Authorization": "Bearer x"},
        verify=emu.ca_file or True,
    )
    assert r.status_code == 200, r.text
    body = r.json()
    assert body["policy"]["secret_props"]["contentType"] == "application/x-pem-file"
    got = certs.get_certificate(name)
    assert got.cer == cert.public_bytes(serialization.Encoding.DER)
    assert "BEGIN PRIVATE KEY" in secrets.get_secret(name).value
