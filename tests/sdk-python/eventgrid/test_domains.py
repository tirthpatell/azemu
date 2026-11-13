"""Event Grid domains: one endpoint, many domain topics. Events are routed by their
`topic` field (Event Grid schema) or `source` (CloudEvents).
"""

import pytest
import requests
from azure.core.exceptions import HttpResponseError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent, EventGridPublisherClient

pytestmark = pytest.mark.emulator_only  # a domain has to be declared through the admin API


@pytest.fixture
def domain(admin, name):
    admin.put_topic(name, {"kind": "domain"})
    return name


@pytest.fixture
def domain_client(emu, domain, key):
    with EventGridPublisherClient(emu.url("eventgrid", domain) + "/api/events", key, retry_total=0) as c:
        yield c


def test_publish_to_a_domain_topic_creates_it(admin, domain, domain_client):
    domain_client.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1", topic="orders"))
    t = admin.request("GET", f"/topics/orders", params={"domain": domain})
    assert t.status_code == 200
    assert t.json()["published"] == 1


def test_event_without_a_topic_is_rejected_by_a_domain(domain_client):
    with pytest.raises(HttpResponseError) as e:
        domain_client.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1"))
    assert e.value.status_code == 400
    assert "domain topic" in e.value.message.lower()


def test_events_for_several_topics_in_one_request(admin, domain, domain_client, hook):
    admin.put_sub(domain, "everything", {"webhookUrl": hook.url}, expect=(201,))
    domain_client.send(
        [
            EventGridEvent(subject="s", event_type="t", data="1", data_version="1", topic="a"),
            EventGridEvent(subject="s", event_type="t", data="2", data_version="1", topic="b"),
        ]
    )
    got = hook.wait_for(2)
    assert sorted(g["body"][0]["data"] for g in got) == ["1", "2"]
    assert admin.request("GET", "/topics/a", params={"domain": domain}).json()["published"] == 1
    assert admin.request("GET", "/topics/b", params={"domain": domain}).json()["published"] == 1


def test_domain_topic_and_domain_level_subscriptions(admin, domain, domain_client, hook):
    other = type(hook)()
    try:
        admin.put_topic("acme", domain=domain)
        admin.put_sub("acme", "acme-hook", {"webhookUrl": hook.url}, domain=domain, expect=(201,))
        admin.put_sub(domain, "all-hook", {"webhookUrl": other.url}, expect=(201,))
        domain_client.send(
            [
                EventGridEvent(subject="s", event_type="t", data="for-acme", data_version="1", topic="acme"),
                EventGridEvent(subject="s", event_type="t", data="for-beta", data_version="1", topic="beta"),
            ]
        )
        assert [g["body"][0]["data"] for g in hook.wait_for(1)] == ["for-acme"]
        assert sorted(g["body"][0]["data"] for g in other.wait_for(2)) == ["for-acme", "for-beta"]
    finally:
        other.close()


def test_delivered_topic_is_the_domain_topic_resource_id(admin, domain, domain_client, hook):
    admin.put_sub(domain, "s", {"webhookUrl": hook.url}, expect=(201,))
    domain_client.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1", topic="orders"))
    (n,) = hook.wait_for(1)
    topic = n["body"][0]["topic"]
    assert topic.endswith(f"/domains/{domain}/topics/orders")
    assert topic.startswith("/subscriptions/")


def test_cloud_events_use_source_as_the_domain_topic(admin, domain, domain_client, hook):
    admin.put_sub(domain, "s", {"webhookUrl": hook.url, "deliverySchema": "CloudEventSchemaV1_0"}, expect=(201,))
    domain_client.send(CloudEvent(source="invoices", type="t", data={"n": 1}))
    (n,) = hook.wait_for(1)
    assert n["body"]["type"] == "t"
    assert admin.request("GET", "/topics/invoices", params={"domain": domain}).json()["published"] == 1


def test_domain_topic_full_resource_id_is_accepted(admin, domain, domain_client):
    rid = f"/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg/providers/Microsoft.EventGrid/domains/{domain}/topics/sales"
    domain_client.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1", topic=rid))
    assert admin.request("GET", "/topics/sales", params={"domain": domain}).status_code == 200


def test_domain_input_schema_can_be_fixed(emu, admin, name, key):
    admin.put_topic(name, {"kind": "domain", "inputSchema": "CloudEventSchemaV1_0"})
    c = EventGridPublisherClient(emu.url("eventgrid", name) + "/api/events", key, retry_total=0)
    with pytest.raises(HttpResponseError) as e:
        c.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1", topic="a"))
    assert e.value.status_code == 400
    c.send(CloudEvent(source="a", type="t"))
