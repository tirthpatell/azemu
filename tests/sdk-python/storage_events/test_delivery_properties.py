"""Event Grid delivery properties, dead letters written as blobs, Azure Function destinations and identities on destinations.

The first group runs against the emulator and against real Azure (`AZEMU_AZURE=1`, see `tests/sdk-python/README.md`): the same
events go to a custom topic whose subscriptions have the delivery properties Azure was given, and the Service Bus messages are
compared with what Azure delivered in November 2026. The rest is the emulator's own (its admin API, the delivery log).

On Azure the topic, its subscriptions and the queues exist already (made by the person who owns the resources); the tests only
publish and receive. Variables: AZ_EVENTGRID_ENDPOINT, AZ_EVENTGRID_KEY, AZ_EVENTGRID_PROPS_QUEUE, AZ_EVENTGRID_SESSION_QUEUE,
AZ_SERVICEBUS_CONN_STR. The three subscriptions are filtered by the start of the subject (`props/`, `session/`).
"""

import json
import os
import threading
import time
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.eventgrid import EventGridEvent, EventGridPublisherClient
from azure.servicebus import NEXT_AVAILABLE_SESSION, ServiceBusClient, ServiceBusReceiveMode
from azure.servicebus.exceptions import OperationTimeoutError
from azure.servicebus.management import ServiceBusAdministrationClient
from azure.storage.blob import BlobServiceClient

import seutil as su

SB_KW = dict(retry_total=1, retry_backoff_factor=0.1, retry_backoff_max=0.5, try_timeout=15)

#  the mappings of the subscription Azure was given, in the flat admin form of the emulator
PROPS_MAPPINGS = [
    {"name": "x-static", "type": "Static", "value": "abc"},
    {"name": "x-dyn", "type": "Dynamic", "sourceField": "data.key1"},
    {"name": "MessageId", "type": "Dynamic", "sourceField": "data.mid"},
    {"name": "PartitionKey", "type": "Dynamic", "sourceField": "data.pk"},
]


def props_of(msg) -> dict:
    out = {}
    for k, v in (msg.application_properties or {}).items():
        out[k.decode() if isinstance(k, bytes) else k] = v.decode() if isinstance(v, bytes) else v
    return out


def text(x):
    return x.decode() if isinstance(x, bytes) else x


class Wiring:
    """The topic, the two Service Bus queues and the way to publish and receive, on the emulator or on Azure."""

    def __init__(self, emu):
        self.emu = emu
        if emu.azure:
            self.endpoint = os.environ["AZ_EVENTGRID_ENDPOINT"]
            self.key = os.environ["AZ_EVENTGRID_KEY"]
            self.props_queue = os.environ["AZ_EVENTGRID_PROPS_QUEUE"]
            self.session_queue = os.environ["AZ_EVENTGRID_SESSION_QUEUE"]
            self.sb = ServiceBusClient.from_connection_string(os.environ["AZ_SERVICEBUS_CONN_STR"].rstrip(";"), **SB_KW)
            return
        topic = su.uniq("dp")
        su.admin(emu, "PUT", f"/topics/{topic}", {}, expect=[200, 201])
        self.endpoint = emu.url("eventgrid", topic) + "/api/events"
        self.key = "c2VjcmV0"
        self.props_queue, self.session_queue = su.uniq("props"), su.uniq("session")
        conn = su.sb_conn_str(emu)
        admin = ServiceBusAdministrationClient.from_connection_string(
            conn, connection_verify=emu.ca_file, retry_total=0, connection_timeout=5, read_timeout=20
        )
        admin.create_queue(self.props_queue)
        admin.create_queue(self.session_queue, requires_session=True)
        self.sb = ServiceBusClient.from_connection_string(conn, **SB_KW)
        dest = lambda q, maps: {"endpointType": "ServiceBusQueue", "namespace": su.SB_NAMESPACE, "queueName": q, "deliveryAttributeMappings": maps}
        for name, prefix, q, maps in (
            ("props", "props/", self.props_queue, PROPS_MAPPINGS),
            ("session", "session/", self.session_queue, [{"name": "SessionId", "type": "Dynamic", "sourceField": "data.sid"}]),
        ):
            su.admin(
                emu, "PUT", f"/topics/{topic}/subscriptions/{name}",
                {"destination": dest(q, maps), "filter": {"subjectBeginsWith": prefix},
                 "retryPolicy": {"maxDeliveryAttempts": 2, "retryIntervalsMs": [50]}},
                expect=[200, 201],
            )

    def publish(self, subject: str, data: dict) -> str:
        eid = str(uuid.uuid4())
        with EventGridPublisherClient(self.endpoint, AzureKeyCredential(self.key), retry_total=1) as c:
            c.send([EventGridEvent(id=eid, subject=subject, event_type="Probe.Event", data=data, data_version="1.0")])
        return eid

    def receive_props(self, n: int, timeout: float = 40):
        """Up to `n` messages of the props queue, by event id."""
        got = {}
        deadline = time.monotonic() + timeout
        with self.sb.get_queue_receiver(self.props_queue, receive_mode=ServiceBusReceiveMode.RECEIVE_AND_DELETE) as rx:
            while len(got) < n and time.monotonic() < deadline:
                for m in rx.receive_messages(max_message_count=10, max_wait_time=3):
                    got[json.loads(b"".join(m.body))["id"]] = m
        return got

    def receive_session(self, session_id: str, timeout: float = 40):
        deadline = time.monotonic() + timeout
        with self.sb.get_queue_receiver(
            self.session_queue, session_id=session_id, receive_mode=ServiceBusReceiveMode.RECEIVE_AND_DELETE
        ) as rx:
            while time.monotonic() < deadline:
                for m in rx.receive_messages(max_message_count=10, max_wait_time=3):
                    return m
        return None


@pytest.fixture
def wiring(emu):
    w = Wiring(emu)
    yield w
    w.sb.close()


# What Azure delivered for these events (November 2026): the mapped MessageId and PartitionKey, the x-dyn property, or nothing
# where the source is missing, null, an object or an array. Numbers are written as decimal text, true as "True".
CASES = {
    "full": ({"key1": "v1", "mid": "m1", "pk": "p1"}, "m1", "p1", "v1"),
    "nokey1": ({"mid": "m2", "pk": "p2"}, "m2", "p2", None),
    "numbers": ({"key1": 42, "mid": 7, "pk": 3.5}, "7", "3.5", "42"),
    "floats": ({"key1": 1.0, "mid": -5, "pk": 1e21}, "-5", "1000000000000000000000", "1.0"),
    "objects": ({"key1": {"a": 1}, "mid": {"b": [1, 2]}, "pk": {"c": "d"}}, "DEFAULT", None, None),
    "allmissing": ({"other": 1}, "DEFAULT", None, None),
    "bool_null_arr": ({"key1": True, "mid": None, "pk": [1, "x"]}, "DEFAULT", None, "True"),
    "empty_key1": ({"key1": "", "mid": "m9", "pk": "p9"}, "m9", "p9", ""),
    "unicode": ({"key1": "héllo ☃", "mid": "m-" + "y" * 100, "pk": "p k/with space"}, "m-" + "y" * 100, "p k/with space", "héllo ☃"),
}


def test_delivery_properties_from_the_event(wiring):
    ids = {name: wiring.publish(f"props/{name}", case[0]) for name, case in CASES.items()}
    got = wiring.receive_props(len(CASES))
    assert set(got) == set(ids.values()), "every event of these cases arrives"
    for name, (_data, mid, pk, dyn) in CASES.items():
        m = got[ids[name]]
        p = props_of(m)
        assert p["x-static"] == "abc", name
        assert p.get("x-dyn") == dyn, name
        if mid == "DEFAULT":
            # Learn: "The default value of MessageId is the internal ID of the Event Grid event." (a different guid on Azure)
            assert m.message_id and not m.message_id.startswith("m"), name
        else:
            assert m.message_id == mid, name
        assert m.partition_key == pk, name
        assert p["aeg-event-type"] == "Notification", name
        # the mapped system properties are not also application properties
        assert "MessageId" not in p and "PartitionKey" not in p, name
        assert m.content_type == "application/json; charset=utf-8", name


def test_a_value_over_4096_bytes_leaves_the_property_out_and_an_empty_message_id_is_refused(wiring):
    long_id = wiring.publish("props/long", {"key1": "K" * 5000, "mid": "m12", "pk": "p"})
    bad_id = wiring.publish("props/emptymid", {"key1": "a", "mid": "", "pk": "p"})
    got = wiring.receive_props(1)
    assert set(got) == {long_id}
    assert "x-dyn" not in props_of(got[long_id])
    assert got[long_id].message_id == "m12"
    # Service Bus refuses an empty MessageId: that event is never delivered
    time.sleep(3)
    assert bad_id not in wiring.receive_props(1, timeout=5)


def test_session_id_from_the_event_for_a_session_queue(wiring):
    wiring.publish("session/a", {"sid": "sess-a"})
    wiring.publish("session/n", {"sid": 12})
    m = wiring.receive_session("sess-a")
    assert m is not None and m.session_id == "sess-a"
    assert json.loads(b"".join(m.body))["subject"] == "session/a"
    m = wiring.receive_session("12")  # a number is taken as its text
    assert m is not None and m.session_id == "12"


def test_events_without_a_usable_session_id_are_not_delivered(wiring):
    for name, data in (("nosid", {"x": 1}), ("emptysid", {"sid": ""}), ("objsid", {"sid": {"a": 1}})):
        wiring.publish(f"session/{name}", data)
    wiring.publish("session/ok", {"sid": "sess-ok"})
    # only the good one arrives; a session queue refuses a message without a session id
    assert wiring.receive_session("sess-ok") is not None
    # no other session has a message: asking for the next one times out (on Azure too)
    with pytest.raises(OperationTimeoutError):
        with wiring.sb.get_queue_receiver(
            wiring.session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=5, receive_mode=ServiceBusReceiveMode.RECEIVE_AND_DELETE
        ) as rx:
            rx.receive_messages(max_message_count=5, max_wait_time=3)


# ---------------------------------------------------------------------------------------------------------------------
# The emulator's own: its admin API and the Service Bus and Event Hubs it fronts.
class Func(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        self.server.seen.append((self.path, dict((k.lower(), v) for k, v in self.headers.items()), raw))
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()


@pytest.fixture
def functions_host():
    srv = ThreadingHTTPServer(("127.0.0.1", 0), Func)
    srv.seen = []
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    yield srv, f"http://127.0.0.1:{srv.server_address[1]}"
    srv.shutdown()
    srv.server_close()


@pytest.mark.emulator_only
def test_azure_function_destination_goes_to_the_functions_host(emu, container, functions_host):
    srv, base = functions_host
    app, fn = su.uniq("app"), "OnBlob"
    rid = f"/subscriptions/{uuid.uuid4()}/resourceGroups/rg/providers/Microsoft.Web/sites/{app}/functions/{fn}"
    view = su.subscribe(emu, "fn", {
        "destination": {"endpointType": "AzureFunction", "properties": {"resourceId": rid, "deliveryAttributeMappings": [
            {"name": "x-static", "type": "Static", "properties": {"value": "abc", "isSecret": True}}]}},
        "retryPolicy": {"maxDeliveryAttempts": 2, "retryIntervalsMs": [50]},
    })
    assert view["validation"]["status"] == "NotRequired"
    assert "value" not in view["deliveryAttributeMappings"][0], "a secret value is not shown"
    # no host is known for the app: the delivery fails with a reason that says what to set
    container.upload_blob("first.txt", b"1")
    dead = su.wait_until(lambda: su.admin(emu, "GET", su.sub_path("fn") + "/deadletters", expect=200)[1]["value"], what="a dead letter")
    log = su.admin(emu, "GET", su.sub_path("fn") + "/deliveries", expect=200)[1]["value"]
    assert app in log[0]["detail"] and "AZEMU_EVENTGRID_FUNCTION_HOSTS" in log[0]["detail"]
    assert dead[0]["reason"] == "MaxDeliveryAttemptsExceeded"
    # tell the emulator where the host runs
    su.admin(emu, "PUT", f"/functions/{app}", {"baseUrl": base}, expect=[200, 201])
    container.upload_blob("second.txt", b"22")
    su.wait_until(lambda: srv.seen, what="a request to the Functions host")
    path, headers, raw = srv.seen[0]
    assert path == f"/runtime/webhooks/EventGrid?functionName={fn}"
    assert headers["aeg-event-type"] == "Notification"
    assert headers["x-static"] == "abc"
    body = json.loads(raw)
    assert isinstance(body, list) and body[0]["eventType"] == "Microsoft.Storage.BlobCreated"


@pytest.mark.emulator_only
def test_webhook_gets_delivery_properties_as_headers(emu, container, functions_host):
    srv, base = functions_host
    su.subscribe(emu, "hook", {
        "destination": {"endpointType": "WebHook", "endpointUrl": base + "/hook", "deliveryAttributeMappings": [
            {"name": "x-static", "type": "Static", "value": "abc"},
            {"name": "x-ct", "type": "Dynamic", "sourceField": "data.contentLength"},
            {"name": "x-none", "type": "Dynamic", "sourceField": "data.nothing"}]},
        "skipValidation": True,
    })
    container.upload_blob("h.txt", b"12345")
    su.wait_until(lambda: srv.seen, what="a webhook call")
    _path, headers, _raw = srv.seen[0]
    assert headers["x-static"] == "abc" and headers["x-ct"] == "5"
    assert "x-none" not in headers


@pytest.mark.emulator_only
def test_dead_letters_are_blobs_in_the_named_container(emu, container, blob_service):
    dl = blob_service.create_container(su.uniq("dead"))
    su.subscribe(emu, "dl", {
        **su.queue_dest("no-such-queue"),
        "deadLetterDestination": {"endpointType": "StorageBlob", "properties": {
            "resourceId": f"/subscriptions/{uuid.uuid4()}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/{su.ACCOUNT}",
            "blobContainerName": dl.container_name}},
        "retryPolicy": {"maxDeliveryAttempts": 2, "retryIntervalsMs": [30]},
    })
    container.upload_blob("lost.txt", b"1")
    names = su.wait_until(lambda: [b.name for b in dl.list_blobs()], what="a dead-letter blob")
    assert len(names) == 1
    parts = names[0].split("/")
    # {topic}/{SUBSCRIPTION}/{yyyy}/{MM}/{dd}/{HH}/{guid}.json (Azure, November 2026)
    assert parts[1] == "DL" and len(parts) == 7 and parts[-1].endswith(".json")
    blob = dl.get_blob_client(names[0])
    assert blob.get_blob_properties().content_settings.content_type == "application/json"
    (ev,) = json.loads(blob.download_blob().readall())
    assert ev["deadLetterReason"] == "MaxDeliveryAttemptsExceeded"
    assert ev["deliveryAttempts"] == 2
    assert ev["lastDeliveryOutcome"] == "NotFound"
    assert ev["subject"] == su.subject_of(container.container_name, "lost.txt")
    assert list(ev)[:7] == ["id", "eventTime", "eventType", "dataVersion", "metadataVersion", "topic", "subject"]
    assert {"publishTime", "lastDeliveryAttemptTime"} <= set(ev)
    # the list of dead letters of the emulator is kept as well
    assert su.admin(emu, "GET", su.sub_path("dl") + "/deadletters", expect=200)[1]["value"]
    # the dead letter is no blob event: a subscription on the account sees the upload only
    time.sleep(0.5)
    assert [b.name for b in dl.list_blobs()] == names


@pytest.mark.emulator_only
def test_identity_blocks_are_stored_and_delivery_is_the_same(emu, container, sb):
    client, admin = sb
    q = su.uniq("idq")
    admin.create_queue(q)
    ident = {"type": "UserAssigned", "userAssignedIdentity": "/subscriptions/s/resourceGroups/r/providers/Microsoft.ManagedIdentity/userAssignedIdentities/u"}
    view = su.subscribe(emu, "ident", {
        "deliveryWithResourceIdentity": {"identity": ident, "destination": {
            "endpointType": "ServiceBusQueue", "namespace": su.SB_NAMESPACE, "queueName": q}},
    })
    assert view["deliveryWithResourceIdentity"]["identity"] == ident
    assert view["deliveryWithResourceIdentity"]["destination"]["queueName"] == q
    container.upload_blob("i.txt", b"1")
    with client.get_queue_receiver(q) as rx:
        (m,) = su.wait_until(lambda: rx.receive_messages(max_message_count=1, max_wait_time=2), what="a message")
    assert json.loads(b"".join(m.body))["eventType"] == "Microsoft.Storage.BlobCreated"


@pytest.mark.emulator_only
def test_event_hub_takes_the_mapped_partition_key(emu, container):
    from test_destinations import create_hub, receive_from_hub

    hub = su.uniq("hub")
    create_hub(emu, hub, 4)
    su.subscribe(emu, "keyed", {"destination": {
        "endpointType": "EventHub", "namespace": su.SB_NAMESPACE, "eventHubName": hub,
        "deliveryAttributeMappings": [
            {"name": "PartitionKey", "type": "Dynamic", "sourceField": "subject"},
            {"name": "x-static", "type": "Static", "value": "abc"}]}})
    for i in range(3):
        container.upload_blob("same.txt", b"v%d" % i, overwrite=True)
    events = receive_from_hub(emu, hub, 3)
    # the subject is the same for the three events: one partition key, so one partition
    assert len({e.partition_key for e in events}) == 1
    assert text(events[0].partition_key) == su.subject_of(container.container_name, "same.txt")
    p = {text(k): text(v) for k, v in events[0].properties.items()}
    assert p["x-static"] == "abc" and "PartitionKey" not in p


@pytest.fixture
def sb(emu):
    client = ServiceBusClient.from_connection_string(su.sb_conn_str(emu), **SB_KW)
    admin = ServiceBusAdministrationClient.from_connection_string(
        su.sb_conn_str(emu), connection_verify=emu.ca_file, retry_total=0, connection_timeout=5, read_timeout=20
    )
    yield client, admin
    client.close()
