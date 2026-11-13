"""Checkpointing and load balancing with the SDK's in-memory checkpoint store.

Sources: SDK `_eventprocessor/` (`PartitionContext.update_checkpoint` stores `{offset, sequence_number}`; a partition with a
checkpoint starts at that offset, exclusive; `OwnershipManager` splits partitions, claims with etags, releases ownership on
shutdown, treats an ownership older than `partition_ownership_expiration_interval` as free; with a store the consumers use
owner_level 0), Microsoft's `test_receive_load_balancing`, `test_receive_no_partition` and `test_receive_batch_no_max_wait_time`
(checkpoint contents), Learn features ("Checkpointing ... a new consumer resumes from the last checkpoint").
"""

import time

import pytest
from azure.eventhub import EventData
from azure.eventhub._eventprocessor.in_memory_checkpoint_store import InMemoryCheckpointStore

import ehutil
from ehutil import Collector, bodies, host, make_consumer, make_producer, read_exact, send_n, texts, wait_until

FAST = dict(load_balancing_interval=0.5, partition_ownership_expiration_interval=20)


def consumer(emu, hub, store, group="$Default", **kw):
    return make_consumer(emu, hub, group, checkpoint_store=store, **{**FAST, **kw})


def checkpoints(store, emu, hub, group="$Default"):
    return {c["partition_id"]: c for c in store.list_checkpoints(host(emu), hub, group)}


def ownerships(store, emu, hub, group="$Default"):
    return {o["partition_id"]: o for o in store.list_ownership(host(emu), hub, group)}


# -- checkpoints -----------------------------------------------------------------------------------------------------------
def test_update_checkpoint_then_a_new_consumer_resumes_after_it(emu, hub1, producer1):
    send_n(producer1, 6, partition_id="0")
    store = InMemoryCheckpointStore()

    def checkpoint_third(ctx, event):
        if event.body_as_str() == "m-2":
            ctx.update_checkpoint(event)

    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1, on_event_hook=checkpoint_third) as col:
        col.wait_events(6)
    third = next(e for e in col.events if e.body_as_str() == "m-2")
    cp = checkpoints(store, emu, hub1)["0"]
    assert cp["offset"] == third.offset and cp["sequence_number"] == third.sequence_number

    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1) as col2:
        assert bodies(col2.wait_events(3)) == ["m-3", "m-4", "m-5"]
        col2.settle()
        assert col2.count() == 3


def test_checkpoint_beats_the_starting_position(emu, hub1, producer1):
    # `starting_position` is only used "if there is no checkpoint data for a partition" (SDK docstring)
    send_n(producer1, 4, partition_id="0")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e) if e.body_as_str() == "m-1" else None) as col:
        col.wait_events(4)
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1) as col2:
        assert bodies(col2.wait_events(2)) == ["m-2", "m-3"]


def test_checkpointing_every_event_leaves_nothing_to_replay(emu, hub1, producer1):
    send_n(producer1, 5, partition_id="0")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e)) as col:
        col.wait_events(5)
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1) as col2:
        col2.wait_initialized(1)
        col2.settle(1.5)
        assert col2.count() == 0


def test_resumed_consumer_gets_events_sent_while_it_was_away(emu, hub1, producer1):
    send_n(producer1, 3, partition_id="0")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e)) as col:
        col.wait_events(3)
    send_n(producer1, 2, prefix="away", partition_id="0")
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1) as col2:
        assert bodies(col2.wait_events(2)) == ["away-0", "away-1"]


def test_update_checkpoint_without_an_event_uses_the_last_received_one(emu, hub1, producer1):
    # Microsoft's test_receive_batch_no_max_wait_time calls partition_context.update_checkpoint() with no argument
    send_n(producer1, 3, partition_id="0")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: None) as col:
        events = col.wait_events(3)
        col.contexts["0"].update_checkpoint()
    cp = checkpoints(store, emu, hub1)["0"]
    assert cp["offset"] == events[-1].offset and cp["sequence_number"] == events[-1].sequence_number


def test_checkpoint_holds_namespace_hub_group_and_partition(emu, hub1, producer1):
    send_n(producer1, 1, partition_id="0")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e)) as col:
        col.wait_events(1)
    cp = checkpoints(store, emu, hub1)["0"]
    assert (cp["fully_qualified_namespace"], cp["eventhub_name"], cp["consumer_group"], cp["partition_id"]) == (
        host(emu), hub1, "$Default", "0")


def test_checkpoints_are_per_partition(emu, hub2, producer2):
    send_n(producer2, 3, prefix="a", partition_id="0")
    send_n(producer2, 3, prefix="b", partition_id="1")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e) if ctx.partition_id == "0" else None) as col:
        col.wait_events(6)
    assert set(checkpoints(store, emu, hub2)) == {"0"}
    with Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1) as col2:
        col2.wait_events(3)
        col2.settle()
        assert {pid: bodies(evs) for pid, evs in col2.by_partition().items()} == {"1": ["b-0", "b-1", "b-2"]}


def test_update_checkpoint_without_a_store_is_harmless(emu, hub1, producer1):
    send_n(producer1, 2, partition_id="0")
    errors = []

    def hook(ctx, e):
        try:
            ctx.update_checkpoint(e)  # logs a warning, no store
        except Exception as exc:  # noqa: BLE001
            errors.append(exc)

    with Collector(make_consumer(emu, hub1), partition_id="0", starting_position="-1", max_wait_time=1, on_event_hook=hook) as col:
        col.wait_events(2)
    assert errors == []


def test_single_partition_receive_with_a_store_still_checkpoints(emu, hub2, producer2):
    send_n(producer2, 3, partition_id="1")
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub2, store), partition_id="1", starting_position="-1", max_wait_time=1,
                   on_event_hook=lambda ctx, e: ctx.update_checkpoint(e)) as col:
        col.wait_events(3)
    assert set(checkpoints(store, emu, hub2)) == {"1"}


# -- ownership and load balancing ------------------------------------------------------------------------------------------------
def test_a_lone_consumer_owns_every_partition(emu, hub, producer):
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub, store), starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(4)
        owners = wait_until(lambda: len(ownerships(store, emu, hub)) == 4 and ownerships(store, emu, hub), 8, what="4 ownerships")
        ids = {o["owner_id"] for o in owners.values()}
    assert len(ids) == 1 and "" not in ids  # one owner for all four partitions


def test_ownership_record_fields(emu, hub1):
    store = InMemoryCheckpointStore()
    with Collector(consumer(emu, hub1, store), starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(1)
        o = dict(wait_until(lambda: ownerships(store, emu, hub1).get("0"), 8, what="ownership of partition 0"))
    assert (o["fully_qualified_namespace"], o["eventhub_name"], o["consumer_group"]) == (host(emu), hub1, "$Default")
    assert o["etag"] and o["owner_id"] and o["last_modified_time"] > time.time() - 60


def test_ownership_is_released_when_the_consumer_closes(emu, hub2):
    store = InMemoryCheckpointStore()
    col = Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1).start()
    col.wait_initialized(2)
    wait_until(lambda: len(ownerships(store, emu, hub2)) == 2, 8, what="2 ownerships")
    col.stop()
    assert {o["owner_id"] for o in ownerships(store, emu, hub2).values()} == {""}


@pytest.mark.slow
def test_two_consumers_with_a_shared_store_split_the_partitions(emu, hub2, producer2):
    # Microsoft's test_receive_load_balancing (2 partitions, 1 s interval): afterwards each client has exactly one consumer
    store = InMemoryCheckpointStore()
    a = Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1).start()
    b = None
    try:
        a.wait_initialized(2)
        b = Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1).start()

        def split():
            owners = [o["owner_id"] for o in ownerships(store, emu, hub2).values()]
            return len(owners) == 2 and len(set(owners)) == 2 and "" not in owners

        wait_until(split, 12, 0.2, "the two partitions to be owned by two different consumers")
        # every partition has exactly one owner record by construction; check that events reach exactly one of the two
        time.sleep(1.5)
        sent = {pid: send_n(producer2, 2, prefix=f"e{pid}", partition_id=pid) for pid in ("0", "1")}
        wait_until(lambda: a.count() + b.count() >= 4, 8, 0.1, "4 events")
        time.sleep(0.5)
        got = bodies(a.events) + bodies(b.events)
        assert sorted(got) == sorted(sent["0"] + sent["1"])
        assert not set(a.by_partition()) & set(b.by_partition())
    finally:
        a.stop()
        if b:
            b.stop()


@pytest.mark.slow
def test_four_partitions_end_up_two_and_two(emu, hub, producer):
    store = InMemoryCheckpointStore()
    a = Collector(consumer(emu, hub, store), starting_position="-1", max_wait_time=1).start()
    b = None
    try:
        a.wait_initialized(4)
        b = Collector(consumer(emu, hub, store), starting_position="-1", max_wait_time=1).start()

        def two_two():
            owners = [o["owner_id"] for o in ownerships(store, emu, hub).values()]
            return len(owners) == 4 and sorted(owners.count(x) for x in set(owners)) == [2, 2]

        wait_until(two_two, 14, 0.2, "ownership to settle at 2 + 2")
    finally:
        a.stop()
        if b:
            b.stop()


@pytest.mark.slow
def test_a_second_consumer_takes_over_when_the_first_closes(emu, hub2):
    store = InMemoryCheckpointStore()
    a = Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1).start()
    b = Collector(consumer(emu, hub2, store), starting_position="-1", max_wait_time=1).start()
    try:
        a.wait_initialized(1)
        b.wait_initialized(1)
        wait_until(lambda: len({o["owner_id"] for o in ownerships(store, emu, hub2).values()}) == 2, 12, 0.2, "a split")
        a.stop()
        def survivor_owns_both():
            owners = {o["owner_id"] for o in ownerships(store, emu, hub2).values()}
            return len(owners) == 1 and "" not in owners

        wait_until(survivor_owns_both, 12, 0.2, "the survivor to own both partitions")
    finally:
        a.stop()
        b.stop()


@pytest.mark.slow
def test_an_expired_ownership_can_be_claimed_by_another_consumer(emu, hub2):
    # The first consumer re-evaluates only every 30 s, so its claim goes stale after 3 s; the second one (0.5 s interval) takes over.
    store = InMemoryCheckpointStore()
    a = Collector(make_consumer(emu, hub2, checkpoint_store=store, load_balancing_interval=30, partition_ownership_expiration_interval=3),
                  starting_position="-1", max_wait_time=1).start()
    b = None
    try:
        a.wait_initialized(2)
        first_owner = {o["owner_id"] for o in ownerships(store, emu, hub2).values()}
        assert len(first_owner) == 1
        b = Collector(consumer(emu, hub2, store, partition_ownership_expiration_interval=3), starting_position="-1", max_wait_time=1).start()
        def taken_over():
            owners = {o["owner_id"] for o in ownerships(store, emu, hub2).values()}
            return len(owners) == 1 and owners.isdisjoint(first_owner)

        wait_until(taken_over, 15, 0.3, "every partition to belong to the second consumer")
    finally:
        a.stop()
        if b:
            b.stop()
