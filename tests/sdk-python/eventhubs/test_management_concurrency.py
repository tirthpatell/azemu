"""`$management` answers on one connection with several
request/response pairs, and what a hub keeps.
"""

import threading

import pytest

import ehutil
from ehutil import make_producer


def test_concurrent_management_calls_on_one_client_each_get_their_own_answer(emu, hub, track):
    client = track(make_producer(emu, hub))
    errors = []

    def ask(pid):
        try:
            for _ in range(5):
                props = client.get_partition_properties(pid)
                assert props["id"] == pid
                assert props["eventhub_name"] == hub
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    threads = [threading.Thread(target=ask, args=(str(p),)) for p in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors, errors


def test_two_clients_on_two_hubs_get_the_right_properties(emu, hub, hub2, track):
    a = track(make_producer(emu, hub))
    b = track(make_producer(emu, hub2))
    for _ in range(3):
        assert a.get_eventhub_properties()["eventhub_name"] == hub
        assert b.get_eventhub_properties()["eventhub_name"] == hub2
