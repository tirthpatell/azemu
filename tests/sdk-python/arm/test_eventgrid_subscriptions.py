"""Event Grid subscriptions and system topics through azure-mgmt-eventgrid, with the events read through the data plane SDKs.

The expected shapes come from the Event Grid management REST spec (resource-manager, 2025-02-15) and the system event
schema for Blob Storage; nothing is copied from the emulator.
"""

import base64
import json
import threading
import time
import urllib.request

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.eventgrid import EventGridEvent, EventGridPublisherClient
from azure.eventhub import EventHubConsumerClient
from azure.mgmt.eventgrid import EventGridManagementClient
from azure.mgmt.eventhub import EventHubManagementClient
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.mgmt.storage import StorageManagementClient
from azure.servicebus import ServiceBusClient
from azure.storage.blob import BlobServiceClient
from azure.storage.queue import QueueClient

from conftest import SUB
from evutil import Webhook, wait_until

CLIENT_KW = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}
BLOB_CREATED = "Microsoft.Storage.BlobCreated"


@pytest.fixture
def storage(cred, mgmt):
    with StorageManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def egm(cred, mgmt):
    with EventGridManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def sbm(cred, mgmt):
    with ServiceBusManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def ehm(cred, mgmt):
    with EventHubManagementClient(cred, SUB, **mgmt) as c:
        yield c


@pytest.fixture
def hook():
    wh = Webhook()
    yield wh
    wh.stop()


class Account:
    """A storage account made through ARM, with the data plane clients for it."""

    def __init__(self, storage, rg, name):
        self.rg = rg
        self.name = name
        self.storage = storage
        self.arm = storage.storage_accounts.begin_create(rg, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}).result()
        self.id = self.arm.id
        self.key = storage.storage_accounts.list_keys(rg, name)["keys"][0].value
        self.blobs = BlobServiceClient(self.arm.primary_endpoints.blob, credential=self.key)

    def queue(self, name):
        self.storage.queue.create(self.rg, self.name, name, {"properties": {}})
        return QueueClient(self.arm.primary_endpoints.queue, queue_name=name, credential=self.key)

    def container(self, name="inbox"):
        self.storage.blob_containers.create(self.rg, self.name, name, {"properties": {}})
        return self.blobs.get_container_client(name)


@pytest.fixture
def acct(storage, rg, uniq):
    return Account(storage, rg, uniq("evacct"))


def queue_events(queue, n=1, timeout=10):
    """Read `n` events from a Storage queue (the message text is the base64 of the event JSON)."""
    got = []

    def poll():
        for m in queue.receive_messages(max_messages=32, visibility_timeout=30):
            got.append(json.loads(base64.b64decode(m.content)))
            queue.delete_message(m)
        return len(got) >= n

    wait_until(poll, timeout, f"{n} events in the queue")
    return got


def queue_stays_empty(queue, seconds=1.5):
    time.sleep(seconds)
    return list(queue.receive_messages(max_messages=10)) == []


def queue_destination(acct, queue):
    return {"endpointType": "StorageQueue", "properties": {"resourceId": acct.id, "queueName": queue}}


def subscribe(egm, scope, name, destination, **props):
    return egm.event_subscriptions.begin_create_or_update(scope, name, {"properties": {"destination": destination, **props}}).result()


def test_blob_created_event_reaches_a_storage_queue(storage, egm, acct):
    queue = acct.queue("blob-events")
    container = acct.container("inbox")
    sub = subscribe(egm, acct.id, "to-queue", queue_destination(acct, "blob-events"))
    assert sub.name == "to-queue"
    assert sub.type == "Microsoft.EventGrid/eventSubscriptions"
    assert sub.id == f"{acct.id}/providers/Microsoft.EventGrid/eventSubscriptions/to-queue"
    assert sub.provisioning_state == "Succeeded"
    assert sub.topic.lower() == acct.id.lower()
    assert sub.event_delivery_schema == "EventGridSchema"
    assert sub.destination.endpoint_type == "StorageQueue"
    assert sub.destination.queue_name == "blob-events"
    assert sub.destination.resource_id == acct.id
    assert sub.retry_policy.max_delivery_attempts == 30
    assert sub.retry_policy.event_time_to_live_in_minutes == 1440

    container.upload_blob("hello.txt", b"hello events")
    ev = queue_events(queue)[0]
    assert ev["eventType"] == BLOB_CREATED
    assert ev["topic"] == acct.id
    assert ev["subject"] == "/blobServices/default/containers/inbox/blobs/hello.txt"
    assert ev["dataVersion"] == ""
    assert ev["metadataVersion"] == "1"
    assert ev["data"]["api"] == "PutBlob"
    assert ev["data"]["contentLength"] == 12
    assert ev["data"]["blobType"] == "BlockBlob"
    assert ev["data"]["url"].endswith("/inbox/hello.txt")


def test_event_reaches_a_service_bus_queue(sbm, egm, acct, rg, uniq):
    ns = uniq("sbns", 10)
    sbm.namespaces.begin_create_or_update(rg, ns, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}).result()
    q = sbm.queues.create_or_update(rg, ns, "blob-events", {"properties": {}})
    container = acct.container("inbox")
    sub = subscribe(egm, acct.id, "to-sb", {"endpointType": "ServiceBusQueue", "properties": {"resourceId": q.id}})
    assert sub.destination.endpoint_type == "ServiceBusQueue"
    assert sub.destination.resource_id == q.id
    container.upload_blob("a.txt", b"x")
    cs = sbm.namespaces.list_keys(rg, ns, "RootManageSharedAccessKey").primary_connection_string
    with ServiceBusClient.from_connection_string(cs, **CLIENT_KW) as client:
        with client.get_queue_receiver("blob-events", max_wait_time=8) as receiver:
            msgs = receiver.receive_messages(max_message_count=1, max_wait_time=8)
            assert len(msgs) == 1
            ev = json.loads(b"".join(msgs[0].body))
            assert ev["eventType"] == BLOB_CREATED
            assert ev["topic"] == acct.id
            assert msgs[0].message_id == ev["id"]
            receiver.complete_message(msgs[0])


def test_event_reaches_a_service_bus_topic(sbm, egm, acct, rg, uniq):
    ns = uniq("sbns", 10)
    sbm.namespaces.begin_create_or_update(rg, ns, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}).result()
    t = sbm.topics.create_or_update(rg, ns, "blob-topic", {"properties": {}})
    sbm.subscriptions.create_or_update(rg, ns, "blob-topic", "all", {"properties": {}})
    container = acct.container("inbox")
    subscribe(egm, acct.id, "to-topic", {"endpointType": "ServiceBusTopic", "properties": {"resourceId": t.id}})
    container.upload_blob("t.txt", b"x")
    cs = sbm.namespaces.list_keys(rg, ns, "RootManageSharedAccessKey").primary_connection_string
    with ServiceBusClient.from_connection_string(cs, **CLIENT_KW) as client:
        with client.get_subscription_receiver("blob-topic", "all", max_wait_time=8) as receiver:
            msgs = receiver.receive_messages(max_message_count=1, max_wait_time=8)
            assert len(msgs) == 1
            assert json.loads(b"".join(msgs[0].body))["eventType"] == BLOB_CREATED


def test_event_reaches_an_event_hub(ehm, egm, acct, rg, uniq):
    ns = uniq("ehns", 10)
    ehm.namespaces.begin_create_or_update(rg, ns, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}).result()
    hub = ehm.event_hubs.create_or_update(rg, ns, "blob-hub", {"properties": {"partitionCount": 1}})
    container = acct.container("inbox")
    subscribe(egm, acct.id, "to-hub", {"endpointType": "EventHub", "properties": {"resourceId": hub.id}})
    container.upload_blob("h.txt", b"x")
    cs = ehm.namespaces.list_keys(rg, ns, "RootManageSharedAccessKey").primary_connection_string
    got = []
    consumer = EventHubConsumerClient.from_connection_string(cs, consumer_group="$Default", eventhub_name="blob-hub", **CLIENT_KW)

    def on_event(ctx, event):
        if event is not None:
            got.append(json.loads(event.body_as_str()))
            consumer.close()

    t = threading.Thread(target=lambda: consumer.receive(on_event=on_event, starting_position="-1"))
    t.start()
    t.join(timeout=20)
    consumer.close()
    assert [e["eventType"] for e in got] == [BLOB_CREATED]


def test_webhook_handshake_and_delivery(egm, acct, hook):
    container = acct.container("inbox")
    sub = subscribe(egm, acct.id, "to-hook", {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}})
    # the handshake happened while the PUT was running
    assert [h[0] for h in hook.handshakes] == ["POST"]
    assert hook.handshakes[0][1]["aeg-event-type"] == "SubscriptionValidation"
    assert sub.provisioning_state == "Succeeded"
    # Azure does not show the full URL (it can hold a secret); the base URL and getFullUrl do
    got = egm.event_subscriptions.get(acct.id, "to-hook")
    assert got.destination.endpoint_base_url == hook.url.split("?")[0]
    assert not getattr(got.destination, "endpoint_url", None)
    assert egm.event_subscriptions.get_full_url(acct.id, "to-hook").endpoint_url == hook.url
    container.upload_blob("w.txt", b"hi")
    ev = hook.wait_events(1)[0]
    assert ev["eventType"] == BLOB_CREATED
    assert ev["topic"] == acct.id
    assert hook.requests[0][0]["aeg-event-type"] == "Notification"


def test_a_webhook_that_refuses_the_handshake_creates_nothing(egm, acct):
    wh = Webhook(refuse_validation=True)
    try:
        with pytest.raises(HttpResponseError) as e:
            subscribe(egm, acct.id, "refused", {"endpointType": "WebHook", "properties": {"endpointUrl": wh.url}})
        assert e.value.status_code == 400
        assert "validation" in str(e.value).lower()
        with pytest.raises(ResourceNotFoundError):
            egm.event_subscriptions.get(acct.id, "refused")
    finally:
        wh.stop()


def test_delete_subscription_stops_delivery(egm, acct):
    queue = acct.queue("events")
    container = acct.container("inbox")
    subscribe(egm, acct.id, "stoppable", queue_destination(acct, "events"))
    container.upload_blob("one.txt", b"1")
    assert len(queue_events(queue)) == 1
    egm.event_subscriptions.begin_delete(acct.id, "stoppable").result()
    with pytest.raises(ResourceNotFoundError):
        egm.event_subscriptions.get(acct.id, "stoppable")
    container.upload_blob("two.txt", b"2")
    assert queue_stays_empty(queue)


def test_deleting_the_group_removes_the_subscription(storage, egm, resource_client, make_group, uniq, emu):
    rg = make_group("rg-evdel-")
    name = uniq("evacct")
    acct = Account(storage, rg, name)
    queue = acct.queue("events")
    acct.container("inbox")
    subscribe(egm, acct.id, "gone-soon", queue_destination(acct, "events"))
    status, body = _admin(emu, f"/sources/storage/{name}/subscriptions")
    assert [s["name"] for s in body["value"]] == ["gone-soon"]
    resource_client.resource_groups.begin_delete(rg).result()
    with pytest.raises(ResourceNotFoundError):
        egm.event_subscriptions.get(acct.id, "gone-soon")
    # the Event Grid service has no subscription for the account any more
    status, body = _admin(emu, f"/sources/storage/{name}/subscriptions")
    assert body["value"] == []
    # the same names again, with no subscription: no event
    resource_client.resource_groups.create_or_update(rg, {"location": "eastus"})
    again = Account(storage, rg, name)
    queue = again.queue("events")
    container = again.container("inbox")
    container.upload_blob("later.txt", b"x")
    assert queue_stays_empty(queue)
    with pytest.raises(ResourceNotFoundError):
        egm.event_subscriptions.get(again.id, "gone-soon")


def test_deleting_the_storage_account_removes_its_subscriptions(storage, egm, acct, rg, emu):
    acct.queue("events")
    subscribe(egm, acct.id, "on-account", queue_destination(acct, "events"))
    storage.storage_accounts.delete(rg, acct.name)
    status, body = _admin(emu, f"/sources/storage/{acct.name}/subscriptions")
    assert body["value"] == []


def _admin(emu, path):
    with urllib.request.urlopen(f"http://127.0.0.1:{emu.port}/_azemu/eventgrid{path}", timeout=10) as r:
        return r.status, json.load(r)


def test_system_topic_and_its_subscription(egm, acct, rg, uniq):
    queue = acct.queue("events")
    container = acct.container("inbox")
    st_name = uniq("st-")
    st = egm.system_topics.begin_create_or_update(
        rg, st_name, {"location": "eastus", "properties": {"source": acct.id, "topicType": "Microsoft.Storage.StorageAccounts"}}
    ).result()
    assert st.name == st_name
    assert st.type == "Microsoft.EventGrid/systemTopics"
    assert st.source == acct.id
    assert st.topic_type == "Microsoft.Storage.StorageAccounts"
    assert st.provisioning_state == "Succeeded"
    assert st.metric_resource_id
    assert [t.name for t in egm.system_topics.list_by_resource_group(rg)] == [st_name]
    assert st_name in [t.name for t in egm.system_topics.list_by_subscription()]

    sub = egm.system_topic_event_subscriptions.begin_create_or_update(
        rg, st_name, "blobs",
        {"properties": {"destination": queue_destination(acct, "events"), "filter": {"includedEventTypes": [BLOB_CREATED]}}},
    ).result()
    assert sub.id == f"{st.id}/eventSubscriptions/blobs"
    assert sub.type == "Microsoft.EventGrid/systemTopics/eventSubscriptions"
    assert sub.filter.included_event_types == [BLOB_CREATED]
    assert [s.name for s in egm.system_topic_event_subscriptions.list_by_system_topic(rg, st_name)] == ["blobs"]
    assert egm.system_topic_event_subscriptions.get(rg, st_name, "blobs").name == "blobs"

    container.upload_blob("st.txt", b"x")
    ev = queue_events(queue)[0]
    assert ev["eventType"] == BLOB_CREATED
    assert ev["topic"] == acct.id

    egm.system_topic_event_subscriptions.begin_delete(rg, st_name, "blobs").result()
    container.upload_blob("st2.txt", b"x")
    assert queue_stays_empty(queue)
    egm.system_topics.begin_delete(rg, st_name).result()
    with pytest.raises(ResourceNotFoundError):
        egm.system_topics.get(rg, st_name)


def test_deleting_a_system_topic_removes_its_subscriptions(egm, acct, rg, uniq):
    queue = acct.queue("events")
    container = acct.container("inbox")
    st_name = uniq("st-")
    egm.system_topics.begin_create_or_update(rg, st_name, {"location": "eastus", "properties": {"source": acct.id, "topicType": "Microsoft.Storage.StorageAccounts"}}).result()
    egm.system_topic_event_subscriptions.begin_create_or_update(rg, st_name, "sub1", {"properties": {"destination": queue_destination(acct, "events")}}).result()
    egm.system_topics.begin_delete(rg, st_name).result()
    container.upload_blob("x.txt", b"x")
    assert queue_stays_empty(queue)


def test_filters_and_the_cloud_event_schema(egm, acct):
    queue = acct.queue("cloud")
    container = acct.container("inbox")
    subscribe(
        egm, acct.id, "filtered", queue_destination(acct, "cloud"),
        eventDeliverySchema="CloudEventSchemaV1_0",
        filter={
            "includedEventTypes": [BLOB_CREATED],
            "subjectBeginsWith": "/blobServices/default/containers/inbox/blobs/in/",
            "subjectEndsWith": ".json",
            "advancedFilters": [{"operatorType": "NumberGreaterThan", "key": "data.contentLength", "value": 2}],
        },
    )
    got = egm.event_subscriptions.get(acct.id, "filtered")
    assert got.event_delivery_schema == "CloudEventSchemaV1_0"
    assert got.filter.subject_begins_with == "/blobServices/default/containers/inbox/blobs/in/"
    assert got.filter.subject_ends_with == ".json"
    assert got.filter.advanced_filters[0].operator_type == "NumberGreaterThan"
    container.upload_blob("in/skip.txt", b"abcdef")     # wrong suffix
    container.upload_blob("out/skip.json", b"abcdef")   # wrong prefix
    container.upload_blob("in/tiny.json", b"a")         # too small
    container.upload_blob("in/keep.json", b"abcdef")
    ev = queue_events(queue)[0]
    assert ev["specversion"] == "1.0"
    assert ev["type"] == BLOB_CREATED
    assert ev["source"] == acct.id
    assert ev["subject"].endswith("/blobs/in/keep.json")
    assert queue_stays_empty(queue)


def test_update_with_patch_changes_what_is_delivered(egm, acct):
    queue = acct.queue("events")
    container = acct.container("inbox")
    subscribe(egm, acct.id, "patched", queue_destination(acct, "events"), filter={"subjectEndsWith": ".txt"})
    container.upload_blob("a.png", b"x")
    assert queue_stays_empty(queue)
    upd = egm.event_subscriptions.begin_update(acct.id, "patched", {"filter": {"subjectEndsWith": ".png"}, "labels": ["l1"]}).result()
    assert upd.filter.subject_ends_with == ".png"
    assert upd.labels == ["l1"]
    assert upd.destination.queue_name == "events"          # untouched by the patch
    container.upload_blob("b.png", b"x")
    ev = queue_events(queue)[0]
    assert ev["subject"].endswith("/b.png")


def test_a_second_put_replaces_the_subscription(egm, acct):
    queue = acct.queue("events")
    container = acct.container("inbox")
    subscribe(egm, acct.id, "again", queue_destination(acct, "events"), labels=["one"])
    second = subscribe(egm, acct.id, "again", queue_destination(acct, "events"), filter={"subjectEndsWith": ".csv"})
    assert second.labels == []
    assert len(list(egm.event_subscriptions.list_by_resource(rg_of(acct), "Microsoft.Storage", "storageAccounts", acct.name))) == 1
    container.upload_blob("a.txt", b"x")
    assert queue_stays_empty(queue)
    container.upload_blob("a.csv", b"x")
    assert len(queue_events(queue)) == 1


def rg_of(acct):
    return acct.rg


def test_retry_policy_labels_and_dead_letter_are_stored(egm, acct):
    acct.queue("events")
    sub = subscribe(
        egm, acct.id, "settings", queue_destination(acct, "events"),
        retryPolicy={"maxDeliveryAttempts": 7, "eventTimeToLiveInMinutes": 60},
        labels=["a", "b"],
        expirationTimeUtc="2099-01-01T00:00:00Z",
        deadLetterDestination={"endpointType": "StorageBlob", "properties": {"resourceId": acct.id, "blobContainerName": "dead"}},
    )
    assert sub.retry_policy.max_delivery_attempts == 7
    assert sub.retry_policy.event_time_to_live_in_minutes == 60
    assert sub.labels == ["a", "b"]
    assert sub.expiration_time_utc.year == 2099
    assert sub.dead_letter_destination.blob_container_name == "dead"


def test_lists(egm, acct, rg):
    acct.queue("events")
    subscribe(egm, acct.id, "list-a", queue_destination(acct, "events"))
    subscribe(egm, acct.id, "list-b", queue_destination(acct, "events"))
    names = sorted(s.name for s in egm.event_subscriptions.list_by_resource(rg, "Microsoft.Storage", "storageAccounts", acct.name))
    assert names == ["list-a", "list-b"]
    assert {"list-a", "list-b"} <= {s.name for s in egm.event_subscriptions.list_global_by_resource_group(rg)}
    assert {"list-a", "list-b"} <= {s.name for s in egm.event_subscriptions.list_global_by_subscription()}


def test_custom_topic_subscriptions(egm, rg, uniq, hook):
    name = uniq("topic-")
    topic = egm.topics.begin_create_or_update(rg, name, {"location": "eastus", "properties": {"inputSchema": "EventGridSchema"}}).result()
    # the extension form on the topic
    sub = subscribe(egm, topic.id, "ext", {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}})
    assert sub.id == f"{topic.id}/providers/Microsoft.EventGrid/eventSubscriptions/ext"
    assert sub.topic == topic.id
    assert len(hook.handshakes) == 1
    # the alias under the topic reaches the same subscription
    alias = egm.topic_event_subscriptions.get(rg, name, "ext")
    assert alias.id == sub.id
    assert [s.name for s in egm.topic_event_subscriptions.list(rg, name)] == ["ext"]
    other = egm.topic_event_subscriptions.begin_create_or_update(
        rg, name, "viaalias", {"properties": {"destination": {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}}}}
    ).result()
    assert other.name == "viaalias"
    assert egm.event_subscriptions.get(topic.id, "viaalias").name == "viaalias"
    assert sorted(s.name for s in egm.event_subscriptions.list_by_resource(rg, "Microsoft.EventGrid", "topics", name)) == ["ext", "viaalias"]
    egm.topic_event_subscriptions.begin_delete(rg, name, "viaalias").result()

    keys = egm.topics.list_shared_access_keys(rg, name)
    pub = EventGridPublisherClient(topic.endpoint.replace("https://", "http://"), AzureKeyCredential(keys.key1), retry_total=0)
    with pub:
        pub.send([EventGridEvent(subject="/orders/1", event_type="Order.Created", data={"n": 1}, data_version="1")])
    ev = hook.wait_events(1)[0]
    assert ev["eventType"] == "Order.Created"
    assert ev["topic"] == topic.id            # the real resource id, as on Azure
    assert [s.name for s in egm.event_subscriptions.list_regional_by_subscription("eastus")] == ["ext"]
    egm.topics.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        egm.event_subscriptions.get(topic.id, "ext")


def test_key_vault_and_app_configuration_are_sources(cred, mgmt, egm, rg, uniq, hook):
    from azure.mgmt.appconfiguration import AppConfigurationManagementClient
    from azure.mgmt.keyvault import KeyVaultManagementClient

    with KeyVaultManagementClient(cred, SUB, **mgmt) as kv:
        name = uniq("kv-", 6)
        vault = kv.vaults.begin_create_or_update(
            rg, name, {"location": "eastus", "properties": {"tenantId": "22222222-2222-2222-2222-222222222222", "sku": {"family": "A", "name": "standard"}, "accessPolicies": []}}
        ).result()
    sub = subscribe(egm, vault.id, "kvhook", {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}})
    assert sub.topic == vault.id
    with AppConfigurationManagementClient(cred, SUB, **mgmt) as ac:
        store = ac.configuration_stores.begin_create(rg, uniq("ac-", 6), {"location": "eastus", "sku": {"name": "standard"}}).result()
    sub = subscribe(egm, store.id, "achook", {"endpointType": "WebHook", "properties": {"endpointUrl": hook.url}})
    assert sub.topic == store.id


def test_errors(egm, acct, rg, uniq, resource_client):
    dest = queue_destination(acct, "events")
    # subscription and group scope carry Azure Resource Manager events: not built
    with pytest.raises(HttpResponseError) as e:
        subscribe(egm, f"/subscriptions/{SUB}", "arm-events", dest)
    assert e.value.status_code == 501
    with pytest.raises(HttpResponseError) as e:
        subscribe(egm, f"/subscriptions/{SUB}/resourceGroups/{rg}", "arm-events", dest)
    assert e.value.status_code == 501
    # a resource that does not exist
    with pytest.raises(ResourceNotFoundError):
        subscribe(egm, f"{acct.id}x", "nope", dest)
    # a type nothing raises events for
    resource_client.resources.begin_create_or_update(rg, "Microsoft.Compute", "", "virtualMachines", "vm1", {"location": "eastus", "properties": {}}, api_version="2024-07-01").result()
    with pytest.raises(HttpResponseError) as e:
        subscribe(egm, f"/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.Compute/virtualMachines/vm1", "vmhook", dest)
    assert e.value.status_code == 501
    assert "Microsoft.Compute/virtualMachines" in str(e.value)
    # destinations that are not built, and a missing destination
    with pytest.raises(HttpResponseError) as e:
        subscribe(egm, acct.id, "func-dest", {"endpointType": "AzureFunction", "properties": {"resourceId": "/subscriptions/x/resourceGroups/y/providers/Microsoft.Web/sites/z/functions/f"}})
    assert e.value.status_code == 501
    with pytest.raises(HttpResponseError) as e:
        egm.event_subscriptions.begin_create_or_update(acct.id, "empty", {"properties": {}}).result()
    assert e.value.status_code == 400
    with pytest.raises(HttpResponseError) as e:
        subscribe(egm, acct.id, "ab", dest)
    assert e.value.status_code == 400
    # a system topic needs a source that exists, and the topic type has to fit it
    with pytest.raises(HttpResponseError) as e:
        egm.system_topics.begin_create_or_update(rg, uniq("st-"), {"location": "eastus", "properties": {"topicType": "Microsoft.Storage.StorageAccounts"}}).result()
    assert e.value.status_code == 400
    with pytest.raises(HttpResponseError) as e:
        egm.system_topics.begin_create_or_update(rg, uniq("st-"), {"location": "eastus", "properties": {"source": acct.id, "topicType": "Microsoft.KeyVault.vaults"}}).result()
    assert e.value.status_code == 400
    with pytest.raises(ResourceNotFoundError):
        egm.system_topics.begin_create_or_update(rg, uniq("st-"), {"location": "eastus", "properties": {"source": acct.id + "zz", "topicType": "Microsoft.Storage.StorageAccounts"}}).result()
    # deleting what is not there is not an error
    egm.event_subscriptions.begin_delete(acct.id, "never-existed").result()


def test_system_topic_names_are_unique_per_group_not_globally(egm, acct, make_group, storage, uniq):
    other_rg = make_group("rg-st2-")
    other = Account(storage, other_rg, uniq("evacct"))
    props = {"location": "eastus", "properties": {"source": acct.id, "topicType": "Microsoft.Storage.StorageAccounts"}}
    egm.system_topics.begin_create_or_update(acct.rg, "shared-name", props).result()
    props2 = {"location": "eastus", "properties": {"source": other.id, "topicType": "Microsoft.Storage.StorageAccounts"}}
    egm.system_topics.begin_create_or_update(other_rg, "shared-name", props2).result()
