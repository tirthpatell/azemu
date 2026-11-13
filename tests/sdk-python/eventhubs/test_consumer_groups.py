"""Consumer groups: `$Default`, groups created through the admin API, independence, unknown groups.

Sources: Learn "Event Hubs features" (every event hub has `$Default`; a consumer group is an independent view of the stream,
each tracking its own position), Learn quotas (consumer group name up to 50 characters over AMQP), Microsoft's tests (they pass
the group as "$default" and "$Default" against the real service, so names are not case sensitive there).
"""

import pytest
from azure.eventhub.exceptions import AuthenticationError, ConnectError

import ehutil
from ehutil import Collector, bodies, make_consumer, read_from_start, send_n, texts


def read_group(emu, hub, group, expected, **kw):
    with Collector(make_consumer(emu, hub, group), starting_position="-1", max_wait_time=1, **kw) as col:
        return col.wait_events(expected), col


def test_default_group_exists_on_a_new_hub(emu, hub, producer):
    sent = send_n(producer, 3, partition_id="0")
    got, _ = read_group(emu, hub, "$Default", 3, partition_id="0")
    assert bodies(got) == sent


def test_default_group_name_is_not_case_sensitive(emu, hub, producer):
    # Microsoft's livetests all use consumer_group="$default" against the real service
    sent = send_n(producer, 2, partition_id="0")
    got, col = read_group(emu, hub, "$default", 2, partition_id="0")
    assert bodies(got) == sent
    assert col.contexts["0"].consumer_group == "$default"


def test_a_created_group_reads_independently(emu, new_hub, track):
    hub = new_hub(2, groups=["analytics"])
    p = track(ehutil.make_producer(emu, hub))
    sent = send_n(p, 4, partition_id="1")
    got, col = read_group(emu, hub, "analytics", 4, partition_id="1")
    assert bodies(got) == sent
    assert col.contexts["1"].consumer_group == "analytics"


def test_groups_do_not_consume_from_each_other(emu, new_hub, track):
    hub = new_hub(1, groups=["g1", "g2"])
    p = track(ehutil.make_producer(emu, hub))
    sent = send_n(p, 3, partition_id="0")
    for group in ["g1", "g2", "g1"]:
        got, _ = read_group(emu, hub, group, 3, partition_id="0")
        assert bodies(got) == sent, group


def test_two_groups_read_at_the_same_time(emu, new_hub, track):
    hub = new_hub(1, groups=["other"])
    p = track(ehutil.make_producer(emu, hub))
    sent = send_n(p, 3, partition_id="0")
    a = Collector(make_consumer(emu, hub, "$Default"), partition_id="0", starting_position="-1", max_wait_time=1).start()
    b = Collector(make_consumer(emu, hub, "other"), partition_id="0", starting_position="-1", max_wait_time=1).start()
    try:
        assert bodies(a.wait_events(3)) == sent
        assert bodies(b.wait_events(3)) == sent
    finally:
        a.stop()
        b.stop()


def test_groups_of_one_hub_are_not_visible_on_another(emu, new_hub, track):
    a = new_hub(1, groups=["only-on-a"])
    b = new_hub(1)
    p = track(ehutil.make_producer(emu, a))
    send_n(p, 1, partition_id="0")
    got, _ = read_group(emu, a, "only-on-a", 1, partition_id="0")
    assert len(got) == 1
    assert b != a


def test_group_with_a_long_name(emu, new_hub, track):
    name = "g" * 50  # the AMQP limit from Learn quotas
    hub = new_hub(1, groups=[name])
    p = track(ehutil.make_producer(emu, hub))
    send_n(p, 1, partition_id="0")
    got, _ = read_group(emu, hub, name, 1, partition_id="0")
    assert len(got) == 1


@pytest.mark.no_autocreate
def test_unknown_group_is_an_error_when_auto_creation_is_off(emu, hub):
    # The consumer's `$cbs` audience is `.../<hub>/consumergroups/<group>`; the real service refuses the token for a group that does
    # not exist (404), which the SDK reports as AuthenticationError (a ConnectError). Only the base type is asserted.
    # retry_total=0 matters: with retries the SDK's receive loop keeps reconnecting without ever calling on_error (as in
    # Microsoft's test_client_invalid_credential, which also uses retry_total=0 for its consumers).
    with Collector(make_consumer(emu, hub, "no-such-group", retry_total=0), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        col.wait_until(lambda c: c.errors, 10, "an on_error call")
        assert isinstance(col.errors[0][1], ConnectError)


@pytest.mark.no_autocreate
def test_receiving_from_a_missing_hub_reports_authentication_error(emu):
    # Microsoft's test_client_invalid_credential: consumer on "fakehub" -> on_error gets AuthenticationError
    with Collector(make_consumer(emu, ehutil.uniq("missing"), retry_total=0), starting_position="-1", max_wait_time=1) as col:
        col.wait_until(lambda c: c.errors, 10, "an on_error call")
        assert isinstance(col.errors[0][1], AuthenticationError)
