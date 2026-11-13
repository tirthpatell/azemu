"""Scheduled messages: schedule_messages / cancel_scheduled_messages / scheduled_enqueue_time_utc.

Sources: Microsoft's test_queue_schedule_message / _schedule_multiple_messages / _cancel_scheduled_messages /
test_state_scheduled, docs "Message sequencing and timestamps" and "Browse or peek messages" (queues show scheduled
messages in peek, subscriptions do not).
"""

import time
from datetime import timedelta

import pytest
from azure.servicebus import ServiceBusMessage, ServiceBusMessageState

from sbutil import body_of, drain_bodies, receive_n, utcnow


def whole_seconds(delta_seconds: float):
    """utcnow() + delta, with the microseconds cut (Microsoft's test does the same before comparing)."""
    return (utcnow() + timedelta(seconds=delta_seconds)).replace(microsecond=0)


def test_schedule_messages_returns_one_sequence_number_per_message(sb, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages([ServiceBusMessage(f"s{i}") for i in range(3)], whole_seconds(60))
    assert len(seqs) == 3
    assert all(isinstance(n, int) and n > 0 for n in seqs)
    assert len(set(seqs)) == 3


def test_schedule_single_message_returns_a_list_with_one_sequence_number(sb, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages(ServiceBusMessage("one"), whole_seconds(60))
    assert isinstance(seqs, list) and len(seqs) == 1


def test_schedule_empty_list_returns_empty_list(sb, queue):
    with sb.get_queue_sender(queue) as s:
        assert s.schedule_messages([], utcnow()) == []
        s.cancel_scheduled_messages([])


def test_scheduled_message_is_not_received_before_its_time(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages(ServiceBusMessage("later"), whole_seconds(120))
    assert drain_bodies(sb, queue, max_wait=1) == []


def test_message_with_scheduled_enqueue_time_is_not_received_before_its_time(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("later", scheduled_enqueue_time_utc=whole_seconds(120)))
    assert drain_bodies(sb, queue, max_wait=1) == []


@pytest.mark.slow
def test_scheduled_message_arrives_after_its_time(sb, queue):
    # 5 s, not 3: cutting the microseconds can take up to a second off, and opening the
    # sender and receiver costs about 1.5 s before the "nothing yet" check below.
    when = whole_seconds(5)
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages(ServiceBusMessage("soon", message_id="sched-1"), when)
    with sb.get_queue_receiver(queue, max_wait_time=8) as r:
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
        (m,) = receive_n(r, 1, max_wait=8)
    assert body_of(m) == "soon"
    assert m.message_id == "sched-1"
    assert m.scheduled_enqueue_time_utc == when
    assert m.enqueued_time_utc.replace(microsecond=0) >= when - timedelta(seconds=1)
    assert m.delivery_count == 0
    assert m.state == ServiceBusMessageState.ACTIVE


@pytest.mark.slow
def test_scheduled_enqueue_time_property_on_send_delivers_after_its_time(sb, queue):
    when = whole_seconds(3)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("prop", scheduled_enqueue_time_utc=when))
    with sb.get_queue_receiver(queue, max_wait_time=8) as r:
        (m,) = receive_n(r, 1, max_wait=8)
    assert body_of(m) == "prop"
    assert m.scheduled_enqueue_time_utc == when


@pytest.mark.slow
def test_scheduled_messages_keep_order_of_their_times(sb, queue):
    base = utcnow().replace(microsecond=0)
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages(ServiceBusMessage("second"), base + timedelta(seconds=5))
        s.schedule_messages(ServiceBusMessage("first"), base + timedelta(seconds=3))
    with sb.get_queue_receiver(queue, max_wait_time=10) as r:
        got = receive_n(r, 2, max_wait=10, rounds=4)
    assert [body_of(m) for m in got] == ["first", "second"]


def test_scheduled_messages_are_visible_to_peek_with_scheduled_state(sb, queue):
    # Microsoft's test_state_scheduled; docs: Peek returns scheduled messages for queues
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages([ServiceBusMessage(f"p{i}") for i in range(3)], whole_seconds(120))
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        peeked = r.peek_messages(max_message_count=10)
    assert len(peeked) == 3
    assert all(m.state == ServiceBusMessageState.SCHEDULED for m in peeked)
    assert sorted(m.sequence_number for m in peeked) == sorted(seqs)


def test_peeked_scheduled_message_has_its_scheduled_enqueue_time(sb, queue):
    when = whole_seconds(120)
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages(ServiceBusMessage("t"), when)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (p,) = r.peek_messages()
    assert p.scheduled_enqueue_time_utc == when


def test_scheduled_message_keeps_its_properties(sb, queue):
    when = whole_seconds(120)
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages(
            ServiceBusMessage("t", subject="sub", application_properties={"k": 1}, correlation_id="c"), when
        )
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (p,) = r.peek_messages()
    assert (p.subject, p.correlation_id) == ("sub", "c")
    assert p.application_properties[b"k"] == 1


def test_cancel_scheduled_message_removes_it_from_the_queue(sb, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages(ServiceBusMessage("cancel-me"), whole_seconds(120))
        s.cancel_scheduled_messages(seqs)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert r.peek_messages() == []


def test_cancel_scheduled_message_with_a_single_int(sb, queue):
    with sb.get_queue_sender(queue) as s:
        (seq,) = s.schedule_messages(ServiceBusMessage("cancel-me"), whole_seconds(120))
        s.cancel_scheduled_messages(seq)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert r.peek_messages() == []


def test_cancel_only_some_of_several_scheduled_messages(sb, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages([ServiceBusMessage(f"c{i}") for i in range(4)], whole_seconds(120))
        s.cancel_scheduled_messages(seqs[:2])
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        left = r.peek_messages(max_message_count=10)
    assert sorted(m.sequence_number for m in left) == sorted(seqs[2:])


@pytest.mark.slow
def test_cancelled_scheduled_message_is_never_delivered(sb, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages(ServiceBusMessage("never"), whole_seconds(2))
        s.cancel_scheduled_messages(seqs)
    time.sleep(3.5)
    assert drain_bodies(sb, queue, max_wait=1) == []


def test_closed_sender_can_not_schedule_or_cancel(sb, queue):
    s = sb.get_queue_sender(queue)
    s.close()
    with pytest.raises(ValueError):
        s.schedule_messages(ServiceBusMessage("x"), utcnow())
    with pytest.raises(ValueError):
        s.cancel_scheduled_messages([1, 2, 3])


def test_scheduled_message_can_be_peeked_but_not_received_in_the_same_session(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("now"))
        s.schedule_messages(ServiceBusMessage("later"), whole_seconds(120))
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert len(r.peek_messages(max_message_count=10)) == 2
        got = receive_n(r, 1)
        assert body_of(got[0]) == "now"
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
