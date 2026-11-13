"""ARM state across restarts, through the management SDKs: a resource group with its tags and resources, the data-plane
objects behind them, and an event subscription made through ARM that still delivers."""

import time
import uuid

import pytest
from azure.core.credentials import AccessToken, AzureKeyCredential
from azure.core.exceptions import ResourceNotFoundError
from azure.eventgrid import EventGridEvent, EventGridPublisherClient
from azure.mgmt.eventgrid import EventGridManagementClient
from azure.mgmt.eventhub import EventHubManagementClient
from azure.mgmt.resource.resources import ResourceManagementClient
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.servicebus import ServiceBusClient, ServiceBusMessage

SUB = "11111111-2222-3333-4444-555555555555"
CLIENT_KW = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}


class Cred:
    def get_token(self, *scopes, **kwargs):
        return AccessToken("azemu-test-token", int(time.time()) + 3600)


def mgmt(p):
    return dict(
        base_url=f"https://management.localhost:{p.emu.port}",
        credential_scopes=["https://management.azure.com/.default"],
        connection_verify=p.emu.ca_file,
        retry_total=0,
    )


def receive_all(conn_str, queue, n, timeout=15):
    got = []
    end = time.time() + timeout
    with ServiceBusClient.from_connection_string(conn_str, **CLIENT_KW) as c:
        with c.get_queue_receiver(queue, max_wait_time=3) as r:
            while len(got) < n and time.time() < end:
                for m in r.receive_messages(max_message_count=n - len(got), max_wait_time=3):
                    got.append(b"".join(m.body).decode())
                    r.complete_message(m)
    return got


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_group_resources_data_plane_and_event_subscription_survive(persistent, how):
    p = persistent()
    sfx = uuid.uuid4().hex[:8]
    rg, sbns, ehns, topic_name = f"rg-{sfx}", f"sbns{sfx}", f"ehns{sfx}", f"topic{sfx}"
    with ResourceManagementClient(Cred(), SUB, **mgmt(p)) as rc, ServiceBusManagementClient(Cred(), SUB, **mgmt(p)) as sbm, \
            EventHubManagementClient(Cred(), SUB, **mgmt(p)) as ehm, EventGridManagementClient(Cred(), SUB, **mgmt(p)) as egm:
        rc.resource_groups.create_or_update(rg, {"location": "eastus", "tags": {"team": "x", "env": "dev"}})
        sbm.namespaces.begin_create_or_update(rg, sbns, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}, "tags": {"k": "v"}}).result()
        qin = sbm.queues.create_or_update(rg, sbns, "orders", {"properties": {"maxDeliveryCount": 3, "lockDuration": "PT30S"}})
        qev = sbm.queues.create_or_update(rg, sbns, "events", {"properties": {}})
        ehm.namespaces.begin_create_or_update(rg, ehns, {"location": "eastus", "sku": {"name": "Standard"}}).result()
        ehm.event_hubs.create_or_update(rg, ehns, "hub1", {"partition_count": 3})
        topic = egm.topics.begin_create_or_update(rg, topic_name, {"location": "eastus", "properties": {"inputSchema": "EventGridSchema"}}).result()
        egm.event_subscriptions.begin_create_or_update(
            topic.id, "to-queue", {"properties": {"destination": {"endpointType": "ServiceBusQueue", "properties": {"resourceId": qev.id}}}}
        ).result()
        keys = egm.topics.list_shared_access_keys(rg, topic_name)
        conn = sbm.namespaces.list_keys(rg, sbns, "RootManageSharedAccessKey").primary_connection_string
        with ServiceBusClient.from_connection_string(conn, **CLIENT_KW) as c, c.get_queue_sender("orders") as s:
            s.send_messages(ServiceBusMessage("sent before the restart"))
        resources_before = sorted((r.name, r.type) for r in rc.resources.list_by_resource_group(rg))
        queue_ids_before = sorted(q.id for q in sbm.queues.list_by_namespace(rg, sbns))
        assert len(resources_before) == 3

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    with ResourceManagementClient(Cred(), SUB, **mgmt(p)) as rc, ServiceBusManagementClient(Cred(), SUB, **mgmt(p)) as sbm, \
            EventGridManagementClient(Cred(), SUB, **mgmt(p)) as egm:
        g = rc.resource_groups.get(rg)
        assert g.tags == {"team": "x", "env": "dev"} and g.location == "eastus"
        assert sorted((r.name, r.type) for r in rc.resources.list_by_resource_group(rg)) == resources_before
        assert sorted(q.id for q in sbm.queues.list_by_namespace(rg, sbns)) == queue_ids_before
        q = sbm.queues.get(rg, sbns, "orders")
        assert q.max_delivery_count == 3 and q.lock_duration.total_seconds() == 30
        assert sbm.namespaces.get(rg, sbns).tags == {"k": "v"}
        # the data plane objects work: the message sent before the restart is there
        conn = sbm.namespaces.list_keys(rg, sbns, "RootManageSharedAccessKey").primary_connection_string
        assert receive_all(conn, "orders", 1) == ["sent before the restart"]
        # the event subscription made through ARM still delivers: a new event reaches the queue behind it
        sub = egm.event_subscriptions.get(topic.id, "to-queue")
        assert sub.destination.endpoint_type == "ServiceBusQueue"
        pub = EventGridPublisherClient(p.emu.url("eventgrid", topic_name) + "/api/events", AzureKeyCredential(keys.key1), retry_total=0)
        with pub:
            pub.send([EventGridEvent(subject="/orders/1", event_type="Order.Created", data={"n": 1}, data_version="1")])
        got = receive_all(conn, "events", 1)
        assert len(got) == 1 and '"Order.Created"' in got[0]
        # a change through ARM reaches the data plane after the restart, and a delete too
        sbm.queues.create_or_update(rg, sbns, "orders", {"properties": {"maxDeliveryCount": 7}})
        from azure.servicebus.management import ServiceBusAdministrationClient

        admin = ServiceBusAdministrationClient.from_connection_string(conn, connection_verify=p.emu.ca_file, retry_total=0)
        assert admin.get_queue("orders").max_delivery_count == 7
        rc.resource_groups.begin_delete(rg).result()
        with pytest.raises(ResourceNotFoundError):
            rc.resource_groups.get(rg)

    assert p.stop() == 0
    p.start()
    with ResourceManagementClient(Cred(), SUB, **mgmt(p)) as rc:
        with pytest.raises(ResourceNotFoundError):
            rc.resource_groups.get(rg)
        assert list(rc.resources.list()) == []
