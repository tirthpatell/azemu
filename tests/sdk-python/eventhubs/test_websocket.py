"""AMQP over WebSocket (`TransportType.AmqpOverWebsocket`) for Event Hubs, sync and async clients, plain (`ws://`) and TLS.

The SDK ignores the port in the connection string for this transport (it always dials 443), so the tests point it at the
emulator with `custom_endpoint_address`: `http://<host>:<port>/` gives `ws://`, `https://<host>:<port>/` gives `wss://`
(the CA comes from `connection_verify`). The AMQP `open` frame still names `<namespace>.eventhub.localhost`.
See docs/services/websocket.md.

The last tests run the unified binary (`target/debug/azemu`, or `AZEMU_UNIFIED_BIN`) on a free port of their own; they are
skipped when it has not been built.
"""

import os
import pathlib
import socket
import subprocess
import tempfile
import time
import urllib.request

import pytest

import emubin
from azure.core.credentials import AzureNamedKeyCredential
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient, TransportType
from azure.eventhub.aio import EventHubConsumerClient as AioConsumer
from azure.eventhub.aio import EventHubProducerClient as AioProducer
from azure.eventhub.exceptions import EventHubError

import ehutil
from ehutil import AsyncCollector, Collector, KEY, KEY_NAME, bodies, host, send_n

ROOT = pathlib.Path(__file__).resolve().parents[3]
WS = TransportType.AmqpOverWebsocket
TLS_MODES = pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])


@pytest.fixture(autouse=True)
def _needs_ca(emu):
    if not emu.ca_file:
        pytest.skip("no CA file: the emulator under test was not started by the fixture (set AZEMU_CA_FILE)")


# -- clients -------------------------------------------------------------------------------------------------------
def ws_kwargs(emu, namespace=ehutil.NAMESPACE, tls=False, **extra):
    kw = {
        **ehutil.CLIENT_KW,
        "transport_type": WS,
        "custom_endpoint_address": f"{'https' if tls else 'http'}://{host(emu, namespace)}/",
        **extra,
    }
    if tls:
        kw["connection_verify"] = emu.ca_file
    return kw


def _make(cls_producer, cls_consumer, emu, hub, kind, group, namespace, tls, extra):
    kw = ws_kwargs(emu, namespace, tls, **extra)
    if tls:
        # TLS: namespace name + credential (no UseDevelopmentEmulator, so the SDK uses wss://)
        fqns = host(emu, namespace).split(":")[0]
        cred = AzureNamedKeyCredential(KEY_NAME, KEY)
        if kind == "producer":
            return cls_producer(fqns, hub, cred, **kw)
        return cls_consumer(fqns, hub, group, cred, **kw)
    conn = ehutil.conn_str(emu, namespace=namespace)
    if kind == "producer":
        return cls_producer.from_connection_string(conn, eventhub_name=hub, **kw)
    return cls_consumer.from_connection_string(conn, consumer_group=group, eventhub_name=hub, **kw)


def ws_producer(emu, hub, namespace=ehutil.NAMESPACE, tls=False, **extra):
    return _make(EventHubProducerClient, EventHubConsumerClient, emu, hub, "producer", None, namespace, tls, extra)


def ws_consumer(emu, hub, group="$Default", namespace=ehutil.NAMESPACE, tls=False, **extra):
    return _make(EventHubProducerClient, EventHubConsumerClient, emu, hub, "consumer", group, namespace, tls, extra)


def aio_producer(emu, hub, namespace=ehutil.NAMESPACE, tls=False, **extra):
    return _make(AioProducer, AioConsumer, emu, hub, "producer", None, namespace, tls, extra)


def aio_consumer(emu, hub, group="$Default", namespace=ehutil.NAMESPACE, tls=False, **extra):
    return _make(AioProducer, AioConsumer, emu, hub, "consumer", group, namespace, tls, {"load_balancing_interval": 1, **extra})


# -- sync -------------------------------------------------------------------------------------------------------------
@TLS_MODES
def test_send_and_receive_over_websocket(emu, hub, track, tls):
    p = track(ws_producer(emu, hub, tls=tls))
    sent = send_n(p, 3, partition_id="0")
    with Collector(ws_consumer(emu, hub, tls=tls), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(3)) == sent


@TLS_MODES
def test_management_operations_over_websocket(emu, hub, track, tls):
    p = track(ws_producer(emu, hub, tls=tls))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]
    assert p.get_eventhub_properties()["eventhub_name"] == hub
    assert p.get_partition_properties("1")["is_empty"] is True
    send_n(p, 2, partition_id="1")
    props = p.get_partition_properties("1")
    assert props["is_empty"] is False
    assert props["last_enqueued_sequence_number"] == 1


@TLS_MODES
def test_partition_keys_and_event_properties_over_websocket(emu, hub, track, tls):
    p = track(ws_producer(emu, hub, tls=tls))
    batch = p.create_batch(partition_key="ws-key")
    e = EventData("with-props")
    e.properties = {"a": 1, "b": "two"}
    batch.add(e)
    p.send_batch(batch)
    # a plain AMQP reader must see the same event
    events = ehutil.read_all_from_start(emu, hub, 1)
    (ev,) = [x for xs in events.values() for x in xs]
    assert ev.body_as_str() == "with-props"
    assert ev.properties[b"a"] == 1 or ev.properties["a"] == 1
    assert ev.partition_key in (b"ws-key", "ws-key")


@TLS_MODES
def test_an_event_that_spans_many_websocket_frames(emu, hub1, track, tls):
    big = "0123456789abcdef" * 50000  # 800 000 bytes: 13 AMQP frames of 64 KB, 25 WebSocket frames of 32 KB
    assert len(big) == 800000
    p = track(ws_producer(emu, hub1, tls=tls))
    p.send_event(EventData(big))
    p.send_event(EventData("after"))
    with Collector(ws_consumer(emu, hub1, tls=tls), partition_id="0", starting_position="-1", max_wait_time=2) as col:
        got = col.wait_events(2, timeout=15)
    assert [e.body_as_str() for e in got] == [big, "after"]


def test_websocket_and_plain_clients_see_the_same_hub(emu, hub1, track):
    ws = track(ws_producer(emu, hub1))
    wss = track(ws_producer(emu, hub1, tls=True))
    plain = track(ehutil.make_producer(emu, hub1))
    sent = send_n(ws, 1, prefix="ws", partition_id="0") + send_n(wss, 1, prefix="wss", partition_id="0") + send_n(
        plain, 1, prefix="amqp", partition_id="0"
    )
    with Collector(ws_consumer(emu, hub1, tls=True), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(3)) == sent


def test_live_receive_after_start_over_websocket(emu, hub1, track):
    # Starts at "@latest". `Collector.wait_live` (a marker every 0.3 s) does not suit this transport: the SDK waits for
    # the next frame for up to a second per read and only hands over a batch when the frames stop, so a steady stream of
    # markers keeps the batch open. Instead wait for the first empty callback (the link is open and idle), then send.
    p = track(ws_producer(emu, hub1))
    col = Collector(ws_consumer(emu, hub1), partition_id="0", max_wait_time=1).start()
    try:
        col.wait_until(lambda c: c.empty_callbacks >= 1, 10, "the link to open and go idle")
        sent = send_n(p, 2, prefix="live", partition_id="0")
        assert bodies(col.wait_events(2)) == sent
    finally:
        col.stop()


def test_consumer_close_ends_the_receive_loop(emu, hub1, track):
    p = track(ws_producer(emu, hub1))
    send_n(p, 1, partition_id="0")
    c = ws_consumer(emu, hub1)
    col = Collector(c, partition_id="0", starting_position="-1", max_wait_time=1).start()
    col.wait_events(1)
    col.stop()
    assert not col._thread.is_alive()
    assert col.thread_error is None


def test_wss_with_an_untrusted_ca_is_refused(emu, hub, track):
    kw = ws_kwargs(emu, tls=True, retry_total=0, auth_timeout=5)
    kw.pop("connection_verify")
    p = track(EventHubProducerClient(host(emu).split(":")[0], hub, AzureNamedKeyCredential(KEY_NAME, KEY), **kw))
    with pytest.raises(EventHubError):
        p.get_partition_ids()


def test_ws_to_a_port_with_nothing_listening_fails(emu, hub, track):
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    dead = s.getsockname()[1]
    s.close()
    kw = {**ehutil.CLIENT_KW, "retry_total": 0, "auth_timeout": 5, "transport_type": WS,
          "custom_endpoint_address": f"http://{ehutil.NAMESPACE}.eventhub.localhost:{dead}/"}
    p = track(EventHubProducerClient.from_connection_string(ehutil.conn_str(emu), eventhub_name=hub, **kw))
    with pytest.raises(EventHubError):
        p.get_partition_ids()


# -- async ---------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("tls", [False, True], ids=["ws", "wss"])
async def test_async_send_and_receive_over_websocket(emu, hub, tls):
    p = aio_producer(emu, hub, tls=tls)
    try:
        assert await p.get_partition_ids() == ["0", "1", "2", "3"]
        sent = await ehutil.async_send_n(p, 3, partition_id="0")
    finally:
        await p.close()
    col = AsyncCollector(aio_consumer(emu, hub, tls=tls), partition_id="0", starting_position="-1", max_wait_time=1).start()
    try:
        got = await col.wait_events(3)
        assert bodies(got) == sent
    finally:
        await col.stop()


async def test_async_large_event_over_websocket(emu, hub1):
    big = "fedcba9876543210" * 40000  # 640 000 bytes
    p = aio_producer(emu, hub1)
    try:
        await p.send_event(EventData(big), partition_id="0")
    finally:
        await p.close()
    col = AsyncCollector(aio_consumer(emu, hub1, tls=True), partition_id="0", starting_position="-1", max_wait_time=2).start()
    try:
        (ev,) = await col.wait_events(1, timeout=15)
        assert ev.body_as_str() == big
    finally:
        await col.stop()


# -- the unified binary on one port -----------------------------------------------------------------------------------
class _Unified:
    """Just enough of the `emu` fixture for ehutil: port, CA file, URLs."""

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


@TLS_MODES
def test_unified_binary_event_hubs_over_websocket(unified, track, tls):
    hub = ehutil.uniq("uhub")
    ehutil.admin_create_hub(unified, hub, 2)
    p = track(ws_producer(unified, hub, tls=tls))
    assert p.get_partition_ids() == ["0", "1"]
    sent = send_n(p, 2, partition_id="1")
    with Collector(ws_consumer(unified, hub, tls=tls), partition_id="1", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(2)) == sent


def test_unified_binary_picks_event_hubs_from_the_open_frame_not_the_http_host(unified, track):
    # the WebSocket request goes to 127.0.0.1 (the Host header says so); only the AMQP open frame names the namespace
    hub = ehutil.uniq("uhub")
    ehutil.admin_create_hub(unified, hub, 1)
    kw = {
        **ehutil.CLIENT_KW,
        "transport_type": WS,
        "custom_endpoint_address": f"http://127.0.0.1:{unified.port}/",
    }
    p = track(EventHubProducerClient.from_connection_string(ehutil.conn_str(unified), eventhub_name=hub, **kw))
    assert p.get_partition_ids() == ["0"]
