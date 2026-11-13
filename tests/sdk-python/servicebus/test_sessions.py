"""Sessions on queues: send/receive by session id, NEXT_AVAILABLE_SESSION, session lock, session state.

Sources: Microsoft's test_sessions.py (real Service Bus), the message-sessions docs and the SDK source
(`_pyamqp_transport.on_attach`: the attach reply of a session receiver link must carry the session id in
`source.filters["com.microsoft:session-filter"]` and the lock expiry in properties["com.microsoft:locked-until-utc"]).
"""

import time
from datetime import timedelta

import pytest
from azure.servicebus import NEXT_AVAILABLE_SESSION, ServiceBusMessage, ServiceBusSessionFilter
from azure.servicebus.exceptions import (
    OperationTimeoutError,
    ServiceBusError,
    SessionCannotBeLockedError,
    SessionLockLostError,
)

from sbutil import DLQ, bodies, body_of, receive_n, utcnow


def send_session(sb, queue, session_id, n=3, prefix=None):
    prefix = prefix or session_id
    with sb.get_queue_sender(queue) as s:
        s.send_messages([ServiceBusMessage(f"{prefix}-{i}", session_id=session_id) for i in range(n)])
    return [f"{prefix}-{i}" for i in range(n)]


def test_send_and_receive_in_a_named_session(sb, session_queue):
    sent = send_session(sb, session_queue, "s1", 3)
    with sb.get_queue_receiver(session_queue, session_id="s1", max_wait_time=2) as r:
        got = receive_n(r, 3)
        assert bodies(got) == sent
        assert all(m.session_id == "s1" for m in got)


def test_messages_in_one_session_are_received_in_send_order(sb, session_queue):
    sent = send_session(sb, session_queue, "ordered", 15)
    with sb.get_queue_receiver(session_queue, session_id="ordered", max_wait_time=2) as r:
        assert bodies(receive_n(r, 15)) == sent


def test_two_sessions_are_isolated(sb, session_queue):
    a = send_session(sb, session_queue, "A", 3)
    b = send_session(sb, session_queue, "B", 4)
    with sb.get_queue_receiver(session_queue, session_id="B", max_wait_time=2) as rb:
        assert bodies(receive_n(rb, 4)) == b
        assert rb.receive_messages(max_message_count=1, max_wait_time=1) == []
    with sb.get_queue_receiver(session_queue, session_id="A", max_wait_time=2) as ra:
        assert bodies(receive_n(ra, 3)) == a


def test_interleaved_sends_keep_order_within_each_session(sb, session_queue):
    with sb.get_queue_sender(session_queue) as s:
        for i in range(4):
            s.send_messages(ServiceBusMessage(f"x{i}", session_id="X"))
            s.send_messages(ServiceBusMessage(f"y{i}", session_id="Y"))
    with sb.get_queue_receiver(session_queue, session_id="Y", max_wait_time=2) as r:
        assert bodies(receive_n(r, 4)) == [f"y{i}" for i in range(4)]
    with sb.get_queue_receiver(session_queue, session_id="X", max_wait_time=2) as r:
        assert bodies(receive_n(r, 4)) == [f"x{i}" for i in range(4)]


def test_session_receiver_iterator_stops_when_session_is_empty(sb, session_queue):
    send_session(sb, session_queue, "it", 4)
    count = 0
    with sb.get_queue_receiver(session_queue, session_id="it", max_wait_time=2) as r:
        for m in r:
            assert m.session_id == "it"
            r.complete_message(m)
            count += 1
    assert count == 4


def test_session_receiver_with_no_messages_returns_empty(sb, session_queue):
    # Microsoft's ..._with_inactive_session
    with sb.get_queue_receiver(session_queue, session_id="nobody", max_wait_time=2) as r:
        assert r.receive_messages(max_message_count=5, max_wait_time=2) == []
        assert r.session.session_id == "nobody"


def test_empty_string_is_a_valid_session_id(sb, session_queue):
    # Microsoft's ..._receive_handler_peeklock runs its whole scenario a second time with session_id=""
    with sb.get_queue_sender(session_queue) as s:
        s.send_messages(ServiceBusMessage("in-empty-session", session_id=""))
    with sb.get_queue_receiver(session_queue, session_id="", max_wait_time=3) as r:
        (m,) = receive_n(r, 1, max_wait=3)
        assert body_of(m) == "in-empty-session"
        assert m.session_id == ""


def test_session_ids_of_different_lengths(sb, session_queue):
    # Microsoft's test_session_id_str_bytes: sessions "a", "aa", "aaa" ...
    ids = ["a" * (i + 1) for i in range(5)]
    for sid in ids:
        send_session(sb, session_queue, sid, 1)
    for sid in ids:
        with sb.get_queue_receiver(session_queue, session_id=sid, max_wait_time=3) as r:
            got = receive_n(r, 1, max_wait=3)
            assert [m.session_id for m in got] == [sid]


def test_unicode_session_id(sb, session_queue):
    sid = "sessión-日本-✓"
    send_session(sb, session_queue, sid, 1)
    with sb.get_queue_receiver(session_queue, session_id=sid, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, max_wait=3)
        assert m.session_id == sid


def test_session_message_properties_round_trip(sb, session_queue):
    with sb.get_queue_sender(session_queue) as s:
        s.send_messages(
            ServiceBusMessage(
                "m",
                session_id="p",
                partition_key="p",
                application_properties={"key": "value"},
                subject="label",
                content_type="application/text",
                correlation_id="cid",
                message_id="mid",
                to="to",
                reply_to="reply_to",
                reply_to_session_id="reply_to_session_id",
            )
        )
    with sb.get_queue_receiver(session_queue, session_id="p", max_wait_time=2) as r:
        (m,) = receive_n(r, 1)
        assert m.application_properties[b"key"] == b"value"
        assert (m.subject, m.content_type, m.correlation_id) == ("label", "application/text", "cid")
        assert (m.message_id, m.to, m.reply_to) == ("mid", "to", "reply_to")
        assert m.reply_to_session_id == "reply_to_session_id"
        assert m.partition_key == "p"
        assert m.sequence_number and m.enqueued_time_utc
        assert m.delivery_count == 0


def test_partition_key_different_from_session_id_is_rejected_client_side():
    # Microsoft's test: message.partition_key = "pkey" with a session id set raises ValueError
    msg = ServiceBusMessage("m", session_id="sid")
    with pytest.raises(ValueError):
        msg.partition_key = "other"


# -- sessions vs non-session usage ------------------------------------------------------------
def test_sending_without_session_id_to_session_queue_fails(sb, session_queue):
    # Microsoft's test_session_non_session_send_to_session_queue_should_fail
    with sb.get_queue_sender(session_queue) as s:
        with pytest.raises(ServiceBusError):
            s.send_messages(ServiceBusMessage("no session"))


def test_receiving_without_session_id_from_session_queue_fails(sb, session_queue):
    # Microsoft: get_queue_receiver(session queue) without a session id -> ServiceBusError on open
    send_session(sb, session_queue, "s", 1)
    with pytest.raises(ServiceBusError):
        with sb.get_queue_receiver(session_queue, max_wait_time=2) as r:
            r.receive_messages(max_wait_time=2)


def test_receiving_with_session_id_from_non_session_queue_fails(sb, queue):
    # Microsoft's test_queue_by_servicebus_client_session_fail
    with pytest.raises(ServiceBusError):
        with sb.get_queue_receiver(queue, session_id="test", max_wait_time=2) as r:
            r.receive_messages(max_wait_time=2)


def test_non_session_receiver_has_no_session(sb, queue):
    with sb.get_queue_receiver(queue, max_wait_time=1) as r:
        assert r.session is None


def test_session_id_and_sub_queue_together_are_rejected_client_side(sb, session_queue):
    with pytest.raises(ValueError):
        sb.get_queue_receiver(session_queue, session_id="s", sub_queue=DLQ)


# -- NEXT_AVAILABLE_SESSION ---------------------------------------------------------------------
def test_next_available_session_picks_a_session_with_messages(sb, session_queue):
    sent = send_session(sb, session_queue, "only", 2)
    with sb.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r:
        assert r.session.session_id == "only"
        assert bodies(receive_n(r, 2)) == sent


def test_next_available_session_enum_value_is_accepted(sb, session_queue):
    send_session(sb, session_queue, "enum", 1)
    with sb.get_queue_receiver(session_queue, session_id=ServiceBusSessionFilter.NEXT_AVAILABLE, max_wait_time=3) as r:
        assert r.session.session_id == "enum"


def test_next_available_session_constant_is_the_filter_enum():
    assert NEXT_AVAILABLE_SESSION is ServiceBusSessionFilter.NEXT_AVAILABLE


def test_next_available_session_times_out_when_there_is_no_session(make_client, session_queue):
    # Microsoft's ..._with_no_session. The SDK sends com.microsoft:timeout (max_wait_time in ms) on the attach; the
    # broker answers with a detach carrying com.microsoft:timeout (or never attaches), which the SDK reports so.
    client = make_client(retry_total=0)
    start = time.monotonic()
    with pytest.raises(OperationTimeoutError):
        with client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=2):
            pass
    assert time.monotonic() - start < 20


def test_next_available_session_does_not_pick_empty_session_after_drain(make_client, session_queue):
    client = make_client(retry_total=0)
    send_session(client, session_queue, "gone", 1)
    with client.get_queue_receiver(session_queue, session_id="gone", max_wait_time=2) as r:
        receive_n(r, 1)
    with pytest.raises(OperationTimeoutError):
        with client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=2):
            pass


def test_two_next_available_receivers_get_different_sessions(sb, session_queue):
    send_session(sb, session_queue, "one", 1)
    send_session(sb, session_queue, "two", 1)
    with sb.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r1:
        with sb.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r2:
            assert {r1.session.session_id, r2.session.session_id} == {"one", "two"}


def test_next_available_session_moves_on_to_the_next_session(sb, session_queue):
    send_session(sb, session_queue, "first", 2)
    send_session(sb, session_queue, "second", 2)
    seen = []
    for _ in range(2):
        with sb.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r:
            seen.append(r.session.session_id)
            assert len(receive_n(r, 2)) == 2
    assert sorted(seen) == ["first", "second"]


# -- session lock -----------------------------------------------------------------------------
def test_session_has_a_lock_expiry_in_the_future(sb, new_queue):
    queue = new_queue(requires_session=True, lock_duration=timedelta(seconds=30))
    send_session(sb, queue, "lock", 1)
    with sb.get_queue_receiver(queue, session_id="lock", max_wait_time=2) as r:
        until = r.session.locked_until_utc
        assert until is not None
        remaining = (until - utcnow()).total_seconds()
        assert 15 <= remaining <= 32


def test_session_lock_renew_moves_expiry_forward(sb, new_queue):
    queue = new_queue(requires_session=True, lock_duration=timedelta(seconds=20))
    send_session(sb, queue, "renew", 1)
    with sb.get_queue_receiver(queue, session_id="renew", max_wait_time=2) as r:
        first = r.session.locked_until_utc
        time.sleep(2)
        new = r.session.renew_lock()
        assert new > first
        assert r.session.locked_until_utc == new


def test_second_receiver_on_a_locked_session_is_refused(sb, session_queue):
    send_session(sb, session_queue, "busy", 2)
    with sb.get_queue_receiver(session_queue, session_id="busy", max_wait_time=2) as first:
        receive_n(first, 1, settle=None)
        with pytest.raises(SessionCannotBeLockedError):
            with sb.get_queue_receiver(session_queue, session_id="busy", max_wait_time=2) as second:
                second.receive_messages(max_wait_time=1)


def test_session_can_be_locked_again_after_first_receiver_closed(sb, session_queue):
    send_session(sb, session_queue, "again", 2)
    with sb.get_queue_receiver(session_queue, session_id="again", max_wait_time=2) as first:
        receive_n(first, 1)
    with sb.get_queue_receiver(session_queue, session_id="again", max_wait_time=2) as second:
        assert len(receive_n(second, 1)) == 1


def test_different_sessions_can_be_locked_at_the_same_time(sb, session_queue):
    send_session(sb, session_queue, "p", 1)
    send_session(sb, session_queue, "q", 1)
    with sb.get_queue_receiver(session_queue, session_id="p", max_wait_time=2) as rp:
        with sb.get_queue_receiver(session_queue, session_id="q", max_wait_time=2) as rq:
            assert len(receive_n(rp, 1)) == 1
            assert len(receive_n(rq, 1)) == 1


@pytest.mark.slow
def test_settling_after_session_lock_expired_raises_session_lock_lost(sb, new_queue):
    # Microsoft's test_session_message_connection_closed: session lock = the queue's lock duration
    queue = new_queue(requires_session=True, lock_duration=timedelta(seconds=5))
    send_session(sb, queue, "expire", 2)
    with sb.get_queue_receiver(queue, session_id="expire", max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        time.sleep(max(0.0, (r.session.locked_until_utc - utcnow()).total_seconds()) + 1)
        with pytest.raises(SessionLockLostError):
            r.complete_message(m)


# -- settlement inside a session ----------------------------------------------------------------
def test_abandon_in_session_redelivers_in_same_session(sb, session_queue):
    send_session(sb, session_queue, "ab", 1)
    with sb.get_queue_receiver(session_queue, session_id="ab", max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle="abandon")
        (again,) = receive_n(r, 1, settle=None)
        assert again.delivery_count == m.delivery_count + 1
        assert again.session_id == "ab"
        r.complete_message(again)


def test_dead_letter_in_session_goes_to_dlq(sb, session_queue):
    # Microsoft's test_session_by_servicebus_client_receive_with_retrieve_deadletter: the DLQ needs no session id
    send_session(sb, session_queue, "dl", 3)
    with sb.get_queue_receiver(session_queue, session_id="dl", max_wait_time=2) as r:
        for m in receive_n(r, 3, settle=None):
            r.dead_letter_message(m, reason="Testing reason", error_description="Testing description")
    with sb.get_queue_receiver(session_queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        got = receive_n(dlq, 3, settle="complete", max_wait=3)
        assert len(got) == 3
        assert {(m.dead_letter_reason, m.dead_letter_error_description) for m in got} == {
            ("Testing reason", "Testing description")
        }
        assert {m.session_id for m in got} == {"dl"}


def test_defer_and_receive_deferred_in_session(sb, session_queue):
    # Microsoft's test_session_by_servicebus_client_iter_messages_with_retrieve_deferred_client
    send_session(sb, session_queue, "df", 3)
    with sb.get_queue_receiver(session_queue, session_id="df", max_wait_time=2) as r:
        seqs = [m.sequence_number for m in receive_n(r, 3, settle="defer")]
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
        deferred = r.receive_deferred_messages(seqs)
        assert sorted(bodies(deferred)) == ["df-0", "df-1", "df-2"]
        for d in deferred:
            r.complete_message(d)


def test_complete_removes_message_from_session(sb, session_queue):
    send_session(sb, session_queue, "cp", 2)
    with sb.get_queue_receiver(session_queue, session_id="cp", max_wait_time=2) as r:
        receive_n(r, 2)
    with sb.get_queue_receiver(session_queue, session_id="cp", max_wait_time=1) as r:
        assert r.receive_messages(max_wait_time=1) == []


def test_receive_and_delete_in_session(sb, session_queue):
    send_session(sb, session_queue, "rd", 3)
    with sb.get_queue_receiver(
        session_queue, session_id="rd", receive_mode="receiveanddelete", max_wait_time=2
    ) as r:
        assert len(receive_n(r, 3, settle=None)) == 3
    with sb.get_queue_receiver(session_queue, session_id="rd", max_wait_time=1) as r:
        assert r.receive_messages(max_wait_time=1) == []


def test_peek_in_session_shows_only_that_session(sb, session_queue):
    # Microsoft's test_session_by_servicebus_client_browse_messages_with_receiver
    send_session(sb, session_queue, "pk", 3)
    send_session(sb, session_queue, "other", 2)
    with sb.get_queue_receiver(session_queue, session_id="pk", max_wait_time=2) as r:
        peeked = r.peek_messages(max_message_count=10)
        assert bodies(peeked) == ["pk-0", "pk-1", "pk-2"]
        assert all(m.session_id == "pk" for m in peeked)


def test_scheduled_message_in_session_is_received_by_that_session(sb, session_queue):
    when = (utcnow() + timedelta(seconds=120)).replace(microsecond=0)
    with sb.get_queue_sender(session_queue) as s:
        seqs = s.schedule_messages(ServiceBusMessage("later", session_id="sc"), when)
    with sb.get_queue_receiver(session_queue, session_id="sc", max_wait_time=2) as r:
        (p,) = r.peek_messages()
        assert p.sequence_number == seqs[0]
        assert r.receive_messages(max_wait_time=1) == []
    with sb.get_queue_sender(session_queue) as s:
        s.cancel_scheduled_messages(seqs)


def test_locked_until_utc_of_session_message_is_none(sb, session_queue):
    # messages of a session are locked through the session: the SDK reports no per-message lock
    send_session(sb, session_queue, "lu", 1)
    with sb.get_queue_receiver(session_queue, session_id="lu", max_wait_time=2) as r:
        (m,) = receive_n(r, 1)
        assert m.locked_until_utc is None


# -- session state ------------------------------------------------------------------------------
def test_session_state_is_none_until_set(sb, session_queue):
    send_session(sb, session_queue, "st0", 1)
    with sb.get_queue_receiver(session_queue, session_id="st0", max_wait_time=2) as r:
        assert r.session.get_state(timeout=5) is None


def test_session_state_set_then_get(sb, session_queue):
    send_session(sb, session_queue, "st1", 1)
    with sb.get_queue_receiver(session_queue, session_id="st1", max_wait_time=2) as r:
        r.session.set_state("first_state", timeout=5)
        assert r.session.get_state() == b"first_state"


def test_session_state_can_be_overwritten(sb, session_queue):
    send_session(sb, session_queue, "st2", 1)
    with sb.get_queue_receiver(session_queue, session_id="st2", max_wait_time=2) as r:
        r.session.set_state("one")
        r.session.set_state("two")
        assert r.session.get_state() == b"two"


def test_session_state_cleared_by_setting_none(sb, session_queue):
    send_session(sb, session_queue, "st3", 1)
    with sb.get_queue_receiver(session_queue, session_id="st3", max_wait_time=2) as r:
        r.session.set_state("something")
        r.session.set_state(None)
        assert r.session.get_state() is None


def test_session_state_binary_value(sb, session_queue):
    send_session(sb, session_queue, "st4", 1)
    blob = bytes(range(256))
    with sb.get_queue_receiver(session_queue, session_id="st4", max_wait_time=2) as r:
        r.session.set_state(blob)
        assert r.session.get_state() == blob


def test_session_state_survives_closing_the_receiver(sb, session_queue):
    send_session(sb, session_queue, "st5", 1)
    with sb.get_queue_receiver(session_queue, session_id="st5", max_wait_time=2) as r:
        r.session.set_state("kept")
    with sb.get_queue_receiver(session_queue, session_id="st5", max_wait_time=2) as r:
        assert r.session.get_state() == b"kept"


def test_session_state_is_per_session(sb, session_queue):
    send_session(sb, session_queue, "stA", 1)
    send_session(sb, session_queue, "stB", 1)
    with sb.get_queue_receiver(session_queue, session_id="stA", max_wait_time=2) as ra:
        ra.session.set_state("A-state")
    with sb.get_queue_receiver(session_queue, session_id="stB", max_wait_time=2) as rb:
        assert rb.session.get_state() is None


def test_session_state_while_receiving_messages(sb, session_queue):
    # Microsoft's test_session_get_set_state_with_receiver
    send_session(sb, session_queue, "st6", 3)
    with sb.get_queue_receiver(session_queue, session_id="st6", max_wait_time=2) as r:
        assert r.session.get_state(timeout=5) is None
        r.session.set_state("first_state", timeout=5)
        assert len(receive_n(r, 3)) == 3
        assert r.session.get_state() == b"first_state"


# -- listing sessions (new in azure-servicebus 7.15: get-message-sessions) ----------------------------
def test_list_queue_sessions_lists_sessions_with_messages(sb, session_queue):
    send_session(sb, session_queue, "ls1", 1)
    send_session(sb, session_queue, "ls2", 2)
    assert set(sb.list_queue_sessions(session_queue)) == {"ls1", "ls2"}


def test_list_queue_sessions_on_empty_queue_is_empty(sb, session_queue):
    assert list(sb.list_queue_sessions(session_queue)) == []


def test_list_queue_sessions_drops_sessions_after_all_messages_completed(sb, session_queue):
    send_session(sb, session_queue, "gone", 1)
    send_session(sb, session_queue, "stays", 1)
    with sb.get_queue_receiver(session_queue, session_id="gone", max_wait_time=2) as r:
        receive_n(r, 1)
    assert set(sb.list_queue_sessions(session_queue)) == {"stays"}


def test_list_queue_sessions_includes_session_with_state_but_no_messages(sb, session_queue):
    send_session(sb, session_queue, "stateful", 1)
    with sb.get_queue_receiver(session_queue, session_id="stateful", max_wait_time=2) as r:
        r.session.set_state("s")
        receive_n(r, 1)
    assert set(sb.list_queue_sessions(session_queue)) == {"stateful"}
