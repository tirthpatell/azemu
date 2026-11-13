"""Key Vault, all services: vault isolation, authentication challenge, error shape, headers, managed items.

Sources (recordings of all three Key Vault packages):
  * Every request without a token gets 401, Content-Type "application/json; charset=utf-8",
    WWW-Authenticate: Bearer authorization="https://login.microsoftonline.com/<tenant>",
    resource="https://vault.azure.net" (login.windows.net also occurs in older recordings), and the body
    {"error": {"code": "Unauthorized", "message": "AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token."}}
    (recordings from before March 2026 say "... a Bearer or PoP token." for every api-version: the wording changed
    in the service over time, not with the api-version).
  * Errors are {"error": {"code": ..., "message": ...}}.
  * Success and 404 answers of the certificates package carry Cache-Control: no-cache, Pragma: no-cache,
    Expires: -1, Content-Type: application/json; charset=utf-8, Content-Length, Date,
    Strict-Transport-Security: max-age=31536000;includeSubDomains, X-Content-Type-Options: nosniff,
    x-ms-keyvault-region and x-ms-keyvault-service-version; the 202 of a create and the in-progress
    operation also carry Retry-After.
  * Secret and key listings contain a certificate's name as an item with "managed": true; the secret item
    has contentType application/x-pkcs12.
  * Vault isolation is how Key Vault works (every vault has its own namespace); no recording shows two vaults.
  * Left out: a request without api-version (neither the recordings nor the spec say what Key Vault answers).
"""

import re

import pytest
import requests
from azure.core.exceptions import ResourceNotFoundError
from azure.keyvault.certificates import CertificateContentType

from kvutil import (
    create_cert,
    delete_and_wait,
    ec_policy,
    error_code,
    unique,
)

API = "api-version=7.5"
# Azure names vault.azure.net. The emulator's hosts are <vault>.vault.localhost:<port>, and a challenge that named
# vault.azure.net would be refused by the clients' own check, so it names the request's domain instead (
# test_challenge_resource.py checks the exact value). Under a real Azure name the real resource is kept.
WWW_AUTH = re.compile(
    r'^Bearer authorization="https://login\.(microsoftonline\.com|windows\.net)/[^"]+", resource="https://(vault\.azure\.net|vault\.localhost:\d+)"$'
)


def _get(emu, url, token=True, **kwargs):
    headers = {"Authorization": "Bearer azemu-test-token"} if token else {}
    return requests.get(url, headers=headers, verify=emu.ca_file, timeout=10, **kwargs)


# -- vault isolation ----------------------------------------------------------------------------
def test_two_vaults_do_not_share_secrets(secrets, make_clients):
    other, _k, _c = make_clients("othervault")
    name = unique("sec")
    secrets.set_secret(name, "in-default")
    with pytest.raises(ResourceNotFoundError):
        other.get_secret(name)
    assert list(other.list_properties_of_secrets()) == []
    other.set_secret(name, "in-other")
    assert secrets.get_secret(name).value == "in-default"
    assert other.get_secret(name).value == "in-other"


def test_two_vaults_do_not_share_keys(keys, make_clients):
    _s, other, _c = make_clients("othervault")
    name = unique("key")
    keys.create_ec_key(name)
    with pytest.raises(ResourceNotFoundError):
        other.get_key(name)
    assert list(other.list_properties_of_keys()) == []
    assert other.create_ec_key(name).properties.version != keys.get_key(name).properties.version


def test_two_vaults_do_not_share_certificates(certs, make_clients):
    _s, _k, other = make_clients("othervault")
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    with pytest.raises(ResourceNotFoundError) as exc:
        other.get_certificate(name)
    assert error_code(exc.value) == "CertificateNotFound"
    assert list(other.list_properties_of_certificates()) == []
    assert list(other.list_properties_of_certificate_versions(name)) == []
    with pytest.raises(ResourceNotFoundError):
        other.get_certificate_policy(name)
    with pytest.raises(ResourceNotFoundError):
        other.get_certificate_operation(name)


def test_the_same_certificate_name_in_two_vaults_is_independent(certs, make_clients):
    _s, _k, other = make_clients("othervault")
    name = unique("cert")
    a = create_cert(certs, name, ec_policy("CN=vault-a"))
    b = create_cert(other, name, ec_policy("CN=vault-b"))
    assert a.id.startswith(certs.vault_url) and b.id.startswith(other.vault_url)
    assert a.properties.version != b.properties.version
    assert certs.get_certificate(name).policy.subject == "CN=vault-a"
    assert other.get_certificate(name).policy.subject == "CN=vault-b"
    delete_and_wait(certs, name)
    assert other.get_certificate(name).policy.subject == "CN=vault-b"
    assert list(other.list_deleted_certificates()) == []
    assert [d.name for d in certs.list_deleted_certificates()] == [name]


def test_a_certificates_secret_and_key_are_only_in_its_own_vault(certs, make_clients):
    other_secrets, other_keys, _c = make_clients("othervault")
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    with pytest.raises(ResourceNotFoundError):
        other_secrets.get_secret(name)
    with pytest.raises(ResourceNotFoundError):
        other_keys.get_key(name)


# -- authentication challenge ---------------------------------------------------------------------------
def test_request_without_token_gets_401_with_challenge(emu, certs):
    r = _get(emu, f"{certs.vault_url}/certificates/{unique('c')}?{API}", token=False)
    assert r.status_code == 401
    assert WWW_AUTH.match(r.headers["WWW-Authenticate"]), r.headers["WWW-Authenticate"]


@pytest.mark.parametrize("path", ["secrets/x", "keys/x", "certificates/x", "certificates", "deletedcertificates"])
def test_every_service_challenges_a_request_without_token(emu, certs, path):
    r = _get(emu, f"{certs.vault_url}/{path}?{API}", token=False)
    assert r.status_code == 401
    assert WWW_AUTH.match(r.headers["WWW-Authenticate"]), r.headers["WWW-Authenticate"]


def test_request_without_token_and_with_a_body_gets_401_with_challenge(emu, certs):
    body = '{"policy": {"x509_props": {"subject": "CN=DefaultPolicy"}, "issuer": {"name": "Self"}}}'
    r = requests.post(
        f"{certs.vault_url}/certificates/{unique('c')}/create?{API}",
        data=body,
        headers={"Content-Type": "application/json"},
        verify=emu.ca_file,
        timeout=10,
    )
    assert r.status_code == 401
    assert WWW_AUTH.match(r.headers["WWW-Authenticate"]), r.headers["WWW-Authenticate"]
    assert r.json()["error"]["code"] == "Unauthorized"


def test_unauthorized_body_and_content_type(emu, certs):
    r = _get(emu, f"{certs.vault_url}/secrets/x?{API}", token=False)
    assert r.headers["Content-Type"] == "application/json; charset=utf-8"
    body = r.json()
    assert set(body) == {"error"}
    assert body["error"]["code"] == "Unauthorized"
    assert body["error"]["message"].startswith("AKV10000: Request is missing a Bearer")


def test_unauthorized_request_does_not_create_anything(emu, certs):
    name = unique("c")
    requests.post(
        f"{certs.vault_url}/certificates/{name}/create?{API}",
        data='{"policy": {"x509_props": {"subject": "CN=x"}, "issuer": {"name": "Self"}}}',
        headers={"Content-Type": "application/json"},
        verify=emu.ca_file,
        timeout=10,
    )
    with pytest.raises(ResourceNotFoundError):
        certs.get_certificate(name)


def test_request_with_a_bearer_token_is_not_challenged(emu, certs):
    r = _get(emu, f"{certs.vault_url}/certificates/{unique('c')}?{API}")
    assert r.status_code == 404
    assert "WWW-Authenticate" not in r.headers


# -- error JSON shape ---------------------------------------------------------------------------------------
@pytest.mark.parametrize(
    "path,code",
    [
        ("certificates/{n}", "CertificateNotFound"),
        ("certificates/{n}/pending", "PendingCertificateNotFound"),
        ("certificates/issuers/{n}", "CertificateIssuerNotFound"),
        ("deletedcertificates/{n}", "CertificateNotFound"),
        ("secrets/{n}", "SecretNotFound"),
        ("keys/{n}", "KeyNotFound"),
    ],
)
def test_404_error_json_shape(emu, certs, path, code):
    # Recorded 404 bodies of certificates, secrets and keys.
    r = _get(emu, f"{certs.vault_url}/{path.format(n=unique('missing'))}?{API}")
    assert r.status_code == 404
    body = r.json()
    assert set(body) == {"error"}
    assert body["error"]["code"] == code
    assert isinstance(body["error"]["message"], str) and body["error"]["message"]
    assert r.headers["Content-Type"] == "application/json; charset=utf-8"


def test_contacts_404_error_json_shape(emu, certs):
    r = _get(emu, f"{certs.vault_url}/certificates/contacts?{API}")
    assert r.status_code == 404
    assert r.json() == {"error": {"code": "ContactsNotFound", "message": "Contacts not found"}}


def test_400_error_json_shape(emu, certs):
    # Code BadParameter for an invalid name (Azure docs, as for secrets and keys).
    r = _get(emu, f"{certs.vault_url}/certificates/not_valid?{API}")
    assert r.status_code == 400
    body = r.json()
    assert set(body) == {"error"}
    assert body["error"]["code"] == "BadParameter"
    assert body["error"]["message"]


# -- response headers --------------------------------------------------------------------------------------------
def _assert_keyvault_headers(r):
    h = r.headers
    assert h["Cache-Control"] == "no-cache"
    assert h["Pragma"] == "no-cache"
    assert h["Expires"] == "-1"
    assert h["Content-Type"] == "application/json; charset=utf-8"
    assert h["Strict-Transport-Security"] == "max-age=31536000;includeSubDomains"
    assert h["X-Content-Type-Options"] == "nosniff"
    assert int(h["Content-Length"]) == len(r.content)
    assert h["Date"]
    assert "x-ms-keyvault-region" in h
    assert "x-ms-keyvault-service-version" in h


def test_success_response_headers_match_key_vault(emu, certs):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    _assert_keyvault_headers(_get(emu, f"{certs.vault_url}/certificates/{name}?{API}"))


def test_not_found_response_headers_match_key_vault(emu, certs):
    r = _get(emu, f"{certs.vault_url}/certificates/{unique('missing')}?{API}")
    assert r.status_code == 404
    _assert_keyvault_headers(r)


def test_create_202_headers_match_key_vault_and_carry_retry_after(emu, certs):
    # Recorded: the 202 of POST .../create has Retry-After (10) next to the usual headers.
    r = requests.post(
        f"{certs.vault_url}/certificates/{unique('cert')}/create?{API}",
        json={"policy": {"x509_props": {"subject": "CN=hdr"}, "issuer": {"name": "Self"},
                         "key_props": {"kty": "EC", "crv": "P-256"}}},
        headers={"Authorization": "Bearer azemu-test-token"},
        verify=emu.ca_file,
        timeout=10,
    )
    assert r.status_code == 202
    _assert_keyvault_headers(r)
    assert "Retry-After" in r.headers
    body = r.json()
    assert body["status"].lower() in {"inprogress", "completed"}
    assert body["id"].endswith("/pending")


# -- certificates as managed secrets and keys -------------------------------------------------------------------------
def test_certificate_name_is_listed_as_a_managed_secret(certs, secrets):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    plain = unique("sec")
    secrets.set_secret(plain, "v")
    items = {p.name: p for p in secrets.list_properties_of_secrets()}
    assert set(items) == {name, plain}
    assert items[name].managed is True  # recorded
    assert items[name].content_type == "application/x-pkcs12"  # recorded
    assert items[name].version is None  # list ids carry no version
    assert items[name].enabled is True
    assert not items[plain].managed  # a plain secret is not managed (recorded items have no "managed")


def test_certificate_name_is_listed_as_a_managed_key(certs, keys):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    plain = unique("key")
    keys.create_ec_key(plain)
    items = {p.name: p for p in keys.list_properties_of_keys()}
    assert set(items) == {name, plain}
    assert items[name].managed is True  # recorded
    assert not items[plain].managed


def test_pem_certificate_is_listed_as_a_pem_secret(certs, secrets):
    # The recorded listing has application/x-pkcs12; the PEM content type is from Azure docs.
    name = unique("cert")
    create_cert(certs, name, ec_policy(content_type=CertificateContentType.pem))
    (item,) = list(secrets.list_properties_of_secrets())
    assert item.name == name
    assert item.managed is True
    assert item.content_type == "application/x-pem-file"


def test_certificate_versions_show_up_as_secret_and_key_versions(certs, secrets, keys):
    name = unique("cert")
    versions = {create_cert(certs, name, ec_policy()).properties.version for _ in range(2)}
    assert {p.version for p in secrets.list_properties_of_secret_versions(name)} == versions
    assert {p.version for p in keys.list_properties_of_key_versions(name)} == versions


def test_deleted_certificate_disappears_from_secret_and_key_lists(certs, secrets, keys):
    name = unique("cert")
    create_cert(certs, name, ec_policy())
    delete_and_wait(certs, name)
    assert list(secrets.list_properties_of_secrets()) == []
    assert list(keys.list_properties_of_keys()) == []
