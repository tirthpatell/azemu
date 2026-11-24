"""Service Bus (AMQP): a throttled send is settled `rejected` with `com.microsoft:server-busy`, a throttled connection or receiving
link is refused with the same condition; the client retries these. Also a dropped connection and a delay."""

import pytest
from azure.servicebus import ServiceBusMessage
from azure.servicebus.exceptions import ServiceBusServerBusyError

import sbutil
from faultutil import Stopwatch

RETRYING = dict(retry_total=5, retry_backoff_factor=0.2, retry_backoff_max=2)


@pytest.fixture
def queue(emu):
    name = sbutil.uniq("fq")
    admin = sbutil.make_admin(emu)
    admin.create_queue(name)
    admin.close()
    return name


def stored(emu, queue):
    with sbutil.make_client(emu) as c:
        return sbutil.drain_bodies(c, queue)


def test_service_bus_retries_a_throttled_send_and_then_succeeds(emu, faults, queue):
    rule = faults.add(service="servicebus", operation="send", path=queue, times=2, fault={"kind": "throttle", "retryAfterSeconds": 1})
    with sbutil.make_client(emu, **RETRYING) as c:
        with c.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("hello"))
    assert faults.fired(rule["id"]) == 2
    assert stored(emu, queue) == ["hello"], "the sends that were refused stored nothing; the one that went through stored one message"


def test_service_bus_with_retries_off_raises_server_busy(emu, faults, queue):
    faults.add(service="servicebus", operation="send", path=queue, fault={"kind": "throttle"})
    with sbutil.make_client(emu, retry_total=0) as c:
        with c.get_queue_sender(queue) as s:
            with pytest.raises(ServiceBusServerBusyError) as e:
                s.send_messages(ServiceBusMessage("hello"))
    assert "server-busy" in str(e.value)
    assert "throttled" in str(e.value)
    assert stored(emu, queue) == []


def test_service_bus_retries_a_throttled_connection(emu, faults, queue):
    rule = faults.add(service="servicebus", operation="connect", times=2, fault={"kind": "throttle"})
    with sbutil.make_client(emu, **RETRYING) as c:
        with c.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("over a retried connection"))
    assert faults.fired(rule["id"]) == 2
    assert stored(emu, queue) == ["over a retried connection"]


def test_service_bus_retries_a_throttled_receiving_link(emu, faults, queue):
    with sbutil.make_client(emu) as c:
        with c.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("m1"))
    rule = faults.add(service="servicebus", operation="receive", path=queue, times=2, fault={"kind": "throttle"})
    with sbutil.make_client(emu, **RETRYING) as c:
        with c.get_queue_receiver(queue, max_wait_time=2) as r:
            got = r.receive_messages(max_message_count=1, max_wait_time=3)
            assert sbutil.bodies(got) == ["m1"]
            r.complete_message(got[0])
    assert faults.fired(rule["id"]) == 2


def test_service_bus_receive_with_retries_off_raises_server_busy(emu, faults, queue):
    faults.add(service="servicebus", operation="receive", path=queue, fault={"kind": "throttle"})
    with sbutil.make_client(emu, retry_total=0) as c:
        with pytest.raises(ServiceBusServerBusyError):
            with c.get_queue_receiver(queue, max_wait_time=2) as r:
                r.receive_messages(max_message_count=1, max_wait_time=2)


def test_service_bus_management_request_refused_is_not_retried_by_the_python_client(emu, faults, queue):
    """A request on `$management` (peek) is settled `rejected` with server-busy. The client raises it and, in azure-servicebus
    7.15.0, does not recover on retries: the management link stays in the failed state. A finding, not a promise."""
    rule = faults.add(service="servicebus", operation="management", path=queue, fault={"kind": "throttle"})
    with sbutil.make_client(emu, retry_total=0) as c:
        with c.get_queue_receiver(queue, max_wait_time=2) as r:
            with pytest.raises(ServiceBusServerBusyError):
                r.peek_messages(1)
    assert faults.fired(rule["id"]) == 1


def test_service_bus_retries_after_a_dropped_connection(emu, faults, queue):
    rule = faults.add(service="servicebus", operation="send", path=queue, times=1, fault={"kind": "drop"})
    with sbutil.make_client(emu, **RETRYING) as c:
        with c.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("after-drop"))
    assert faults.fired(rule["id"]) == 1
    assert stored(emu, queue) == ["after-drop"]


def test_service_bus_latency_delays_the_send(emu, faults, queue):
    faults.add(service="servicebus", operation="send", path=queue, times=1, fault={"kind": "latency", "ms": 800})
    with sbutil.make_client(emu) as c:
        with c.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage("warm up the link first"))
            with Stopwatch() as t:
                s.send_messages(ServiceBusMessage("second"))
    # the first send was the slow one; the second is not
    assert t.seconds < 0.7
    faults.add(service="servicebus", operation="send", path=queue, times=1, fault={"kind": "latency", "ms": 800})
    with sbutil.make_client(emu) as c:
        with c.get_queue_sender(queue) as s:
            with Stopwatch() as t:
                s.send_messages(ServiceBusMessage("slow"))
    assert t.seconds >= 0.75


def test_service_bus_a_send_timeout_shorter_than_the_latency_fails(emu, faults, queue):
    faults.add(service="servicebus", operation="send", path=queue, times=1, fault={"kind": "latency", "ms": 4000})
    with sbutil.make_client(emu, retry_total=0, try_timeout=1) as c:
        with c.get_queue_sender(queue) as s:
            with Stopwatch() as t:
                with pytest.raises(Exception) as e:
                    s.send_messages(ServiceBusMessage("slow"))
    assert t.seconds < 3.9, "the client gave up at its own timeout"
    assert "time" in (type(e.value).__name__ + str(e.value)).lower()
