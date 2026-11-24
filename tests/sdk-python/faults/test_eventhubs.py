"""Event Hubs (AMQP): the hub's name is the `path` of a rule, the namespace is its `account`."""

import json
import urllib.request

import pytest
from azure.eventhub import EventData, EventHubProducerClient
from azure.eventhub.exceptions import EventDataSendError

NAMESPACE = "faultns"


def make_hub(emu, name):
    req = urllib.request.Request(
        f"{emu.gateway}/_azemu/eventhubs/{NAMESPACE}/hubs/{name}", method="PUT", data=json.dumps({"partitionCount": 2}).encode(),
        headers={"content-type": "application/json"},
    )
    urllib.request.urlopen(req, timeout=10).read()


def producer(emu, hub, **kw):
    conn = f"Endpoint=sb://{NAMESPACE}.eventhub.localhost:{emu.port};SharedAccessKeyName=k;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true"
    return EventHubProducerClient.from_connection_string(conn, eventhub_name=hub, **kw)


def test_event_hubs_python_client_does_not_retry_a_refused_send(emu, faults):
    """A throttled send is settled `rejected` with `com.microsoft:server-busy`. azure-eventhub 5.15.1 raises it as
    `EventDataSendError` and does not retry, with retries on or off. (Tried too, and the same: the link detached with the error,
    and the connection closed with the error.) Retries are a Service Bus client behaviour here, not an Event Hubs one."""
    make_hub(emu, "orders")
    rule = faults.add(service="eventhubs", account=NAMESPACE, operation="send", path="orders", times=1, fault={"kind": "throttle"})
    with producer(emu, "orders", retry_total=5, retry_backoff_factor=0.1, retry_backoff_max=0.5) as p:
        batch = p.create_batch(partition_id="0")
        batch.add(EventData("hello"))
        with pytest.raises(EventDataSendError) as e:
            p.send_batch(batch)
        assert "server-busy" in str(e.value)
        # the rule is spent: the same batch goes through
        p.send_batch(batch)
    assert faults.fired(rule["id"]) == 1


def test_event_hubs_python_client_retries_a_throttled_connection(emu, faults):
    make_hub(emu, "orders")
    rule = faults.add(service="eventhubs", account=NAMESPACE, operation="connect", times=2, fault={"kind": "throttle"})
    with producer(emu, "orders", retry_total=5, retry_backoff_factor=0.2, retry_backoff_max=1) as p:
        batch = p.create_batch(partition_id="0")
        batch.add(EventData("hello"))
        p.send_batch(batch)
    assert faults.fired(rule["id"]) == 2


def test_event_hubs_rule_for_another_hub_does_not_apply(emu, faults):
    make_hub(emu, "orders")
    faults.add(service="eventhubs", operation="send", path="other-hub", fault={"kind": "throttle"})
    with producer(emu, "orders", retry_total=0) as p:
        batch = p.create_batch(partition_id="0")
        batch.add(EventData("hello"))
        p.send_batch(batch)
