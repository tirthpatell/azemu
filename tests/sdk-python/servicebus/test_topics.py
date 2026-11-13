"""Topics and subscriptions: fan-out, settlement on subscriptions, subscription DLQ, sessions, errors."""

from datetime import timedelta

import pytest
from azure.servicebus import NEXT_AVAILABLE_SESSION, ServiceBusMessage, ServiceBusMessageState
from azure.servicebus.exceptions import ServiceBusError

from sbutil import DLQ, RECEIVE_AND_DELETE, bodies, body_of, drain_sub, receive_n, send_n, utcnow


def sub_receiver(sb, topic, sub, **kw):
    kw.setdefault("max_wait_time", 2)
    return sb.get_subscription_receiver(topic, sub, **kw)


def test_message_sent_to_topic_arrives_in_subscription(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 1, prefix="t", topic=True)
    with sub_receiver(sb, topic, sub) as r:
        (m,) = receive_n(r, 1)
        assert body_of(m) == "t-0"


def test_default_rule_delivers_every_message(sb, topic_sub):
    topic, sub = topic_sub
    sent = send_n(sb, topic, 8, topic=True)
    with sub_receiver(sb, topic, sub) as r:
        assert bodies(receive_n(r, 8)) == sent


def test_each_subscription_gets_its_own_copy_of_every_message(sb, topic, new_sub):
    subs = [new_sub(topic) for _ in range(3)]
    sent = send_n(sb, topic, 4, topic=True)
    for sub in subs:
        with sub_receiver(sb, topic, sub) as r:
            assert bodies(receive_n(r, 4)) == sent


def test_completing_in_one_subscription_does_not_affect_another(sb, topic, new_sub):
    s1, s2 = new_sub(topic), new_sub(topic)
    send_n(sb, topic, 2, topic=True)
    assert len(drain_sub(sb, topic, s1)) == 2
    assert len(drain_sub(sb, topic, s1)) == 0
    assert len(drain_sub(sb, topic, s2)) == 2


def test_subscription_does_not_get_messages_sent_before_it_existed(sb, topic, new_sub):
    first = new_sub(topic)
    send_n(sb, topic, 2, prefix="early", topic=True)
    late = new_sub(topic)
    send_n(sb, topic, 1, prefix="late", topic=True)
    assert bodies(drain_sub(sb, topic, late)) == ["late-0"]
    assert bodies(drain_sub(sb, topic, first)) == ["early-0", "early-1", "late-0"]


def test_message_to_topic_without_subscriptions_is_dropped(sb, topic, new_sub):
    send_n(sb, topic, 2, prefix="dropped", topic=True)
    sub = new_sub(topic)
    assert drain_sub(sb, topic, sub) == []


def test_topic_batch_send_reaches_subscription_in_order(sb, topic_sub):
    topic, sub = topic_sub
    with sb.get_topic_sender(topic) as s:
        batch = s.create_message_batch()
        for i in range(5):
            batch.add_message(ServiceBusMessage(f"b{i}"))
        s.send_messages(batch)
    with sub_receiver(sb, topic, sub) as r:
        assert bodies(receive_n(r, 5)) == [f"b{i}" for i in range(5)]


def test_topic_message_properties_round_trip(sb, topic_sub):
    topic, sub = topic_sub
    with sb.get_topic_sender(topic) as s:
        s.send_messages(
            ServiceBusMessage(
                "x",
                application_properties={"k": 7},
                subject="subj",
                correlation_id="c",
                message_id="mid",
                content_type="ct",
                to="to",
                reply_to="rt",
                time_to_live=timedelta(seconds=120),
            )
        )
    with sub_receiver(sb, topic, sub) as r:
        (m,) = receive_n(r, 1)
        assert m.application_properties[b"k"] == 7
        assert (m.subject, m.correlation_id, m.message_id) == ("subj", "c", "mid")
        assert (m.content_type, m.to, m.reply_to) == ("ct", "to", "rt")
        assert m.time_to_live == timedelta(seconds=120)


def test_subscription_peek_does_not_remove_messages(sb, topic_sub):
    topic, sub = topic_sub
    sent = send_n(sb, topic, 3, topic=True)
    with sub_receiver(sb, topic, sub) as r:
        assert bodies(r.peek_messages(max_message_count=5)) == sent
        assert bodies(receive_n(r, 3)) == sent


def test_subscription_sequence_numbers_increase(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 4, topic=True)
    with sub_receiver(sb, topic, sub) as r:
        seqs = [m.sequence_number for m in receive_n(r, 4)]
    assert seqs == sorted(set(seqs)) and seqs[0] > 0


def test_subscription_abandon_increments_delivery_count(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 1, topic=True)
    with sub_receiver(sb, topic, sub, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle="abandon", max_wait=3)
        (again,) = receive_n(r, 1, settle="complete", max_wait=3)
        assert (m.delivery_count, again.delivery_count) == (0, 1)


def test_subscription_dead_letter_goes_to_subscription_dlq(sb, topic_sub):
    # Microsoft's test_subscription_by_servicebus_client_receive_batch_with_deadletter
    topic, sub = topic_sub
    send_n(sb, topic, 3, prefix="dl", topic=True)
    with sub_receiver(sb, topic, sub) as r:
        for m in receive_n(r, 3, settle=None):
            r.dead_letter_message(m, reason="Testing reason", error_description="Testing description")
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
    with sub_receiver(sb, topic, sub, sub_queue=DLQ, max_wait_time=3) as dlq:
        got = receive_n(dlq, 3, max_wait=3)
        assert bodies(got) == ["dl-0", "dl-1", "dl-2"]
        for m in got:
            assert m.dead_letter_reason == "Testing reason"
            assert m.dead_letter_error_description == "Testing description"
            assert m.application_properties[b"DeadLetterReason"] == b"Testing reason"


def test_dlq_of_one_subscription_is_separate_from_another(sb, topic, new_sub):
    s1, s2 = new_sub(topic), new_sub(topic)
    send_n(sb, topic, 1, topic=True)
    with sub_receiver(sb, topic, s1) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    assert len(drain_sub(sb, topic, s1, sub_queue=DLQ, max_wait=2)) == 1
    assert drain_sub(sb, topic, s2, sub_queue=DLQ, max_wait=1) == []
    assert len(drain_sub(sb, topic, s2)) == 1


def test_subscription_max_delivery_count_dead_letters(sb, topic, new_sub):
    sub = new_sub(topic, max_delivery_count=2)
    send_n(sb, topic, 1, prefix="poison", topic=True)
    with sub_receiver(sb, topic, sub, max_wait_time=3) as r:
        for _ in range(2):
            receive_n(r, 1, settle="abandon", max_wait=3)
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
    with sub_receiver(sb, topic, sub, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, max_wait=3)
        assert d.dead_letter_reason == "MaxDeliveryCountExceeded"


def test_subscription_defer_and_receive_deferred(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 1, prefix="df", topic=True)
    with sub_receiver(sb, topic, sub) as r:
        (m,) = receive_n(r, 1, settle="defer")
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
        (d,) = r.receive_deferred_messages([m.sequence_number])
        assert body_of(d) == "df-0" and d.state == ServiceBusMessageState.DEFERRED
        r.complete_message(d)


def test_subscription_receive_and_delete(sb, topic_sub):
    topic, sub = topic_sub
    send_n(sb, topic, 3, topic=True)
    with sub_receiver(sb, topic, sub, receive_mode=RECEIVE_AND_DELETE) as r:
        assert len(receive_n(r, 3, settle=None)) == 3
    assert drain_sub(sb, topic, sub) == []


def test_two_receivers_on_one_subscription_compete(sb, topic_sub):
    topic, sub = topic_sub
    sent = send_n(sb, topic, 6, topic=True)
    with sub_receiver(sb, topic, sub) as a, sub_receiver(sb, topic, sub) as b:
        got_a = receive_n(a, 3, settle=None)
        got_b = receive_n(b, 3, settle=None)
        assert sorted(bodies(got_a) + bodies(got_b)) == sorted(sent)
        assert set(bodies(got_a)).isdisjoint(bodies(got_b))
        for m in got_a:
            a.complete_message(m)
        for m in got_b:
            b.complete_message(m)


def test_scheduled_topic_message_is_not_received_or_peeked_before_its_time(sb, topic_sub):
    # docs (message-browsing): peek returns scheduled messages for queues but not for subscriptions
    topic, sub = topic_sub
    with sb.get_topic_sender(topic) as s:
        s.schedule_messages(ServiceBusMessage("later"), (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
    with sub_receiver(sb, topic, sub) as r:
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
        assert r.peek_messages() == []


@pytest.mark.slow
def test_scheduled_topic_message_arrives_in_subscription_after_its_time(sb, topic_sub):
    topic, sub = topic_sub
    with sb.get_topic_sender(topic) as s:
        s.schedule_messages(ServiceBusMessage("soon"), (utcnow() + timedelta(seconds=3)).replace(microsecond=0))
    with sub_receiver(sb, topic, sub, max_wait_time=8) as r:
        (m,) = receive_n(r, 1, max_wait=8)
        assert body_of(m) == "soon"


def test_cancel_scheduled_topic_message(sb, topic_sub):
    topic, sub = topic_sub
    with sb.get_topic_sender(topic) as s:
        seqs = s.schedule_messages(ServiceBusMessage("never"), (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
        s.cancel_scheduled_messages(seqs)
    assert drain_sub(sb, topic, sub) == []


# -- sessions on subscriptions ------------------------------------------------------------------
def test_session_subscription_receives_by_session_id(sb, topic, new_sub):
    sub = new_sub(topic, requires_session=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages([ServiceBusMessage(f"a{i}", session_id="A") for i in range(2)])
        s.send_messages([ServiceBusMessage(f"b{i}", session_id="B") for i in range(2)])
    with sub_receiver(sb, topic, sub, session_id="B") as r:
        assert bodies(receive_n(r, 2)) == ["b0", "b1"]
        assert r.session.session_id == "B"
    with sub_receiver(sb, topic, sub, session_id="A") as r:
        assert bodies(receive_n(r, 2)) == ["a0", "a1"]


def test_session_subscription_next_available_session(sb, topic, new_sub):
    sub = new_sub(topic, requires_session=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("x", session_id="only"))
    with sb.get_subscription_receiver(topic, sub, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r:
        assert r.session.session_id == "only"


def test_session_subscription_requires_session_id_on_receive(sb, topic, new_sub):
    sub = new_sub(topic, requires_session=True)
    with pytest.raises(ServiceBusError):
        with sub_receiver(sb, topic, sub) as r:
            r.receive_messages(max_wait_time=1)


def test_session_state_on_subscription(sb, topic, new_sub):
    sub = new_sub(topic, requires_session=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("x", session_id="S"))
    with sub_receiver(sb, topic, sub, session_id="S") as r:
        assert r.session.get_state() is None
        r.session.set_state(b"state")
        assert r.session.get_state() == b"state"


def test_list_subscription_sessions(sb, topic, new_sub):
    sub = new_sub(topic, requires_session=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages([ServiceBusMessage("x", session_id="S1"), ServiceBusMessage("y", session_id="S2")])
    assert set(sb.list_subscription_sessions(topic, sub)) == {"S1", "S2"}


# -- errors -------------------------------------------------------------------------------------
def test_sending_to_a_subscription_path_is_an_error(sb, topic_sub):
    topic, sub = topic_sub
    with pytest.raises(ServiceBusError):
        with sb.get_queue_sender(f"{topic}/Subscriptions/{sub}") as s:
            s.send_messages(ServiceBusMessage("x"))


def test_receiving_from_a_topic_name_is_an_error(sb, topic):
    with pytest.raises(ServiceBusError):
        with sb.get_queue_receiver(topic, max_wait_time=1) as r:
            r.receive_messages(max_wait_time=1)


@pytest.mark.no_autocreate
@pytest.mark.emulator_only
def test_send_to_missing_topic_raises_entity_not_found(sb):
    from azure.servicebus.exceptions import MessagingEntityNotFoundError

    with pytest.raises(MessagingEntityNotFoundError):
        with sb.get_topic_sender("no-such-topic") as s:
            s.send_messages(ServiceBusMessage("x"))


@pytest.mark.no_autocreate
@pytest.mark.emulator_only
def test_receive_from_missing_subscription_raises_entity_not_found(sb, topic):
    from azure.servicebus.exceptions import MessagingEntityNotFoundError

    with pytest.raises(MessagingEntityNotFoundError):
        with sb.get_subscription_receiver(topic, "no-such-sub", max_wait_time=1) as r:
            r.receive_messages(max_wait_time=1)
