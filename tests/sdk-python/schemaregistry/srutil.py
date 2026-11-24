"""Helpers for the Schema Registry suite (azure-schemaregistry 1.3.0, azure-schemaregistry-avroencoder 1.0.0).

The registry is served on the Event Hubs namespace host (`https://<namespace>.eventhub.localhost:<port>/$schemaGroups/...`; the
SDK only sends a bearer token over TLS, so every client here trusts the emulator's CA). A group is not made through the data plane: on
the emulator the admin API makes it (`ensure_group`), on Azure it is the namespace's one group (`AZ_SCHEMA_GROUP`, Avro, Forward).

With AZEMU_AZURE=1 the tests run against a real namespace (tests/sdk-python/README.md): `AZ_EVENTHUBS_NAMESPACE`, `AZ_SCHEMA_GROUP`,
`AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` (a principal with the role Schema Registry Contributor). Every schema a test
makes has a name of its own and is deleted when the test ends (the REST API has a DELETE that the SDKs do not call).
"""

import json
import os
import sys
import time
import urllib.request
import uuid
from pathlib import Path
from urllib.parse import urlparse

# the Event Hubs suite's helpers (hubs, producers and consumers for the round trip)
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "eventhubs"))

import azure_eh  # noqa: E402
import requests  # noqa: E402
from azure.core.credentials import AccessToken  # noqa: E402

AZURE = azure_eh.enabled()
NS = "myns"
SCOPE = "https://eventhubs.azure.net/.default"
EMU_GROUP = "azemu-group"
API = "2023-07-01"
AVRO = "application/json; serialization=Avro"


def uniq(prefix: str = "azemu-sr") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


class FakeCredential:
    """Any token passes the emulator."""

    def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    def close(self):
        pass


class FakeAsyncCredential:
    async def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)

    async def close(self):
        pass

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        pass


def credential(emu, aio: bool = False):
    if not emu.azure:
        return FakeAsyncCredential() if aio else FakeCredential()
    if aio:
        from azure.identity.aio import ClientSecretCredential
    else:
        from azure.identity import ClientSecretCredential
    return ClientSecretCredential(os.environ["AZURE_TENANT_ID"], os.environ["AZURE_CLIENT_ID"], os.environ["AZURE_CLIENT_SECRET"])


def fqdn(emu) -> str:
    """`<namespace>.eventhub.localhost:<port>` on the emulator, `<namespace>.servicebus.windows.net` on Azure."""
    return azure_eh.fqdn() if emu.azure else urlparse(emu.url("eventhub", NS)).netloc


def client_kwargs(emu) -> dict:
    return {} if emu.azure else {"connection_verify": emu.ca_file}


def make_client(emu, **kw):
    from azure.schemaregistry import SchemaRegistryClient

    return SchemaRegistryClient(fully_qualified_namespace=fqdn(emu), credential=credential(emu), **client_kwargs(emu), **kw)


def group_name(emu) -> str:
    return os.environ["AZ_SCHEMA_GROUP"] if emu.azure else EMU_GROUP


def ensure_group(emu, name: str = EMU_GROUP, mode: str = "Forward", kind: str = "Avro") -> str:
    """The emulator's group (made through its admin API); on Azure the namespace's one group, which is not made or changed."""
    if emu.azure:
        return os.environ["AZ_SCHEMA_GROUP"]
    body = json.dumps({"schemaType": kind, "schemaCompatibility": mode}).encode()
    path = f"/_azemu/eventhubs/{NS}/schemagroups/{name}"
    req = urllib.request.Request(emu.gateway + path, data=body, method="PUT", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(req, timeout=10).read()
    return name


_token = {"value": None, "expires": 0.0}


def _bearer(emu) -> str:
    if not emu.azure:
        return "azemu-test-token"
    if _token["expires"] < time.time() + 60:
        t = credential(emu).get_token(SCOPE)
        _token.update(value=t.token, expires=t.expires_on)
    return _token["value"]


def http(emu, method: str, path: str, *, body=None, ctype=None, api=API, token=True, headers=None, timeout=60):
    """A raw request to the registry (`path` starts with `/$schemaGroups`); `api=None` sends no api-version."""
    base = f"https://{fqdn(emu)}" if emu.azure else emu.https_url("eventhub", NS)
    url = base + path
    if api is not None:
        url += ("&" if "?" in url else "?") + f"api-version={api}"
    h = {}
    if token:
        h["Authorization"] = f"Bearer {_bearer(emu)}"
    if ctype:
        h["Content-Type"] = ctype
    h.update(headers or {})
    data = body.encode() if isinstance(body, str) else body
    return requests.request(method, url, data=data, headers=h, verify=True if emu.azure else emu.ca_file, timeout=timeout)


def error_text(r) -> str:
    """The message of a JSON error without the tracking suffix (`TrackingId:...`), which differs on every call."""
    return r.json()["error"]["message"].split(" TrackingId:")[0]


def delete_schema(emu, group: str, name: str) -> None:
    """DELETE on a schema takes away its latest version only (Azure and the emulator): repeat until no version is left."""
    for _ in range(30):
        if not http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions").json()["Value"]:
            return
        r = http(emu, "DELETE", f"/$schemaGroups/{group}/schemas/{name}")
        assert r.status_code == 204, (r.status_code, r.text[:200])
    raise AssertionError(f"schema {name} still has versions after 30 deletes")


def order_schema(name: str, extra_fields=()) -> str:
    """An Avro record `azemu.sr.<name>` with an `id` and the fields given (name, type) as compact JSON: what the registry hands back."""
    fields = [{"name": "id", "type": "string"}] + [{"name": n, "type": t} for n, t in extra_fields]
    return json.dumps({"type": "record", "name": name, "namespace": "azemu.sr", "fields": fields}, separators=(",", ":"))


# -- Event Hubs for the round trip ------------------------------------------------------------------------------
# Not the helpers of the Event Hubs suite: against Azure those use a shared access key, and a namespace may have those turned off. Here a
# client on Azure signs in with the same principal as the registry's.

def create_hub(emu, name: str, request) -> str:
    """The event hub of a round trip. On the emulator a one-partition hub made through the admin API. On Azure the namespace's existing
    hub `AZ_EVENTHUBS_AUTH_HUB` (a test of this suite never calls Resource Manager); its events are told apart by the marker property
    of `tag` and by the time they were sent."""
    if emu.azure:
        return os.environ["AZ_EVENTHUBS_AUTH_HUB"]
    body = json.dumps({"partitionCount": 1}).encode()
    req = urllib.request.Request(emu.gateway + f"/_azemu/eventhubs/{NS}/hubs/{name}", data=body, method="PUT", headers={"Content-Type": "application/json"})
    urllib.request.urlopen(req, timeout=10).read()
    return name


def tag(event, marker: str):
    """Mark an event as this test's (a shared hub holds the events of other runs)."""
    event.properties = {**(event.properties or {}), "azemu-test": marker}
    return event


def start_position(emu):
    """Where a receiver starts: the start of the new hub of the emulator; on Azure a moment before now."""
    import datetime

    return "-1" if not emu.azure else datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=30)


def _eh_client(emu, hub: str, cls, **kw):
    if emu.azure:
        return cls(fully_qualified_namespace=azure_eh.fqdn(), eventhub_name=hub, credential=credential(emu), retry_total=1, **kw)
    host = urlparse(emu.url("eventhub", NS)).netloc
    cs = f"Endpoint=sb://{host};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0c2VjcmV0;UseDevelopmentEmulator=true"
    return cls.from_connection_string(cs, eventhub_name=hub, retry_total=1, **kw)


def send(emu, hub: str, events) -> None:
    from azure.eventhub import EventHubProducerClient

    with _eh_client(emu, hub, EventHubProducerClient) as p:
        batch = p.create_batch()
        for e in events:
            batch.add(e)
        p.send_batch(batch)


def _marker_of(event):
    """The marker of `tag`, however the SDK hands the property back (text or bytes)."""
    for k, v in (event.properties or {}).items():
        if (k.decode() if isinstance(k, bytes) else k) == "azemu-test":
            return v.decode() if isinstance(v, bytes) else v
    return None


def receive(emu, hub: str, n: int, marker: str, since, timeout: float = 30.0) -> list:
    """The first `n` events tagged `marker`, read from `since` (`start_position`) on every partition."""
    import threading

    from azure.eventhub import EventHubConsumerClient

    got, errors = [], []
    client = _eh_client(emu, hub, EventHubConsumerClient, consumer_group="$Default")

    def run():
        try:
            client.receive(lambda ctx, ev: got.append(ev) if ev is not None and _marker_of(ev) == marker else None,
                           starting_position=since, max_wait_time=1,
                           on_error=lambda ctx, err: errors.append(err))
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    t = threading.Thread(target=run, daemon=True)
    t.start()
    deadline = time.monotonic() + timeout
    while len(got) < n and time.monotonic() < deadline:
        time.sleep(0.05)
    client.close()
    t.join(5)
    assert len(got) >= n, f"expected {n} events, got {len(got)}; errors={errors!r}"
    return got
