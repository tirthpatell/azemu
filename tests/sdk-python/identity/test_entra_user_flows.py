"""User flows of the Entra v2 endpoints: device code, username/password,
authorization code and on-behalf-of. The emulator accepts any user and password."""

import urllib.parse
import urllib.request

import pytest
from azure.core.exceptions import ClientAuthenticationError
from azure.identity import (
    AuthorizationCodeCredential,
    DeviceCodeCredential,
    OnBehalfOfCredential,
    UsernamePasswordCredential,
)

import idutil
from idutil import CLIENT_ID, TENANT, RoutingTransport, decode

STORAGE = "https://storage.azure.com/.default"
AUD = "https://storage.azure.com"


def common(emu):
    return dict(authority=idutil.host_authority(emu), disable_instance_discovery=True, connection_verify=emu.ca_file)


def test_device_code_credential(emu):
    prompts = []
    cred = DeviceCodeCredential(
        client_id=CLIENT_ID, tenant_id=TENANT, prompt_callback=lambda uri, code, exp: prompts.append((uri, code, exp)),
        **common(emu),
    )
    tok = cred.get_token(STORAGE)
    claims = decode(emu, tok.token, AUD)
    assert claims["appid"] == CLIENT_ID
    assert claims["tid"] == TENANT
    assert "upn" in claims and claims["scp"] == "user_impersonation"
    assert "idtyp" not in claims
    assert len(prompts) == 1
    uri, code, _ = prompts[0]
    assert uri.startswith(idutil.host_authority(emu)) and code
    # the second call comes from MSAL's cache or a refresh token, never a new device flow
    cred.get_token("https://vault.azure.net/.default")
    assert len(prompts) == 1


def test_username_password_credential_needs_the_port_to_be_kept(emu):
    """MSAL builds the user realm discovery URL from the authority host WITHOUT the port
    (https://login.localhost/common/userrealm/...), so on a port other than 443 the call
    does not reach the emulator."""
    transport = RoutingTransport(connection_verify=emu.ca_file)
    cred = UsernamePasswordCredential(
        client_id=CLIENT_ID, username="alice@example.com", password="pw", tenant_id=TENANT,
        authority=idutil.host_authority(emu), disable_instance_discovery=True, transport=transport, retry_total=0,
    )
    with pytest.raises(ClientAuthenticationError):
        cred.get_token(STORAGE)
    assert any("/common/userrealm/alice@example.com" in u and f":{emu.port}" not in u for u in transport.urls)


def test_username_password_credential_with_a_443_forward(emu):
    """The same flow works once the port-less request reaches the emulator (here the
    transport adds the port, standing in for a 443 forward or a reverse proxy)."""
    transport = RoutingTransport(connection_verify=emu.ca_file, forward_port=emu.port)
    cred = UsernamePasswordCredential(
        client_id=CLIENT_ID, username="alice@example.com", password="pw", tenant_id=TENANT,
        authority=idutil.host_authority(emu), disable_instance_discovery=True, transport=transport,
    )
    tok = cred.get_token(STORAGE)
    claims = decode(emu, tok.token, AUD)
    assert claims["upn"] == "alice@example.com"
    assert claims["appidacr"] == "0"


def _auth_code(emu, redirect="http://localhost:8400"):
    q = urllib.parse.urlencode(
        {"client_id": CLIENT_ID, "response_type": "code", "redirect_uri": redirect, "scope": f"{STORAGE} offline_access",
         "login_hint": "bob@example.com"}
    )
    url = f"{emu.gateway}/_azemu/identity/login/{TENANT}/oauth2/v2.0/authorize?{q}"

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *a, **k):
            return None

    try:
        urllib.request.build_opener(NoRedirect).open(url, timeout=10)
    except urllib.error.HTTPError as e:
        assert e.code == 302
        loc = e.headers["Location"]
        return urllib.parse.parse_qs(urllib.parse.urlsplit(loc).query)["code"][0]
    raise AssertionError("authorize did not redirect")


def test_authorization_code_credential(emu):
    code = _auth_code(emu)
    cred = AuthorizationCodeCredential(
        TENANT, CLIENT_ID, code, "http://localhost:8400", client_secret="s", **common(emu)
    )
    tok = cred.get_token(STORAGE)
    claims = decode(emu, tok.token, AUD)
    assert claims["upn"] == "bob@example.com"
    assert claims["appidacr"] == "1"


def test_on_behalf_of_credential(emu):
    user = DeviceCodeCredential(client_id=CLIENT_ID, tenant_id=TENANT, prompt_callback=lambda *a: None, **common(emu))
    user_token = user.get_token("api://my-api/.default").token
    cred = OnBehalfOfCredential(
        TENANT, "dddddddd-0000-4000-8000-000000000004", client_secret="s", user_assertion=user_token, **common(emu)
    )
    tok = cred.get_token(STORAGE)
    claims = decode(emu, tok.token, AUD)
    assert claims["upn"] == decode(emu, user_token, "api://my-api")["upn"]
    assert claims["appid"] == "dddddddd-0000-4000-8000-000000000004"
