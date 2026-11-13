"""Helpers for the storage-events suite."""

import base64
import json
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACCOUNT = "evtest"
KEY = "c2VjcmV0c2VjcmV0c2VjcmV0"  # any base64 value: the emulator accepts the signature of every account but the dev one

WAIT = 8.0


def uniq(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


# -- the emulator's admin API ---------------------------------------------------------------------------------------
def admin(emu, method: str, path: str, body=None, *, expect=None):
    """Call /_azemu/eventgrid{path}. Returns (status, parsed JSON or None)."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        emu.gateway + "/_azemu/eventgrid" + path, data=data, method=method, headers={"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            status, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read()
    parsed = json.loads(raw) if raw else None
    if expect is not None:
        assert status in (expect if isinstance(expect, (list, tuple)) else [expect]), (status, parsed)
    return status, parsed


def sub_path(name: str = None, account: str = ACCOUNT) -> str:
    base = f"/sources/storage/{account}/subscriptions"
    return base if name is None else f"{base}/{name}"


def subscribe(emu, name: str, body: dict, account: str = ACCOUNT) -> dict:
    status, view = admin(emu, "PUT", sub_path(name, account), body, expect=[200, 201])
    return view


def queue_dest(queue: str, **extra) -> dict:
    return {"destination": {"endpointType": "StorageQueue", "storageAccount": ACCOUNT, "queueName": queue, **extra}}


def wait_until(fn, timeout: float = WAIT, what: str = "condition", interval: float = 0.05):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out after {timeout}s waiting for {what} (last value: {last!r})")


# -- a webhook the emulator can call -------------------------------------------------------------------------------
class _Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _reply(self, status=200, body=b"", headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        self.server.owner.handshakes.append(("OPTIONS", dict((k.lower(), v) for k, v in self.headers.items())))
        self._reply(200, headers=[("WebHook-Allowed-Origin", "eventgrid.azure.net")])

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        headers = dict((k.lower(), v) for k, v in self.headers.items())
        owner = self.server.owner
        if headers.get("aeg-event-type") == "SubscriptionValidation":
            code = json.loads(raw)[0]["data"]["validationCode"]
            owner.handshakes.append(("POST", headers))
            return self._reply(200, json.dumps({"validationResponse": code}).encode(), [("Content-Type", "application/json")])
        with owner.lock:
            owner.requests.append((headers, raw))
        self._reply(200)


class Webhook:
    """A local HTTP endpoint on a free port. `events` is every delivered event: the Event Grid schema arrives as a JSON
    array of one event, CloudEvents as the event itself."""

    def __init__(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self.server.owner = self
        self.requests = []
        self.handshakes = []
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/hook"

    def stop(self):
        self.server.shutdown()
        self.server.server_close()

    @property
    def events(self) -> list:
        out = []
        with self.lock:
            for _, raw in self.requests:
                body = json.loads(raw)
                out.extend(body if isinstance(body, list) else [body])
        return out

    def wait_events(self, n: int, timeout: float = WAIT) -> list:
        wait_until(lambda: len(self.events) >= n, timeout, f"{n} events at the webhook")
        return self.events


# -- what the Microsoft schemas say an event looks like -----------------------------------------------------------
# refs/specs/specification/eventgrid/data-plane/EventGridSystemEvents/Microsoft.Storage/Storage.tsp
BLOB_CREATED_DATA = {"api", "clientRequestId", "requestId", "eTag", "contentType", "contentLength", "blobType",
                     "accessTier", "url", "sequencer", "storageDiagnostics"}
BLOB_DELETED_DATA = {"api", "clientRequestId", "requestId", "contentType", "blobType", "url", "sequencer",
                     "storageDiagnostics"}
BLOB_TIER_CHANGED_DATA = {"api", "clientRequestId", "requestId", "contentType", "contentLength", "blobType",
                          "accessTier", "previousTier", "url", "sequencer", "storageDiagnostics"}


def subject_of(container: str, blob: str) -> str:
    return f"/blobServices/default/containers/{container}/blobs/{blob}"


def check_eg_envelope(ev: dict, event_type: str, subject: str, data_version: str = ""):
    """Top-level properties of an Event Grid schema event (Learn: "Azure Blob Storage as an Event Grid source")."""
    assert set(ev) == {"topic", "subject", "eventType", "eventTime", "id", "data", "dataVersion", "metadataVersion"}, ev
    assert ev["topic"].endswith(f"/providers/Microsoft.Storage/storageAccounts/{ACCOUNT}"), ev["topic"]
    assert ev["topic"].startswith("/subscriptions/")
    assert ev["eventType"] == event_type
    assert ev["subject"] == subject
    assert ev["dataVersion"] == data_version
    assert ev["metadataVersion"] == "1"
    assert uuid.UUID(ev["id"])
    assert ev["eventTime"].endswith("Z") and "T" in ev["eventTime"]


def check_sequencer(data: dict):
    assert isinstance(data["sequencer"], str) and data["sequencer"]
    assert uuid.UUID(data["storageDiagnostics"]["batchId"])
    assert uuid.UUID(data["requestId"])


# -- Service Bus / Event Hubs helpers -------------------------------------------------------------------------------
SB_NAMESPACE = "evns"
SB_KEY_NAME = "RootManageSharedAccessKey"


def sb_conn_str(emu, namespace: str = SB_NAMESPACE) -> str:
    from urllib.parse import urlparse

    host = urlparse(emu.url("servicebus", namespace)).netloc
    return f"Endpoint=sb://{host};SharedAccessKeyName={SB_KEY_NAME};SharedAccessKey={KEY};UseDevelopmentEmulator=true"


def eh_conn_str(emu, namespace: str = SB_NAMESPACE) -> str:
    from urllib.parse import urlparse

    host = urlparse(emu.url("eventhub", namespace)).netloc
    return f"Endpoint=sb://{host};SharedAccessKeyName={SB_KEY_NAME};SharedAccessKey={KEY};UseDevelopmentEmulator=true"


def b64_json(text: str) -> dict:
    return json.loads(base64.b64decode(text))
