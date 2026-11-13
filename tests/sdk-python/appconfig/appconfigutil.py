"""Small helpers shared by the App Configuration tests (not a test module)."""

import re
import time
import uuid
from email.utils import parsedate_to_datetime
from urllib.parse import quote

import requests
from azure.core.credentials import AccessToken

# The API version most of the recordings were made with. The installed SDK
# itself defaults to 2026-04-01, so the SDK-based tests exercise that one.
API_VERSION = "2023-11-01"

HMAC_ID = "azemu-id"
HMAC_SECRET = "YXplbXUtc2VjcmV0"  # base64("azemu-secret")

KV_CT = "application/vnd.microsoft.appconfig.kv+json; charset=utf-8"
KVSET_CT = "application/vnd.microsoft.appconfig.kvset+json; charset=utf-8"
PROBLEM_CT = "application/problem+json; charset=utf-8"

FF_PREFIX = ".appconfig.featureflag/"
FF_CONTENT_TYPE = "application/vnd.microsoft.appconfig.ff+json;charset=utf-8"
KVREF_CONTENT_TYPE = "application/vnd.microsoft.appconfig.keyvaultref+json;charset=utf-8"

# last_modified as Azure sends it: second precision, explicit UTC offset.
ISO_TS_RE = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(\+00:00|Z)$")
# Sync-Token as Azure sends it: "<id>=<value>;sn=<number>"
SYNC_TOKEN_RE = re.compile(r"^[^=;,\s]+=[^;,\s]+;sn=\d+$")


def conn_string(endpoint: str, id_: str = HMAC_ID, secret: str = HMAC_SECRET) -> str:
    return f"Endpoint={endpoint};Id={id_};Secret={secret}"


class FakeCredential:
    """Entra ID style credential. The emulator accepts any bearer token."""

    def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)


class FakeAsyncCredential:
    async def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    async def close(self):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        pass


def uid(prefix: str = "k") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:12]}"


def next_link(resp: "requests.Response"):
    """The target of `Link: <...>; rel="next"`, or None."""
    m = re.search(r'<([^>]*)>\s*;\s*rel="next"', resp.headers.get("Link", ""))
    return m.group(1) if m else None


def parse_http_date(value: str):
    return parsedate_to_datetime(value)


def sync_token_parts(value: str):
    """Split "id=value;sn=N" into (id, value, N)."""
    head, _, sn = value.partition(";sn=")
    tid, _, tval = head.partition("=")
    return tid, tval, int(sn)


class Raw:
    """Plain `requests` access to a store: no SDK, no signing, just a bearer header."""

    def __init__(self, base: str, verify, version: str = API_VERSION):
        self.base = base
        self.verify = verify if verify else True
        self.version = version

    def url(self, path: str, **params) -> str:
        return self.base + path

    def request(self, method, path, params=None, headers=None, auth=True, version="default", **kw):
        p = {}
        v = self.version if version == "default" else version
        if v:
            p["api-version"] = v
        p.update(params or {})
        h = {}
        if auth:
            h["Authorization"] = "Bearer x"
        h.update(headers or {})
        return requests.request(
            method, self.base + path, params=p, headers=h, verify=self.verify,
            allow_redirects=False, timeout=30, **kw,
        )

    def kv_path(self, key: str) -> str:
        return "/kv/" + quote(key, safe="")

    def put_kv(self, key, label=None, headers=None, **fields):
        body = {"key": key, "label": label}
        body.update(fields)
        params = {"label": label} if label is not None else None
        return self.request("PUT", self.kv_path(key), params=params, headers=headers, json=body)

    def get_kv(self, key, label=None, headers=None):
        params = {"label": label} if label is not None else None
        return self.request("GET", self.kv_path(key), params=params, headers=headers)

    def delete_kv(self, key, label=None, headers=None):
        params = {"label": label} if label is not None else None
        return self.request("DELETE", self.kv_path(key), params=params, headers=headers)

    def lock(self, key, label=None, delete=False):
        params = {"label": label} if label is not None else None
        return self.request("DELETE" if delete else "PUT", "/locks/" + quote(key, safe=""), params=params)


def seed(client, prefix: str, count: int, **fields):
    """Create `count` settings named `<prefix>-<i:04d>`; returns the list of keys."""
    from azure.appconfiguration import ConfigurationSetting

    keys = []
    for i in range(count):
        key = f"{prefix}-{i:04d}"
        client.set_configuration_setting(ConfigurationSetting(key=key, value=f"v{i}", **fields))
        keys.append(key)
    return keys


def status_of(excinfo) -> int:
    return excinfo.value.status_code
