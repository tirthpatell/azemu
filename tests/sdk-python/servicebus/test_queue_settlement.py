"""Queues: settlement (complete / abandon / defer / dead-letter), deferred messages, delivery count, DLQ.

Expected values: Microsoft's test_queues.py (iter_messages_with_abandon / _with_defer /
_with_retrieve_deferred_* / receive_batch_with_deadletter), the dead-letter and deferral docs.
"""

import pytest
from azure.servicebus import ServiceBusMessage, ServiceBusMessageState
from azure.servicebus.exceptions import MessageAlreadySettled, ServiceBusError

from sbutil import DLQ, RECEIVE_AND_DELETE, bodies, body_of, drain, drain_bodies, receive_n, send_n


# -- complete ---------------------------------------------------------------------------------
def test_complete_twice_raises_message_already_settled(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="complete")
        with pytest.raises(MessageAlreadySettled):
            r.complete_message(m)


def test_every_settle_call_after_complete_raises_message_already_settled(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="complete")
        for call in (r.abandon_message, r.defer_message, r.dead_letter_message):
            with pytest.raises(MessageAlreadySettled):
                call(m)
        with pytest.raises(MessageAlreadySettled):
            r.renew_message_lock(m)


def test_message_already_settled_is_a_value_error():
    assert issubclass(MessageAlreadySettled, ValueError)


def test_settle_with_wrong_type_raises_type_error(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        with pytest.raises(TypeError):
            r.complete_message(ServiceBusMessage("not a received message"))


def test_complete_after_receiver_closed_raises_value_error(sb, queue):
    # Microsoft's test_queue_message_connection_closed
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        msgs = r.receive_messages(max_message_count=1, max_wait_time=3)
        assert len(msgs) == 1
    with pytest.raises(ValueError):
        r.complete_message(msgs[0])


# -- abandon / delivery count -----------------------------------------------------------------
def test_new_message_has_delivery_count_zero(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1)
        assert m.delivery_count == 0


def test_abandon_makes_message_available_again_with_delivery_count_one(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle="abandon")
        assert m.delivery_count == 0
        (again,) = receive_n(r, 1, settle=None, max_wait=3)
        assert again.delivery_count == 1
        assert body_of(again) == body_of(m)
        assert again.sequence_number == m.sequence_number
        r.complete_message(again)


def test_delivery_count_increments_on_each_abandon(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        counts = []
        for _ in range(4):
            (m,) = receive_n(r, 1, settle="abandon", max_wait=3)
            counts.append(m.delivery_count)
    assert counts == [0, 1, 2, 3]


def test_abandon_returns_message_immediately_not_after_lock_expiry(sb, queue):
    # the default lock is 60 s; getting the message back within the 3 s wait proves it was released at once
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        receive_n(r, 1, settle="abandon", max_wait=3)
        assert len(r.receive_messages(max_message_count=1, max_wait_time=3)) == 1


def test_abandoned_message_keeps_its_queue_position_and_properties(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(
            ServiceBusMessage("first", application_properties={"k": "v"}, subject="sub", message_id="id-1")
        )
        s.send_messages(ServiceBusMessage("second"))
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle="abandon", max_wait=3)
        assert body_of(m) == "first"
        got = r.receive_messages(max_message_count=1, max_wait_time=3)
        assert body_of(got[0]) == "first"
        assert got[0].application_properties[b"k"] == b"v"
        assert (got[0].subject, got[0].message_id) == ("sub", "id-1")
        r.complete_message(got[0])


def test_abandon_after_abandon_raises_already_settled(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="abandon")
        with pytest.raises(MessageAlreadySettled):
            r.abandon_message(m)


def test_unsettled_message_stays_locked_after_receiver_closes(sb, queue):
    # docs (dead-letter queues, "Settle messages before you close the receiver"): closing the receiver does not
    # release a delivered message; it stays locked until the lock expires
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        receive_n(r, 1, settle=None)
    with sb.get_queue_receiver(queue, max_wait_time=1) as other:
        assert other.receive_messages(max_message_count=1, max_wait_time=1) == []


# -- dead-letter ------------------------------------------------------------------------------
def test_dead_letter_moves_message_to_dlq_with_reason_and_description(sb, queue):
    send_n(sb, queue, 1, prefix="dl")
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="Testing reason", error_description="Testing description")
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, settle=None, max_wait=3)
        assert body_of(d) == "dl-0"
        assert d.dead_letter_reason == "Testing reason"
        assert d.dead_letter_error_description == "Testing description"
        assert d.application_properties[b"DeadLetterReason"] == b"Testing reason"
        assert d.application_properties[b"DeadLetterErrorDescription"] == b"Testing description"
        dlq.complete_message(d)


def test_dead_letter_without_reason_still_moves_message(sb, queue):
    send_n(sb, queue, 1, prefix="dl")
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    assert drain_bodies(sb, queue) == []
    assert drain_bodies(sb, queue, sub_queue=DLQ, max_wait=3) == ["dl-0"]


def test_dead_letter_many_messages_all_arrive_in_dlq_in_order(sb, queue):
    sent = send_n(sb, queue, 8)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        for m in receive_n(r, 8, settle=None):
            r.dead_letter_message(m, reason="r", error_description="d")
    assert drain_bodies(sb, queue) == []
    assert drain_bodies(sb, queue, sub_queue=DLQ, max_wait=2) == sent


def test_dlq_message_can_be_completed_and_is_then_gone(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="x")
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=2) as dlq:
        assert len(receive_n(dlq, 1, max_wait=3)) == 1
        assert dlq.receive_messages(max_message_count=1, max_wait_time=1) == []


def test_dlq_message_can_be_abandoned_and_redelivered(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="x")
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        receive_n(dlq, 1, settle="abandon", max_wait=3)
        (again,) = receive_n(dlq, 1, settle="complete", max_wait=3)
        assert again.delivery_count == 1


def test_dlq_can_be_peeked(sb, queue):
    send_n(sb, queue, 2)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        for m in receive_n(r, 2, settle=None):
            r.dead_letter_message(m, reason="peekme")
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=2) as dlq:
        peeked = dlq.peek_messages(max_message_count=5)
        assert len(peeked) == 2
        assert {m.dead_letter_reason for m in peeked} == {"peekme"}


def test_dlq_resubmit_pattern_new_message_goes_back_to_queue(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("again", application_properties={"n": 1}))
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="bad")
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq, sb.get_queue_sender(queue) as s:
        (d,) = receive_n(dlq, 1, settle=None, max_wait=3)
        s.send_messages(ServiceBusMessage(body_of(d), application_properties={"n": 2}))
        dlq.complete_message(d)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (back,) = receive_n(r, 1, max_wait=3)
        assert body_of(back) == "again"
        assert back.application_properties[b"n"] == 2
        assert back.delivery_count == 0


def test_dlq_can_be_read_in_receive_and_delete_mode(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    with sb.get_queue_receiver(queue, sub_queue=DLQ, receive_mode=RECEIVE_AND_DELETE, max_wait_time=3) as dlq:
        assert len(receive_n(dlq, 1, settle=None, max_wait=3)) == 1
    assert drain_bodies(sb, queue, sub_queue=DLQ) == []


def test_dlq_sub_queue_given_as_string(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    with sb.get_queue_receiver(queue, sub_queue="deadletter", max_wait_time=3) as dlq:
        assert len(receive_n(dlq, 1, max_wait=3)) == 1


def test_max_delivery_count_moves_message_to_dlq(sb, new_queue):
    queue = new_queue(max_delivery_count=3)
    send_n(sb, queue, 1, prefix="poison")
    deliveries = []
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        for _ in range(3):
            (m,) = receive_n(r, 1, settle="abandon", max_wait=3)
            deliveries.append(m.delivery_count)
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
    assert deliveries == [0, 1, 2]
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, max_wait=3)
        assert body_of(d) == "poison-0"
        assert d.dead_letter_reason == "MaxDeliveryCountExceeded"


def test_max_delivery_count_dlq_description_mentions_delivery_attempts(sb, new_queue):
    # Real Service Bus: "Message could not be consumed after 2 delivery attempts."
    queue = new_queue(max_delivery_count=2)
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        for _ in range(2):
            receive_n(r, 1, settle="abandon", max_wait=3)
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, settle=None, max_wait=3)
        assert d.dead_letter_error_description == "Message could not be consumed after 2 delivery attempts."
        dlq.complete_message(d)


# -- defer ------------------------------------------------------------------------------------
def test_deferred_message_is_not_returned_by_normal_receive(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []


def test_receive_deferred_message_by_sequence_number(sb, queue):
    send_n(sb, queue, 1, prefix="df")
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        (d,) = r.receive_deferred_messages(m.sequence_number)
        assert body_of(d) == "df-0"
        assert d.sequence_number == m.sequence_number
        assert d.state == ServiceBusMessageState.DEFERRED
        assert d.lock_token
        assert d.locked_until_utc
        r.complete_message(d)


def test_receive_deferred_messages_by_list_of_sequence_numbers(sb, queue):
    sent = send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        seqs = [m.sequence_number for m in receive_n(r, 5, settle="defer")]
        deferred = r.receive_deferred_messages(seqs)
        assert sorted(bodies(deferred)) == sorted(sent)
        for d in deferred:
            r.complete_message(d)


def test_receive_deferred_from_a_different_receiver(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r1:
        (m,) = receive_n(r1, 1, settle="defer")
    with sb.get_queue_receiver(queue, max_wait_time=2) as r2:
        (d,) = r2.receive_deferred_messages([m.sequence_number])
        r2.complete_message(d)


def test_deferred_message_dead_lettered(sb, queue):
    send_n(sb, queue, 1, prefix="dd")
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        (d,) = r.receive_deferred_messages([m.sequence_number])
        r.dead_letter_message(d, reason="Testing reason", error_description="Testing description")
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (x,) = receive_n(dlq, 1, max_wait=3)
        assert (x.dead_letter_reason, x.dead_letter_error_description) == ("Testing reason", "Testing description")


def test_deferred_message_abandon_keeps_it_deferred(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        (d,) = r.receive_deferred_messages([m.sequence_number])
        r.abandon_message(d)
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
        (again,) = r.receive_deferred_messages([m.sequence_number])
        r.complete_message(again)


def test_deferred_message_lock_can_be_renewed(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        (d,) = r.receive_deferred_messages([m.sequence_number])
        new_until = r.renew_message_lock(d)
        assert new_until >= d.locked_until_utc
        r.complete_message(d)


def test_receive_deferred_after_complete_raises_service_bus_error(sb, queue):
    # Microsoft: receive_deferred_messages for completed (gone) messages -> ServiceBusError
    send_n(sb, queue, 2)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        seqs = [m.sequence_number for m in receive_n(r, 2, settle="defer")]
        for d in r.receive_deferred_messages(seqs):
            r.complete_message(d)
        with pytest.raises(ServiceBusError):
            r.receive_deferred_messages(seqs)


def test_receive_deferred_with_unknown_sequence_numbers_raises_service_bus_error(sb, queue):
    # Microsoft's ..._retrieve_deferred_not_found: sequence numbers 3,4 and 5,6,7 of a 3-message queue
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        for m in receive_n(r, 3, settle="defer"):
            pass
        with pytest.raises(ServiceBusError):
            r.receive_deferred_messages([3000, 4000])
        with pytest.raises(ServiceBusError):
            r.receive_deferred_messages([5000, 6000, 7000])


def test_receive_deferred_empty_list_returns_empty(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        assert r.receive_deferred_messages([]) == []


def test_receive_deferred_in_receive_and_delete_mode_deletes_them(sb, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        seqs = [m.sequence_number for m in receive_n(r, 3, settle="defer")]
    with sb.get_queue_receiver(queue, receive_mode=RECEIVE_AND_DELETE, max_wait_time=2) as r:
        deferred = r.receive_deferred_messages(seqs)
        assert len(deferred) == 3
        for d in deferred:
            with pytest.raises(ValueError):
                r.complete_message(d)
        with pytest.raises(ServiceBusError):
            r.receive_deferred_messages(seqs)


def test_deferring_first_message_lets_later_messages_flow(sb, queue):
    sent = send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        first = receive_n(r, 1, settle="defer")[0]
        rest = receive_n(r, 2)
        assert bodies(rest) == sent[1:]
        (d,) = r.receive_deferred_messages([first.sequence_number])
        assert body_of(d) == sent[0]
        r.complete_message(d)


def test_deferred_messages_are_visible_to_peek_with_state_deferred(sb, queue):
    # docs (message-browsing): Peek returns deferred messages
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="defer")
        # Without sequence_number the SDK peeks from after the last message this
        # receiver got, which would skip the deferred one.
        (p,) = r.peek_messages(max_message_count=5, sequence_number=1)
        assert p.sequence_number == m.sequence_number
        assert p.state == ServiceBusMessageState.DEFERRED


def test_active_message_state_is_active(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1)
        assert m.state == ServiceBusMessageState.ACTIVE


# -- misc ---------------------------------------------------------------------------------------
def test_settling_on_other_receiver_instance_works(sb, queue):
    # Microsoft's test_queue_complete_message_on_different_receiver (settled over the management link)
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r1, sb.get_queue_receiver(queue, max_wait_time=2) as r2:
        got = receive_n(r1, 3, settle=None)
        for m in got:
            r2.complete_message(m)
        assert r1.peek_messages() == []


def test_drain_helper_leaves_queue_empty(sb, queue):
    send_n(sb, queue, 5)
    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        assert len(drain(r)) == 5
        assert drain(r) == []
