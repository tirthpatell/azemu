"""Service Bus state across restarts, through the official SDK: entities and their settings, messages in every state
(ready, locked, deferred, scheduled, dead-lettered), sequence numbers, session state and duplicate detection."""

import pathlib
import sys
import time
from datetime import datetime, timedelta, timezone

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "servicebus"))
import sbutil  # noqa: E402
from sbutil import DLQ, PEEK_LOCK  # noqa: E402

from azure.servicebus import ServiceBusMessage  # noqa: E402
from azure.servicebus.management import CorrelationRuleFilter, SqlRuleFilter  # noqa: E402


def drain(client, queue, n, **kw):
    """Receive up to n messages (peek-lock) from a queue; settle nothing."""
    out = []
    with client.get_queue_receiver(queue, receive_mode=PEEK_LOCK, max_wait_time=5, **kw) as r:
        while len(out) < n:
            got = r.receive_messages(max_message_count=n - len(out), max_wait_time=3)
            if not got:
                break
            out.extend(got)
            for m in got:
                r.complete_message(m)
    return out


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_entities_and_messages_in_every_state_survive(persistent, how):
    p = persistent()
    admin = sbutil.make_admin(p.emu)
    admin.create_queue(
        "orders", lock_duration=timedelta(seconds=45), max_delivery_count=5, dead_lettering_on_message_expiration=True,
        default_message_time_to_live=timedelta(hours=1), requires_duplicate_detection=True, max_size_in_megabytes=2048,
        user_metadata="kept",
    )
    admin.create_topic("events")
    admin.create_subscription("events", "red", max_delivery_count=7)
    admin.create_rule("events", "red", "only-red", filter=SqlRuleFilter("color = 'red'"))
    admin.create_rule("events", "red", "by-label", filter=CorrelationRuleFilter(label="L1"))
    admin.delete_rule("events", "red", "$Default")

    client = sbutil.make_client(p.emu)
    with client:
        with client.get_queue_sender("orders") as s:
            for i in range(5):
                s.send_messages(ServiceBusMessage(f"m{i}", message_id=f"id{i}", application_properties={"n": i}, subject="subj"))
            s.send_messages(ServiceBusMessage("again", message_id="id0"))  # a duplicate: dropped
            when = datetime.now(timezone.utc) + timedelta(seconds=6)
            sched_seq = s.schedule_messages(ServiceBusMessage("later", message_id="sched"), when)[0]
        with client.get_topic_sender("events") as s:
            s.send_messages([ServiceBusMessage("red one", application_properties={"color": "red"}), ServiceBusMessage("blue one", application_properties={"color": "blue"})])
        # m0 is deferred, m1 dead-lettered with a reason, m2 abandoned once, m3 stays locked (the receiver goes away)
        with client.get_queue_receiver("orders", receive_mode=PEEK_LOCK, max_wait_time=5) as r:
            msgs = r.receive_messages(max_message_count=4, max_wait_time=5)
            assert [b"".join(m.body).decode() for m in msgs] == ["m0", "m1", "m2", "m3"]
            r.defer_message(msgs[0])
            deferred_seq = msgs[0].sequence_number
            r.dead_letter_message(msgs[1], reason="why", error_description="because")
            r.abandon_message(msgs[2])
            seqs_before = {b"".join(m.body).decode(): m.sequence_number for m in msgs}
            locked_count = msgs[3].delivery_count
        props_before = admin.get_queue_runtime_properties("orders")

        if how == "sigterm":
            assert p.stop() == 0
        else:
            p.save()
            p.stop(kill=True)
    p.start()

    admin = sbutil.make_admin(p.emu)
    q = admin.get_queue("orders")
    assert q.lock_duration == timedelta(seconds=45) and q.max_delivery_count == 5
    assert q.dead_lettering_on_message_expiration and q.requires_duplicate_detection
    assert q.default_message_time_to_live == timedelta(hours=1) and q.max_size_in_megabytes == 2048 and q.user_metadata == "kept"
    rules = {r.name: r for r in admin.list_rules("events", "red")}
    assert sorted(rules) == ["by-label", "only-red"]
    assert rules["only-red"].filter.sql_expression == "color = 'red'" and rules["by-label"].filter.label == "L1"
    assert admin.get_subscription("events", "red").max_delivery_count == 7
    props = admin.get_queue_runtime_properties("orders")
    assert props.total_message_count == props_before.total_message_count and props.active_message_count == props_before.active_message_count
    assert props.dead_letter_message_count == 1 and props.scheduled_message_count == 1

    client = sbutil.make_client(p.emu)
    with client:
        # the dead letter has its reason and description
        with client.get_queue_receiver("orders", sub_queue=DLQ, max_wait_time=5) as r:
            dl = r.receive_messages(max_message_count=1, max_wait_time=5)
            assert len(dl) == 1 and dl[0].dead_letter_reason == "why" and dl[0].dead_letter_error_description == "because"
            r.complete_message(dl[0])
        # the deferred message comes by its sequence number
        with client.get_queue_receiver("orders", receive_mode=PEEK_LOCK, max_wait_time=5) as r:
            d = r.receive_deferred_messages([deferred_seq])
            assert len(d) == 1 and b"".join(d[0].body).decode() == "m0" and d[0].sequence_number == deferred_seq
            r.complete_message(d[0])
        # the rest is ready, in order; the message that was locked is available again with one more delivery attempt
        with client.get_queue_receiver("orders", receive_mode=PEEK_LOCK, max_wait_time=5) as r:
            ready = r.receive_messages(max_message_count=5, max_wait_time=5)
            bodies = [b"".join(m.body).decode() for m in ready]
            assert bodies == ["m2", "m3", "m4"], bodies
            by_body = {b"".join(m.body).decode(): m for m in ready}
            assert by_body["m3"].delivery_count == locked_count + 1, "the message that was locked counts one more attempt"
            for m in ready:
                assert m.sequence_number == seqs_before.get(b"".join(m.body).decode(), m.sequence_number)
                assert m.subject == "subj" and m.application_properties.get(b"n", m.application_properties.get("n")) is not None
                r.complete_message(m)
        # the duplicate id is still inside the window
        with client.get_queue_sender("orders") as s:
            s.send_messages(ServiceBusMessage("dup", message_id="id1"))
        # the scheduled message arrives when it was due
        deadline = time.time() + 20
        later = []
        while not later and time.time() < deadline:
            later = drain(client, "orders", 1)
        assert [b"".join(m.body).decode() for m in later] == ["later"], "the duplicate was dropped, the scheduled message arrived"
        assert later[0].sequence_number == sched_seq
        # the topic's copies: only the red one reached the subscription
        with client.get_subscription_receiver("events", "red", max_wait_time=5) as r:
            got = r.receive_messages(max_message_count=5, max_wait_time=3)
            assert [b"".join(m.body).decode() for m in got] == ["red one"]


def test_session_state_and_a_session_queue_survive(persistent):
    p = persistent()
    admin = sbutil.make_admin(p.emu)
    admin.create_queue("sessions", requires_session=True)
    client = sbutil.make_client(p.emu)
    with client:
        with client.get_queue_sender("sessions") as s:
            s.send_messages([ServiceBusMessage("a1", session_id="A"), ServiceBusMessage("b1", session_id="B")])
        with client.get_queue_receiver("sessions", session_id="A", max_wait_time=5) as r:
            r.session.set_state(b"\x00state\xff")
            m = r.receive_messages(max_message_count=1, max_wait_time=5)
            assert b"".join(m[0].body).decode() == "a1"
            r.complete_message(m[0])
    p.save()
    p.stop(kill=True)
    p.start()
    client = sbutil.make_client(p.emu)
    with client:
        with client.get_queue_receiver("sessions", session_id="A", max_wait_time=5) as r:
            assert r.session.get_state() == b"\x00state\xff"
        with client.get_queue_receiver("sessions", session_id="B", max_wait_time=5) as r:
            got = r.receive_messages(max_message_count=1, max_wait_time=5)
            assert b"".join(got[0].body).decode() == "b1" and got[0].session_id == "B"
            r.complete_message(got[0])


def test_messages_sent_before_the_restart_are_received_after_it_and_nothing_twice(persistent):
    p = persistent()
    sbutil.make_admin(p.emu).create_queue("plain")
    client = sbutil.make_client(p.emu)
    with client:
        with client.get_queue_sender("plain") as s:
            s.send_messages([ServiceBusMessage(f"msg-{i}") for i in range(20)])
        first = drain(client, "plain", 5)
    assert len(first) == 5
    assert p.stop() == 0
    p.start()
    client = sbutil.make_client(p.emu)
    with client:
        rest = drain(client, "plain", 30)
    assert sorted(b"".join(m.body).decode() for m in first + rest) == sorted(f"msg-{i}" for i in range(20))
    assert len(rest) == 15
