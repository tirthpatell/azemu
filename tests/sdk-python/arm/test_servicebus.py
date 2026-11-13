"""Microsoft.ServiceBus through azure-mgmt-servicebus, then used over AMQP."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.mgmt.servicebus import ServiceBusManagementClient
from azure.servicebus import ServiceBusClient, ServiceBusMessage
from azure.servicebus.management import ServiceBusAdministrationClient

from conftest import SUB

CLIENT_KW = {"retry_total": 1, "retry_backoff_factor": 0.1, "retry_backoff_max": 0.5}


@pytest.fixture
def sbm(cred, mgmt):
    with ServiceBusManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_ns(sbm, rg, name):
    return sbm.namespaces.begin_create_or_update(rg, name, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}}).result()


def conn_str(sbm, rg, ns, entity=None):
    keys = sbm.namespaces.list_keys(rg, ns, "RootManageSharedAccessKey")
    cs = keys.primary_connection_string
    return cs + (f";EntityPath={entity}" if entity else "")


def test_namespace(sbm, rg, uniq, emu):
    name = uniq("sbns", 10)
    ns = make_ns(sbm, rg, name)
    assert ns.name == name
    assert ns.sku.name == "Standard"
    assert ns.provisioning_state == "Succeeded"
    assert ns.service_bus_endpoint == f"https://{name}.servicebus.localhost:{emu.port}/"
    assert name in [n.name for n in sbm.namespaces.list_by_resource_group(rg)]
    assert name in [n.name for n in sbm.namespaces.list()]
    rules = [r.name for r in sbm.namespaces.list_authorization_rules(rg, name)]
    assert rules == ["RootManageSharedAccessKey"]


def test_name_availability(sbm, rg, uniq):
    name = uniq("sbns", 10)
    assert sbm.namespaces.check_name_availability({"name": name}).name_available is True
    make_ns(sbm, rg, name)
    r = sbm.namespaces.check_name_availability({"name": name})
    assert r.name_available is False


def test_queue_works_over_amqp(sbm, rg, uniq):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    q = sbm.queues.create_or_update(rg, name, "orders", {"properties": {"maxDeliveryCount": 3, "lockDuration": "PT30S"}})
    assert q.name == "orders"
    assert q.max_delivery_count == 3
    assert q.lock_duration.total_seconds() == 30
    got = sbm.queues.get(rg, name, "orders")
    assert got.status == "Active"
    assert [x.name for x in sbm.queues.list_by_namespace(rg, name)] == ["orders"]
    cs = conn_str(sbm, rg, name)
    with ServiceBusClient.from_connection_string(cs, **CLIENT_KW) as client:
        with client.get_queue_sender("orders") as sender:
            sender.send_messages(ServiceBusMessage("hello arm"))
        with client.get_queue_receiver("orders", max_wait_time=5) as receiver:
            msgs = receiver.receive_messages(max_message_count=1, max_wait_time=5)
            assert [str(m) for m in msgs] == ["hello arm"]
            receiver.complete_message(msgs[0])
    after = sbm.queues.get(rg, name, "orders")
    assert after.count_details.active_message_count == 0
    sbm.queues.delete(rg, name, "orders")
    with pytest.raises(ResourceNotFoundError):
        sbm.queues.get(rg, name, "orders")


def test_message_count_is_live(sbm, rg, uniq):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    sbm.queues.create_or_update(rg, name, "counted", {"properties": {}})
    with ServiceBusClient.from_connection_string(conn_str(sbm, rg, name), **CLIENT_KW) as client:
        with client.get_queue_sender("counted") as sender:
            sender.send_messages([ServiceBusMessage("a"), ServiceBusMessage("b")])
    q = sbm.queues.get(rg, name, "counted")
    assert q.message_count == 2
    assert q.count_details.active_message_count == 2


def test_topic_subscription_rule(sbm, rg, uniq):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    sbm.topics.create_or_update(rg, name, "events", {"properties": {}})
    sbm.subscriptions.create_or_update(rg, name, "events", "reds", {"properties": {"maxDeliveryCount": 5}})
    default = [r.name for r in sbm.rules.list_by_subscriptions(rg, name, "events", "reds")]
    assert default == ["$Default"]
    sbm.rules.delete(rg, name, "events", "reds", "$Default")
    rule = sbm.rules.create_or_update(
        rg, name, "events", "reds", "only-red",
        {"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "color = 'red'"}}},
    )
    assert rule.sql_filter.sql_expression == "color = 'red'"
    with ServiceBusClient.from_connection_string(conn_str(sbm, rg, name), **CLIENT_KW) as client:
        with client.get_topic_sender("events") as sender:
            sender.send_messages(ServiceBusMessage("red one", application_properties={"color": "red"}))
            sender.send_messages(ServiceBusMessage("blue one", application_properties={"color": "blue"}))
        with client.get_subscription_receiver("events", "reds", max_wait_time=5) as receiver:
            msgs = receiver.receive_messages(max_message_count=5, max_wait_time=3)
            assert [str(m) for m in msgs] == ["red one"]


def test_correlation_rule_and_bad_rule(sbm, rg, uniq):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    sbm.topics.create_or_update(rg, name, "t1", {"properties": {}})
    sbm.subscriptions.create_or_update(rg, name, "t1", "s1", {"properties": {}})
    r = sbm.rules.create_or_update(
        rg, name, "t1", "s1", "corr",
        {"properties": {"filterType": "CorrelationFilter", "correlationFilter": {"correlationId": "abc", "label": "x"}}},
    )
    assert r.correlation_filter.correlation_id == "abc"
    from azure.core.exceptions import HttpResponseError

    with pytest.raises(HttpResponseError) as e:
        sbm.rules.create_or_update(rg, name, "t1", "s1", "bad", {"properties": {"filterType": "SqlFilter", "sqlFilter": {"sqlExpression": "a = = "}}})
    assert e.value.status_code == 400


def test_authorization_rule_keys(sbm, rg, uniq):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    sbm.namespaces.create_or_update_authorization_rule(rg, name, "sender", {"properties": {"rights": ["Send"]}})
    k = sbm.namespaces.list_keys(rg, name, "sender")
    assert "SharedAccessKeyName=sender" in k.primary_connection_string
    assert k.key_name == "sender"
    k2 = sbm.namespaces.regenerate_keys(rg, name, "sender", {"keyType": "PrimaryKey"})
    assert k2.primary_key != k.primary_key
    assert k2.secondary_key == k.secondary_key


def test_delete_namespace_removes_entities(sbm, rg, uniq, emu):
    name = uniq("sbns", 10)
    make_ns(sbm, rg, name)
    sbm.queues.create_or_update(rg, name, "gone", {"properties": {}})
    cs = conn_str(sbm, rg, name)
    with ServiceBusClient.from_connection_string(cs, **CLIENT_KW) as client:
        with client.get_queue_sender("gone") as sender:
            sender.send_messages(ServiceBusMessage("x"))
    sbm.namespaces.begin_delete(rg, name).result()
    with pytest.raises(ResourceNotFoundError):
        sbm.namespaces.get(rg, name)
    # the queue and its message are gone: the namespace's administration endpoint no longer has the queue (this holds
    # whether or not the emulator creates entities by being named, which it does not by default)
    with ServiceBusAdministrationClient.from_connection_string(cs, connection_verify=emu.ca_file) as admin:
        with pytest.raises(ResourceNotFoundError):
            admin.get_queue("gone")
