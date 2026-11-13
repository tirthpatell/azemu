"""Event Hubs and the Kafka endpoint across restarts: events with their sequence numbers and offsets, a consumer that
resumes from its checkpoint, a Kafka consumer group that resumes from its committed offset, an idempotent Kafka producer."""

import pathlib
import sys

import pytest

here = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(here / "eventhubs"))
sys.path.insert(0, str(here / "kafka"))
import ehutil  # noqa: E402
import kutil  # noqa: E402
from ehutil import Collector, bodies, make_consumer, make_producer, send_n  # noqa: E402

from azure.eventhub import EventData  # noqa: E402
from azure.eventhub._eventprocessor.in_memory_checkpoint_store import InMemoryCheckpointStore  # noqa: E402

FAST = dict(load_balancing_interval=0.5, partition_ownership_expiration_interval=20)


def numbers(events):
    return {e.body_as_str(): (e.sequence_number, e.offset, e.enqueued_time) for e in events}


def props_of(event):
    return {(k.decode() if isinstance(k, bytes) else k): v for k, v in (event.properties or {}).items()}


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_events_and_a_checkpointed_consumer_resume_after_a_restart(persistent, how):
    p = persistent()
    ehutil.admin_create_hub(p.emu, "hub1", partitions=3, groups=["cg1"])
    with make_producer(p.emu, "hub1") as producer:
        send_n(producer, 6, "m", partition_id="0")
        batch = producer.create_batch(partition_id="1")
        e = EventData("keyed")
        e.properties = {"color": "blue", "n": 3}
        batch.add(e)
        producer.send_batch(batch)
    store = InMemoryCheckpointStore()

    def checkpoint_third(ctx, event):
        if event.body_as_str() == "m-2":
            ctx.update_checkpoint(event)

    with Collector(make_consumer(p.emu, "hub1", checkpoint_store=store, **FAST), starting_position="-1", max_wait_time=1, on_event_hook=checkpoint_third) as col:
        col.wait_events(7)
    before = numbers(col.events)
    keyed_props_before = props_of(next(e for e in col.events if e.body_as_str() == "keyed"))
    assert keyed_props_before["n"] == 3
    with make_consumer(p.emu, "hub1") as c:
        props_before = c.get_partition_properties("0")

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    # nothing about the events changed: bodies, properties, sequence numbers, offsets, enqueue times
    with Collector(make_consumer(p.emu, "hub1", "cg1"), starting_position="-1", max_wait_time=1) as col2:
        events = col2.wait_events(7)
    assert numbers(events) == before
    keyed = next(e for e in events if e.body_as_str() == "keyed")
    assert props_of(keyed) == keyed_props_before
    with make_consumer(p.emu, "hub1") as c:
        props_after = c.get_partition_properties("0")
    for k in ("beginning_sequence_number", "last_enqueued_sequence_number", "last_enqueued_offset", "last_enqueued_time_utc"):
        assert props_after[k] == props_before[k], k
    # new events go on from the old numbers; a consumer with the checkpoint from before the restart resumes after it
    with make_producer(p.emu, "hub1") as producer:
        send_n(producer, 3, "later", partition_id="0")
    with Collector(make_consumer(p.emu, "hub1", checkpoint_store=store, **FAST), starting_position="-1", max_wait_time=1) as col3:
        got = col3.wait_events(7)
    part0 = [e for e in got if e.body_as_str() != "keyed"]
    assert bodies(part0) == ["m-3", "m-4", "m-5", "later-0", "later-1", "later-2"]
    assert [e.sequence_number for e in part0] == [3, 4, 5, 6, 7, 8]
    assert [e.body_as_str() for e in got if e.body_as_str() == "keyed"] == ["keyed"], "the partition without a checkpoint starts from the beginning"


def test_a_kafka_consumer_group_resumes_from_its_committed_offset(persistent):
    from confluent_kafka import TopicPartition

    p = persistent()
    kutil.admin_create(p.emu, "t1", partitions=1)
    prod = kutil.producer(kutil.plain(p.emu), **{"enable.idempotence": True})
    kutil.produce_all(prod, "t1", [f"v{i}".encode() for i in range(6)])
    cons = kutil.consumer(kutil.plain(p.emu), "g1")
    cons.subscribe(["t1"])
    got = kutil.consume(cons, 3)
    assert [m.value() for m in got[:3]] == [b"v0", b"v1", b"v2"]
    cons.commit(offsets=[TopicPartition("t1", 0, 3)], asynchronous=False)
    cons.close()
    assert p.stop() == 0
    p.start()
    cons = kutil.consumer(kutil.plain(p.emu), "g1")
    cons.subscribe(["t1"])
    got = kutil.consume(cons, 3)
    assert [m.value() for m in got[:3]] == [b"v3", b"v4", b"v5"], "the group resumes at the offset it committed"
    assert [m.offset() for m in got[:3]] == [3, 4, 5]
    cons.close()
    # a producer with idempotence that connects after the restart is accepted, and its records follow the old ones
    prod = kutil.producer(kutil.plain(p.emu), **{"enable.idempotence": True})
    reports = kutil.produce_all(prod, "t1", [b"after-restart"])
    assert reports[0].offset() == 6


def test_events_written_over_amqp_and_kafka_keep_their_numbers(persistent):
    p = persistent()
    kutil.admin_create(p.emu, "mix", partitions=1)
    with make_producer(p.emu, "mix", namespace="default") as producer:
        send_n(producer, 2, "amqp", partition_id="0")
    kutil.produce_all(kutil.producer(kutil.plain(p.emu)), "mix", [b"kafka-0", b"kafka-1"])
    p.save()
    p.stop(kill=True)
    p.start()
    cons = kutil.consumer(kutil.plain(p.emu), "readers")
    cons.subscribe(["mix"])
    got = kutil.consume(cons, 4)
    assert [m.offset() for m in got[:4]] == [0, 1, 2, 3]
    assert [m.value() for m in got[2:4]] == [b"kafka-0", b"kafka-1"]
    cons.close()
