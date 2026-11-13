"""Helpers shared by the Cosmos tests."""

import base64
import hashlib
import hmac
import json
import urllib.parse
import uuid
from email.utils import formatdate

import requests
from azure.cosmos import CosmosClient

# The well-known Cosmos DB emulator key. The emulator accepts any key.
KEY = (
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw=="
)


def uid(prefix="t"):
    """Unique, URL-safe resource id: `<prefix>-<12 hex chars>`."""
    return f"{prefix}-{uuid.uuid4().hex[:12]}" if prefix else uuid.uuid4().hex[:12]


def client_kwargs(ca_file, **overrides):
    """Keyword arguments for CosmosClient / aio.CosmosClient.

    - connection_verify: path of the emulator CA bundle (None = default store;
      only used for https).
    - retry_total / retry_throttle_total = 1, short backoff: the SDK treats a
      falsy retry count (0) as "use the default of 9", so 1 is the smallest
      value that actually shortens retries. With these settings a 429/5xx/
      connection error costs about a second instead of tens of seconds.
    - connection_timeout / read_timeout: fail fast on a hung emulator.
    """
    kw = dict(
        connection_verify=ca_file,
        retry_total=1,
        retry_throttle_total=1,
        retry_backoff_max=1,
        retry_fixed_interval=10,
        connection_timeout=5,
        read_timeout=30,
    )
    kw.update(overrides)
    # azure-cosmos ignores a CA path in connection_verify unless an ssl_config
    # object is passed too (cosmos_client.py: `if ssl: ssl.SSLCaCerts = verify`).
    if isinstance(kw.get("connection_verify"), str) and "ssl_config" not in kw:
        from azure.cosmos.documents import SSLConfiguration
        kw["ssl_config"] = SSLConfiguration()
    return kw


def async_client_kwargs(ca_file, **overrides):
    """Keyword arguments for aio.CosmosClient.

    The async client cannot take a CA path: with an ssl_config it passes
    connection_cert=(None, None) to aiohttp, which raises TypeError. The CA is
    trusted through SSL_CERT_FILE instead, which the default SSL context reads.
    """
    import os
    kw = client_kwargs(None, **overrides)
    kw.pop("ssl_config", None)
    if ca_file:
        os.environ["SSL_CERT_FILE"] = ca_file
    kw["connection_verify"] = True
    return kw


def new_client(endpoint, ca_file, credential=KEY, **overrides):
    return CosmosClient(endpoint, credential=credential, **client_kwargs(ca_file, **overrides))


def pks(items, key="id"):
    """Sorted list of one property over a list of documents (order-insensitive compare)."""
    return sorted(i[key] for i in items)


# --------------------------------------------------------------------------- #
# Raw HTTP with a master-key Authorization header (algorithm from azure/cosmos/auth.py)
# --------------------------------------------------------------------------- #
def _sign(verb, resource_type, resource_link, x_date, key=KEY):
    text = "{}\n{}\n{}\n{}\n\n".format(
        verb.lower(), resource_type.lower(), resource_link, x_date.lower()
    )
    digest = hmac.new(base64.b64decode(key), text.encode("utf-8"), hashlib.sha256).digest()
    sig = base64.b64encode(digest).decode()
    return urllib.parse.quote(f"type=master&ver=1.0&sig={sig}", safe="")


class RawCosmos:
    """Minimal signed REST client for the handful of raw HTTP tests.

    `resource_link` is the name-based link used for signing and is also the URL
    path of the resource, for example `dbs/db1/colls/c1/docs/doc1`. The
    resource type is the last path segment that is a collection name
    (`docs`, `colls`, ...), as in the SDK.
    """

    def __init__(self, endpoint, ca_file):
        self.endpoint = endpoint.rstrip("/")
        self.verify = ca_file if ca_file else True
        self.session = requests.Session()

    def request(self, verb, path, *, body=None, headers=None, resource_type=None, signed_link=None):
        path = path.lstrip("/")
        parts = [p for p in path.split("/") if p]
        if resource_type is None:
            # "dbs/x" -> dbs ; "dbs/x/colls" -> colls ; "dbs/x/colls/y/docs/z" -> docs
            resource_type = parts[-2] if len(parts) % 2 == 0 else (parts[-1] if parts else "")
        if signed_link is None:
            signed_link = "/".join(parts[:-1]) if len(parts) % 2 == 1 and parts else "/".join(parts)
        x_date = formatdate(usegmt=True)
        hdrs = {
            "x-ms-date": x_date,
            "x-ms-version": "2020-07-15",
            "authorization": _sign(verb, resource_type, signed_link, x_date),
            "accept": "application/json",
        }
        if headers:
            hdrs.update(headers)
        data = body
        if isinstance(body, (dict, list)):
            data = json.dumps(body)
            hdrs.setdefault("content-type", "application/json")
        return self.session.request(
            verb, f"{self.endpoint}/{path}", data=data, headers=hdrs, verify=self.verify, timeout=30
        )

    def get(self, path, **kw):
        return self.request("GET", path, **kw)

    def post(self, path, **kw):
        return self.request("POST", path, **kw)

    def put(self, path, **kw):
        return self.request("PUT", path, **kw)

    def delete(self, path, **kw):
        return self.request("DELETE", path, **kw)


# --------------------------------------------------------------------------- #
# query helpers
# --------------------------------------------------------------------------- #
def run(container, query, parameters=None, **kw):
    """Run a query across partitions (unless a partition_key is given) and return a list."""
    if "partition_key" not in kw:
        kw.setdefault("enable_cross_partition_query", True)
    if parameters is not None:
        kw["parameters"] = parameters
    return list(container.query_items(query=query, **kw))


def ids(items):
    """Sorted ids of documents (queries without ORDER BY have no defined order)."""
    return sorted(i["id"] for i in items)


def idlist(items):
    """Ids in returned order (for ORDER BY queries)."""
    return [i["id"] for i in items]
