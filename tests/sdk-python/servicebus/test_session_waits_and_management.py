"""Waiting for "the next available session", and
`$management` answers on a connection that has several request/response pairs.

Expectations come from the Service Bus docs (message sessions: a session receiver reads only the session it was
given, and a session lock is released when its receiver closes) and from the SDK, not from the emulator.
"""

import threading
import time

import pytest
from azure.servicebus import NEXT_AVAILABLE_SESSION, ServiceBusMessage
from azure.servicebus.exceptions import OperationTimeoutError

from sbutil import bodies, receive_n, uniq


def send_session(client, queue, session_id, n=2, prefix=None):
    prefix = prefix or session_id
    with client.get_queue_sender(queue) as s:
        s.send_messages([ServiceBusMessage(f"{prefix}-{i}", session_id=session_id) for i in range(n)])
    return [f"{prefix}-{i}" for i in range(n)]


def test_a_receiver_that_waited_for_a_session_reads_only_that_session(make_client, session_queue):
    sender_client = make_client()
    receiver_client = make_client()

    def send_later():
        time.sleep(1.0)
        with sender_client.get_queue_sender(session_queue) as s:
            s.send_messages(
                [
                    ServiceBusMessage("a-0", session_id="A"),
                    ServiceBusMessage("b-0", session_id="B"),
                    ServiceBusMessage("a-1", session_id="A"),
                ]
            )

    t = threading.Thread(target=send_later)
    t.start()
    try:
        with receiver_client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=10) as r:
            assert r.session.session_id == "A"
            got = receive_n(r, 2)
            assert bodies(got) == ["a-0", "a-1"]
            assert {m.session_id for m in got} == {"A"}
    finally:
        t.join()
    # session B was not touched by that receiver
    with receiver_client.get_queue_receiver(session_queue, session_id="B", max_wait_time=2) as r:
        assert bodies(receive_n(r, 1)) == ["b-0"]


def test_a_session_found_after_waiting_is_released_when_its_receiver_closes(make_client, session_queue):
    sender_client = make_client()
    client = make_client()
    t = threading.Timer(0.8, lambda: send_session(sender_client, session_queue, "late", 3))
    t.start()
    try:
        with client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=10) as r:
            assert r.session.session_id == "late"
            first = r.receive_messages(max_message_count=1, max_wait_time=3)
            assert bodies(first) == ["late-0"]
            r.complete_message(first[0])
    finally:
        t.join()
    # the lock is gone with the receiver: a new one gets the rest at once, not after the lock duration
    start = time.monotonic()
    with client.get_queue_receiver(session_queue, session_id="late", max_wait_time=2) as r2:
        assert bodies(receive_n(r2, 2)) == ["late-1", "late-2"]
    assert time.monotonic() - start < 10


def test_a_next_session_wait_that_gave_up_does_not_hold_a_later_session(make_client, session_queue):
    client = make_client(retry_total=0)
    with pytest.raises(OperationTimeoutError):
        with client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=1):
            pass
    send_session(client, session_queue, "after", 2)
    time.sleep(0.5)
    start = time.monotonic()
    with client.get_queue_receiver(session_queue, session_id="after", max_wait_time=2) as r:
        assert bodies(receive_n(r, 2)) == ["after-0", "after-1"]
    assert time.monotonic() - start < 10


def test_receivers_of_two_entities_on_one_client_each_get_their_own_peek_answers(make_client, new_queue):
    q1, q2 = new_queue("qa"), new_queue("qb")
    client = make_client()
    with client.get_queue_sender(q1) as s:
        s.send_messages([ServiceBusMessage(f"one-{i}") for i in range(3)])
    with client.get_queue_sender(q2) as s:
        s.send_messages([ServiceBusMessage(f"two-{i}") for i in range(5)])
    with client.get_queue_receiver(q1, max_wait_time=1) as r1, client.get_queue_receiver(q2, max_wait_time=1) as r2:
        for _ in range(5):
            assert bodies(r1.peek_messages(max_message_count=10, sequence_number=1)) == ["one-0", "one-1", "one-2"]
            assert bodies(r2.peek_messages(max_message_count=10, sequence_number=1)) == [f"two-{i}" for i in range(5)]


def test_two_receivers_of_one_entity_peek_in_threads_on_one_client(make_client, queue):
    client = make_client()
    with client.get_queue_sender(queue) as s:
        s.send_messages([ServiceBusMessage(f"m-{i}") for i in range(4)])
    errors = []
    with client.get_queue_receiver(queue, max_wait_time=1) as r1, client.get_queue_receiver(queue, max_wait_time=1) as r2:

        def peek(r):
            try:
                for _ in range(5):
                    assert bodies(r.peek_messages(max_message_count=10, sequence_number=1)) == [f"m-{i}" for i in range(4)]
            except Exception as e:  # noqa: BLE001
                errors.append(e)

        threads = [threading.Thread(target=peek, args=(r,)) for r in (r1, r2)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    assert not errors, errors


def test_peek_returns_at_most_250_messages(make_client, queue):
    # Service Bus quotas: "Maximum number of messages returned in PeekMessagesAsync call: 250"
    client = make_client()
    with client.get_queue_sender(queue) as s:
        for start in range(0, 300, 100):
            s.send_messages([ServiceBusMessage(f"m-{i}") for i in range(start, start + 100)])
    with client.get_queue_receiver(queue, max_wait_time=1) as r:
        assert len(r.peek_messages(max_message_count=300)) == 250
        assert len(r.peek_messages(max_message_count=300, sequence_number=251)) == 50
