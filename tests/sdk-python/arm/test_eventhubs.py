"""Microsoft.EventHub through azure-mgmt-eventhub, then used over AMQP."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient
from azure.mgmt.eventhub import EventHubManagementClient

from conftest import SUB

CLIENT_KW = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}


@pytest.fixture
def ehm(cred, mgmt):
    with EventHubManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_ns(ehm, rg, name):
    return ehm.namespaces.begin_create_or_update(rg, name, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}).result()


def conn_str(ehm, rg, ns):
    return ehm.namespaces.list_keys(rg, ns, "RootManageSharedAccessKey").primary_connection_string


def test_namespace_and_event_hub(ehm, rg, uniq, emu):
    name = uniq("ehns", 10)
    ns = make_ns(ehm, rg, name)
    assert ns.provisioning_state == "Succeeded"
    assert ns.service_bus_endpoint == f"https://{name}.eventhub.localhost:{emu.port}/"
    hub = ehm.event_hubs.create_or_update(rg, name, "hub1", {"properties": {"partitionCount": 2, "messageRetentionInDays": 2}})
    assert hub.partition_count == 2
    assert hub.partition_ids == ["0", "1"]
    assert hub.message_retention_in_days == 2
    assert [h.name for h in ehm.event_hubs.list_by_namespace(rg, name)] == ["hub1"]
    assert "$Default" in [c.name for c in ehm.consumer_groups.list_by_event_hub(rg, name, "hub1")]


def test_send_and_receive_through_arm_created_hub(ehm, rg, uniq):
    name = uniq("ehns", 10)
    make_ns(ehm, rg, name)
    ehm.event_hubs.create_or_update(rg, name, "hub1", {"properties": {"partitionCount": 2}})
    ehm.consumer_groups.create_or_update(rg, name, "hub1", "workers", {"properties": {"userMetadata": "w"}})
    cs = conn_str(ehm, rg, name)
    producer = EventHubProducerClient.from_connection_string(cs, eventhub_name="hub1", **CLIENT_KW)
    with producer:
        assert len(producer.get_partition_ids()) == 2
        batch = producer.create_batch(partition_id="0")
        batch.add(EventData("one"))
        batch.add(EventData("two"))
        producer.send_batch(batch)
    got = []
    consumer = EventHubConsumerClient.from_connection_string(cs, consumer_group="workers", eventhub_name="hub1", **CLIENT_KW)
    import threading

    def on_event(ctx, event):
        if event is not None:
            got.append(event.body_as_str())
            if len(got) >= 2:
                consumer.close()

    t = threading.Thread(target=lambda: consumer.receive(on_event=on_event, partition_id="0", starting_position="-1"))
    t.start()
    t.join(timeout=15)
    consumer.close()
    assert got == ["one", "two"]


def test_partition_count_cannot_change(ehm, rg, uniq):
    name = uniq("ehns", 10)
    make_ns(ehm, rg, name)
    ehm.event_hubs.create_or_update(rg, name, "hub1", {"properties": {"partitionCount": 2}})
    with pytest.raises(HttpResponseError) as e:
        ehm.event_hubs.create_or_update(rg, name, "hub1", {"properties": {"partitionCount": 4}})
    assert e.value.status_code == 400


def test_delete_hub_and_namespace(ehm, rg, uniq):
    name = uniq("ehns", 10)
    make_ns(ehm, rg, name)
    ehm.event_hubs.create_or_update(rg, name, "hub1", {"properties": {}})
    ehm.event_hubs.delete(rg, name, "hub1")
    with pytest.raises(ResourceNotFoundError):
        ehm.event_hubs.get(rg, name, "hub1")
    ehm.namespaces.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        ehm.namespaces.get(rg, name)


def test_authorization_rules(ehm, rg, uniq):
    name = uniq("ehns", 10)
    make_ns(ehm, rg, name)
    ehm.namespaces.create_or_update_authorization_rule(rg, name, "listener", {"properties": {"rights": ["Listen"]}})
    k = ehm.namespaces.list_keys(rg, name, "listener")
    assert "SharedAccessKeyName=listener" in k.primary_connection_string
    k2 = ehm.namespaces.regenerate_keys(rg, name, "listener", {"keyType": "SecondaryKey"})
    assert k2.secondary_key != k.secondary_key
    assert k2.primary_key == k.primary_key
