"""Behaviour of the emulator itself: creating event hubs on first use and through the admin API. Not real Event Hubs behaviour.

Real Event Hubs has no auto-creation (the hub must exist, a missing one gives an error); the emulator does not create hubs by being
named unless it is started with AZEMU_EVENTHUBS_AUTOCREATE=1 (or AZEMU_LENIENT=1). Then it creates a hub with 4 partitions the first
time a client uses an unknown name. The four tests marked `autocreate` are about that and run in the runner's second pass with the
variable set; the others check the admin route that creates a hub with a chosen partition count and consumer groups, and reset, and
run by default. Tests that only need a hub use the admin helper (`new_hub`) and never rely on auto-creation.
"""

import pytest
from azure.eventhub import EventData

import ehutil
from ehutil import Collector, bodies, make_consumer, make_producer, read_from_start, send_n

pytestmark = pytest.mark.emulator_only


@pytest.mark.autocreate
def test_unknown_hub_is_created_with_four_partitions_on_first_use(emu, track):
    p = track(make_producer(emu, ehutil.uniq("auto")))
    assert p.get_partition_ids() == ["0", "1", "2", "3"]


@pytest.mark.autocreate
def test_auto_created_hub_works_end_to_end(emu, track):
    name = ehutil.uniq("auto")
    p = track(make_producer(emu, name))
    send_n(p, 3, partition_id="2")
    assert bodies(read_from_start(emu, name, 3, partition_id="2")) == ["m-0", "m-1", "m-2"]


@pytest.mark.autocreate
def test_auto_created_hub_has_the_default_consumer_group(emu, track):
    name = ehutil.uniq("auto")
    p = track(make_producer(emu, name))
    send_n(p, 1, partition_id="0")
    with Collector(make_consumer(emu, name, "$Default"), partition_id="0", starting_position="-1", max_wait_time=1) as col:
        assert len(col.wait_events(1)) == 1


@pytest.mark.autocreate
def test_receiving_from_an_unknown_hub_creates_it(emu, track):
    name = ehutil.uniq("auto")
    c = track(make_consumer(emu, name))
    assert c.get_partition_ids() == ["0", "1", "2", "3"]


def test_admin_api_creates_a_hub_with_the_requested_partitions(emu, track):
    name = ehutil.uniq("admin")
    ehutil.admin_create_hub(emu, name, partitions=3)
    assert track(make_producer(emu, name)).get_partition_ids() == ["0", "1", "2"]


def test_admin_api_creates_the_requested_consumer_groups(emu, track):
    name = ehutil.uniq("admin")
    ehutil.admin_create_hub(emu, name, partitions=1, groups=["alpha", "beta"])
    p = track(make_producer(emu, name))
    send_n(p, 2, partition_id="0")
    for group in ("alpha", "beta"):
        with Collector(make_consumer(emu, name, group), partition_id="0", starting_position="-1", max_wait_time=1) as col:
            assert len(col.wait_events(2)) == 2


def test_same_hub_name_in_two_namespaces_is_two_hubs(emu, track):
    name = ehutil.uniq("ns")
    ehutil.admin_create_hub(emu, name, partitions=2, namespace="myns")
    ehutil.admin_create_hub(emu, name, partitions=3, namespace="other")
    a = track(make_producer(emu, name, namespace="myns"))
    b = track(make_producer(emu, name, namespace="other"))
    assert a.get_partition_ids() == ["0", "1"]
    assert b.get_partition_ids() == ["0", "1", "2"]
    send_n(a, 1, partition_id="0")
    assert b.get_partition_properties("0")["is_empty"] is True


def test_reset_removes_hubs_and_their_events(emu, track):
    import urllib.request

    name = ehutil.uniq("reset")
    ehutil.admin_create_hub(emu, name, partitions=1)
    p = track(make_producer(emu, name))
    send_n(p, 2, partition_id="0")
    urllib.request.urlopen(urllib.request.Request(f"{emu.gateway}/_azemu/reset", method="POST", data=b""), timeout=10).read()
    ehutil.admin_create_hub(emu, name, partitions=1)
    fresh = track(make_producer(emu, name))
    assert fresh.get_partition_properties("0")["is_empty"] is True
