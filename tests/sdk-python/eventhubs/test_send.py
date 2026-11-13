"""Sending: batches, single events, lists, partition ids and keys, size limits, callbacks, error types.

Sources: SDK `_producer_client.py` / `_producer.py` / `_common.py` (EventDataBatch), Microsoft's `livetest/synctests/test_send.py`
and `test_negative.py` (the scenarios and expected exception types below are theirs), Microsoft Learn "Event Hubs features"
(partition keys: related events stay together, in order; no key -> round robin) and "quotas" (1 MB per publication on Standard).
"""

import threading

import pytest
from azure.eventhub import EventData, EventDataBatch
from azure.eventhub.exceptions import ConnectError, EventHubError, OperationTimeoutError

import ehutil
from ehutil import (
    Collector, bodies, make_consumer, make_producer, read_exact, read_from_start, send_each, send_n, texts,
)

ALL = ["0", "1", "2", "3"]


def read_all_partitions(emu, hub, expected, **kw):
    """Read the whole hub from the start; returns {partition id: [events]} once `expected` events arrived."""
    client = make_consumer(emu, hub)
    with Collector(client, starting_position="-1", max_wait_time=1, **kw) as col:
        col.wait_events(expected)
        col.settle(0.4)
        return col.by_partition()


# -- batches, single events, lists ----------------------------------------------------------------------------------------
def test_send_batch_created_with_create_batch(emu, hub, producer):
    sent = send_n(producer, 5, partition_id="0")
    assert bodies(read_from_start(emu, hub, 5, partition_id="0")) == sent


def test_send_event_single(emu, hub, producer):
    producer.send_event(EventData("one"), partition_id="0")
    assert bodies(read_from_start(emu, hub, 1, partition_id="0")) == ["one"]


def test_send_batch_with_a_list_of_events(emu, hub, producer):
    items = texts(4, "l")
    producer.send_batch([EventData(t) for t in items], partition_id="1")
    assert bodies(read_from_start(emu, hub, 4, partition_id="1")) == items


def test_send_batch_with_a_one_item_list(emu, hub, producer):
    # Microsoft's test_send_list_partition
    producer.send_batch([EventData("A1")], partition_id="0")
    assert bodies(read_from_start(emu, hub, 1, partition_id="0")) == ["A1"]


def test_events_of_one_batch_arrive_in_order(emu, hub, producer):
    sent = send_n(producer, 50, partition_id="2")
    assert bodies(read_from_start(emu, hub, 50, partition_id="2")) == sent


def test_single_events_keep_their_send_order_in_a_partition(emu, hub, producer):
    items = texts(15, "s")
    send_each(producer, items, partition_id="3")
    assert bodies(read_from_start(emu, hub, 15, partition_id="3")) == items


def test_batches_and_single_events_interleave_in_send_order(emu, hub, producer):
    producer.send_event(EventData("a"), partition_id="0")
    producer.send_batch([EventData("b"), EventData("c")], partition_id="0")
    producer.send_event(EventData("d"), partition_id="0")
    assert bodies(read_from_start(emu, hub, 4, partition_id="0")) == ["a", "b", "c", "d"]


def test_many_batches_to_one_partition(emu, hub, producer):
    expected = []
    for i in range(20):
        expected += send_n(producer, 5, prefix=f"b{i}", partition_id="1")
    assert bodies(read_from_start(emu, hub, 100, partition_id="1")) == expected


def test_one_producer_is_reused_for_many_sends(emu, hub, producer):
    for i in range(30):
        producer.send_event(EventData(f"e{i}"), partition_id="0")
    assert bodies(read_from_start(emu, hub, 30, partition_id="0")) == [f"e{i}" for i in range(30)]


def test_send_from_two_threads_with_one_producer(emu, hub, producer):
    def work(tag):
        for i in range(10):
            producer.send_event(EventData(f"{tag}{i}"), partition_id="0")

    threads = [threading.Thread(target=work, args=(t,)) for t in "ab"]
    [t.start() for t in threads]
    [t.join(30) for t in threads]
    got = bodies(read_from_start(emu, hub, 20, partition_id="0"))
    assert sorted(got) == sorted([f"a{i}" for i in range(10)] + [f"b{i}" for i in range(10)])
    assert [g for g in got if g[0] == "a"] == [f"a{i}" for i in range(10)]  # per-thread order is kept


def test_empty_batch_sends_nothing(emu, hub, producer):
    # Microsoft's test_send_with_partition_key does `client.send_batch(client.create_batch())`
    producer.send_batch(producer.create_batch())
    producer.send_batch([])
    read_exact(emu, hub, 0, partition_id="0")
    assert all(producer.get_partition_properties(p)["is_empty"] for p in ALL)


def test_two_producers_write_to_the_same_partition_in_the_order_they_finish(emu, hub, track):
    a, b = track(make_producer(emu, hub)), track(make_producer(emu, hub))
    a.send_event(EventData("from-a"), partition_id="0")
    b.send_event(EventData("from-b"), partition_id="0")
    a.send_event(EventData("from-a-2"), partition_id="0")
    assert bodies(read_from_start(emu, hub, 3, partition_id="0")) == ["from-a", "from-b", "from-a-2"]


# -- partition id ----------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("pid", ALL)
def test_partition_id_routes_events_to_that_partition_only(emu, hub, producer, pid):
    send_n(producer, 3, prefix=f"p{pid}", partition_id=pid)
    by = read_all_partitions(emu, hub, 3)
    assert set(by) == {pid}
    assert bodies(by[pid]) == texts(3, f"p{pid}")


def test_each_partition_gets_its_own_events(emu, hub, producer):
    for pid in ALL:
        send_n(producer, 2, prefix=f"q{pid}", partition_id=pid)
    by = read_all_partitions(emu, hub, 8)
    assert {pid: bodies(evs) for pid, evs in by.items()} == {pid: texts(2, f"q{pid}") for pid in ALL}


@pytest.mark.parametrize("bad", ["XYZ", "-1", "1000", "-", "4"])
def test_sending_to_an_unknown_partition_is_connect_error(emu, hub, track, bad):
    # Microsoft's test_send_batch_to_invalid_partitions: ConnectError (raised by the SDK before any send)
    p = track(make_producer(emu, hub))
    with pytest.raises(ConnectError):
        batch = p.create_batch(partition_id=bad)
        batch.add(EventData("test data"))
        p.send_batch(batch)


def test_send_event_to_an_unknown_partition_is_connect_error(emu, hub, producer):
    with pytest.raises(ConnectError):
        producer.send_event(EventData("x"), partition_id="99")


def test_partition_id_wins_over_partition_key(emu, hub, producer):
    # SDK docstring: "If both partition_id and partition_key are provided, the partition_id will take precedence."
    batch = producer.create_batch(partition_id="1", partition_key="some-key")
    batch.add(EventData("both"))
    producer.send_batch(batch)
    assert set(read_all_partitions(emu, hub, 1)) == {"1"}


# -- partition key -----------------------------------------------------------------------------------------------------
def test_same_partition_key_lands_in_one_partition_for_many_keys(emu, hub, producer):
    keys = [f"key-{c}" for c in "abcdef"]
    total = 0
    for k in keys:
        for i in range(5):
            batch = producer.create_batch(partition_key=k)
            batch.add(EventData(f"{k}:{i}"))
            producer.send_batch(batch)
            total += 1
    by = read_all_partitions(emu, hub, total)
    home = {}
    for pid, events in by.items():
        for e in events:
            key = e.body_as_str().split(":")[0]
            assert home.setdefault(key, pid) == pid, f"key {key} appeared in partitions {home[key]} and {pid}"
    assert sorted(home) == sorted(keys)


def test_partition_key_comes_back_on_the_received_event(emu, hub, producer):
    # Microsoft's test_receive_end_of_stream: `event.partition_key == b"0"` for send(..., partition_key="0")
    producer.send_event(EventData("k"), partition_key="the-key")
    (ev,) = [e for evs in read_all_partitions(emu, hub, 1).values() for e in evs]
    assert ev.partition_key == b"the-key"


def test_partition_key_on_batch_is_set_on_every_received_event(emu, hub, producer):
    batch = producer.create_batch(partition_key="batch-key")
    for i in range(3):
        batch.add(EventData(f"e{i}"))
    producer.send_batch(batch)
    events = [e for evs in read_all_partitions(emu, hub, 3).values() for e in evs]
    assert len(events) == 3 and {e.partition_key for e in events} == {b"batch-key"}


def test_events_with_one_key_keep_their_order(emu, hub, producer):
    items = texts(10, "ord")
    for t in items:
        producer.send_event(EventData(t), partition_key="same")
    by = read_all_partitions(emu, hub, 10)
    assert len(by) == 1
    assert bodies(next(iter(by.values()))) == items


def test_partition_key_given_to_send_batch_list(emu, hub, producer):
    producer.send_batch([EventData("a"), EventData("b"), EventData("c")], partition_key="list-key")
    by = read_all_partitions(emu, hub, 3)
    assert len(by) == 1
    assert bodies(next(iter(by.values()))) == ["a", "b", "c"]
    assert {e.partition_key for e in next(iter(by.values()))} == {b"list-key"}


def test_bytes_partition_key_goes_to_the_same_partition_as_the_str_key(emu, hub, producer):
    producer.send_event(EventData("as-str"), partition_key="k-7")
    batch = producer.create_batch(partition_key=b"k-7")
    batch.add(EventData("as-bytes"))
    producer.send_batch(batch)
    by = read_all_partitions(emu, hub, 2)
    assert len(by) == 1


def test_different_producers_agree_on_the_partition_of_a_key(emu, hub, track):
    a, b = track(make_producer(emu, hub)), track(make_producer(emu, hub))
    a.send_event(EventData("a"), partition_key="shared")
    b.send_event(EventData("b"), partition_key="shared")
    assert len(read_all_partitions(emu, hub, 2)) == 1


def test_many_different_keys_spread_over_the_partitions(emu, hub, producer):
    for i in range(40):
        producer.send_event(EventData(f"v{i}"), partition_key=f"device-{i}")
    by = read_all_partitions(emu, hub, 40)
    assert sum(len(v) for v in by.values()) == 40
    assert len(by) >= 2, "40 distinct keys all hashed to a single partition"


def test_partition_key_that_mismatches_the_batch_key_is_a_value_error(emu, hub, producer):
    batch = producer.create_batch(partition_key="one")
    ev = EventData("x")
    ev.raw_amqp_message.annotations[b"x-opt-partition-key"] = "other"
    with pytest.raises(ValueError):
        batch.add(ev)


def test_no_partition_key_means_the_received_event_has_none(emu, hub, producer):
    send_n(producer, 1, partition_id="0")
    (ev,) = read_from_start(emu, hub, 1, partition_id="0")
    assert ev.partition_key is None


# -- no partition id / key -------------------------------------------------------------------------------------------------
def test_send_without_partition_id_or_key_delivers_everything(emu, hub, producer):
    for i in range(20):
        producer.send_event(EventData(f"rr{i}"))
    by = read_all_partitions(emu, hub, 20)
    assert sorted(b for evs in by.values() for b in bodies(evs)) == sorted(f"rr{i}" for i in range(20))


def test_unrouted_batch_stays_together_on_one_partition(emu, hub, producer):
    send_n(producer, 6)
    by = read_all_partitions(emu, hub, 6)
    assert len(by) == 1
    assert bodies(next(iter(by.values()))) == texts(6)


# -- size limits -----------------------------------------------------------------------------------------------------------
def test_default_batch_size_is_the_link_limit(producer):
    # Standard tier: 1 MB per publication (Learn quotas); the SDK reads it from the sender link's max-message-size.
    batch = producer.create_batch()
    assert 900_000 <= batch.max_size_in_bytes <= 1_048_576


def test_batch_add_beyond_max_size_raises_value_error(producer):
    batch = producer.create_batch(max_size_in_bytes=2000)
    added = 0
    with pytest.raises(ValueError):
        while True:
            batch.add(EventData("A" * 100))
            added += 1
            assert added < 100, "the batch never filled up"
    assert added >= 5
    assert batch.size_in_bytes <= 2000
    assert len(batch) == added


def test_batch_full_stops_exactly_when_the_next_event_does_not_fit(producer):
    batch = producer.create_batch(max_size_in_bytes=1000)
    while True:
        try:
            batch.add(EventData("B" * 50))
        except ValueError:
            break
    before = batch.size_in_bytes
    with pytest.raises(ValueError):
        batch.add(EventData("B" * 50))
    assert batch.size_in_bytes == before  # a refused add changes nothing


def test_filled_batch_is_sent_completely(emu, hub, producer):
    batch = producer.create_batch(max_size_in_bytes=100_000, partition_id="0")
    n = 0
    while True:
        try:
            batch.add(EventData(f"x{n:05d}" + "y" * 100))
            n += 1
        except ValueError:
            break
    assert n > 100
    producer.send_batch(batch)
    got = read_from_start(emu, hub, n, partition_id="0")
    assert len(got) == n and got[0].body_as_str().startswith("x00000") and got[-1].body_as_str().startswith(f"x{n-1:05d}")


def test_single_event_larger_than_the_batch_limit_raises_value_error(producer):
    # Microsoft's test_send_batch_too_large_message: 1,100,000 bytes > 1 MB
    batch = producer.create_batch()
    with pytest.raises(ValueError):
        batch.add(EventData(b"A" * 1_100_000))


def test_create_batch_with_a_size_above_the_link_limit_raises_value_error(producer):
    # Microsoft's test_create_batch_with_too_large_size_sync
    with pytest.raises(ValueError):
        producer.create_batch(max_size_in_bytes=5 * 1024 * 1024)


def test_send_batch_list_that_does_not_fit_raises_value_error(producer):
    # Microsoft's test_send_list_wrong_data: 1100 x 1 KB > 1 MB
    with pytest.raises(ValueError):
        producer.send_batch([EventData("A" * 1024)] * 1100)


def test_send_batch_with_a_string_raises_attribute_error(producer):
    # Microsoft's test_send_list_wrong_data ("any str", AttributeError)
    with pytest.raises(AttributeError):
        producer.send_batch("any str")


def test_event_with_a_none_body_is_rejected_by_the_sdk():
    with pytest.raises(ValueError):
        EventData(None)


def test_send_a_250_kb_event_in_a_batch_and_alone(emu, hub, producer):
    # Microsoft's test_send_and_receive_large_body_size
    payload = 250 * 1024
    batch = producer.create_batch(partition_id="0")
    batch.add(EventData("A" * payload))
    producer.send_batch(batch)
    producer.send_event(EventData("A" * payload), partition_id="0")
    got = read_from_start(emu, hub, 2, partition_id="0")
    assert [len(e.body_as_str()) for e in got] == [payload, payload]


def test_send_an_event_close_to_one_megabyte(emu, hub, producer):
    body = bytes(range(256)) * 3900  # 998,400 bytes: below the 1 MB limit once the headers are added
    producer.send_event(EventData(body), partition_id="0")
    (ev,) = read_from_start(emu, hub, 1, partition_id="0")
    assert b"".join(ev.body) == body


def test_send_a_batch_close_to_one_megabyte(emu, hub, producer):
    batch = producer.create_batch(partition_id="1")
    n = 0
    while True:
        try:
            batch.add(EventData(b"z" * 30_000))
            n += 1
        except ValueError:
            break
    assert 30 <= n <= 34  # 1 MB / 30 KB
    producer.send_batch(batch)
    got = read_from_start(emu, hub, n, partition_id="1")
    assert len(got) == n and all(len(b"".join(e.body)) == 30_000 for e in got)


def test_event_over_the_one_megabyte_limit_is_refused_by_the_service(emu, hub, producer):
    # No client-side check for single events: the service rejects it. Which EventHubError subclass depends on how the broker
    # answers (detach with amqp:link:message-size-exceeded, or a rejected delivery); only the base type is asserted.
    with pytest.raises(EventHubError):
        producer.send_event(EventData(b"A" * 1_300_000), partition_id="0")


def test_events_after_a_refused_event_still_go_through(emu, hub, producer):
    with pytest.raises(EventHubError):
        producer.send_event(EventData(b"A" * 1_300_000), partition_id="0")
    producer.send_event(EventData("after"), partition_id="0")
    assert bodies(read_exact(emu, hub, 1, partition_id="0")) == ["after"]


# -- batch / argument errors --------------------------------------------------------------------------------------------
@pytest.mark.parametrize("kw", [{"partition_id": "0"}, {"partition_key": "pk"}])
def test_partition_arguments_with_an_event_data_batch_are_a_type_error(producer, kw):
    # Microsoft's test_send_batch_pid_pk
    batch = EventDataBatch(**{k: v for k, v in kw.items()})
    with pytest.raises(TypeError):
        producer.send_batch(batch, **kw)


def test_event_data_batch_created_by_hand_can_be_sent(emu, hub, producer):
    # Microsoft's test_send_partition: `EventDataBatch(partition_id="0")` then send_batch
    batch = EventDataBatch(partition_id="0")
    batch.add(EventData("by hand"))
    producer.send_batch(batch)
    assert bodies(read_from_start(emu, hub, 1, partition_id="0")) == ["by hand"]


def test_batch_len_and_size_grow_with_each_event(producer):
    batch = producer.create_batch()
    sizes = [batch.size_in_bytes]
    for i in range(4):
        batch.add(EventData("x" * 10))
        sizes.append(batch.size_in_bytes)
    assert len(batch) == 4
    assert sizes == sorted(sizes) and len(set(sizes)) == 5


def test_send_with_negative_timeout_is_an_operation_timeout(producer):
    # Microsoft's test_client_send_timeout
    with pytest.raises(OperationTimeoutError):
        producer.send_batch([EventData(b"Data")], timeout=-1)
    with pytest.raises(OperationTimeoutError):
        producer.send_event(EventData(b"Data"), timeout=-1)


# -- callbacks (non-buffered) ---------------------------------------------------------------------------------------------
def test_on_success_is_called_with_the_events_and_partition_id(emu, hub, track):
    # Microsoft's test_send_with_callback
    sent, errs = [], []
    p = track(make_producer(emu, hub, on_success=lambda ev, pid: sent.append((ev, pid)), on_error=lambda ev, pid, e: errs.append(e)))
    batch = p.create_batch()
    batch.add(EventData(b"Data"))
    batch.add(EventData(b"Data"))
    p.send_batch(batch)
    assert len(sent[-1][0]) == 2 and not sent[-1][1]
    p.send_event(EventData(b"Data"))
    assert len(sent[-1][0]) == 1 and not sent[-1][1]
    p.send_event(EventData(b"Data"), partition_id="0")
    assert len(sent[-1][0]) == 1 and sent[-1][1] == "0"
    batch = p.create_batch(partition_id="1")
    batch.add(EventData(b"Data"))
    p.send_batch(batch)
    assert sent[-1][1] == "1"
    assert errs == []


def test_on_error_receives_the_failure_instead_of_raising(emu, hub, track):
    errs = []
    p = track(make_producer(emu, hub, on_error=lambda ev, pid, e: errs.append((ev, pid, e))))
    p.send_event(EventData("x"), timeout=-1)
    assert len(errs) == 1 and isinstance(errs[0][2], OperationTimeoutError) and len(errs[0][0]) == 1


@pytest.mark.no_autocreate
def test_sending_to_a_missing_hub_is_a_connect_error(emu, track):
    # Microsoft's test_client_invalid_credential: a producer for "fakehub" gets ConnectError from create_batch. On the real service
    # the `$cbs` put-token for the missing hub fails (status 404), which the SDK reports as AuthenticationError (a ConnectError).
    p = track(make_producer(emu, ehutil.uniq("missing")))
    with pytest.raises(ConnectError):
        p.create_batch(partition_id="0")
    with pytest.raises(ConnectError):
        p.send_event(EventData("x"))
