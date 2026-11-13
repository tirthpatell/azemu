"""Event Grid delivery of Blob Storage events to a Storage queue, a Service Bus queue and topic, and an event hub."""

import json
import threading
import time
import urllib.request
import uuid
from datetime import datetime, timedelta, timezone

import pytest
from azure.eventhub import EventHubConsumerClient
from azure.servicebus import ServiceBusClient
from azure.servicebus.management import ServiceBusAdministrationClient
from azure.storage.queue import QueueClient, TextBase64DecodePolicy

import seutil as su

SB_CLIENT_KW = dict(retry_total=1, retry_backoff_factor=0.1, retry_backoff_max=0.5, try_timeout=15)


def props_of(msg) -> dict:
    out = {}
    for k, v in (msg.application_properties or {}).items():
        out[k.decode() if isinstance(k, bytes) else k] = v.decode() if isinstance(v, bytes) else v
    return out


def check_blob_created_event(ev, container, blob_name, size):
    su.check_eg_envelope(ev, "Microsoft.Storage.BlobCreated", su.subject_of(container, blob_name))
    assert ev["data"]["api"] == "PutBlob"
    assert ev["data"]["contentLength"] == size
    assert ev["data"]["blobType"] == "BlockBlob"


# -- Storage queue ---------------------------------------------------------------------------------------------------
def make_queue(emu, name, policy=None):
    kw = {"message_decode_policy": policy} if policy else {}
    return QueueClient.from_connection_string(emu.storage_conn_str(su.ACCOUNT, su.KEY), name, **kw)


def test_storage_queue_gets_the_event_base64_encoded(emu, container):
    qname = su.uniq("events")
    plain = make_queue(emu, qname)
    plain.create_queue()
    su.subscribe(emu, "toq", su.queue_dest(qname))
    container.upload_blob("q.txt", b"queued!")

    msgs = su.wait_until(lambda: list(plain.peek_messages(max_messages=5)), what="a queue message")
    assert len(msgs) == 1
    # stored as base64 text (what Event Grid writes; the decoding policy of the SDK reads it back)
    ev = su.b64_json(msgs[0].content)
    check_blob_created_event(ev, container.container_name, "q.txt", 7)
    # a single event object (Event Grid schema), not an array
    assert isinstance(ev, dict)

    decoded = make_queue(emu, qname, TextBase64DecodePolicy())
    (m,) = decoded.receive_messages(max_messages=1)
    assert json.loads(m.content) == ev


def test_storage_queue_default_time_to_live_is_seven_days(emu, container):
    qname = su.uniq("ttl")
    q = make_queue(emu, qname)
    q.create_queue()
    su.subscribe(emu, "toq", su.queue_dest(qname))
    container.upload_blob("t.txt", b"1")
    (m,) = su.wait_until(lambda: list(q.peek_messages(max_messages=5)), what="a queue message")
    # Learn (delivery properties): "If no time is provided, the message's default time to live is 7 days."
    assert m.expires_on - m.inserted_on == timedelta(days=7)


def test_storage_queue_time_to_live_setting(emu, container):
    short, never = su.uniq("short"), su.uniq("never")
    for n in (short, never):
        make_queue(emu, n).create_queue()
    su.subscribe(emu, "short", su.queue_dest(short, queueMessageTimeToLiveInSeconds=3600))
    su.subscribe(emu, "never", su.queue_dest(never, queueMessageTimeToLiveInSeconds=-1))
    container.upload_blob("t.txt", b"1")
    (a,) = su.wait_until(lambda: list(make_queue(emu, short).peek_messages(max_messages=5)), what="short message")
    (b,) = su.wait_until(lambda: list(make_queue(emu, never).peek_messages(max_messages=5)), what="never message")
    assert a.expires_on - a.inserted_on == timedelta(hours=1)
    assert b.expires_on.year == 9999  # -1: the message never expires


def test_storage_queue_arm_style_destination_and_cloudevents(emu, container):
    qname = su.uniq("arm")
    q = make_queue(emu, qname, TextBase64DecodePolicy())
    q.create_queue()
    resource = f"/subscriptions/{uuid.uuid4()}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/{su.ACCOUNT}"
    su.subscribe(emu, "arm", {
        "destination": {"endpointType": "StorageQueue", "properties": {"resourceId": resource, "queueName": qname}},
        "deliverySchema": "CloudEventSchemaV1_0",
    })
    container.upload_blob("arm.txt", b"abc")
    (m,) = su.wait_until(lambda: list(q.receive_messages(max_messages=1)), what="a queue message")
    ev = json.loads(m.content)
    assert ev["specversion"] == "1.0" and ev["type"] == "Microsoft.Storage.BlobCreated"
    assert ev["subject"] == su.subject_of(container.container_name, "arm.txt")
    assert ev["data"]["contentLength"] == 3


def test_storage_queue_filter_and_two_subscriptions(emu, blob_service):
    c = blob_service.create_container(su.uniq("c"))
    qa, qb = su.uniq("qa"), su.uniq("qb")
    for n in (qa, qb):
        make_queue(emu, n).create_queue()
    su.subscribe(emu, "a", su.queue_dest(qa))
    su.subscribe(emu, "b", {**su.queue_dest(qb), "filter": {"includedEventTypes": ["Microsoft.Storage.BlobDeleted"]}})
    blob = c.get_blob_client("x")
    blob.upload_blob(b"1")
    blob.delete_blob()
    su.wait_until(lambda: len(list(make_queue(emu, qa).peek_messages(max_messages=10))) == 2, what="two events in queue a")
    time.sleep(0.3)
    (only,) = make_queue(emu, qb).peek_messages(max_messages=10)
    assert su.b64_json(only.content)["eventType"] == "Microsoft.Storage.BlobDeleted"


def test_missing_queue_is_retried_then_dead_lettered(emu, container):
    su.subscribe(emu, "lost", {**su.queue_dest("no-such-queue"), "retryPolicy": {"maxDeliveryAttempts": 3, "retryIntervalsMs": [30]}})
    container.upload_blob("l.txt", b"1")
    path = su.sub_path("lost") + "/deadletters"
    dead = su.wait_until(lambda: su.admin(emu, "GET", path, expect=200)[1]["value"], what="a dead letter")
    assert dead[0]["reason"] == "MaxDeliveryAttemptsExceeded"
    assert dead[0]["deliveryCount"] == 3
    log = su.admin(emu, "GET", su.sub_path("lost") + "/deliveries", expect=200)[1]["value"]
    assert [a["outcome"] for a in log] == ["Failure", "Failure", "DeadLettered"]
    assert "QueueNotFound" in log[0]["detail"]


# -- Service Bus ---------------------------------------------------------------------------------------------------------
@pytest.fixture
def sb(emu):
    client = ServiceBusClient.from_connection_string(su.sb_conn_str(emu), **SB_CLIENT_KW)
    admin = ServiceBusAdministrationClient.from_connection_string(
        su.sb_conn_str(emu), connection_verify=emu.ca_file, retry_total=0, connection_timeout=5, read_timeout=20
    )
    yield client, admin
    client.close()


def sb_dest(kind, **names):
    return {"destination": {"endpointType": kind, "namespace": su.SB_NAMESPACE, **names}}


def test_service_bus_queue_gets_the_event(emu, container, sb):
    client, admin = sb
    qname = su.uniq("sbq")
    admin.create_queue(qname)
    view = su.subscribe(emu, "tosb", sb_dest("ServiceBusQueue", queueName=qname))
    assert view["validation"]["status"] == "NotRequired"
    container.upload_blob("sb.txt", b"to service bus")
    with client.get_queue_receiver(qname) as rx:
        (msg,) = su.wait_until(lambda: rx.receive_messages(max_message_count=1, max_wait_time=2), what="a Service Bus message")
        rx.complete_message(msg)
    ev = json.loads(b"".join(msg.body))
    check_blob_created_event(ev, container.container_name, "sb.txt", 14)
    # Learn "Event handlers / Service Bus": the event is the body; aeg-* properties; MessageId is the event id
    assert msg.content_type == "application/json"
    assert msg.message_id == ev["id"]
    p = props_of(msg)
    assert p["aeg-event-type"] == "Notification"
    assert p["aeg-subscription-name"] == "tosb"
    assert p["aeg-delivery-count"] == "0"
    assert p["aeg-metadata-version"] == "1"
    assert p["aeg-data-version"] == ""
    assert p["aeg-output-event-id"] == ev["id"]


def test_service_bus_queue_cloudevents_content_type(emu, container, sb):
    client, admin = sb
    qname = su.uniq("sbce")
    admin.create_queue(qname)
    su.subscribe(emu, "ce", {**sb_dest("ServiceBusQueue", queueName=qname), "deliverySchema": "CloudEventSchemaV1_0"})
    container.upload_blob("ce.txt", b"1")
    with client.get_queue_receiver(qname) as rx:
        (msg,) = su.wait_until(lambda: rx.receive_messages(max_message_count=1, max_wait_time=2), what="a message")
    assert msg.content_type.startswith("application/cloudevents+json")
    ev = json.loads(b"".join(msg.body))
    assert ev["specversion"] == "1.0" and ev["type"] == "Microsoft.Storage.BlobCreated"


def test_service_bus_topic_subscription_gets_the_event(emu, container, sb):
    client, admin = sb
    topic, subname = su.uniq("sbt"), "reader"
    admin.create_topic(topic)
    admin.create_subscription(topic, subname)
    su.subscribe(emu, "totopic", sb_dest("ServiceBusTopic", topicName=topic))
    b = container.get_blob_client("topic.txt")
    b.upload_blob(b"12345", overwrite=True)
    b.delete_blob()
    with client.get_subscription_receiver(topic, subname) as rx:
        got = []
        su.wait_until(lambda: got.extend(rx.receive_messages(max_message_count=5, max_wait_time=2)) or len(got) >= 2, what="two messages")
        for m in got:
            rx.complete_message(m)
    types = sorted(json.loads(b"".join(m.body))["eventType"] for m in got)
    assert types == ["Microsoft.Storage.BlobCreated", "Microsoft.Storage.BlobDeleted"]


def test_service_bus_missing_queue_goes_to_dead_letters(emu, container):
    su.subscribe(emu, "lost", {**sb_dest("ServiceBusQueue", queueName="not-there"), "retryPolicy": {"maxDeliveryAttempts": 2, "retryIntervalsMs": [30]}})
    container.upload_blob("l.txt", b"1")
    dead = su.wait_until(lambda: su.admin(emu, "GET", su.sub_path("lost") + "/deadletters", expect=200)[1]["value"], what="a dead letter")
    assert dead[0]["reason"] == "MaxDeliveryAttemptsExceeded"


# -- Event Hubs ------------------------------------------------------------------------------------------------------
def receive_from_hub(emu, hub, n, timeout=10):
    got = []
    client = EventHubConsumerClient.from_connection_string(
        su.eh_conn_str(emu), consumer_group="$Default", eventhub_name=hub, retry_total=1, retry_backoff_factor=0.05, retry_backoff_max=0.2
    )

    def on_event(ctx, event):
        if event is not None:
            got.append(event)

    t = threading.Thread(target=lambda: client.receive(on_event=on_event, starting_position="-1", max_wait_time=1), daemon=True)
    t.start()
    try:
        su.wait_until(lambda: len(got) >= n, timeout, f"{n} events from the event hub")
    finally:
        client.close()
        t.join(5)
    return got


def create_hub(emu, hub, partitions=2):
    req = urllib.request.Request(
        emu.gateway + f"/_azemu/eventhubs/{su.SB_NAMESPACE}/hubs/{hub}",
        data=json.dumps({"partitionCount": partitions}).encode(), method="PUT", headers={"Content-Type": "application/json"},
    )
    urllib.request.urlopen(req, timeout=10).read()


def test_event_hub_gets_the_event(emu, container):
    hub = su.uniq("hub")
    create_hub(emu, hub)
    su.subscribe(emu, "tohub", {"destination": {"endpointType": "EventHub", "namespace": su.SB_NAMESPACE, "eventHubName": hub}})
    container.upload_blob("eh.txt", b"to event hub")
    (event,) = receive_from_hub(emu, hub, 1)
    ev = json.loads(event.body_as_str())
    check_blob_created_event(ev, container.container_name, "eh.txt", 12)
    assert event.content_type == "application/json"
    p = {(k.decode() if isinstance(k, bytes) else k): (v.decode() if isinstance(v, bytes) else v) for k, v in event.properties.items()}
    assert p["aeg-event-type"] == "Notification"
    assert p["aeg-subscription-name"] == "tohub"
    assert p["aeg-output-event-id"] == ev["id"]


def test_event_hub_resource_id_destination(emu, container):
    hub = su.uniq("hub")
    create_hub(emu, hub, 1)
    rid = f"/subscriptions/{uuid.uuid4()}/resourceGroups/rg/providers/Microsoft.EventHub/namespaces/{su.SB_NAMESPACE}/eventhubs/{hub}"
    su.subscribe(emu, "arm", {"destination": {"endpointType": "EventHub", "properties": {"resourceId": rid}}})
    container.upload_blob("eh2.txt", b"1")
    (event,) = receive_from_hub(emu, hub, 1)
    assert json.loads(event.body_as_str())["subject"] == su.subject_of(container.container_name, "eh2.txt")
