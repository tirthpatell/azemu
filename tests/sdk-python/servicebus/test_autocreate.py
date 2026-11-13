"""Auto-creation of queues on first attach: a behaviour of the emulator, not of real Service Bus.

Real Service Bus refuses an attach to a missing queue (amqp:not-found), and so does the emulator by default (the
`no_autocreate` tests in test_queue_basic.py / test_topics.py). It creates the queue on first attach only when it is started with
AZEMU_SERVICEBUS_AUTOCREATE=1 (or AZEMU_LENIENT=1): these tests run in the runner's second pass with that variable set and are
skipped otherwise.
"""

import pytest
from azure.servicebus import ServiceBusMessage

from sbutil import body_of, receive_n, uniq

pytestmark = [pytest.mark.emulator_only, pytest.mark.autocreate]


def test_sending_to_a_missing_queue_creates_it(sb, admin):
    name = uniq("auto")
    with sb.get_queue_sender(name) as s:
        s.send_messages(ServiceBusMessage("created"))
    assert admin.get_queue(name).name == name
    with sb.get_queue_receiver(name, max_wait_time=2) as r:
        (m,) = receive_n(r, 1)
        assert body_of(m) == "created"


def test_receiving_from_a_missing_queue_creates_it(sb, admin):
    name = uniq("auto")
    with sb.get_queue_receiver(name, max_wait_time=1) as r:
        assert r.receive_messages(max_message_count=1, max_wait_time=1) == []
    assert admin.get_queue(name).name == name


def test_auto_created_queue_has_default_settings(sb, admin):
    name = uniq("auto")
    with sb.get_queue_sender(name) as s:
        s.send_messages(ServiceBusMessage("x"))
    q = admin.get_queue(name)
    assert q.max_delivery_count == 10
    assert q.requires_session is False
    assert q.lock_duration.total_seconds() == 60
