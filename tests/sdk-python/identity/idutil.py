"""Helpers shared by the identity SDK tests."""

import json
import urllib.request
from urllib.parse import urlsplit, urlunsplit

import jwt  # PyJWT, installed with msal

from azure.core.exceptions import ServiceRequestError
from azure.core.pipeline.transport import RequestsTransport

TENANT = "11111111-2222-3333-4444-555555555555"
CLIENT_ID = "aaaaaaaa-0000-4000-8000-000000000001"
USER_ASSIGNED_ID = "bbbbbbbb-0000-4000-8000-000000000002"

SCOPES = {
    "https://storage.azure.com/.default": "https://storage.azure.com",
    "https://vault.azure.net/.default": "https://vault.azure.net",
}

IDENTITY_ENV = [
    "IDENTITY_ENDPOINT", "IDENTITY_HEADER", "IDENTITY_SERVER_THUMBPRINT", "IMDS_ENDPOINT",
    "MSI_ENDPOINT", "MSI_SECRET", "AZURE_POD_IDENTITY_AUTHORITY_HOST",
    "AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET", "AZURE_CLIENT_CERTIFICATE_PATH",
    "AZURE_CLIENT_CERTIFICATE_PASSWORD", "AZURE_USERNAME", "AZURE_PASSWORD",
    "AZURE_FEDERATED_TOKEN_FILE", "AZURE_AUTHORITY_HOST", "AZURE_REGIONAL_AUTHORITY_NAME",
    "AZURE_TOKEN_CREDENTIALS", "AZURE_ADDITIONALLY_ALLOWED_TENANTS",
    "REQUESTS_CA_BUNDLE", "SSL_CERT_FILE", "CURL_CA_BUNDLE",
]


# -- URLs ---------------------------------------------------------------------
def host_authority(emu) -> str:
    """Entra authority on the `login.*` host. HTTPS is mandatory for MSAL."""
    return f"https://login.localhost:{emu.port}"


def prefix_authority(emu) -> str:
    """Entra authority under the path prefix (no `login.` host name needed)."""
    return f"https://localhost:{emu.port}/_azemu/identity/login"


def app_service_endpoint(emu) -> str:
    return f"http://localhost:{emu.port}/_azemu/identity/msi/token"


def imds_authority(emu) -> str:
    return f"http://localhost:{emu.port}"


# -- tokens -------------------------------------------------------------------
def jwks(emu) -> dict:
    url = f"{emu.gateway}/_azemu/identity/login/common/discovery/v2.0/keys"
    with urllib.request.urlopen(url, timeout=10) as r:
        return json.load(r)


def decode(emu, token: str, audience: str) -> dict:
    """Verify the RS256 signature against the emulator's JWKS, then return the claims."""
    kid = jwt.get_unverified_header(token)["kid"]
    key = next(k for k in jwks(emu)["keys"] if k["kid"] == kid)
    pub = jwt.PyJWK(key).key
    return jwt.decode(token, pub, algorithms=["RS256"], audience=audience, leeway=10)


def check_token(emu, access_token, scope, *, appid=None, appidacr=None, tenant=None):
    """Common assertions for an app-only token for `scope`."""
    aud = SCOPES[scope]
    claims = decode(emu, access_token.token, aud)
    assert claims["aud"] == aud
    assert claims["ver"] == "1.0"
    assert claims["idtyp"] == "app"
    assert claims["oid"] == claims["sub"]
    assert claims["iss"] == f"https://sts.windows.net/{claims['tid']}/"
    assert claims["exp"] - claims["iat"] == 3600
    assert access_token.expires_on == claims["exp"] or abs(access_token.expires_on - claims["exp"]) <= 5
    if appid:
        assert claims["appid"] == appid
    if appidacr:
        assert claims["appidacr"] == appidacr
    if tenant:
        assert claims["tid"] == tenant
    return claims


# -- transport ------------------------------------------------------------------
class RoutingTransport(RequestsTransport):
    """A requests transport that never leaves the machine.

    Records every request URL. `map_microsoft_to` (an emulator base such as
    `https://localhost:PORT/_azemu/identity/login`) makes requests for
    `login.microsoftonline.com` go to the emulator instead, the way a hosts-file
    entry would. Any other host that is not local fails with a connection error.
    """

    LOCAL = {"login.localhost", "localhost", "127.0.0.1"}

    def __init__(self, map_microsoft_to=None, forward_port=None, **kwargs):
        super().__init__(**kwargs)
        self.urls = []
        self.map_microsoft_to = map_microsoft_to
        # MSAL drops the port of the authority in some calls (instance discovery, user
        # realm discovery). `forward_port` plays the part of a 443 -> emulator port forward.
        self.forward_port = forward_port

    def send(self, request, **kwargs):
        http = getattr(request, "http_request", request)  # PipelineRequest or the request itself
        parts = urlsplit(http.url)
        self.urls.append(http.url)
        if parts.hostname == "login.microsoftonline.com" and self.map_microsoft_to:
            target = urlsplit(self.map_microsoft_to)
            http.url = urlunsplit(
                (target.scheme, target.netloc, target.path.rstrip("/") + parts.path, parts.query, "")
            )
        elif parts.hostname not in self.LOCAL:
            raise ServiceRequestError(f"blocked: tests never reach {parts.hostname}")
        elif parts.port is None:
            if not self.forward_port:
                raise ServiceRequestError(f"blocked: no port in {http.url} (would use 443)")
            http.url = urlunsplit(
                (parts.scheme, f"{parts.hostname}:{self.forward_port}", parts.path, parts.query, "")
            )
        return super().send(request, **kwargs)

    def hosts(self):
        return [urlsplit(u).hostname for u in self.urls]

    def paths(self):
        return [urlsplit(u).path for u in self.urls]
