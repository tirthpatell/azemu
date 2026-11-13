"""Time to live and expiry (slow group: each test waits 3-4 seconds).

Sources: docs "Message expiration (Time to Live)" and "Dead-letter queues", Microsoft's test_queue_message_time_to_live.
Real Service Bus expires lazily: an expired message is never delivered, and is moved to the dead-letter queue (if
enabled on the entity) when the next receive on the queue runs, so every DLQ check below first receives from the
main queue, as Microsoft's test does.
"""

import time
from datetime import timedelta

import pytest
from azure.servicebus import ServiceBusMessage

from sbutil import DLQ, body_of, drain_bodies, receive_n, send_n, td

pytestmark = pytest.mark.slow


def test_message_ttl_expired_message_is_not_delivered(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("short-lived", time_to_live=td(2)))
    time.sleep(3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert r.receive_messages(max_message_count=5, max_wait_time=2) == []


def test_only_unexpired_messages_are_delivered(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("expires", time_to_live=td(2)))
        s.send_messages(ServiceBusMessage("stays", time_to_live=timedelta(hours=1)))
    time.sleep(3)
    assert drain_bodies(sb, queue, max_wait=2) == ["stays"]


def test_expired_message_goes_to_dlq_when_dead_lettering_on_expiration(sb, new_queue):
    queue = new_queue(dead_lettering_on_message_expiration=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("gone", time_to_live=td(2)))
    time.sleep(3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert r.receive_messages(max_message_count=1, max_wait_time=2) == []
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, max_wait=3)
        assert body_of(d) == "gone"
        assert d.dead_letter_reason == "TTLExpiredException"
        assert "expired" in d.dead_letter_error_description.lower()


def test_expired_message_is_dropped_without_dead_lettering_on_expiration(sb, new_queue):
    queue = new_queue(dead_lettering_on_message_expiration=False)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("dropped", time_to_live=td(2)))
    time.sleep(3)
    assert drain_bodies(sb, queue, max_wait=2) == []
    assert drain_bodies(sb, queue, sub_queue=DLQ, max_wait=1) == []


def test_default_message_ttl_of_entity_expires_messages_without_ttl(sb, new_queue):
    queue = new_queue(default_message_time_to_live=td(2), dead_lettering_on_message_expiration=True)
    send_n(sb, queue, 1, prefix="dflt")
    time.sleep(3)
    assert drain_bodies(sb, queue, max_wait=2) == []
    assert drain_bodies(sb, queue, sub_queue=DLQ, max_wait=3) == ["dflt-0"]


def test_entity_default_ttl_is_a_ceiling_for_longer_message_ttl(sb, new_queue):
    # docs: a message TTL longer than the entity default is silently reduced to the default
    queue = new_queue(default_message_time_to_live=td(2))
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("capped", time_to_live=timedelta(hours=1)))
    time.sleep(3)
    assert drain_bodies(sb, queue, max_wait=2) == []


def test_message_ttl_shorter_than_entity_default_wins(sb, new_queue):
    queue = new_queue(default_message_time_to_live=timedelta(hours=1))
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("short", time_to_live=td(2)))
        s.send_messages(ServiceBusMessage("long"))
    time.sleep(3)
    assert drain_bodies(sb, queue, max_wait=2) == ["long"]


def test_subscription_default_ttl_expires_messages_into_its_dlq(sb, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, default_message_time_to_live=td(2), dead_lettering_on_message_expiration=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("sub-ttl"))
    time.sleep(3)
    with sb.get_subscription_receiver(topic, sub, max_wait_time=2) as r:
        assert r.receive_messages(max_message_count=1, max_wait_time=2) == []
    with sb.get_subscription_receiver(topic, sub, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, max_wait=3)
        assert d.dead_letter_reason == "TTLExpiredException"
        assert body_of(d) == "sub-ttl"
