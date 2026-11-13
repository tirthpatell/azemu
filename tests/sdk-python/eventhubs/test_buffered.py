"""EventHubProducerClient in buffered mode (buffered_mode=True, on_success / on_error, max_wait_time, max_buffer_length, flush).

Sources: SDK `_producer_client.py` and `_buffered_producer/` (the client picks the partition itself - round robin from partition "0",
or a client-side hash of the partition key - and sends to a partition-specific link), Microsoft's `livetest/synctests/
test_buffered_producer.py` (constructor errors, round robin, flush/close, max_wait_time, the max_buffer_length scenario 7 + 9 + 5
events with a buffer of 10, and the `timeout=-1` case in test_negative.py). Everything about buffering itself happens in the SDK;
the service only sees ordinary sends to a partition.
"""

import time
from collections import defaultdict

import pytest
from azure.eventhub import EventData
from azure.eventhub.exceptions import ConnectError, OperationTimeoutError
from azure.eventhub.amqp import AmqpAnnotatedMessage

import ehutil
from ehutil import Collector, bodies, make_consumer, make_producer, read_from_start, texts, wait_until


class Sink:
    """Collects the buffered producer's callbacks."""

    def __init__(self):
        self.sent = defaultdict(list)  # partition id -> events
        self.errors = []
        self.calls = []

    def on_success(self, events, pid):
        self.calls.append((len(events), pid))
        self.sent[pid].extend(events)

    def on_error(self, events, pid, err):
        self.errors.append((events, pid, err))

    @property
    def total(self):
        return sum(len(v) for v in self.sent.values())

    def kwargs(self, **extra):
        return dict(buffered_mode=True, on_success=self.on_success, on_error=self.on_error, **extra)


# -- constructor (client side only) --------------------------------------------------------------------------------------------------
def test_buffered_mode_needs_both_callbacks(emu, hub):
    with pytest.raises(TypeError):
        make_producer(emu, hub, buffered_mode=True)
    with pytest.raises(TypeError):
        make_producer(emu, hub, buffered_mode=True, on_success=lambda e, p: None)
    with pytest.raises(TypeError):
        make_producer(emu, hub, buffered_mode=True, on_error=lambda e, p, x: None)


@pytest.mark.parametrize("kw", [{"max_wait_time": 0}, {"max_buffer_length": 0}, {"buffer_concurrency": 0}])
def test_buffered_mode_rejects_non_positive_limits(emu, hub, kw):
    s = Sink()
    with pytest.raises(ValueError):
        make_producer(emu, hub, **s.kwargs(**kw))


def test_buffer_counts_are_none_when_not_buffered(emu, hub, producer):
    assert producer.get_buffered_event_count("0") is None
    assert producer.total_buffered_event_count is None


# -- flushing -------------------------------------------------------------------------------------------------------------------------
def test_flush_sends_everything_buffered_and_calls_on_success(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(6):
        p.send_event(EventData(f"f{i}"), partition_id="1")
    assert s.total == 0  # still in the buffer
    assert p.get_buffered_event_count("1") == 6 and p.total_buffered_event_count == 6
    p.flush()
    assert s.total == 6 and set(s.sent) == {"1"} and s.errors == []
    assert p.get_buffered_event_count("1") == 0 and p.total_buffered_event_count == 0
    assert bodies(read_from_start(emu, hub, 6, partition_id="1")) == [f"f{i}" for i in range(6)]


def test_close_flushes_by_default(emu, hub):
    s = Sink()
    p = make_producer(emu, hub, **s.kwargs(max_wait_time=3))
    p.send_event(EventData("closing"), partition_id="0")
    p.close()
    assert s.total == 1 and s.errors == []
    assert bodies(read_from_start(emu, hub, 1, partition_id="0")) == ["closing"]


def test_context_manager_flushes_on_exit(emu, hub):
    s = Sink()
    with make_producer(emu, hub, **s.kwargs(max_wait_time=3)) as p:
        p.send_batch([EventData("a"), EventData("b")], partition_id="2")
    assert s.total == 2
    assert bodies(read_from_start(emu, hub, 2, partition_id="2")) == ["a", "b"]


def test_flush_with_nothing_buffered_is_a_no_op(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs()))
    p.flush()
    p.send_event(EventData("x"), partition_id="0")
    p.flush()
    p.flush()
    assert s.total == 1


def test_events_batched_together_are_reported_in_one_on_success_call(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(10):
        p.send_event(EventData(f"b{i}"), partition_id="0")
    p.flush()
    assert s.calls == [(10, "0")]


# -- timing and sizes ---------------------------------------------------------------------------------------------------------------------
def test_max_wait_time_sends_without_a_flush(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=1)))
    p.send_event(EventData("timed"), partition_id="0")
    assert s.total == 0
    wait_until(lambda: s.total == 1, 6, 0.1, "the background send after max_wait_time")
    assert bodies(read_from_start(emu, hub, 1, partition_id="0")) == ["timed"]


def test_max_buffer_length_forces_a_flush(emu, hub, track):
    # Microsoft's test_send_with_timing_configuration: buffer of 10 per partition
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=1000, max_buffer_length=10)))
    for _ in range(7):
        p.send_event(EventData("data"), partition_id="0")
    assert s.total == 0
    batch = p.create_batch(partition_id="0")
    for _ in range(9):
        batch.add(EventData("9"))
    p.send_batch(batch)  # does not fit next to the 7: flushes them, the batch waits in the buffer
    assert s.total == 7
    for _ in range(5):
        p.send_event(EventData("data"), partition_id="0")  # flushes the batch (9) + 1, leaving 4
    assert s.total == 17
    p.flush()
    assert s.total == 21
    assert len(read_from_start(emu, hub, 21, partition_id="0")) == 21


# -- partition choice -----------------------------------------------------------------------------------------------------------------------
def test_partition_id_is_passed_through_and_reported(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for pid in ["0", "1", "2", "3"]:
        p.send_event(EventData(f"to-{pid}"), partition_id=pid)
    p.flush()
    assert {pid: bodies(evs) for pid, evs in s.sent.items()} == {pid: [f"to-{pid}"] for pid in "0123"}


def test_single_events_without_a_key_go_round_robin_from_partition_zero(emu, hub, track):
    # Microsoft's test_basic_send_single_events_round_robin: partitions[event_idx % partition_count] == pid
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(8):
        e = EventData(f"rr{i}")
        e.properties = {"idx": i}
        p.send_event(e)
    p.flush()
    for pid, events in s.sent.items():
        assert [e.properties["idx"] % 4 for e in events] == [int(pid)] * 2
    got = {}
    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        col.wait_events(8)
        got = {pid: bodies(evs) for pid, evs in col.by_partition().items()}
    assert got == {"0": ["rr0", "rr4"], "1": ["rr1", "rr5"], "2": ["rr2", "rr6"], "3": ["rr3", "rr7"]}


def test_same_partition_key_always_the_same_partition(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(5):
        p.send_event(EventData(f"k{i}"), partition_key="device-1")
    p.send_batch([EventData("kb")], partition_key="device-1")
    p.flush()
    assert len(s.sent) == 1 and s.total == 6
    pid = next(iter(s.sent))
    assert bodies(read_from_start(emu, hub, 6, partition_id=pid)) == [f"k{i}" for i in range(5)] + ["kb"]


def test_different_keys_can_use_different_partitions(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(40):
        p.send_event(EventData(f"v{i}"), partition_key=f"key-{i}")
    p.flush()
    assert s.total == 40 and len(s.sent) >= 2


def test_buffered_batches_of_amqp_annotated_messages(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    p.send_event(AmqpAnnotatedMessage(data_body="amqp message"), partition_id="0")
    p.flush()
    (ev,) = read_from_start(emu, hub, 1, partition_id="0")
    assert ev.body_as_str() == "amqp message"


def test_unknown_partition_is_a_connect_error_even_in_buffered_mode(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs()))
    with pytest.raises(ConnectError):
        p.send_event(EventData("x"), partition_id="99")


def test_negative_timeout_is_an_operation_timeout_in_buffered_mode(emu, hub, track):
    # Microsoft's test_client_send_timeout, buffered half
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs()))
    with pytest.raises(OperationTimeoutError):
        p.send_batch([EventData(b"Data")], timeout=-1)
    with pytest.raises(OperationTimeoutError):
        p.send_event(EventData(b"Data"), timeout=-1)


def test_on_error_is_not_called_for_successful_sends(emu, hub, track):
    s = Sink()
    p = track(make_producer(emu, hub, **s.kwargs(max_wait_time=3)))
    for i in range(20):
        p.send_event(EventData(f"ok{i}"), partition_id=str(i % 4))
    p.flush()
    assert s.errors == [] and s.total == 20
