"""Exclusive receivers: owner_level (the "epoch" of a receiver).

Sources: Microsoft Learn "Event Hubs features" ("Only one epoch consumer can own a given partition in a consumer group at a time ... If
a new epoch consumer connects with a higher owner level, the service disconnects the current owner with a ConsumerDisconnected
error ... Up to 5 non-epoch receivers can read the same partition concurrently ... Connecting an epoch consumer to a partition
disconnects all non-epoch consumers"), Learn quotas ("Number of non-epoch receivers per consumer group: 5"), the SDK
(`check_link_stolen` turns `amqp:link:stolen` into ConnectionLostError, which `on_error` receives and which closes the partition
consumer with CloseReason.OWNERSHIP_LOST), Microsoft's `test_receive_owner_level`.

What is ASSUMED rather than documented: that a receiver with a LOWER owner level is refused (the docs only describe the higher
case; the .NET/Java SDK docs say the new link fails) and that a non-epoch receiver is refused while an epoch receiver exists.
"""

import pytest
from azure.eventhub import CloseReason
from azure.eventhub.exceptions import ConnectionLostError, EventHubError

from ehutil import Collector, bodies, live_receiver, make_consumer, make_producer, send_n


def start(emu, hub, **kw):
    kw.setdefault("max_wait_time", 1)
    return Collector(make_consumer(emu, hub), partition_id="0", starting_position="@latest", **kw).start()


def take_over(emu, hub, producer, first_level, second_level):
    """Start receiver A (owner level `first_level`), confirm it is live, start B with `second_level`; returns (a, b)."""
    a = live_receiver(emu, hub, producer, ["0"], owner_level=first_level)
    b = start(emu, hub, owner_level=second_level)
    return a, b


def test_higher_owner_level_takes_over_and_the_first_receiver_gets_an_error(emu, hub1, producer1):
    a, b = take_over(emu, hub1, producer1, 1, 2)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.errors, 8, "an on_error call on the first receiver")
        assert isinstance(a.errors[0][1], EventHubError)
        assert a.errors[0][0] == "0"
    finally:
        a.stop()
        b.stop()


def test_the_error_the_first_receiver_sees_is_connection_lost(emu, hub1, producer1):
    # amqp:link:stolen is a standard AMQP condition: `_create_eventhub_exception` maps it to ConnectionLostError
    a, b = take_over(emu, hub1, producer1, 1, 2)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.errors, 8, "an on_error call")
        assert isinstance(a.errors[0][1], ConnectionLostError)
    finally:
        a.stop()
        b.stop()


def test_the_displaced_receiver_closes_its_partition_with_ownership_lost(emu, hub1, producer1):
    a, b = take_over(emu, hub1, producer1, 1, 2)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.closed, 8, "on_partition_close on the first receiver")
        assert a.closed[0] == ("0", CloseReason.OWNERSHIP_LOST)
    finally:
        a.stop()
        b.stop()


def test_only_the_new_owner_receives_events_after_the_takeover(emu, hub1, producer1):
    a, b = take_over(emu, hub1, producer1, 1, 2)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.errors, 8, "the takeover")
        before = a.count()
        sent = send_n(producer1, 3, prefix="after", partition_id="0")
        assert bodies(b.wait_events(3)) == sent
        a.settle(0.5)
        assert a.count() == before
    finally:
        a.stop()
        b.stop()


def test_equal_owner_level_also_takes_over(emu, hub1, producer1):
    # The SDK's own load balancer relies on this: every checkpoint-store consumer uses owner_level 0.
    a, b = take_over(emu, hub1, producer1, 3, 3)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.errors, 8, "an on_error call on the first receiver")
    finally:
        a.stop()
        b.stop()


def test_lower_owner_level_is_refused_and_the_owner_keeps_receiving(emu, hub1, producer1):
    a = live_receiver(emu, hub1, producer1, ["0"], owner_level=5)
    b = start(emu, hub1, owner_level=1)
    try:
        b.wait_until(lambda c: c.errors, 8, "an on_error call on the refused receiver")
        assert isinstance(b.errors[0][1], EventHubError)
        sent = send_n(producer1, 2, prefix="still", partition_id="0")
        assert bodies(a.wait_events(2)) == sent
        assert a.errors == []
        assert b.count() == 0
    finally:
        a.stop()
        b.stop()


def test_epoch_receiver_disconnects_a_non_epoch_receiver(emu, hub1, producer1):
    # Microsoft's test_receive_owner_level: the first client has no owner level, the second uses owner_level=1
    a = live_receiver(emu, hub1, producer1, ["0"])
    b = start(emu, hub1, owner_level=1)
    try:
        b.wait_live(producer1, ["0"])
        a.wait_until(lambda c: c.errors, 8, "an on_error call on the non-epoch receiver")
        assert isinstance(a.errors[0][1], EventHubError)
    finally:
        a.stop()
        b.stop()


def test_non_epoch_receiver_is_refused_while_an_epoch_receiver_exists(emu, hub1, producer1):
    # ASSUMED (see module docstring)
    a = live_receiver(emu, hub1, producer1, ["0"], owner_level=1)
    b = start(emu, hub1)
    try:
        b.wait_until(lambda c: c.errors, 8, "an on_error call on the refused receiver")
        sent = send_n(producer1, 1, prefix="mine", partition_id="0")
        assert bodies(a.wait_events(1)) == sent
    finally:
        a.stop()
        b.stop()


def test_non_epoch_receivers_share_a_partition(emu, hub1, producer1):
    a = live_receiver(emu, hub1, producer1, ["0"])
    b = live_receiver(emu, hub1, producer1, ["0"])
    try:
        sent = send_n(producer1, 3, prefix="fan", partition_id="0")
        assert bodies(a.wait_events(3)) == sent
        assert bodies(b.wait_events(3)) == sent
        assert a.errors == [] and b.errors == []
    finally:
        a.stop()
        b.stop()


@pytest.mark.slow
def test_a_sixth_non_epoch_receiver_is_refused(emu, hub1, producer1):
    # Learn quotas: "Number of non-epoch receivers per consumer group: 5"
    live = [start(emu, hub1) for _ in range(5)]
    sixth = None
    try:
        for c in live:
            c.wait_live(producer1, ["0"])  # the first call's markers reach every attached receiver, so the rest return at once
        # Azure refuses the sixth with `amqp:resource-limit-exceeded` (QuotaExceededException in .NET). The Python SDK
        # only reports that through on_error when retries are off: with retry_total >= 1 each receive call uses up one
        # retry, reopens, and returns quietly, for ever (the SDK's own TODO in `_consumer.receive`, issue #27137).
        sixth = Collector(
            make_consumer(emu, hub1, retry_total=0), partition_id="0", starting_position="@latest", max_wait_time=1
        ).start()
        sixth.wait_until(lambda c: c.errors, 8, "an on_error call on the sixth receiver")
        pid, err = sixth.errors[0]
        assert pid == "0"
        assert isinstance(err, ConnectionLostError)
        assert "maximum number of allowed receivers" in str(err)
        assert all(c.errors == [] for c in live)
    finally:
        if sixth:
            sixth.stop()
        for c in live:
            c.stop()


def test_owner_levels_in_different_consumer_groups_do_not_conflict(emu, new_hub, track):
    hub = new_hub(1, groups=["g2"])
    p = track(make_producer(emu, hub))
    a = live_receiver(emu, hub, p, ["0"], group="$Default", owner_level=1)
    b = live_receiver(emu, hub, p, ["0"], group="g2", owner_level=1)
    try:
        sent = send_n(p, 2, prefix="both", partition_id="0")
        assert bodies(a.wait_events(2)) == sent
        assert bodies(b.wait_events(2)) == sent
        assert a.errors == [] and b.errors == []
    finally:
        a.stop()
        b.stop()


def test_owner_levels_on_different_partitions_do_not_conflict(emu, hub2, producer2):
    a = Collector(make_consumer(emu, hub2), partition_id="0", starting_position="@latest", owner_level=1, max_wait_time=1).start()
    b = Collector(make_consumer(emu, hub2), partition_id="1", starting_position="@latest", owner_level=1, max_wait_time=1).start()
    try:
        a.wait_live(producer2, ["0"])
        b.wait_live(producer2, ["1"])
        send_n(producer2, 2, prefix="x", partition_id="0")
        send_n(producer2, 2, prefix="y", partition_id="1")
        assert bodies(a.wait_events(2)) == ["x-0", "x-1"]
        assert bodies(b.wait_events(2)) == ["y-0", "y-1"]
        assert a.errors == [] and b.errors == []
    finally:
        a.stop()
        b.stop()


def test_a_receiver_reconnecting_with_the_same_owner_level_after_close_works(emu, hub1, producer1):
    a = live_receiver(emu, hub1, producer1, ["0"], owner_level=4)
    a.stop()
    b = live_receiver(emu, hub1, producer1, ["0"], owner_level=4)
    try:
        sent = send_n(producer1, 1, prefix="again", partition_id="0")
        assert bodies(b.wait_events(1)) == sent
    finally:
        b.stop()
