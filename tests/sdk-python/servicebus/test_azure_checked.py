"""Behaviour that was checked against a real Standard namespace in November 2026 and that the emulator now copies.

Every test here passes against the emulator and against Azure (`AZEMU_AZURE=1`).
"""

import time
from datetime import timedelta

import pytest
from azure.core.exceptions import HttpResponseError
from azure.servicebus import ServiceBusMessage
from azure.servicebus.exceptions import MessageNotFoundError, ServiceBusError
from azure.servicebus.management import SqlRuleAction, SqlRuleFilter

from sbutil import DLQ, body_of, receive_n, send_n, utcnow


def test_receiver_on_a_queue_that_forwards_is_refused(sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    with pytest.raises(ServiceBusError, match="auto-forwarding enabled"):
        with sb.get_queue_receiver(source, max_wait_time=1) as r:
            r.receive_messages(max_wait_time=1)


def test_dead_letter_receiver_is_refused_when_dead_letters_are_forwarded(sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_dead_lettered_messages_to=target)
    with sb.get_queue_receiver(source, max_wait_time=1) as r:  # the queue itself is a plain queue
        assert r.receive_messages(max_wait_time=1) == []
    with pytest.raises(ServiceBusError, match="auto-forwarding enabled"):
        with sb.get_queue_receiver(source, sub_queue=DLQ, max_wait_time=1) as r:
            r.receive_messages(max_wait_time=1)


def test_chain_too_long_dead_letters_in_the_fifth_queue(sb, admin, new_queue):
    # seven queues, each forwarding to the next: a message moves four times and is dead-lettered in the fifth
    chain = [new_queue("l6")]
    for i in range(5, -1, -1):
        chain.insert(0, new_queue(f"l{i}", forward_to=chain[0]))
    send_n(sb, chain[0], 1)
    time.sleep(2)
    counts = [admin.get_queue_runtime_properties(q).dead_letter_message_count for q in chain]
    assert counts == [0, 0, 0, 0, 1, 0, 0]
    with sb.get_queue_receiver(chain[4], sub_queue=DLQ, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, max_wait=3)
        assert m.dead_letter_reason == "Maximum transfer hop count is exceeded."
        assert m.dead_letter_error_description is None


def test_messages_stay_in_the_source_when_the_forward_target_was_deleted(sb, admin, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    admin.delete_queue(target)
    send_n(sb, source, 1)
    assert admin.get_queue_runtime_properties(source).total_message_count == 1


def test_forward_to_a_queue_that_is_not_there_is_refused(admin):
    with pytest.raises(HttpResponseError) as e:
        admin.create_queue(f"fwd-{int(time.time())}", forward_to="no-such-queue")
    assert e.value.status_code == 400
    assert "does not exist" in str(e.value)


def test_filter_that_cannot_be_evaluated_dead_letters_the_message(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, remove_default=True)
    admin.create_rule(topic, sub, "gt", filter=SqlRuleFilter("n > 5"))
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("text", application_properties={"n": "abc"}))
        s.send_messages(ServiceBusMessage("big", application_properties={"n": 10}))
        s.send_messages(ServiceBusMessage("none"))
    with sb.get_subscription_receiver(topic, sub, max_wait_time=3) as r:
        assert [body_of(m) for m in receive_n(r, 1, max_wait=3)] == ["big"]
    with sb.get_subscription_receiver(topic, sub, sub_queue=DLQ, max_wait_time=3) as d:
        (m,) = receive_n(d, 1, max_wait=3)
        assert body_of(m) == "text"
        assert m.dead_letter_reason == "FilterException"
        assert m.dead_letter_error_description == "Operator '>' cannot be applied to operands of type 'string' and 'long'"


def test_action_that_cannot_be_evaluated_dead_letters_the_message(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, remove_default=True)
    admin.create_rule(topic, sub, "act", filter=SqlRuleFilter("n IS NOT NULL"), action=SqlRuleAction("SET m = n - 1"))
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("text", application_properties={"n": "abc"}))
    with sb.get_subscription_receiver(topic, sub, sub_queue=DLQ, max_wait_time=3) as d:
        (m,) = receive_n(d, 1, max_wait=3)
        assert m.dead_letter_reason == "RuleActionException"
        assert m.dead_letter_error_description == "Operator '-' cannot be applied to operands of type 'string' and 'long'"


def test_dead_letter_without_a_reason_has_none(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m)
    with sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=2) as d:
        (m,) = receive_n(d, 1, max_wait=3)
        assert m.dead_letter_reason is None
        assert m.dead_letter_error_description is None
        assert b"DeadLetterReason" not in (m.application_properties or {})


def test_deferred_message_counts_as_delivered_and_receive_deferred_errors(sb, queue):
    send_n(sb, queue, 2)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        a, b = receive_n(r, 2, settle=None)
        r.defer_message(a)
        r.complete_message(b)
        (got,) = r.receive_deferred_messages(a.sequence_number)
        assert got.delivery_count == 1
        with pytest.raises(MessageNotFoundError, match="Either the message is already locked or does not exist"):
            r.receive_deferred_messages(a.sequence_number)
        with pytest.raises(MessageNotFoundError, match="not deferred messages"):
            r.receive_deferred_messages(b.sequence_number)
        with pytest.raises(MessageNotFoundError, match="Messages not found"):
            r.receive_deferred_messages(987654321)


def test_message_with_zero_time_to_live_is_refused(sb, queue):
    with sb.get_queue_sender(queue) as s:
        with pytest.raises(ServiceBusError, match="TimeToLIve must be a positive timeout value"):
            s.send_messages(ServiceBusMessage("x", time_to_live=timedelta(0)))


def test_sending_without_a_session_id_to_a_session_queue_is_refused(sb, session_queue):
    with sb.get_queue_sender(session_queue) as s:
        with pytest.raises(ServiceBusError, match="The SessionId was not set on a message"):
            s.send_messages(ServiceBusMessage("x"))


def test_scheduled_duplicate_answers_a_real_sequence_number(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        when = utcnow() + timedelta(seconds=120)
        (first,) = s.schedule_messages(ServiceBusMessage("x", message_id="same"), when)
        (second,) = s.schedule_messages(ServiceBusMessage("x", message_id="same"), when)
        assert second > first
        s.cancel_scheduled_messages([first])


@pytest.mark.parametrize(
    "kwargs, text",
    [
        ({"lock_duration": timedelta(seconds=0)}, "LockDuration must be a positive timeout value"),
        ({"default_message_time_to_live": timedelta(seconds=0)}, "between 00:00:01 and"),
        ({"max_delivery_count": 0}, "The value supplied must be between 1 and 2147483647"),
        ({"max_size_in_megabytes": 100}, "The specified value 100 is invalid"),
        ({"auto_delete_on_idle": timedelta(minutes=1)}, "between 00:05:00 and"),
    ],
)
def test_queue_settings_that_azure_refuses(admin, kwargs, text):
    with pytest.raises(HttpResponseError) as e:
        admin.create_queue(f"limit-{int(time.time() * 1000)}", **kwargs)
    assert e.value.status_code == 400
    assert text in str(e.value)


def test_queue_names_with_other_characters_are_refused(admin):
    with pytest.raises(HttpResponseError) as e:
        admin.create_queue("a b")
    assert e.value.status_code == 400
    assert "contains character(s) that is not allowed by Service Bus" in str(e.value)


def test_sql_rule_that_azure_refuses(admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic)
    with pytest.raises(HttpResponseError) as e:
        admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a = @p"))
    assert "A parameter (@p) used in the expression specified in SqlExpression is missing" in str(e.value)
    with pytest.raises(HttpResponseError) as e:
        admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a ==", parameters=None))
    assert "There was an error parsing the SQL expression." in str(e.value)
