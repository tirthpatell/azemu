"""Credentials that talk to the Entra ID v2 endpoints of the emulator.

What MSAL needs from an authority (measured, see docs/services/identity.md):
  * HTTPS. `authority="http://..."` is rewritten to https by azure-identity, and
    plain-HTTP authorities are refused. The emulator serves HTTPS on the same port.
  * the emulator's CA must be trusted: `connection_verify=<ca.pem>` or
    REQUESTS_CA_BUNDLE.
  * `disable_instance_discovery=True`, or MSAL asks login.microsoftonline.com.
"""

import datetime
import os

import pytest
from azure.core.exceptions import ClientAuthenticationError
from azure.identity import (
    CertificateCredential,
    ClientSecretCredential,
    DefaultAzureCredential,
    EnvironmentCredential,
    WorkloadIdentityCredential,
)
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

import idutil
from idutil import CLIENT_ID, SCOPES, TENANT, RoutingTransport, check_token

ALL_SCOPES = list(SCOPES)


def secret_credential(emu, authority=None, **kw):
    return ClientSecretCredential(
        TENANT,
        CLIENT_ID,
        "any-secret",
        authority=authority or idutil.host_authority(emu),
        disable_instance_discovery=True,
        connection_verify=emu.ca_file,
        **kw,
    )


# -- ClientSecretCredential --------------------------------------------------------------
@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_client_secret_credential_login_host(emu, scope):
    tok = secret_credential(emu).get_token(scope)
    claims = check_token(emu, tok, scope, appid=CLIENT_ID, appidacr="1", tenant=TENANT)
    assert tok.token.count(".") == 2
    assert "roles" not in claims


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_client_secret_credential_path_prefix_authority(emu, scope):
    cred = secret_credential(emu, authority=idutil.prefix_authority(emu))
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, tenant=TENANT)


def test_client_secret_oid_is_stable_per_client_id(emu):
    scope = "https://storage.azure.com/.default"
    a1 = check_token(emu, secret_credential(emu).get_token(scope), scope)
    a2 = check_token(emu, secret_credential(emu).get_token(scope), scope)
    other = ClientSecretCredential(
        TENANT, "dddddddd-0000-4000-8000-000000000004", "x", authority=idutil.host_authority(emu),
        disable_instance_discovery=True, connection_verify=emu.ca_file,
    )
    b = check_token(emu, other.get_token(scope), scope)
    assert a1["oid"] == a2["oid"] != b["oid"]


def test_client_secret_tenant_names_are_accepted(emu):
    scope = "https://storage.azure.com/.default"
    cred = ClientSecretCredential(
        "contoso.onmicrosoft.com", CLIENT_ID, "x", authority=idutil.host_authority(emu),
        disable_instance_discovery=True, connection_verify=emu.ca_file,
    )
    claims = check_token(emu, cred.get_token(scope), scope)
    assert len(claims["tid"]) == 36


def test_untrusted_ca_is_rejected(emu):
    """Without the CA, HTTPS verification fails: the emulator's CA is not a public one."""
    cred = ClientSecretCredential(
        TENANT, CLIENT_ID, "x", authority=idutil.host_authority(emu), disable_instance_discovery=True,
        retry_total=0,
    )
    with pytest.raises(Exception) as e:
        cred.get_token("https://storage.azure.com/.default")
    assert "CERTIFICATE_VERIFY_FAILED" in str(e.value) or "certificate" in str(e.value).lower()


def test_ca_from_requests_ca_bundle_env(emu, monkeypatch):
    """The CA can come from the environment instead of a constructor argument."""
    monkeypatch.setenv("REQUESTS_CA_BUNDLE", emu.ca_file)
    cred = ClientSecretCredential(
        TENANT, CLIENT_ID, "x", authority=idutil.host_authority(emu), disable_instance_discovery=True
    )
    check_token(emu, cred.get_token("https://vault.azure.net/.default"), "https://vault.azure.net/.default")


# -- instance discovery left on ------------------------------------------------------------
def test_instance_discovery_on_asks_login_microsoftonline_com(emu):
    """MSAL does not know login.localhost, so with instance discovery on it first calls
    https://login.microsoftonline.com/common/discovery/instance, whatever the authority
    is. Here that host is unreachable (the transport refuses it), the emulator is never
    asked, and the credential fails. A real run reaches the real Entra ID, which answers
    `invalid_instance` for an unknown host."""
    transport = RoutingTransport(connection_verify=emu.ca_file)
    cred = ClientSecretCredential(
        TENANT, CLIENT_ID, "x", authority=idutil.host_authority(emu), connection_verify=emu.ca_file,
        transport=transport, retry_total=0,
    )
    with pytest.raises(Exception):
        cred.get_token("https://storage.azure.com/.default")
    assert transport.hosts() == ["login.microsoftonline.com"]
    assert transport.paths() == ["/common/discovery/instance"]
    # MSAL puts the authority in the query, without the port
    assert f"login.localhost%2F{TENANT}" in transport.urls[0] or f"login.localhost/{TENANT}" in transport.urls[0]


def test_instance_discovery_on_uses_our_endpoint_when_the_microsoft_host_maps_to_the_emulator(emu):
    """What a hosts-file entry for login.microsoftonline.com does: the discovery call
    reaches the emulator's /common/discovery/instance, whose answer then sends MSAL to
    the emulator's tenant discovery document and token endpoint."""
    transport = RoutingTransport(map_microsoft_to=idutil.prefix_authority(emu), connection_verify=emu.ca_file)
    cred = ClientSecretCredential(
        TENANT, CLIENT_ID, "x", authority=idutil.host_authority(emu), connection_verify=emu.ca_file,
        transport=transport,
    )
    scope = "https://storage.azure.com/.default"
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID)
    assert transport.hosts()[0] == "login.microsoftonline.com"
    assert transport.paths()[0] == "/common/discovery/instance"
    assert any(p.endswith("/v2.0/.well-known/openid-configuration") for p in transport.paths())
    assert any(p.endswith("/oauth2/v2.0/token") for p in transport.paths())
    assert all(h != "login.microsoftonline.com" or p == "/common/discovery/instance"
               for h, p in zip(transport.hosts(), transport.paths()))


def test_error_response_from_the_token_endpoint_reaches_the_caller(emu):
    """An Entra-shaped error (here: a scope without /.default) becomes ClientAuthenticationError."""
    cred = secret_credential(emu)
    with pytest.raises(ClientAuthenticationError) as e:
        cred.get_token("https://graph.microsoft.com/User.Read")
    assert "AADSTS70011" in str(e.value)


# -- EnvironmentCredential / DefaultAzureCredential ---------------------------------------------
@pytest.fixture
def secret_env(emu, monkeypatch):
    monkeypatch.setenv("AZURE_TENANT_ID", TENANT)
    monkeypatch.setenv("AZURE_CLIENT_ID", CLIENT_ID)
    monkeypatch.setenv("AZURE_CLIENT_SECRET", "any-secret")
    monkeypatch.setenv("AZURE_AUTHORITY_HOST", idutil.host_authority(emu))


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_environment_credential(emu, secret_env, scope):
    cred = EnvironmentCredential(disable_instance_discovery=True, connection_verify=emu.ca_file)
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, tenant=TENANT)


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_default_azure_credential_environment(emu, secret_env, scope):
    cred = DefaultAzureCredential(disable_instance_discovery=True, connection_verify=emu.ca_file)
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, tenant=TENANT)


def test_environment_credential_with_everything_from_the_environment(emu, secret_env, monkeypatch):
    """Only env vars, no constructor arguments, apart from the instance-discovery switch:
    the CA comes from REQUESTS_CA_BUNDLE."""
    monkeypatch.setenv("REQUESTS_CA_BUNDLE", emu.ca_file)
    cred = EnvironmentCredential(disable_instance_discovery=True)
    check_token(emu, cred.get_token("https://storage.azure.com/.default"), "https://storage.azure.com/.default")


# -- WorkloadIdentityCredential -------------------------------------------------------------------
@pytest.fixture
def federated_token_file(tmp_path):
    # Any text is accepted by the emulator; a JWT-looking value is what Kubernetes writes.
    p = tmp_path / "token"
    p.write_text("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJzeXN0ZW06c2VydmljZWFjY291bnQ6ZGVmYXVsdDpzYSJ9.c2ln")
    return str(p)


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_workload_identity_credential(emu, federated_token_file, scope):
    cred = WorkloadIdentityCredential(
        tenant_id=TENANT, client_id=CLIENT_ID, token_file_path=federated_token_file,
        authority=idutil.host_authority(emu), disable_instance_discovery=True, connection_verify=emu.ca_file,
    )
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, appidacr="2", tenant=TENANT)


def test_workload_identity_from_environment_through_default_azure_credential(emu, federated_token_file, monkeypatch):
    monkeypatch.setenv("AZURE_TENANT_ID", TENANT)
    monkeypatch.setenv("AZURE_CLIENT_ID", CLIENT_ID)
    monkeypatch.setenv("AZURE_FEDERATED_TOKEN_FILE", federated_token_file)
    monkeypatch.setenv("AZURE_AUTHORITY_HOST", idutil.host_authority(emu))
    cred = DefaultAzureCredential(disable_instance_discovery=True, connection_verify=emu.ca_file)
    scope = "https://vault.azure.net/.default"
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, appidacr="2")


# -- CertificateCredential --------------------------------------------------------------------------
@pytest.fixture
def certificate_pem(tmp_path):
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "azemu-test")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name).issuer_name(name).public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .sign(key, hashes.SHA256())
    )
    pem = key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
    ) + cert.public_bytes(serialization.Encoding.PEM)
    path = tmp_path / "cert.pem"
    path.write_bytes(pem)
    return str(path)


@pytest.mark.parametrize("scope", ALL_SCOPES)
def test_certificate_credential(emu, certificate_pem, scope):
    cred = CertificateCredential(
        TENANT, CLIENT_ID, certificate_path=certificate_pem, authority=idutil.host_authority(emu),
        disable_instance_discovery=True, connection_verify=emu.ca_file,
    )
    check_token(emu, cred.get_token(scope), scope, appid=CLIENT_ID, appidacr="2", tenant=TENANT)


def test_certificate_credential_from_environment(emu, certificate_pem, monkeypatch):
    monkeypatch.setenv("AZURE_TENANT_ID", TENANT)
    monkeypatch.setenv("AZURE_CLIENT_ID", CLIENT_ID)
    monkeypatch.setenv("AZURE_CLIENT_CERTIFICATE_PATH", certificate_pem)
    monkeypatch.setenv("AZURE_AUTHORITY_HOST", idutil.host_authority(emu))
    cred = EnvironmentCredential(disable_instance_discovery=True, connection_verify=emu.ca_file)
    check_token(emu, cred.get_token("https://storage.azure.com/.default"), "https://storage.azure.com/.default", appidacr="2")
