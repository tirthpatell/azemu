"""What the emulator adds to Event Grid: auto-creation, the admin API, the config file.

Everything here is `emulator_only`: on Azure, topics and subscriptions are ARM resources.

Nothing is created by being named unless the emulator is started with AZEMU_EVENTGRID_AUTOCREATE=1 (or AZEMU_LENIENT=1). The tests
marked `autocreate` are about that and run in the runner's second pass with the variable set; the `autocreate_off_*` tests start a
separate emulator with the variable at 0 and run by default.
"""

import json
import time

import pytest
import requests
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient

import autocreate_gate
from conftest import make_consumer, make_producer
from egutil import EmuProc

pytestmark = pytest.mark.emulator_only


def ev(data="x"):
    return EventGridEvent(subject="s", event_type="t", data=data, data_version="1")


# -- auto-creation ----------------------------------------------------------------------

@pytest.mark.autocreate
def test_publishing_to_an_unknown_topic_creates_it(emu, admin, name, key):
    with EventGridPublisherClient(emu.url("eventgrid", name) + "/api/events", key, retry_total=0) as publisher:
        publisher.send(ev())
    t = admin.request("GET", f"/topics/{name}")
    assert t.status_code == 200
    body = t.json()
    assert body["kind"] == "custom"
    assert body["published"] == 1


@pytest.mark.autocreate
def test_receiving_from_an_unknown_subscription_creates_it(admin, ns, ns_endpoint, key):
    producer = make_producer(ns_endpoint, key, "auto-topic")
    consumer = make_consumer(ns_endpoint, key, "auto-topic", "auto-sub")
    assert consumer.receive(max_events=1, max_wait_time=1) == []  # creates the subscription
    sub = admin.get_sub("auto-topic", "auto-sub", namespace=ns)
    assert sub["deliveryMode"] == "pull"
    # it gets events published from then on
    producer.send(CloudEvent(source="/s", type="t", data="hello"))
    (d,) = consumer.receive(max_events=1, max_wait_time=3)
    assert d.event.data == "hello"


@pytest.mark.autocreate
def test_events_published_before_the_subscription_existed_are_not_delivered(ns, ns_endpoint, key):
    producer = make_producer(ns_endpoint, key, "late-topic")
    producer.send(CloudEvent(source="/s", type="t", data="too early"))
    consumer = make_consumer(ns_endpoint, key, "late-topic", "late-sub")
    assert consumer.receive(max_events=1, max_wait_time=1) == []


def test_autocreate_off_publish_to_unknown_topic_is_not_found(strict_emu, name, key):
    c = EventGridPublisherClient(strict_emu.url("eventgrid", name) + "/api/events", key, retry_total=0)
    with pytest.raises(ResourceNotFoundError):
        c.send(ev())


def test_autocreate_off_unknown_namespace_topic_and_subscription_are_not_found(strict_emu, key):
    ep = strict_emu.url("eventgrid", "nsx")
    with pytest.raises(ResourceNotFoundError):
        make_producer(ep, key, "nope").send(CloudEvent(source="/s", type="t"))
    with pytest.raises(HttpResponseError) as e:
        make_consumer(ep, key, "nope", "nosub").receive(max_events=1, max_wait_time=0)
    assert e.value.status_code == 404


def test_autocreate_off_works_for_declared_resources(strict_emu, key):
    base = f"{strict_emu.gateway}/_azemu/eventgrid"
    assert requests.put(f"{base}/topics/jobs", params={"namespace": "nsy"}, json={}, timeout=10).status_code == 201
    assert requests.put(f"{base}/topics/jobs/subscriptions/s", params={"namespace": "nsy"}, json={}, timeout=10).status_code == 201
    ep = strict_emu.url("eventgrid", "nsy")
    make_producer(ep, key, "jobs").send(CloudEvent(source="/s", type="t", data=1))
    (d,) = make_consumer(ep, key, "jobs", "s").receive(max_events=1, max_wait_time=2)
    assert d.event.data == 1
    # a subscription that was not declared is still not found
    with pytest.raises(HttpResponseError) as e:
        make_consumer(ep, key, "jobs", "other").receive(max_events=1, max_wait_time=0)
    assert e.value.status_code == 404


def test_autocreate_off_error_body_shape(strict_emu, name):
    r = requests.post(
        strict_emu.url("eventgrid", name) + "/api/events",
        headers={"aeg-sas-key": "k", "Content-Type": "application/json"},
        data="[{}]",
        timeout=10,
    )
    assert r.status_code == 404
    assert r.json()["error"]["code"] == "NotFound"


# -- admin API --------------------------------------------------------------------------

def test_admin_topic_lifecycle(admin, name):
    created = admin.put_topic(name, {"inputSchema": "EventGridSchema"}, expect=(201,))
    assert created["kind"] == "custom"
    assert created["inputSchema"] == "EventGridSchema"
    assert admin.put_topic(name, {"inputSchema": "Auto"}, expect=(200,))["inputSchema"] == "Auto"
    assert any(t["name"] == name for t in admin.request("GET", "/topics").json()["value"])
    assert admin.request("DELETE", f"/topics/{name}").status_code == 204
    assert admin.request("GET", f"/topics/{name}").status_code == 404


def test_admin_declared_input_schema_is_enforced(emu, admin, name, key):
    admin.put_topic(name, {"inputSchema": "EventGridSchema"})
    c = EventGridPublisherClient(emu.url("eventgrid", name) + "/api/events", key, retry_total=0)
    c.send(ev())
    with pytest.raises(HttpResponseError) as e:
        c.send({"customOnly": "shape"})
    assert e.value.status_code == 400
    assert "EventGridEvent" in e.value.message


def test_admin_subscription_listing_and_get(admin, ns):
    admin.put_topic("jobs", namespace=ns)
    admin.put_sub("jobs", "a", {"lockDurationSeconds": 30, "maxDeliveryCount": 3}, namespace=ns, expect=(201,))
    admin.put_sub("jobs", "b", {}, namespace=ns, expect=(201,))
    listed = admin.request("GET", "/topics/jobs/subscriptions", params={"namespace": ns}).json()["value"]
    assert sorted(s["name"] for s in listed) == ["a", "b"]
    a = admin.get_sub("jobs", "a", namespace=ns)
    assert a["lockDurationSeconds"] == 30
    assert a["maxDeliveryCount"] == 3
    assert a["deliveryMode"] == "pull"
    assert admin.request("DELETE", "/topics/jobs/subscriptions/a", params={"namespace": ns}).status_code == 204
    assert admin.request("GET", "/topics/jobs/subscriptions/a", params={"namespace": ns}).status_code == 404


@pytest.mark.parametrize(
    "body",
    [
        {"deliveryMode": "pull", "webhookUrl": "http://h/x"},
        {"lockDurationSeconds": -1},
        {"maxDeliveryCount": 0},
        {"filter": {"advancedFilters": [{"operatorType": "NotAnOperator", "key": "k", "values": ["a"]}]}},
        {"filter": {"advancedFilters": [{"operatorType": "NumberIn", "key": "k", "values": ["not a number"]}]}},
    ],
)
def test_admin_rejects_invalid_subscriptions(admin, ns, body):
    admin.put_topic("jobs", namespace=ns)
    r = admin.request("PUT", "/topics/jobs/subscriptions/s", params={"namespace": ns}, json=body)
    assert r.status_code == 400
    assert r.json()["error"]["message"]


def test_admin_rejects_pull_and_missing_url_on_custom_topics(admin, name):
    admin.put_topic(name)
    assert admin.request("PUT", f"/topics/{name}/subscriptions/s", json={"deliveryMode": "pull"}).status_code == 400
    assert admin.request("PUT", f"/topics/{name}/subscriptions/s", json={"deliveryMode": "push"}).status_code == 400
    assert admin.request("PUT", f"/topics/{name}/subscriptions/s", json={"webhookUrl": "ftp://x/y"}).status_code == 400


def test_admin_status_summary(admin, publisher):
    publisher.send(ev())
    s = admin.request("GET", "").json()
    assert s["autocreate"] is autocreate_gate.autocreate_on("AZEMU_EVENTGRID_AUTOCREATE")
    assert s["topics"]


def test_admin_unknown_path_is_501(admin):
    r = admin.request("PATCH", "/topics/x")
    assert r.status_code == 501
    assert r.json()["error"]["code"] == "NotImplemented"


def test_reset_drops_topics_subscriptions_and_events(emu, admin, ns, ns_endpoint, key, ns_topic):
    make_producer(ns_endpoint, key, ns_topic).send(CloudEvent(source="/s", type="t"))
    assert admin.request("GET", f"/topics/{ns_topic}", params={"namespace": ns}).status_code == 200
    emu.reset()
    assert admin.request("GET", f"/topics/{ns_topic}", params={"namespace": ns}).status_code == 404


def test_the_namespace_default_is_used_without_a_namespace_parameter(admin, key, emu):
    admin.put_topic("dflt-topic", {"kind": "namespace"})
    admin.put_sub("dflt-topic", "s", {})
    ep = emu.url("eventgrid", "default")
    make_producer(ep, key, "dflt-topic").send(CloudEvent(source="/s", type="t", data="d"))
    (d,) = make_consumer(ep, key, "dflt-topic", "s").receive(max_events=1, max_wait_time=2)
    assert d.event.data == "d"


# -- config file ------------------------------------------------------------------------

def test_config_file_declares_topics_and_subscriptions(tmp_path, key):
    cfg = {
        "topics": [
            {
                "name": "wired",
                "inputSchema": "EventGridSchema",
                "subscriptions": [{"name": "hook", "webhookUrl": "http://127.0.0.1:1/never", "filter": {"includedEventTypes": ["A.B"]}}],
            }
        ],
        "namespaces": [
            {"name": "cfgns", "topics": [{"name": "jobs", "subscriptions": [{"name": "workers", "lockDurationSeconds": 30, "maxDeliveryCount": 3}]}]}
        ],
    }
    path = tmp_path / "eventgrid.json"
    path.write_text(json.dumps(cfg))
    p = EmuProc({"AZEMU_EVENTGRID_CONFIG": str(path), "AZEMU_EVENTGRID_AUTOCREATE": "0"})
    try:
        base = f"{p.gateway}/_azemu/eventgrid"
        for _ in range(2):  # the second round runs after a reset
            sub = requests.get(f"{base}/topics/wired/subscriptions/hook", timeout=10).json()
            assert sub["webhookUrl"] == "http://127.0.0.1:1/never"
            assert sub["filter"]["includedEventTypes"] == ["A.B"]
            ns_sub = requests.get(f"{base}/topics/jobs/subscriptions/workers", params={"namespace": "cfgns"}, timeout=10).json()
            assert ns_sub["maxDeliveryCount"] == 3
            # usable through the SDK even with auto-creation off
            ep = p.url("eventgrid", "cfgns")
            make_producer(ep, key, "jobs").send(CloudEvent(source="/s", type="t", data="from config"))
            (d,) = make_consumer(ep, key, "jobs", "workers").receive(max_events=1, max_wait_time=2)
            assert d.event.data == "from config"
            requests.put(f"{base}/topics/temporary", json={}, timeout=10)
            p.reset()
            assert requests.get(f"{base}/topics/temporary", timeout=10).status_code == 404
    finally:
        p.close()


def test_a_config_file_that_cannot_be_read_does_not_stop_the_service(tmp_path):
    bad = tmp_path / "bad.json"
    bad.write_text("{ not json")
    p = EmuProc({"AZEMU_EVENTGRID_CONFIG": str(bad)})
    try:
        assert requests.get(f"{p.gateway}/_azemu/eventgrid/topics", timeout=10).json() == {"value": []}
    finally:
        p.close()


def test_retry_timing_environment_variables(tmp_path):
    """AZEMU_EVENTGRID_RETRY_BASE_MS / _MAX_MS set the default backoff of push deliveries."""
    from egutil import Hook

    p = EmuProc({"AZEMU_EVENTGRID_RETRY_BASE_MS": "100", "AZEMU_EVENTGRID_RETRY_MAX_MS": "100"})
    hook = Hook(statuses=[500, 500, 500, 200])
    try:
        base = f"{p.gateway}/_azemu/eventgrid"
        requests.put(f"{base}/topics/envt", json={}, timeout=10)
        requests.put(f"{base}/topics/envt/subscriptions/s", json={"webhookUrl": hook.url}, timeout=10)
        c = EventGridPublisherClient(p.url("eventgrid", "envt") + "/api/events", AzureSasCredentialFree(), retry_total=0)
        c.send(ev())
        got = hook.wait_for(4, timeout=5)
        assert len(got) == 4
        gaps = [b["time"] - a["time"] for a, b in zip(got, got[1:])]
        assert all(0.08 < g < 0.5 for g in gaps), gaps
    finally:
        hook.close()
        p.close()


def AzureSasCredentialFree():
    return AzureKeyCredential("c2VjcmV0")
