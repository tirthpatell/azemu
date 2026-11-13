"""AMQP over WebSocket (`TransportType.AmqpOverWebsocket`), sync and async clients, plain (`ws://`) and TLS (`wss://`).

The SDK ignores the port of the connection string for this transport (it always dials 443), so the tests point it at the
emulator with `custom_endpoint_address`: `http://<host>:<port>/` gives `ws://`, `https://<host>:<port>/` gives `wss://`
(the CA comes from `connection_verify`). The AMQP `open` frame still names `<namespace>.servicebus.localhost`, which is
what picks Service Bus when the emulator also serves Event Hubs. See docs/services/websocket.md.

The last tests start the unified binary (`target/debug/azemu`, or `AZEMU_UNIFIED_BIN`) on a free port of their own and use
Service Bus and Event Hubs over WebSocket on that one port. They are skipped when the binary has not been built.
"""

import asyncio
import os
import pathlib
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from datetime import timedelta

import pytest

import emubin
from azure.core.credentials import AzureNamedKeyCredential
from azure.servicebus import ServiceBusClient, ServiceBusMessage, TransportType
from azure.servicebus.aio import ServiceBusClient as AioServiceBusClient
from azure.servicebus.exceptions import ServiceBusError
from azure.servicebus.management import ServiceBusAdministrationClient

import sbutil
from sbutil import async_drain, body_of, bodies, drain_bodies, drain_sub, receive_n, send_n

ROOT = pathlib.Path(__file__).resolve().parents[3]
WS = TransportType.AmqpOverWebsocket


@pytest.fixture(autouse=True)
def _needs_ca(emu):
    if not emu.ca_file:
        pytest.skip("no CA file: the emulator under test was not started by the fixture (set AZEMU_CA_FILE)")


# -- clients ------------------------------------------------------------------------------------------------
def ws_kwargs(emu, namespace="default", tls=False, **extra):
    host = sbutil.host(emu, namespace)
    kw = {
        **sbutil.CLIENT_KW,
        "transport_type": WS,
        "custom_endpoint_address": f"{'https' if tls else 'http'}://{host}/",
        **extra,
    }
    if tls:
        kw["connection_verify"] = emu.ca_file
    return kw


def _args(emu, namespace, tls):
    """(fully qualified namespace, credential, kwargs) for ServiceBusClient / its aio twin."""
    host = sbutil.host(emu, namespace)
    cred = AzureNamedKeyCredential(sbutil.KEY_NAME, sbutil.KEY)
    return host, cred


def ws_client(emu, namespace="default", tls=False, **extra):
    """Plain: connection string with UseDevelopmentEmulator (ws://). TLS: namespace name + credential (wss://)."""
    kw = ws_kwargs(emu, namespace, tls, **extra)
    if tls:
        host, cred = _args(emu, namespace, tls)
        return ServiceBusClient(host.split(":")[0], cred, **kw)
    return ServiceBusClient.from_connection_string(sbutil.conn_str(emu, namespace), **kw)


def aio_ws_client(emu, namespace="default", tls=False, **extra):
    kw = ws_kwargs(emu, namespace, tls, **extra)
    if tls:
        host, cred = _args(emu, namespace, tls)
        return AioServiceBusClient(host.split(":")[0], cred, **kw)
    return AioServiceBusClient.from_connection_string(sbutil.conn_str(emu, namespace), **kw)


TLS_MODES = pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])


# -- sync: queue ----------------------------------------------------------------------------------------------
@TLS_MODES
def test_send_receive_and_complete_a_queue_message(emu, queue, tls):
    with ws_client(emu, tls=tls) as client:
        with client.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("over websocket", application_properties={"k": "v"}, subject="sub"))
        with client.get_queue_receiver(queue, max_wait_time=3) as r:
            (m,) = receive_n(r, 1, max_wait=3)
            assert body_of(m) == "over websocket"
            assert m.application_properties == {b"k": b"v"} or m.application_properties == {"k": "v"}
            assert m.subject == "sub"
    # settled: nothing is left
    with sbutil.make_client(emu) as plain:
        assert drain_bodies(plain, queue) == []


@TLS_MODES
def test_many_messages_in_order_with_settlement_choices(emu, queue, tls):
    with ws_client(emu, tls=tls) as client:
        sent = send_n(client, queue, 20)
        with client.get_queue_receiver(queue, max_wait_time=3) as r:
            first = receive_n(r, 3, max_wait=3, settle=None)
            r.abandon_message(first[0])
            r.complete_message(first[1])
            r.dead_letter_message(first[2], reason="r", error_description="d")
        assert [body_of(m) for m in first] == sent[:3]
        dlq = drain_bodies(client, queue, sub_queue=sbutil.DLQ)
        assert dlq == [sent[2]]
        rest = drain_bodies(client, queue)
        assert sorted(rest) == sorted([sent[0]] + sent[3:])


@TLS_MODES
def test_management_operations_peek_schedule_cancel(emu, queue, tls):
    with ws_client(emu, tls=tls) as client:
        send_n(client, queue, 2)
        with client.get_queue_receiver(queue, max_wait_time=2) as r:
            assert len(r.peek_messages(max_message_count=5)) == 2
        with client.get_queue_sender(queue) as s:
            seqs = s.schedule_messages(ServiceBusMessage("later"), sbutil.utcnow() + timedelta(seconds=120))
            assert len(seqs) == 1
            s.cancel_scheduled_messages(seqs)
        with client.get_queue_receiver(queue, max_wait_time=2) as r:
            assert len(r.peek_messages(max_message_count=10)) == 2


@TLS_MODES
def test_session_state_over_websocket(emu, session_queue, tls):
    with ws_client(emu, tls=tls) as client:
        with client.get_queue_sender(session_queue) as s:
            s.send_messages(ServiceBusMessage("in-session", session_id="ws"))
        with client.get_queue_receiver(session_queue, session_id="ws", max_wait_time=3) as r:
            (m,) = receive_n(r, 1, max_wait=3)
            assert m.session_id == "ws"
            r.session.set_state("ws-state")
            assert r.session.get_state() == b"ws-state"


@TLS_MODES
def test_a_message_that_spans_many_websocket_frames(emu, queue, tls):
    # 240 000 bytes: about four AMQP frames of 64 KB, and eight WebSocket frames of at most 32 KB each way
    big = ("0123456789abcdef" * 15000)
    assert len(big) == 240000
    with ws_client(emu, tls=tls) as client:
        with client.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage(big))
            s.send_messages([ServiceBusMessage(big[::-1]), ServiceBusMessage("small")])
        with client.get_queue_receiver(queue, max_wait_time=5) as r:
            got = receive_n(r, 3, max_wait=5)
    assert [body_of(m) for m in got] == [big, big[::-1], "small"]


def test_websocket_and_plain_clients_share_one_namespace(emu, queue):
    with ws_client(emu) as ws, ws_client(emu, tls=True) as wss, sbutil.make_client(emu) as plain:
        sent = send_n(ws, queue, 2, prefix="ws") + send_n(wss, queue, 2, prefix="wss") + send_n(plain, queue, 2, prefix="amqp")
        assert sorted(drain_bodies(wss, queue)) == sorted(sent)


def test_two_connections_at_once_and_reconnect_after_close(emu, queue):
    c1 = ws_client(emu)
    c2 = ws_client(emu)
    try:
        send_n(c1, queue, 1, prefix="a")
        send_n(c2, queue, 1, prefix="b")
    finally:
        c1.close()
        c2.close()
    with ws_client(emu) as c3:
        assert sorted(drain_bodies(c3, queue)) == ["a-0", "b-0"]


# -- sync: topic and subscription -------------------------------------------------------------------------------
@TLS_MODES
def test_topic_and_subscription(emu, topic_sub, tls):
    topic, sub = topic_sub
    with ws_client(emu, tls=tls) as client:
        with client.get_topic_sender(topic) as s:
            s.send_messages([ServiceBusMessage(f"t-{i}") for i in range(3)])
        got = drain_sub(client, topic, sub)
        assert bodies(got) == ["t-0", "t-1", "t-2"]


def test_subscription_rule_filtering_over_websocket(emu, topic, new_sub, admin):
    from azure.servicebus.management import SqlRuleFilter

    sub = new_sub(topic, remove_default=True)
    admin.create_rule(topic, sub, "only-red", filter=SqlRuleFilter("color = 'red'"))
    with ws_client(emu) as client:
        with client.get_topic_sender(topic) as s:
            s.send_messages([
                ServiceBusMessage("r", application_properties={"color": "red"}),
                ServiceBusMessage("b", application_properties={"color": "blue"}),
            ])
        assert bodies(drain_sub(client, topic, sub)) == ["r"]


# -- failures --------------------------------------------------------------------------------------------------------
def test_wss_with_an_untrusted_ca_is_refused(emu, queue):
    host, cred = _args(emu, "default", True)
    kw = ws_kwargs(emu, tls=True, retry_total=0)
    kw.pop("connection_verify")  # the system CA bundle does not know the emulator CA
    with ServiceBusClient(host.split(":")[0], cred, **kw) as client:
        with pytest.raises(ServiceBusError):
            with client.get_queue_sender(queue) as s:
                s.send_messages(ServiceBusMessage("x"))


def test_ws_to_a_port_with_nothing_listening_fails_fast(emu, queue):
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    dead = s.getsockname()[1]
    s.close()
    kw = {**sbutil.CLIENT_KW, "retry_total": 0, "transport_type": WS, "custom_endpoint_address": f"http://default.servicebus.localhost:{dead}/"}
    client = ServiceBusClient.from_connection_string(sbutil.conn_str(emu), **kw)
    with client:
        with pytest.raises(ServiceBusError):
            with client.get_queue_sender(queue) as snd:
                snd.send_messages(ServiceBusMessage("x"))


# -- async -----------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])
async def test_async_queue_send_receive_complete(emu, queue, tls):
    async with aio_ws_client(emu, tls=tls) as client:
        async with client.get_queue_sender(queue) as s:
            await s.send_messages([ServiceBusMessage(f"a-{i}") for i in range(5)])
        async with client.get_queue_receiver(queue, max_wait_time=3) as r:
            got = []
            for _ in range(6):
                if len(got) >= 5:
                    break
                for m in await r.receive_messages(max_message_count=5 - len(got), max_wait_time=3):
                    got.append(m)
                    await r.complete_message(m)
            assert bodies(got) == [f"a-{i}" for i in range(5)]
        async with client.get_queue_receiver(queue, max_wait_time=1) as r:
            assert await async_drain(r) == []


@pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])
async def test_async_large_message_and_peek(emu, queue, tls):
    big = "x0y1z2w3" * 30000  # 240 000 bytes
    async with aio_ws_client(emu, tls=tls) as client:
        async with client.get_queue_sender(queue) as s:
            await s.send_messages(ServiceBusMessage(big))
        async with client.get_queue_receiver(queue, max_wait_time=3) as r:
            peeked = await r.peek_messages(max_message_count=2)
            assert [body_of(m) for m in peeked] == [big]
            (m,) = await r.receive_messages(max_message_count=1, max_wait_time=5)
            assert body_of(m) == big
            await r.complete_message(m)


async def test_async_topic_and_subscription(emu, topic_sub):
    topic, sub = topic_sub
    async with aio_ws_client(emu) as client:
        async with client.get_topic_sender(topic) as s:
            await s.send_messages(ServiceBusMessage("fanout"))
        async with client.get_subscription_receiver(topic, sub, max_wait_time=3) as r:
            (m,) = await r.receive_messages(max_message_count=1, max_wait_time=3)
            assert body_of(m) == "fanout"
            await r.complete_message(m)


async def test_async_connection_answers_the_clients_websocket_pings(emu, queue):
    # aiohttp pings every 10 seconds and closes the connection when no pong comes back
    async with aio_ws_client(emu) as client:
        async with client.get_queue_sender(queue) as s:
            await s.send_messages(ServiceBusMessage("before"))
            await asyncio.sleep(13)
            await s.send_messages(ServiceBusMessage("after"))
        async with client.get_queue_receiver(queue, max_wait_time=3) as r:
            got = await r.receive_messages(max_message_count=2, max_wait_time=3)
            assert bodies(got) == ["before", "after"]
            for m in got:
                await r.complete_message(m)


# -- the unified binary: Service Bus and Event Hubs over WebSocket on one port -------------------------------------
class _Unified:
    """Just enough of the `emu` fixture for sbutil: port, CA file, URLs."""

    def __init__(self, port, ca_file):
        self.port = port
        self.ca_file = ca_file

    def url(self, label, account, scheme="http"):
        return f"{scheme}://{account}.{label}.localhost:{self.port}"

    @property
    def gateway(self):
        return f"http://127.0.0.1:{self.port}"


@pytest.fixture(scope="module")
def unified():
    binary = emubin.unified()
    if not binary or not os.path.exists(binary):
        pytest.fail(emubin.missing_message())
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with tempfile.TemporaryDirectory(prefix="azemu-ws-") as d:
        proc = subprocess.Popen([binary, "--port", str(port), "--cert-dir", d], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.time() + 20
            while True:
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{port}/_azemu/health", timeout=1).read()
                    break
                except Exception:  # noqa: BLE001 - not up yet
                    if proc.poll() is not None or time.time() > deadline:
                        raise RuntimeError("the unified emulator did not start")
                    time.sleep(0.1)
            yield _Unified(port, os.path.join(d, "ca.pem"))
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


@pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])
def test_unified_binary_service_bus_and_event_hubs_over_websocket_on_one_port(unified, tls):
    import json

    from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

    queue = sbutil.uniq("uq")
    admin = ServiceBusAdministrationClient.from_connection_string(sbutil.conn_str(unified), **{**sbutil.ADMIN_KW, "connection_verify": unified.ca_file})
    admin.create_queue(queue)
    admin.close()

    hub = sbutil.uniq("uhub")
    req = urllib.request.Request(
        f"{unified.gateway}/_azemu/eventhubs/myns/hubs/{hub}",
        data=json.dumps({"partitionCount": 2, "consumerGroups": []}).encode(),
        method="PUT",
        headers={"Content-Type": "application/json"},
    )
    urllib.request.urlopen(req, timeout=10).read()

    # Service Bus: the open frame names <ns>.servicebus.localhost
    with ws_client(unified, tls=tls) as sb:
        send_n(sb, queue, 2, prefix="sb")
        assert drain_bodies(sb, queue) == ["sb-0", "sb-1"]

    # Event Hubs on the same port: the open frame names <ns>.eventhub.localhost
    eh_host = f"myns.eventhub.localhost:{unified.port}"
    kw = {"transport_type": WS, "retry_total": 1, "retry_backoff_factor": 0.05, "auth_timeout": 10,
          "custom_endpoint_address": f"{'https' if tls else 'http'}://{eh_host}/"}
    if tls:
        kw["connection_verify"] = unified.ca_file
        key = "RootManageSharedAccessKey"
        cred = AzureNamedKeyCredential(key, "c2VjcmV0")
        producer = EventHubProducerClient("myns.eventhub.localhost", hub, cred, **kw)
        consumer = EventHubConsumerClient("myns.eventhub.localhost", hub, "$Default", cred, **kw)
    else:
        conn = f"Endpoint=sb://{eh_host};SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=c2VjcmV0;UseDevelopmentEmulator=true"
        producer = EventHubProducerClient.from_connection_string(conn, eventhub_name=hub, **kw)
        consumer = EventHubConsumerClient.from_connection_string(conn, consumer_group="$Default", eventhub_name=hub, **kw)
    try:
        assert producer.get_partition_ids() == ["0", "1"]
        batch = producer.create_batch(partition_id="0")
        batch.add(EventData("eh-over-ws"))
        producer.send_batch(batch)
        got = []
        t = threading.Thread(
            target=lambda: consumer.receive(
                lambda ctx, event: got.append(event.body_as_str()), starting_position="-1", max_wait_time=1
            ),
            daemon=True,
        )
        t.start()
        sbutil.wait_until(lambda: got, timeout=10, interval=0.1)
        assert got == ["eh-over-ws"]
    finally:
        consumer.close()
        producer.close()
