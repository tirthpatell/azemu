"""Starting positions: "-1", "@latest", sequence number, offset, datetime, per-partition dicts, inclusive and exclusive.

Source: SDK `_utils.event_position_selector` (the filter text sent on the receiver link: `amqp.annotation.x-opt-offset > '-1'`,
`x-opt-sequence-number >= '5'`, `x-opt-enqueued-time > '<ms>'`), `_eventprocessor_mixin.get_init_event_position` (a partition missing
from a dict starts at "-1"), Microsoft's `test_receive_with_event_position_sync` (offset / sequence / enqueued_time, inclusive and
exclusive), Learn features page ("start from a specific offset, a timestamp, the beginning or the end of the stream").
"""

import time
from datetime import datetime, timedelta, timezone

import pytest
from azure.eventhub import EventData

from ehutil import Collector, bodies, make_consumer, read_all_from_start, read_exact, read_from_start, send_n, texts


@pytest.fixture
def five(emu, hub, producer):
    """Five events (m-0..m-4) in partition 0 of `hub`, as received (so with their sequence numbers, offsets and times)."""
    send_n(producer, 5, partition_id="0")
    return read_from_start(emu, hub, 5, partition_id="0")


@pytest.fixture
def spaced(emu, hub, producer):
    """Five events sent one by one, 0.15 s apart, so every one has its own enqueued time (a batch may share one)."""
    for i in range(5):
        producer.send_event(EventData(f"m-{i}"), partition_id="0")
        time.sleep(0.15)
    return read_from_start(emu, hub, 5, partition_id="0")


def read(emu, hub, expected, **kw):
    kw.setdefault("partition_id", "0")
    kw.setdefault("max_wait_time", 1)
    with Collector(make_consumer(emu, hub), **kw) as col:
        if expected:
            col.wait_events(expected)
        col.settle(0.5)
        got = col.events
    assert len(got) == expected, f"expected {expected} events, got {bodies(got)}"
    return got


# -- sequence number ---------------------------------------------------------------------------------------------------------
def test_sequence_number_position_is_exclusive_by_default(emu, hub, five):
    got = read(emu, hub, 3, starting_position=five[1].sequence_number)
    assert bodies(got) == ["m-2", "m-3", "m-4"]


def test_sequence_number_position_inclusive(emu, hub, five):
    got = read(emu, hub, 4, starting_position=five[1].sequence_number, starting_position_inclusive=True)
    assert bodies(got) == ["m-1", "m-2", "m-3", "m-4"]


def test_sequence_number_of_the_last_event_exclusive_gives_nothing(emu, hub, five):
    read(emu, hub, 0, starting_position=five[-1].sequence_number)


def test_sequence_number_of_the_last_event_inclusive_gives_that_event(emu, hub, five):
    got = read(emu, hub, 1, starting_position=five[-1].sequence_number, starting_position_inclusive=True)
    assert bodies(got) == ["m-4"]


def test_sequence_number_of_the_first_event_inclusive_gives_everything(emu, hub, five):
    got = read(emu, hub, 5, starting_position=five[0].sequence_number, starting_position_inclusive=True)
    assert bodies(got) == texts(5)


def test_sequence_number_of_the_first_event_exclusive_skips_it(emu, hub, five):
    assert bodies(read(emu, hub, 4, starting_position=five[0].sequence_number)) == texts(5)[1:]


# -- offset ------------------------------------------------------------------------------------------------------------------------
def test_offset_position_is_exclusive_by_default(emu, hub, five):
    got = read(emu, hub, 3, starting_position=five[1].offset)
    assert bodies(got) == ["m-2", "m-3", "m-4"]


def test_offset_position_inclusive(emu, hub, five):
    got = read(emu, hub, 4, starting_position=five[1].offset, starting_position_inclusive=True)
    assert bodies(got) == ["m-1", "m-2", "m-3", "m-4"]


def test_offset_of_the_last_event_exclusive_gives_nothing(emu, hub, five):
    read(emu, hub, 0, starting_position=five[-1].offset)


def test_resuming_from_the_offset_of_a_previous_event_sees_only_newer_ones(emu, hub, producer, five):
    # the pattern a checkpoint store relies on: remember event.offset, resume with it
    last = five[-1].offset
    col = Collector(make_consumer(emu, hub), partition_id="0", starting_position=last, max_wait_time=1).start()
    try:
        col.wait_initialized(1)
        send_n(producer, 2, prefix="new", partition_id="0")
        assert bodies(col.wait_events(2)) == ["new-0", "new-1"]
        col.settle()
        assert col.count() == 2
    finally:
        col.stop()


def test_offset_position_on_another_partition_is_independent(emu, hub, producer, five):
    send_n(producer, 3, prefix="other", partition_id="1")
    got = read(emu, hub, 3, partition_id="1", starting_position="-1")
    assert bodies(got) == texts(3, "other")


# -- datetime ----------------------------------------------------------------------------------------------------------------------
def test_datetime_position_exclusive_skips_the_event_with_that_time(emu, hub, spaced):
    got = read(emu, hub, 3, starting_position=spaced[1].enqueued_time)
    assert bodies(got) == ["m-2", "m-3", "m-4"]


def test_datetime_position_inclusive_includes_the_event_with_that_time(emu, hub, spaced):
    got = read(emu, hub, 4, starting_position=spaced[1].enqueued_time, starting_position_inclusive=True)
    assert bodies(got) == ["m-1", "m-2", "m-3", "m-4"]


def test_datetime_position_of_the_first_event_inclusive_gives_everything(emu, hub, spaced):
    got = read(emu, hub, 5, starting_position=spaced[0].enqueued_time, starting_position_inclusive=True)
    assert bodies(got) == texts(5)


def test_datetime_taken_between_two_sends_splits_the_stream(emu, hub, producer):
    send_n(producer, 2, prefix="before", partition_id="0")
    time.sleep(0.4)
    cut = datetime.now(timezone.utc)
    time.sleep(0.4)
    send_n(producer, 2, prefix="after", partition_id="0")
    got = read(emu, hub, 2, starting_position=cut)
    assert bodies(got) == ["after-0", "after-1"]


def test_datetime_in_the_future_gives_nothing(emu, hub, five):
    read(emu, hub, 0, starting_position=datetime.now(timezone.utc) + timedelta(hours=1))


def test_datetime_long_ago_gives_everything(emu, hub, five):
    got = read(emu, hub, 5, starting_position=datetime.now(timezone.utc) - timedelta(days=1))
    assert bodies(got) == texts(5)


def test_naive_datetime_is_read_as_utc(emu, hub, five):
    # event_position_selector uses calendar.timegm(value.utctimetuple()): a naive datetime counts as UTC
    long_ago = datetime.now(timezone.utc).replace(tzinfo=None) - timedelta(days=1)
    assert len(read(emu, hub, 5, starting_position=long_ago)) == 5


# -- "-1" and "@latest" ----------------------------------------------------------------------------------------------------------
def test_minus_one_starts_at_the_beginning(emu, hub, five):
    assert bodies(read(emu, hub, 5, starting_position="-1")) == texts(5)


def test_latest_ignores_everything_already_there(emu, hub1, producer1):
    send_n(producer1, 3, prefix="old", partition_id="0")
    from ehutil import live_receiver

    col = live_receiver(emu, hub1, producer1, ["0"], starting_position="@latest")
    try:
        col.settle(0.5)
        assert col.events == []
        send_n(producer1, 1, prefix="fresh", partition_id="0")
        assert bodies(col.wait_events(1)) == ["fresh-0"]
    finally:
        col.stop()


# -- one position per partition ---------------------------------------------------------------------------------------------------
@pytest.fixture
def two_partitions(emu, hub2, producer2):
    send_n(producer2, 4, prefix="a", partition_id="0")
    send_n(producer2, 4, prefix="b", partition_id="1")
    by = read_all_from_start(emu, hub2, 8)
    return hub2, by["0"], by["1"]


def read_all(emu, hub, expected, **kw):
    with Collector(make_consumer(emu, hub), max_wait_time=1, **kw) as col:
        if expected:
            col.wait_events(expected)
        col.settle(0.5)
        by = col.by_partition()
    assert sum(len(v) for v in by.values()) == expected, {k: bodies(v) for k, v in by.items()}
    return {pid: bodies(evs) for pid, evs in by.items()}


def test_dict_of_positions_one_per_partition(emu, two_partitions):
    hub, p0, p1 = two_partitions
    got = read_all(emu, hub, 4 + 2, starting_position={"0": "-1", "1": p1[1].sequence_number})
    assert got == {"0": texts(4, "a"), "1": ["b-2", "b-3"]}


def test_partition_missing_from_the_dict_starts_at_the_beginning(emu, two_partitions):
    hub, p0, p1 = two_partitions
    got = read_all(emu, hub, 4 + 1, starting_position={"1": p1[2].sequence_number})
    assert got == {"0": texts(4, "a"), "1": ["b-3"]}


def test_dict_with_offsets_and_a_sequence_number_mixed(emu, two_partitions):
    hub, p0, p1 = two_partitions
    got = read_all(emu, hub, 3 + 2, starting_position={"0": p0[0].offset, "1": p1[1].sequence_number})
    assert got == {"0": texts(4, "a")[1:], "1": ["b-2", "b-3"]}


def test_inclusive_flag_per_partition(emu, two_partitions):
    hub, p0, p1 = two_partitions
    got = read_all(
        emu, hub, 3 + 2,
        starting_position={"0": p0[1].sequence_number, "1": p1[1].sequence_number},
        starting_position_inclusive={"0": True},
    )
    assert got == {"0": ["a-1", "a-2", "a-3"], "1": ["b-2", "b-3"]}


def test_one_inclusive_flag_applies_to_every_partition(emu, two_partitions):
    hub, p0, p1 = two_partitions
    got = read_all(emu, hub, 3 + 3, starting_position={"0": p0[1].sequence_number, "1": p1[1].sequence_number},
                   starting_position_inclusive=True)
    assert got == {"0": ["a-1", "a-2", "a-3"], "1": ["b-1", "b-2", "b-3"]}


def test_one_sequence_number_for_all_partitions_applies_to_each(emu, two_partitions):
    hub, p0, p1 = two_partitions
    assert p0[1].sequence_number == p1[1].sequence_number  # both partitions started empty
    got = read_all(emu, hub, 2 + 2, starting_position=p0[1].sequence_number)
    assert got == {"0": ["a-2", "a-3"], "1": ["b-2", "b-3"]}


def test_latest_for_one_partition_and_start_for_another(emu, hub2, producer2):
    send_n(producer2, 2, prefix="old", partition_id="0")
    send_n(producer2, 2, prefix="old", partition_id="1")
    col = Collector(make_consumer(emu, hub2), starting_position={"0": "-1", "1": "@latest"}, max_wait_time=1).start()
    try:
        col.wait_live(producer2, ["1"])  # partition 1 is fixed at "@latest" once its marker came back
        send_n(producer2, 1, prefix="new", partition_id="1")
        col.wait_events(3)
        col.settle(0.5)
        by = {pid: bodies(evs) for pid, evs in col.by_partition().items()}
        assert by == {"0": ["old-0", "old-1"], "1": ["new-0"]}
    finally:
        col.stop()
