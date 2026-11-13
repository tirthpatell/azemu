"""Message locks: renewal, expiry, AutoLockRenewer.

The few tests here that wait for a lock to run out are marked `slow` (the shortest lock Service Bus allows is
5 seconds). Note that the SDK checks `locked_until_utc` itself before settling or renewing, so for an expired lock
`MessageLockLostError` is raised client-side from the `x-opt-locked-until` annotation the broker sent.
"""

import time
from datetime import timedelta

import pytest
from azure.servicebus import AutoLockRenewer
from azure.servicebus.exceptions import MessageAlreadySettled, MessageLockLostError

from sbutil import body_of, receive_n, send_n, utcnow


def sleep_past(locked_until, extra=1.0):
    time.sleep(max(0.0, (locked_until - utcnow()).total_seconds()) + extra)


def test_renew_message_lock_moves_locked_until_forward(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=15))
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        first = m.locked_until_utc
        time.sleep(2)
        new_until = r.renew_message_lock(m)
        assert new_until > first
        assert (m.locked_until_utc - first) >= timedelta(seconds=1.5)
        assert m.locked_until_utc == new_until
        r.complete_message(m)


def test_renew_message_lock_returns_a_utc_datetime(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        until = r.renew_message_lock(m)
        assert until.tzinfo is not None
        assert until > utcnow()
        r.complete_message(m)


def test_renew_lock_of_each_message_in_a_batch(sb, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, prefetch_count=5, max_wait_time=3) as r:
        got = receive_n(r, 3, settle=None)
        for m in got:
            before = m.locked_until_utc
            r.renew_message_lock(m)
            assert m.locked_until_utc >= before
            r.complete_message(m)


def test_renew_lock_on_peeked_message_is_refused_client_side(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (p,) = r.peek_messages()
        with pytest.raises(ValueError):
            r.renew_message_lock(p)


def test_renew_lock_in_receive_and_delete_mode_is_refused_client_side(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, receive_mode="receiveanddelete", max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        with pytest.raises(ValueError):
            r.renew_message_lock(m)


def test_auto_lock_renewer_cannot_register_receive_and_delete_message(sb, queue):
    send_n(sb, queue, 1)
    renewer = AutoLockRenewer()
    try:
        with sb.get_queue_receiver(queue, receive_mode="receiveanddelete", max_wait_time=3) as r:
            (m,) = receive_n(r, 1, settle=None, max_wait=3)
            with pytest.raises(ValueError):
                renewer.register(r, m)
    finally:
        renewer.close()


def test_auto_lock_renewer_cannot_be_given_to_receive_and_delete_receiver(sb, queue):
    renewer = AutoLockRenewer()
    try:
        with pytest.raises(ValueError):
            sb.get_queue_receiver(queue, receive_mode="receiveanddelete", auto_lock_renewer=renewer)
    finally:
        renewer.close()


# -- waiting for a 5 s lock to run out (slow) -----------------------------------------------------
@pytest.mark.slow
def test_complete_after_lock_expired_raises_message_lock_lost(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        sleep_past(m.locked_until_utc)
        with pytest.raises(MessageLockLostError):
            r.complete_message(m)


@pytest.mark.slow
def test_renew_after_lock_expired_raises_message_lock_lost(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        sleep_past(m.locked_until_utc)
        with pytest.raises(MessageLockLostError):
            r.renew_message_lock(m)


@pytest.mark.slow
def test_message_is_redelivered_with_higher_delivery_count_after_lock_expired(sb, new_queue):
    # Microsoft's test_queue_message_connection_closed: after the lock ran out the message comes back, count > 0
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1, prefix="lk")
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        sleep_past(m.locked_until_utc, extra=0.5)
        (again,) = receive_n(r, 1, settle=None, max_wait=4)
        assert body_of(again) == "lk-0"
        assert again.delivery_count == 1
        assert again.sequence_number == m.sequence_number
        r.complete_message(again)


@pytest.mark.slow
def test_renewed_lock_outlives_the_original_lock(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        original = m.locked_until_utc
        time.sleep(3)
        r.renew_message_lock(m)
        sleep_past(original, extra=0.5)  # the first lock would be over by now
        assert m.locked_until_utc > utcnow()
        r.complete_message(m)


@pytest.mark.slow
def test_auto_lock_renewer_keeps_message_locked_past_the_lock_duration(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1)
    renewer = AutoLockRenewer(max_lock_renewal_duration=30)
    try:
        with sb.get_queue_receiver(queue, max_wait_time=3) as r:
            (m,) = receive_n(r, 1, settle=None, max_wait=3)
            original = m.locked_until_utc
            renewer.register(r, m)
            sleep_past(original, extra=1.0)
            assert m.locked_until_utc > utcnow()
            r.complete_message(m)
    finally:
        renewer.close()


@pytest.mark.slow
def test_receiver_with_auto_lock_renewer_renews_received_messages(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=5))
    send_n(sb, queue, 1)
    renewer = AutoLockRenewer(max_lock_renewal_duration=30)
    try:
        with sb.get_queue_receiver(queue, max_wait_time=3, auto_lock_renewer=renewer) as r:
            (m,) = receive_n(r, 1, settle=None, max_wait=3)
            original = m.locked_until_utc
            sleep_past(original, extra=1.0)
            r.complete_message(m)
            with pytest.raises(MessageAlreadySettled):
                r.complete_message(m)
    finally:
        renewer.close()
