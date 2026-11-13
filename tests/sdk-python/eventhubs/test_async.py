"""The same scenarios through `azure.eventhub.aio` (asyncio; pytest-asyncio runs in auto mode via ../pytest.ini).

The aio clients share the transport and the wire behaviour with the sync ones; these tests exist because the async code paths
(`_producer_async`, `_consumer_async`, the async event processor, async buffered producer, async checkpoint store) are separate
code. Callbacks are coroutines. Sources as for the sync modules.
"""

import asyncio
import time
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.credentials import AzureNamedKeyCredential
from azure.eventhub import CloseReason, EventData
from azure.eventhub.aio import EventHubConsumerClient, EventHubProducerClient
from azure.eventhub.aio._eventprocessor.in_memory_checkpoint_store import InMemoryCheckpointStore
from azure.eventhub.amqp import AmqpAnnotatedMessage, AmqpMessageBodyType
from azure.eventhub.exceptions import ConnectError, EventHubError, OperationTimeoutError

import ehutil
from ehutil import (
    AsyncCollector, async_read_from_start, async_send_n, bodies, host, make_async_consumer, make_async_producer, texts,
)

ALL = ["0", "1", "2", "3"]


async def until(fn, timeout=8.0, what="condition"):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if asyncio.iscoroutine(last):
            last = await last
        if last:
            return last
        await asyncio.sleep(0.05)
    raise AssertionError(f"{what} not met within {timeout}s (last: {last!r})")


@pytest.fixture
async def aproducer(emu, hub):
    p = make_async_producer(emu, hub)
    yield p
    await p.close()


# -- properties -----------------------------------------------------------------------------------------------------------------
async def test_async_eventhub_properties(emu, hub, aproducer):
    props = await aproducer.get_eventhub_properties()
    assert props["eventhub_name"] == hub and props["partition_ids"] == ALL
    assert props["created_at"].tzinfo is not None


async def test_async_partition_ids_on_consumer(emu, hub):
    async with make_async_consumer(emu, hub) as c:
        assert await c.get_partition_ids() == ALL


async def test_async_partition_properties_empty_then_not(emu, hub, aproducer):
    assert (await aproducer.get_partition_properties("0"))["is_empty"] is True
    await async_send_n(aproducer, 2, partition_id="0")
    props = await aproducer.get_partition_properties("0")
    assert props["is_empty"] is False
    assert props["last_enqueued_sequence_number"] - props["beginning_sequence_number"] == 1


async def test_async_unknown_partition_properties_is_connect_error(emu, hub, aproducer):
    with pytest.raises(ConnectError):
        await aproducer.get_partition_properties("XYZ")


# -- sending ---------------------------------------------------------------------------------------------------------------------
async def test_async_send_batch_and_read(emu, hub, aproducer):
    sent = await async_send_n(aproducer, 5, partition_id="0")
    assert bodies(await async_read_from_start(emu, hub, 5, partition_id="0")) == sent


async def test_async_send_event_and_list(emu, hub, aproducer):
    await aproducer.send_event(EventData("one"), partition_id="1")
    await aproducer.send_batch([EventData("two"), EventData("three")], partition_id="1")
    assert bodies(await async_read_from_start(emu, hub, 3, partition_id="1")) == ["one", "two", "three"]


async def test_async_partition_id_routes_events(emu, hub, aproducer):
    for pid in ALL:
        await async_send_n(aproducer, 2, prefix=f"p{pid}", partition_id=pid)
    got = await async_read_from_start(emu, hub, 8)
    col_by = {}
    for e in got:
        col_by.setdefault(e.body_as_str().split("-")[0], []).append(e.body_as_str())
    assert sorted(col_by) == [f"p{pid}" for pid in ALL]


async def test_async_partition_key_keeps_events_together(emu, hub, aproducer):
    batch = await aproducer.create_batch(partition_key="async-key")
    for i in range(4):
        batch.add(EventData(f"k{i}"))
    await aproducer.send_batch(batch)
    client = make_async_consumer(emu, hub)
    col = AsyncCollector(client, starting_position="-1", max_wait_time=1).start()
    try:
        await col.wait_events(4)
        assert len(col.by_partition()) == 1
        assert {e.partition_key for e in col.events} == {b"async-key"}
    finally:
        await col.stop()


async def test_async_empty_batch_sends_nothing(emu, hub, aproducer):
    await aproducer.send_batch(await aproducer.create_batch())
    await aproducer.send_batch([])
    for pid in ALL:
        assert (await aproducer.get_partition_properties(pid))["is_empty"] is True


async def test_async_bodies_and_properties_round_trip(emu, hub, aproducer):
    ev = EventData("text")
    ev.properties = {"s": "str", "i": 5, "b": True}
    ev.content_type = "text/plain"
    ev.message_id = "mid"
    await aproducer.send_event(ev, partition_id="0")
    await aproducer.send_event(AmqpAnnotatedMessage(sequence_body=[1, b"x"]), partition_id="0")
    await aproducer.send_event(AmqpAnnotatedMessage(value_body={b"k": 1}), partition_id="0")
    a, b, c = await async_read_from_start(emu, hub, 3, partition_id="0")
    assert a.body_as_str() == "text" and a.properties == {b"s": b"str", b"i": 5, b"b": True}
    assert (a.content_type, a.message_id) == ("text/plain", "mid")
    assert b.body_type == AmqpMessageBodyType.SEQUENCE and c.raw_amqp_message.body == {b"k": 1}


async def test_async_system_properties(emu, hub, aproducer):
    await async_send_n(aproducer, 4, partition_id="2")
    events = await async_read_from_start(emu, hub, 4, partition_id="2")
    seqs = [e.sequence_number for e in events]
    assert seqs == list(range(seqs[0], seqs[0] + 4))
    offsets = [int(e.offset) for e in events]
    assert offsets == sorted(set(offsets))
    assert all(abs(datetime.now(timezone.utc) - e.enqueued_time) < timedelta(minutes=1) for e in events)


async def test_async_batch_size_limit(emu, hub, aproducer):
    batch = await aproducer.create_batch(max_size_in_bytes=1000)
    with pytest.raises(ValueError):
        for _ in range(100):
            batch.add(EventData("A" * 100))
    with pytest.raises(ValueError):
        await aproducer.create_batch(max_size_in_bytes=5 * 1024 * 1024)


async def test_async_send_to_unknown_partition_is_connect_error(emu, hub, aproducer):
    with pytest.raises(ConnectError):
        await aproducer.send_event(EventData("x"), partition_id="99")


async def test_async_negative_timeout_is_an_operation_timeout(emu, hub, aproducer):
    with pytest.raises(OperationTimeoutError):
        await aproducer.send_event(EventData("x"), timeout=-1)


async def test_async_on_success_callback(emu, hub):
    sent = []

    async def on_success(events, pid):
        sent.append((len(events), pid))

    async with make_async_producer(emu, hub, on_success=on_success) as p:
        await p.send_batch([EventData("a"), EventData("b")], partition_id="1")
        await p.send_event(EventData("c"))
    assert sent == [(2, "1"), (1, None)]


async def test_async_large_event(emu, hub, aproducer):
    await aproducer.send_event(EventData(b"x" * 900_000), partition_id="0")
    (ev,) = await async_read_from_start(emu, hub, 1, partition_id="0")
    assert len(b"".join(ev.body)) == 900_000


# -- receiving ---------------------------------------------------------------------------------------------------------------------
async def test_async_receive_all_partitions(emu, hub, aproducer):
    for pid in ALL:
        await async_send_n(aproducer, 2, prefix=f"r{pid}", partition_id=pid)
    col = AsyncCollector(make_async_consumer(emu, hub), starting_position="-1", max_wait_time=1).start()
    try:
        await col.wait_events(8)
        assert {pid: bodies(v) for pid, v in col.by_partition().items()} == {pid: texts(2, f"r{pid}") for pid in ALL}
    finally:
        await col.stop()


async def test_async_receive_per_event_callback(emu, hub, aproducer):
    sent = await async_send_n(aproducer, 3, partition_id="0")
    col = AsyncCollector(make_async_consumer(emu, hub), batch=False, partition_id="0", starting_position="-1", max_wait_time=1).start()
    try:
        assert bodies(await col.wait_events(3)) == sent
    finally:
        await col.stop()


async def test_async_live_receiver_at_latest(emu, hub1):
    async with make_async_producer(emu, hub1) as p:
        await async_send_n(p, 2, prefix="old", partition_id="0")
        col = AsyncCollector(make_async_consumer(emu, hub1), partition_id="0", max_wait_time=1).start()
        try:
            await col.wait_live(p, ["0"])
            sent = await async_send_n(p, 2, prefix="new", partition_id="0")
            assert bodies(await col.wait_events(2)) == sent
            await asyncio.sleep(0.5)
            assert len(col.items) == 2
        finally:
            await col.stop()


async def test_async_sequence_number_positions(emu, hub, aproducer):
    await async_send_n(aproducer, 5, partition_id="0")
    first = await async_read_from_start(emu, hub, 5, partition_id="0")
    seq = first[1].sequence_number
    assert bodies(await async_read_from_start(emu, hub, 3, partition_id="0", starting_position=seq)) == ["m-2", "m-3", "m-4"]
    got = await async_read_from_start(emu, hub, 4, partition_id="0", starting_position=seq, starting_position_inclusive=True)
    assert bodies(got) == ["m-1", "m-2", "m-3", "m-4"]


async def test_async_offset_and_datetime_positions(emu, hub, aproducer):
    await async_send_n(aproducer, 4, partition_id="0")
    first = await async_read_from_start(emu, hub, 4, partition_id="0")
    assert bodies(await async_read_from_start(emu, hub, 2, partition_id="0", starting_position=first[1].offset)) == ["m-2", "m-3"]
    got = await async_read_from_start(emu, hub, 4, partition_id="0", starting_position=datetime.now(timezone.utc) - timedelta(days=1))
    assert bodies(got) == texts(4)


async def test_async_dict_of_positions(emu, hub2):
    async with make_async_producer(emu, hub2) as p:
        await async_send_n(p, 3, prefix="a", partition_id="0")
        await async_send_n(p, 3, prefix="b", partition_id="1")
    first = await async_read_from_start(emu, hub2, 3, partition_id="1")
    col = AsyncCollector(make_async_consumer(emu, hub2), max_wait_time=1, starting_position={"0": "-1", "1": first[0].sequence_number}).start()
    try:
        await col.wait_events(3 + 2)
        await asyncio.sleep(0.5)
        assert {pid: bodies(v) for pid, v in col.by_partition().items()} == {"0": texts(3, "a"), "1": ["b-1", "b-2"]}
    finally:
        await col.stop()


async def test_async_max_batch_size_and_idle_callback(emu, hub, aproducer):
    await async_send_n(aproducer, 10, partition_id="0")
    col = AsyncCollector(make_async_consumer(emu, hub), partition_id="0", starting_position="-1", max_batch_size=3, max_wait_time=1).start()
    try:
        await col.wait_events(10)
        await until(lambda: col.empty_callbacks >= 1, 6, "an empty callback")
    finally:
        await col.stop()


async def test_async_partition_callbacks(emu, hub):
    col = AsyncCollector(make_async_consumer(emu, hub), starting_position="-1", max_wait_time=1).start()
    await until(lambda: len(col.initialized) >= 4, 8, "4 partitions initialized")
    await col.stop()
    assert sorted(col.initialized) == ALL
    assert sorted(pid for pid, _ in col.closed) == ALL and {r for _, r in col.closed} == {CloseReason.SHUTDOWN}


async def test_async_last_enqueued_event_properties(emu, hub, aproducer):
    await async_send_n(aproducer, 3, partition_id="0")
    seen = {}

    async def on_batch(ctx, events):
        if events:
            seen["props"] = ctx.last_enqueued_event_properties
            seen["last"] = events[-1]

    async with make_async_consumer(emu, hub) as c:
        task = asyncio.ensure_future(c.receive_batch(on_batch, partition_id="0", starting_position="-1", max_wait_time=1,
                                                      track_last_enqueued_event_properties=True))
        await until(lambda: "props" in seen, 8, "a batch")
        await c.close()
        try:
            await asyncio.wait_for(task, 5)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            task.cancel()
    assert set(seen["props"]) == {"sequence_number", "offset", "enqueued_time", "retrieval_time"}
    assert seen["props"]["sequence_number"] == seen["last"].sequence_number


async def test_async_receive_twice_is_a_value_error(emu, hub):
    client = make_async_consumer(emu, hub)
    col = AsyncCollector(client, starting_position="-1", max_wait_time=1).start()
    try:
        await until(lambda: col.initialized, 8, "initialisation")

        async def noop(ctx, evs):
            pass

        with pytest.raises(ValueError):
            await client.receive_batch(noop)
    finally:
        await col.stop()


async def test_async_owner_level_takeover(emu, hub1):
    async with make_async_producer(emu, hub1) as p:
        a = AsyncCollector(make_async_consumer(emu, hub1), partition_id="0", owner_level=1, max_wait_time=1).start()
        b = None
        try:
            await a.wait_live(p, ["0"])
            b = AsyncCollector(make_async_consumer(emu, hub1), partition_id="0", owner_level=2, max_wait_time=1).start()
            await b.wait_live(p, ["0"])
            await until(lambda: a.errors, 8, "an on_error call on the first receiver")
            assert isinstance(a.errors[0][1], EventHubError)
            await until(lambda: a.closed, 8, "on_partition_close on the first receiver")
            assert a.closed[0] == ("0", CloseReason.OWNERSHIP_LOST)
        finally:
            await a.stop()
            if b:
                await b.stop()


async def test_async_custom_consumer_group(emu, new_hub):
    hub = new_hub(1, groups=["analytics"])
    async with make_async_producer(emu, hub) as p:
        sent = await async_send_n(p, 3, partition_id="0")
    assert bodies(await async_read_from_start(emu, hub, 3, partition_id="0", group="analytics")) == sent


# -- checkpoints and load balancing ----------------------------------------------------------------------------------------------------
async def test_async_checkpoint_resume(emu, hub1):
    async with make_async_producer(emu, hub1) as p:
        await async_send_n(p, 5, partition_id="0")
    store = InMemoryCheckpointStore()
    kw = dict(checkpoint_store=store, load_balancing_interval=0.5, partition_ownership_expiration_interval=20)

    class Checkpointing(AsyncCollector):
        async def _record(self, ctx, event):
            await super()._record(ctx, event)
            if event.body_as_str() == "m-1":
                await ctx.update_checkpoint(event)

    col = Checkpointing(make_async_consumer(emu, hub1, **kw), starting_position="-1", max_wait_time=1).start()
    try:
        await col.wait_events(5)
    finally:
        await col.stop()
    col2 = AsyncCollector(make_async_consumer(emu, hub1, **kw), starting_position="-1", max_wait_time=1).start()
    try:
        assert bodies(await col2.wait_events(3)) == ["m-2", "m-3", "m-4"]
    finally:
        await col2.stop()


@pytest.mark.slow
async def test_async_two_consumers_split_two_partitions(emu, hub2):
    store = InMemoryCheckpointStore()
    kw = dict(checkpoint_store=store, load_balancing_interval=0.5, partition_ownership_expiration_interval=20)
    a = AsyncCollector(make_async_consumer(emu, hub2, **kw), starting_position="-1", max_wait_time=1).start()
    b = None
    try:
        await until(lambda: len(a.initialized) >= 2, 8, "the first consumer to own both partitions")
        b = AsyncCollector(make_async_consumer(emu, hub2, **kw), starting_position="-1", max_wait_time=1).start()

        async def split():
            owners = [o["owner_id"] for o in await store.list_ownership(host(emu), hub2, "$Default")]
            return len(owners) == 2 and len(set(owners)) == 2 and all(owners)

        await until(split, 14, "two different owners")
    finally:
        await a.stop()
        if b:
            await b.stop()


# -- buffered producer, credentials, failures ---------------------------------------------------------------------------------------------
async def test_async_buffered_producer_flush(emu, hub):
    sent = []

    async def on_success(events, pid):
        sent.append((len(events), pid))

    async def on_error(events, pid, err):
        sent.append(("error", err))

    async with make_async_producer(emu, hub, buffered_mode=True, on_success=on_success, on_error=on_error, max_wait_time=30) as p:
        for i in range(5):
            await p.send_event(EventData(f"b{i}"), partition_id="2")
        assert p.get_buffered_event_count("2") == 5 and sent == []
        await p.flush()
        assert sent == [(5, "2")] and p.get_buffered_event_count("2") == 0
    assert bodies(await async_read_from_start(emu, hub, 5, partition_id="2")) == [f"b{i}" for i in range(5)]


async def test_async_buffered_mode_needs_callbacks(emu, hub):
    with pytest.raises(TypeError):
        make_async_producer(emu, hub, buffered_mode=True)


async def test_async_producer_with_a_named_key_credential(emu, hub):
    async with EventHubProducerClient(host(emu), hub, AzureNamedKeyCredential(ehutil.KEY_NAME, ehutil.KEY), use_tls=False,
                                      **ehutil.CLIENT_KW) as p:
        assert await p.get_partition_ids() == ALL


async def test_async_connection_refused_is_a_connect_error():
    from test_client_behaviour import KW0, _Dead

    dead = _Dead()
    async with EventHubProducerClient.from_connection_string(ehutil.conn_str(dead), eventhub_name="h", **KW0) as p:
        with pytest.raises(ConnectError):
            await p.get_eventhub_properties()
        with pytest.raises(ConnectError):
            await p.send_event(EventData("x"))


async def test_async_two_consumers_both_get_every_event(emu, hub1):
    async with make_async_producer(emu, hub1) as p:
        sent = await async_send_n(p, 3, partition_id="0")
    a = AsyncCollector(make_async_consumer(emu, hub1), partition_id="0", starting_position="-1", max_wait_time=1).start()
    b = AsyncCollector(make_async_consumer(emu, hub1), partition_id="0", starting_position="-1", max_wait_time=1).start()
    try:
        assert bodies(await a.wait_events(3)) == sent
        assert bodies(await b.wait_events(3)) == sent
    finally:
        await a.stop()
        await b.stop()
