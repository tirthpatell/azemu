"""AMQP inside TLS on the same port as plain AMQP (certificate from the emulator's CA, `emu.ca_file`).

The SDK dials `<namespace>.eventhub.localhost:<port>` taken from `custom_endpoint_address`, checks the server certificate against
`connection_verify` for that name, then runs the same SASL ANONYMOUS / AMQP open / `$cbs` exchange as without TLS (proved with
`sdk_dial_probe.py`; see docs/sdk-notes.md). `transport_type=TransportType.Amqp` is needed because a custom endpoint address otherwise
selects AMQP over WebSocket. Skipped when the run has no CA file (an external emulator without AZEMU_CA_FILE).
"""

import tempfile

import pytest
from azure.core.credentials import AzureNamedKeyCredential
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient, TransportType
from azure.eventhub.exceptions import EventHubError

import ehutil
from ehutil import Collector, KEY, KEY_NAME, bodies, host, make_consumer, make_producer, read_from_start, send_n


@pytest.fixture(autouse=True)
def _needs_ca(emu):
    if not emu.ca_file:
        pytest.skip("no CA file: the emulator under test was not started by the fixture (set AZEMU_CA_FILE)")


def tls_kwargs(emu, namespace=ehutil.NAMESPACE, **extra):
    return dict(
        custom_endpoint_address=f"sb://{host(emu, namespace)}", transport_type=TransportType.Amqp, connection_verify=emu.ca_file,
        **{**ehutil.CLIENT_KW, **extra},
    )


def tls_producer(emu, hub, namespace=ehutil.NAMESPACE, **extra):
    return EventHubProducerClient(
        host(emu, namespace).split(":")[0], hub, AzureNamedKeyCredential(KEY_NAME, KEY), **tls_kwargs(emu, namespace, **extra))


def tls_consumer(emu, hub, group="$Default", namespace=ehutil.NAMESPACE, **extra):
    return EventHubConsumerClient(
        host(emu, namespace).split(":")[0], hub, group, AzureNamedKeyCredential(KEY_NAME, KEY), **tls_kwargs(emu, namespace, **extra))


def test_send_and_receive_over_tls(emu, hub, track):
    p = track(tls_producer(emu, hub))
    sent = send_n(p, 3, partition_id="0")
    with Collector(tls_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(3)) == sent


def test_management_operations_work_over_tls(emu, hub, track):
    p = track(tls_producer(emu, hub))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]
    send_n(p, 2, partition_id="1")
    assert p.get_partition_properties("1")["is_empty"] is False
    assert track(tls_consumer(emu, hub)).get_eventhub_properties()["eventhub_name"] == hub


def test_batches_and_partition_keys_over_tls(emu, hub, track):
    p = track(tls_producer(emu, hub))
    batch = p.create_batch(partition_key="tls-key")
    batch.add(EventData("k1"))
    batch.add(EventData("k2"))
    p.send_batch(batch)
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        events = col.wait_events(2)
        assert {e.partition_key for e in events} == {b"tls-key"} and len(col.by_partition()) == 1


def test_tls_and_plain_clients_see_the_same_hub(emu, hub, track):
    tls = track(tls_producer(emu, hub))
    plain = track(make_producer(emu, hub))
    a = send_n(tls, 2, prefix="tls", partition_id="0")
    b = send_n(plain, 2, prefix="plain", partition_id="0")
    assert bodies(read_from_start(emu, hub, 4, partition_id="0")) == a + b


def test_tls_consumer_receives_events_sent_over_plain_amqp(emu, hub, producer):
    sent = send_n(producer, 3, partition_id="2")
    with Collector(tls_consumer(emu, hub), partition_id="2", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(3)) == sent


def test_tls_client_for_another_namespace(emu, track):
    name = ehutil.uniq("tlsns")
    ehutil.admin_create_hub(emu, name, partitions=2, namespace="other")
    p = track(tls_producer(emu, name, namespace="other"))
    assert p.get_partition_ids() == ["0", "1"]


def test_tls_with_an_untrusted_ca_is_refused(emu, hub, track):
    from sdk_dial_probe import make_ca

    with tempfile.TemporaryDirectory() as d:
        other_ca, _, _ = make_ca(d)
        p = track(EventHubProducerClient(
            host(emu).split(":")[0], hub, AzureNamedKeyCredential(KEY_NAME, KEY),
            **{**tls_kwargs(emu), "connection_verify": other_ca}))
        with pytest.raises(EventHubError):
            p.get_partition_ids()
