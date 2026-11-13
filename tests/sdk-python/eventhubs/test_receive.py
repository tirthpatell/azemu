"""Receiving with EventHubConsumerClient.receive / receive_batch: one partition, all partitions, live tailing, callbacks.

Sources: SDK `_consumer_client.py`, `_eventprocessor/event_processor.py`, `_consumer.py`; Microsoft's `livetest/synctests/
test_receive.py` and `test_consumer_client.py` (scenarios and expected values), Learn "Event Hubs features" (events are not
removed by reading; consumer groups are independent views).

`receive` never returns by itself, so everything goes through ehutil.Collector (a thread, hard deadline, closed at the end).
"""

import threading
import time

import pytest
from azure.eventhub import CloseReason, EventData

import ehutil
from ehutil import (
    Collector, bodies, host, live_receiver, make_consumer, read_exact, read_from_start, send_each, send_n, texts,
)

ALL = ["0", "1", "2", "3"]


# -- from the beginning ------------------------------------------------------------------------------------------------------
def test_receive_batch_from_start_single_partition(emu, hub, producer):
    sent = send_n(producer, 5, partition_id="0")
    assert bodies(read_from_start(emu, hub, 5, partition_id="0")) == sent


def test_receive_per_event_callback_from_start(emu, hub, producer):
    sent = send_n(producer, 5, partition_id="1")
    with Collector(make_consumer(emu, hub), batch=False, partition_id="1", starting_position="-1", max_wait_time=1) as col:
        assert bodies(col.wait_events(5)) == sent


def test_receive_all_partitions_gets_every_event(emu, hub, producer):
    expected = {}
    for pid in ALL:
        expected[pid] = send_n(producer, 3, prefix=f"p{pid}", partition_id=pid)
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        col.wait_events(12)
        assert {pid: bodies(evs) for pid, evs in col.by_partition().items()} == expected


def test_partition_context_names_the_partition_of_each_event(emu, hub, producer):
    send_n(producer, 1, prefix="two", partition_id="2")
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        col.wait_events(1)
        assert [pid for pid, _ in col.items] == ["2"]


def test_partition_context_fields(emu, hub, producer):
    send_n(producer, 1, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        col.wait_events(1)
        ctx = col.contexts["0"]
    assert ctx.partition_id == "0"
    assert ctx.eventhub_name == hub
    assert ctx.consumer_group == "$Default"
    assert ctx.fully_qualified_namespace == host(emu)


def test_empty_partition_delivers_nothing(emu, hub):
    read_exact(emu, hub, 0, partition_id="0")


def test_events_arrive_in_order_within_a_partition(emu, hub, producer):
    sent = send_n(producer, 100, partition_id="3")
    assert bodies(read_from_start(emu, hub, 100, partition_id="3")) == sent


def test_order_is_kept_per_partition_when_reading_all(emu, hub, producer):
    exp = {}
    for pid in ALL:
        exp[pid] = send_n(producer, 20, prefix=f"o{pid}-", partition_id=pid)
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        col.wait_events(80)
        by = col.by_partition()
    assert {pid: bodies(evs) for pid, evs in by.items()} == exp


def test_batches_and_single_events_both_arrive(emu, hub, producer):
    send_n(producer, 3, prefix="b", partition_id="0")
    send_each(producer, ["s0", "s1"], partition_id="0")
    assert bodies(read_from_start(emu, hub, 5, partition_id="0")) == ["b-0", "b-1", "b-2", "s0", "s1"]


# -- events are not consumed -------------------------------------------------------------------------------------------------
def test_reading_twice_gets_the_events_again(emu, hub, producer):
    sent = send_n(producer, 4, partition_id="0")
    assert bodies(read_from_start(emu, hub, 4, partition_id="0")) == sent
    assert bodies(read_from_start(emu, hub, 4, partition_id="0")) == sent


def test_two_consumers_in_one_group_each_get_everything(emu, hub, producer):
    sent = send_n(producer, 5, partition_id="0")
    a = Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1).start()
    b = Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1).start()
    try:
        assert bodies(a.wait_events(5)) == sent
        assert bodies(b.wait_events(5)) == sent
    finally:
        a.stop()
        b.stop()


def test_events_stay_after_a_receiver_that_saw_them_was_closed(emu, hub, producer):
    send_n(producer, 3, partition_id="1")
    read_from_start(emu, hub, 3, partition_id="1")
    assert not producer.get_partition_properties("1")["is_empty"]


# -- live tailing -----------------------------------------------------------------------------------------------------------------
def test_live_receiver_gets_events_sent_after_it_started(emu, hub1, producer1):
    col = live_receiver(emu, hub1, producer1, ["0"])
    try:
        sent = send_n(producer1, 3, prefix="live", partition_id="0")
        assert bodies(col.wait_events(3)) == sent
    finally:
        col.stop()


def test_live_receiver_keeps_receiving_later_sends(emu, hub1, producer1):
    col = live_receiver(emu, hub1, producer1, ["0"])
    try:
        for i in range(4):
            producer1.send_event(EventData(f"late-{i}"), partition_id="0")
            col.wait_events(i + 1)
            time.sleep(0.1)
        assert bodies(col.events) == [f"late-{i}" for i in range(4)]
    finally:
        col.stop()


def test_live_receiver_on_all_partitions(emu, hub, producer):
    col = live_receiver(emu, hub, producer, ALL)
    try:
        for pid in ALL:
            send_n(producer, 2, prefix=f"l{pid}", partition_id=pid)
        col.wait_events(8)
        assert {pid: bodies(evs) for pid, evs in col.by_partition().items()} == {pid: texts(2, f"l{pid}") for pid in ALL}
    finally:
        col.stop()


def test_events_sent_from_a_background_thread_are_received_in_order(emu, hub1, producer1):
    col = live_receiver(emu, hub1, producer1, ["0"])
    try:
        def work():
            for i in range(25):
                producer1.send_event(EventData(f"bg-{i}"), partition_id="0")

        t = threading.Thread(target=work)
        t.start()
        col.wait_events(25)
        t.join(10)
        assert bodies(col.events) == [f"bg-{i}" for i in range(25)]
    finally:
        col.stop()


def test_live_receiver_default_position_ignores_older_events(emu, hub1, producer1):
    # The SDK default starting position is "@latest" (_consumer_client._receive)
    send_n(producer1, 3, prefix="old", partition_id="0")
    col = live_receiver(emu, hub1, producer1, ["0"])
    try:
        send_n(producer1, 2, prefix="new", partition_id="0")
        col.wait_events(2)
        col.settle()
        assert bodies(col.events) == ["new-0", "new-1"]
    finally:
        col.stop()


# -- max_wait_time, batch sizes, prefetch --------------------------------------------------------------------------------------------
def test_receive_batch_calls_back_with_an_empty_list_when_idle(emu, hub):
    # Microsoft's test_receive_batch_empty_with_max_wait_time: the handler is called with [] after max_wait_time
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        col.wait_until(lambda c: c.empty_callbacks >= 1, 6, "an empty callback")
        assert col.count() == 0


def test_receive_calls_back_with_none_when_idle(emu, hub):
    with Collector(make_consumer(emu, hub), batch=False, partition_id="0", starting_position="-1", max_wait_time=1) as col:
        col.wait_until(lambda c: c.empty_callbacks >= 1, 6, "a None callback")


def test_no_idle_callbacks_without_max_wait_time(emu, hub):
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1") as col:
        col.wait_initialized(1)
        col.settle(1.5)
        assert col.empty_callbacks == 0


@pytest.mark.parametrize("size", [1, 3, 10])
def test_batches_never_exceed_max_batch_size(emu, hub, producer, size):
    send_n(producer, 25, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1, max_batch_size=size) as col:
        col.wait_events(25)
        assert max(col.batch_sizes) <= size
        assert sum(col.batch_sizes) == 25


def test_batch_callback_fires_as_soon_as_max_batch_size_is_reached(emu, hub, producer):
    # Microsoft's test_receive_batch_early_callback: max_batch_size 10, huge max_wait_time, 10 events -> handler gets them
    send_n(producer, 10, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_batch_size=10, max_wait_time=60) as col:
        col.wait_events(10, 6)
        assert col.batch_sizes[0] == 10


@pytest.mark.parametrize("prefetch", [1, 2, 10, 300])
def test_every_event_arrives_whatever_the_prefetch(emu, hub, producer, prefetch):
    sent = send_n(producer, 12, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", max_wait_time=1, prefetch=prefetch) as col:
        assert bodies(col.wait_events(12)) == sent


def test_a_large_event_is_received_with_a_small_prefetch(emu, hub, producer):
    # Microsoft's test_receive_batch_large_event (prefetch 2, 15,700 character body)
    producer.send_event(EventData("A" * 15700), partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", prefetch=2) as col:
        (ev,) = col.wait_events(1)
    assert ev.body_as_str() == "A" * 15700


# -- callbacks ---------------------------------------------------------------------------------------------------------------------
def test_on_partition_initialize_is_called_once_per_partition(emu, hub, producer):
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(4)
        col.settle(0.5)
        assert sorted(col.initialized) == ALL


def test_on_partition_initialize_for_a_single_partition(emu, hub):
    with Collector(make_consumer(emu, hub), partition_id="2", starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(1)
        col.settle(0.5)
        assert col.initialized == ["2"]


def test_on_partition_close_reports_shutdown_when_the_client_closes(emu, hub):
    col = Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1).start()
    col.wait_initialized(4)
    col.settle(0.5)
    col.stop()
    assert sorted(pid for pid, _ in col.closed) == ALL
    assert {reason for _, reason in col.closed} == {CloseReason.SHUTDOWN}


def test_exception_in_the_event_callback_goes_to_on_error(emu, hub, producer):
    send_n(producer, 1, partition_id="0")

    def boom(ctx, event):
        raise RuntimeError("callback failed")

    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", on_event_hook=boom) as col:
        col.wait_until(lambda c: c.errors, 6, "an on_error call")
        pid, err = col.errors[0]
        assert pid == "0" and isinstance(err, RuntimeError) and str(err) == "callback failed"
        col.wait_until(lambda c: c.closed, 6, "on_partition_close")
        assert col.closed[0] == ("0", CloseReason.OWNERSHIP_LOST)


def test_receive_from_an_unknown_partition_reports_value_error_to_on_error(emu, hub):
    # OwnershipManager.claim_ownership raises ValueError("Wrong partition id...") which the load balancer passes to on_error(None, err)
    with Collector(make_consumer(emu, hub), partition_id="99", starting_position="-1") as col:
        col.wait_until(lambda c: c.errors, 6, "an on_error call")
        pid, err = col.errors[0]
        assert pid is None and isinstance(err, ValueError)


def test_receive_twice_on_one_client_is_a_value_error(emu, hub):
    client = make_consumer(emu, hub)
    with Collector(client, starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(1)
        with pytest.raises(ValueError):
            client.receive_batch(lambda ctx, evs: None, partition_id="0")
        with pytest.raises(ValueError):
            client.receive_batch(lambda ctx, evs: None)


def test_receive_same_partition_twice_on_one_client_is_a_value_error(emu, hub):
    client = make_consumer(emu, hub)
    with Collector(client, partition_id="1", starting_position="-1", max_wait_time=1) as col:
        col.wait_initialized(1)
        with pytest.raises(ValueError):
            client.receive_batch(lambda ctx, evs: None, partition_id="1")
        with pytest.raises(ValueError):
            client.receive_batch(lambda ctx, evs: None)  # all partitions while one is active


def test_two_partitions_can_be_received_on_one_client_by_two_calls(emu, hub, producer):
    send_n(producer, 2, prefix="a", partition_id="0")
    send_n(producer, 2, prefix="b", partition_id="1")
    client = make_consumer(emu, hub)
    got = []
    t = threading.Thread(
        target=client.receive_batch, args=(lambda ctx, evs: got.extend((ctx.partition_id, e.body_as_str()) for e in evs),),
        kwargs=dict(partition_id="1", starting_position="-1", max_wait_time=1), daemon=True,
    )
    with Collector(client, partition_id="0", starting_position="-1", max_wait_time=1) as col:
        t.start()
        col.wait_events(2)
        ehutil.wait_until(lambda: len(got) >= 2, 6, what="partition 1 events")
    t.join(5)
    assert sorted(got) == [("1", "b-0"), ("1", "b-1")]
    assert bodies(col.events) == ["a-0", "a-1"]


# -- last enqueued event properties ----------------------------------------------------------------------------------------------
def test_last_enqueued_event_properties_have_four_fields(emu, hub, producer):
    send_n(producer, 3, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", track_last_enqueued_event_properties=True) as col:
        col.wait_events(3)
        props = col.contexts["0"].last_enqueued_event_properties
    assert set(props) == {"sequence_number", "offset", "enqueued_time", "retrieval_time"}


def test_last_enqueued_event_properties_match_the_last_event(emu, hub, producer):
    # Microsoft's test_receive_with_event_position_sync asserts these equalities inside the callback
    send_n(producer, 5, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", track_last_enqueued_event_properties=True) as col:
        events = col.wait_events(5)
        props = col.contexts["0"].last_enqueued_event_properties
    assert props["sequence_number"] == events[-1].sequence_number
    assert props["offset"] == events[-1].offset
    assert props["enqueued_time"] == events[-1].enqueued_time
    assert props["retrieval_time"] is not None


def test_retrieval_time_is_recent_and_after_the_last_enqueue(emu, hub, producer):
    from datetime import datetime, timedelta, timezone

    send_n(producer, 2, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1", track_last_enqueued_event_properties=True) as col:
        col.wait_events(2)
        props = col.contexts["0"].last_enqueued_event_properties
    assert props["retrieval_time"] >= props["enqueued_time"]
    assert abs(datetime.now(timezone.utc) - props["retrieval_time"]) < timedelta(minutes=1)


def test_last_enqueued_event_properties_follow_new_events_in_per_event_mode(emu, hub, producer):
    col = Collector(make_consumer(emu, hub), batch=False, partition_id="0", starting_position="-1", track_last_enqueued_event_properties=True).start()
    try:
        send_n(producer, 2, partition_id="0")
        col.wait_events(2)
        first = col.contexts["0"].last_enqueued_event_properties["sequence_number"]
        send_n(producer, 3, prefix="more", partition_id="0")
        col.wait_events(5)
        col.wait_until(lambda c: c.contexts["0"].last_enqueued_event_properties["sequence_number"] == first + 3, 5, "last enqueued to advance")
    finally:
        col.stop()


def test_last_enqueued_event_properties_are_none_when_not_tracked(emu, hub, producer):
    send_n(producer, 2, partition_id="0")
    with Collector(make_consumer(emu, hub), partition_id="0", starting_position="-1") as col:
        col.wait_events(2)
        assert col.contexts["0"].last_enqueued_event_properties is None


def test_last_enqueued_properties_are_per_partition(emu, hub, producer):
    send_n(producer, 2, partition_id="0")
    send_n(producer, 4, partition_id="1")
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1, track_last_enqueued_event_properties=True) as col:
        col.wait_events(6)
        by = col.by_partition()
        p0 = col.contexts["0"].last_enqueued_event_properties
        p1 = col.contexts["1"].last_enqueued_event_properties
    assert p0["sequence_number"] == by["0"][-1].sequence_number
    assert p1["sequence_number"] == by["1"][-1].sequence_number
