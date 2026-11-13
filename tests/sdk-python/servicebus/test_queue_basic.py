"""Queues: send, receive, batch, peek, receive-and-delete, ordering, competing receivers.

Behaviour taken from Microsoft's test_queues.py (real Service Bus) and the Service Bus docs.
"""

import time

import pytest
from azure.servicebus import ServiceBusMessage, ServiceBusMessageBatch
from azure.servicebus.exceptions import (
    MessageSizeExceededError,
    MessagingEntityNotFoundError,
    ServiceBusError,
)

from sbutil import (
    RECEIVE_AND_DELETE,
    bodies,
    body_of,
    drain,
    drain_bodies,
    receive_n,
    send_n,
)


def test_send_one_message_and_receive_it(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(ServiceBusMessage("hello"))
    with sb.get_queue_receiver(queue, max_wait_time=3) as receiver:
        msgs = receiver.receive_messages(max_message_count=5, max_wait_time=3)
        assert bodies(msgs) == ["hello"]
        receiver.complete_message(msgs[0])


def test_send_many_messages_one_call_each(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        for i in range(10):
            sender.send_messages(ServiceBusMessage(f"m-{i}"))
    assert drain_bodies(sb, queue) == [f"m-{i}" for i in range(10)]


def test_send_list_of_messages_in_one_call(sb, queue):
    sent = send_n(sb, queue, 7)
    assert drain_bodies(sb, queue) == sent


def test_send_message_batch_object(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        batch = sender.create_message_batch()
        for i in range(5):
            batch.add_message(ServiceBusMessage(f"b-{i}"))
        assert len(batch) == 5
        assert batch.size_in_bytes > 0
        sender.send_messages(batch)
    assert drain_bodies(sb, queue) == [f"b-{i}" for i in range(5)]


def test_manually_made_batch_object_can_be_sent(sb, queue):
    batch = ServiceBusMessageBatch()
    for i in range(3):
        batch.add_message(ServiceBusMessage(f"x-{i}"))
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(batch)
    assert drain_bodies(sb, queue) == ["x-0", "x-1", "x-2"]


def test_batch_add_beyond_max_size_raises_message_size_exceeded(sb, queue):
    # client-side check against max_size_in_bytes: needs no emulator support beyond a working sender
    with sb.get_queue_sender(queue) as sender:
        batch = sender.create_message_batch(max_size_in_bytes=2000)
        added = 0
        with pytest.raises(MessageSizeExceededError):
            for _ in range(100):
                batch.add_message(ServiceBusMessage("x" * 300))
                added += 1
        assert 0 < added < 100
        assert len(batch) == added
        sender.send_messages(batch)
    assert len(drain_bodies(sb, queue)) == added


def test_message_size_exceeded_is_a_value_error():
    assert issubclass(MessageSizeExceededError, ValueError)
    assert issubclass(MessageSizeExceededError, ServiceBusError)


def test_message_over_256kb_is_rejected_on_standard_tier(sb, queue):
    # Standard tier limit is 256 KB per message (Microsoft's test sends "A" * 256 * 1024 and expects this error).
    # The sender link must advertise max-message-size 262144 (or the broker must detach with message-size-exceeded).
    with sb.get_queue_sender(queue) as sender:
        with pytest.raises(MessageSizeExceededError):
            sender.send_messages(ServiceBusMessage("A" * 256 * 1024))


def test_two_messages_together_over_256kb_are_rejected(sb, queue):
    half = "A" * (128 * 1024)
    with sb.get_queue_sender(queue) as sender:
        with pytest.raises(MessageSizeExceededError):
            sender.send_messages([ServiceBusMessage(half), ServiceBusMessage(half)])


def test_message_of_200kb_round_trips(sb, queue):
    big = "B" * (200 * 1024)
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(ServiceBusMessage(big))
    with sb.get_queue_receiver(queue, max_wait_time=5) as receiver:
        (msg,) = receive_n(receiver, 1, max_wait=5)
        assert body_of(msg) == big


def test_send_empty_list_is_a_no_op(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        assert sender.send_messages([]) is None
        assert sender.send_messages(ServiceBusMessageBatch()) is None
    assert drain_bodies(sb, queue) == []


def test_receive_from_empty_queue_returns_empty_list(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as receiver:
        assert receiver.receive_messages(max_message_count=5, max_wait_time=1) == []


def test_receive_on_empty_queue_returns_after_max_wait_time(sb, queue):
    with sb.get_queue_receiver(queue) as receiver:
        start = time.monotonic()
        assert receiver.receive_messages(max_message_count=1, max_wait_time=1) == []
        assert time.monotonic() - start < 8


def test_iterator_receive_stops_when_queue_is_empty(sb, queue):
    sent = send_n(sb, queue, 6)
    got = []
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        for msg in receiver:
            got.append(body_of(msg))
            receiver.complete_message(msg)
    assert got == sent


def test_next_on_empty_iterator_raises_stop_iteration(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as receiver:
        with pytest.raises(StopIteration):
            next(receiver)


def test_receive_max_message_count_limits_the_batch(sb, queue):
    send_n(sb, queue, 6)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        first = receiver.receive_messages(max_message_count=2, max_wait_time=3)
        assert 1 <= len(first) <= 2
        for m in first:
            receiver.complete_message(m)
        rest = drain(receiver)
        assert len(first) + len(rest) == 6


def test_receive_single_message_by_default(sb, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        first = receiver.receive_messages(max_wait_time=3)
        assert len(first) == 1
        receiver.complete_message(first[0])
        assert len(drain(receiver)) == 2


def test_messages_are_received_in_send_order(sb, queue):
    sent = send_n(sb, queue, 25)
    assert drain_bodies(sb, queue) == sent


def test_order_is_kept_across_separate_sends_and_senders(sb, queue):
    with sb.get_queue_sender(queue) as s1:
        s1.send_messages(ServiceBusMessage("a"))
    with sb.get_queue_sender(queue) as s2:
        s2.send_messages(ServiceBusMessage("b"))
    with sb.get_queue_sender(queue) as s3:
        s3.send_messages(ServiceBusMessage("c"))
    assert drain_bodies(sb, queue) == ["a", "b", "c"]


def test_sequence_numbers_increase_by_one_and_start_at_one(sb, queue):
    # Microsoft's test_queue_by_queue_client_peek_auto_increment: the first message of a fresh queue has number 1.
    send_n(sb, queue, 4)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        msgs = receiver.peek_messages(max_message_count=4)
    assert [m.sequence_number for m in msgs] == [1, 2, 3, 4]


def test_peek_returns_messages_without_removing_or_locking_them(sb, queue):
    sent = send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        peeked = receiver.peek_messages(max_message_count=3)
        assert bodies(peeked) == sent
        # peeked messages are not locked, so another receiver can lock and complete them right away
        with sb.get_queue_receiver(queue, max_wait_time=2) as other:
            got = receive_n(other, 3)
            assert bodies(got) == sent


def test_peek_messages_cannot_be_settled(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        (peeked,) = receiver.peek_messages()
        with pytest.raises(ValueError):
            receiver.complete_message(peeked)
        with pytest.raises(ValueError):
            receiver.abandon_message(peeked)


def test_peek_on_empty_queue_returns_empty_list(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as receiver:
        assert receiver.peek_messages(max_message_count=10) == []


def test_peek_count_limits_result(sb, queue):
    sent = send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        assert bodies(receiver.peek_messages(max_message_count=3)) == sent[:3]


def test_peek_advances_on_each_call(sb, queue):
    # peek_messages remembers the last peeked sequence number and continues after it
    sent = send_n(sb, queue, 4)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        assert bodies(receiver.peek_messages(max_message_count=2)) == sent[:2]
        assert bodies(receiver.peek_messages(max_message_count=2)) == sent[2:]
        assert receiver.peek_messages(max_message_count=2) == []


def test_peek_from_sequence_number(sb, queue):
    sent = send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        all_msgs = receiver.peek_messages(max_message_count=5, sequence_number=0)
        third = all_msgs[2].sequence_number
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        from_third = receiver.peek_messages(max_message_count=10, sequence_number=third)
        assert bodies(from_third) == sent[2:]


def test_peek_after_receive_starts_after_last_received(sb, queue):
    # Microsoft's test_queue_by_queue_client_peek_and_receive
    sent = send_n(sb, queue, 4)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        got = receive_n(receiver, 3)
        assert len(got) == 3
        nxt = receiver.peek_messages(max_message_count=2)
        assert bodies(nxt) == sent[3:]
        assert nxt[0].sequence_number == got[-1].sequence_number + 1


def test_peeked_message_properties_are_set(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(
            ServiceBusMessage(
                "Test message",
                application_properties={"key": "value"},
                subject="label",
                content_type="application/text",
                correlation_id="cid",
                message_id="mid",
                to="to",
                reply_to="reply_to",
            )
        )
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        (m,) = receiver.peek_messages(1)
    assert body_of(m) == "Test message"
    assert m.application_properties[b"key"] == b"value"
    assert (m.subject, m.content_type, m.correlation_id) == ("label", "application/text", "cid")
    assert (m.message_id, m.to, m.reply_to) == ("mid", "to", "reply_to")


def test_receive_and_delete_removes_messages_at_receive_time(sb, queue):
    sent = send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, receive_mode=RECEIVE_AND_DELETE, max_wait_time=2) as receiver:
        got = []
        while len(got) < 5:
            batch = receiver.receive_messages(max_message_count=5, max_wait_time=2)
            assert batch, "queue ran dry before all messages were received"
            got.extend(batch)
        assert bodies(got) == sent
    assert drain_bodies(sb, queue) == []


def test_receive_and_delete_messages_cannot_be_settled(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, receive_mode=RECEIVE_AND_DELETE, max_wait_time=2) as receiver:
        (m,) = receive_n(receiver, 1, settle=None)
        with pytest.raises(ValueError):
            receiver.complete_message(m)
        with pytest.raises(ValueError):
            receiver.abandon_message(m)
        with pytest.raises(ValueError):
            receiver.dead_letter_message(m)


def test_receive_and_delete_mode_given_as_string(sb, queue):
    send_n(sb, queue, 2)
    with sb.get_queue_receiver(queue, receive_mode="receiveanddelete", max_wait_time=2) as receiver:
        assert len(receive_n(receiver, 2, settle=None)) == 2
    assert drain_bodies(sb, queue) == []


def test_receive_and_delete_peek_does_not_see_received_messages(sb, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, receive_mode=RECEIVE_AND_DELETE, max_wait_time=2) as receiver:
        assert len(receiver.peek_messages(max_message_count=10)) == 3
        receive_n(receiver, 3, settle=None)
        assert receiver.peek_messages(max_message_count=10) == []


def test_invalid_receive_mode_raises_value_error(sb, queue):
    with pytest.raises(ValueError):
        sb.get_queue_receiver(queue, receive_mode="notamode")


def test_max_wait_time_zero_is_rejected_by_receiver(sb, queue):
    with pytest.raises(ValueError):
        sb.get_queue_receiver(queue, max_wait_time=0)


def test_competing_receivers_never_get_the_same_locked_message(sb, queue):
    sent = send_n(sb, queue, 6)
    with sb.get_queue_receiver(queue, max_wait_time=2) as a, sb.get_queue_receiver(queue, max_wait_time=2) as b:
        got_a = receive_n(a, 3, settle=None)
        got_b = receive_n(b, 3, settle=None)
        assert len(got_a) == 3 and len(got_b) == 3
        assert set(bodies(got_a)).isdisjoint(bodies(got_b))
        assert sorted(bodies(got_a) + bodies(got_b)) == sorted(sent)
        for m in got_a:
            a.complete_message(m)
        for m in got_b:
            b.complete_message(m)


def test_locked_message_is_invisible_to_other_receivers(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as a, sb.get_queue_receiver(queue, max_wait_time=1) as b:
        (m,) = receive_n(a, 1, settle=None)
        assert b.receive_messages(max_message_count=1, max_wait_time=1) == []
        a.complete_message(m)


def test_prefetch_count_receiver_gets_all_messages(sb, queue):
    sent = send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, prefetch_count=5, max_wait_time=2) as receiver:
        got = receive_n(receiver, 5)
        assert bodies(got) == sent


def test_prefetch_count_zero_is_the_default(sb, queue):
    sent = send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, prefetch_count=0, max_wait_time=2) as receiver:
        assert bodies(receive_n(receiver, 3)) == sent


def test_receiver_created_before_messages_are_sent_gets_them(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=3) as receiver:
        assert receiver.receive_messages(max_message_count=1, max_wait_time=1) == []
        send_n(sb, queue, 2)
        assert len(receive_n(receiver, 2)) == 2


def test_same_sender_reused_for_many_sends(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        for i in range(40):
            sender.send_messages(ServiceBusMessage(f"r-{i}"))
    assert drain_bodies(sb, queue) == [f"r-{i}" for i in range(40)]


def test_sender_can_not_send_after_close(sb, queue):
    sender = sb.get_queue_sender(queue)
    sender.send_messages(ServiceBusMessage("one"))
    sender.close()
    with pytest.raises(ValueError):
        sender.send_messages(ServiceBusMessage("two"))
    assert drain_bodies(sb, queue) == ["one"]


def test_receiver_can_not_receive_after_close(sb, queue):
    receiver = sb.get_queue_receiver(queue, max_wait_time=1)
    receiver.close()
    with pytest.raises(ValueError):
        receiver.receive_messages()
    with pytest.raises(ValueError):
        receiver.peek_messages()


def test_complete_removes_message_for_good(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        (m,) = receive_n(receiver, 1, settle=None)
        receiver.complete_message(m)
        assert receiver.receive_messages(max_message_count=1, max_wait_time=1) == []
        assert receiver.peek_messages() == []


def test_send_message_with_empty_string_body(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(ServiceBusMessage(""))
    with sb.get_queue_receiver(queue, max_wait_time=2) as receiver:
        (m,) = receive_n(receiver, 1)
        assert body_of(m) == ""


def test_send_message_with_no_body_is_received_with_body_none(sb, queue):
    # Microsoft's test_send_message_no_body
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(ServiceBusMessage(body=None))
    with sb.get_queue_receiver(queue, max_wait_time=3) as receiver:
        (m,) = receive_n(receiver, 1, max_wait=3)
        assert m.body is None


@pytest.mark.no_autocreate
@pytest.mark.emulator_only
def test_send_to_missing_queue_raises_entity_not_found(sb):
    # Real Service Bus: attach to a queue that does not exist is refused with amqp:not-found, and so is the emulator
    # by default (skipped when it is started with AZEMU_SERVICEBUS_AUTOCREATE=1 / AZEMU_LENIENT=1).
    with pytest.raises(MessagingEntityNotFoundError):
        with sb.get_queue_sender("does-not-exist-q") as sender:
            sender.send_messages(ServiceBusMessage("x"))


@pytest.mark.no_autocreate
@pytest.mark.emulator_only
def test_receive_from_missing_queue_raises_entity_not_found(sb):
    with pytest.raises(MessagingEntityNotFoundError):
        with sb.get_queue_receiver("does-not-exist-q", max_wait_time=1) as receiver:
            receiver.receive_messages(max_wait_time=1)


@pytest.mark.no_autocreate
@pytest.mark.emulator_only
def test_send_to_deleted_queue_raises_entity_not_found(sb, admin, new_queue):
    name = new_queue()
    admin.delete_queue(name)
    with pytest.raises(MessagingEntityNotFoundError):
        with sb.get_queue_sender(name) as sender:
            sender.send_messages(ServiceBusMessage("x"))
