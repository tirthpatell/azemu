"""Administration client: runtime properties (message counts reflect what the messaging tests did) and namespace.

Counts on real Service Bus can lag a little behind, so each check polls for a few seconds (`wait_until`).
Source: Microsoft's test_mgmt_*_get_runtime_properties_* and the recordings; a locked (received but unsettled)
message still counts as active.
"""

from datetime import timedelta

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.servicebus import ServiceBusMessage
from azure.servicebus.management import CorrelationRuleFilter

from sbutil import receive_n, send_n, utcnow, wait_until


def counts(rp):
    return (rp.active_message_count, rp.dead_letter_message_count, rp.scheduled_message_count)


def test_new_queue_runtime_properties_are_zero(admin, queue):
    rp = admin.get_queue_runtime_properties(queue)
    assert rp.name == queue
    assert rp.size_in_bytes == 0
    assert rp.total_message_count == 0
    assert counts(rp) == (0, 0, 0)
    assert rp.transfer_message_count == 0 and rp.transfer_dead_letter_message_count == 0
    assert rp.created_at_utc is not None and rp.updated_at_utc is not None and rp.accessed_at_utc is not None


def test_runtime_properties_created_at_is_recent(admin, queue):
    rp = admin.get_queue_runtime_properties(queue)
    assert abs((utcnow() - rp.created_at_utc).total_seconds()) < 120


def test_active_message_count_follows_sends(sb, admin, queue):
    send_n(sb, queue, 5)
    rp = wait_until(lambda: (r := admin.get_queue_runtime_properties(queue)) and r.active_message_count == 5 and r)
    assert rp.total_message_count == 5
    assert rp.size_in_bytes > 0


def test_completed_messages_are_not_counted(sb, admin, queue):
    send_n(sb, queue, 4)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        receive_n(r, 2)
    wait_until(lambda: admin.get_queue_runtime_properties(queue).active_message_count == 2)


def test_locked_message_still_counts_as_active(sb, admin, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        assert admin.get_queue_runtime_properties(queue).active_message_count == 1
        r.complete_message(m)
    wait_until(lambda: admin.get_queue_runtime_properties(queue).active_message_count == 0)


def test_dead_lettered_message_moves_from_active_to_dead_letter_count(sb, admin, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="x")
    wait_until(lambda: counts(admin.get_queue_runtime_properties(queue)) == (2, 1, 0))


def test_scheduled_message_count(sb, admin, queue):
    with sb.get_queue_sender(queue) as s:
        s.schedule_messages([ServiceBusMessage("a"), ServiceBusMessage("b")], (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
    wait_until(lambda: counts(admin.get_queue_runtime_properties(queue)) == (0, 0, 2))


def test_cancelled_scheduled_messages_are_not_counted(sb, admin, queue):
    with sb.get_queue_sender(queue) as s:
        seqs = s.schedule_messages([ServiceBusMessage("a")], (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
        s.cancel_scheduled_messages(seqs)
    wait_until(lambda: counts(admin.get_queue_runtime_properties(queue)) == (0, 0, 0))


def test_deferred_message_counts_as_active(sb, admin, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        receive_n(r, 1, settle="defer")
    wait_until(lambda: admin.get_queue_runtime_properties(queue).active_message_count == 1)


def test_get_runtime_properties_of_missing_entities_raise_resource_not_found(admin, topic):
    with pytest.raises(ResourceNotFoundError):
        admin.get_queue_runtime_properties("non_existing_queue")
    with pytest.raises(ResourceNotFoundError):
        admin.get_topic_runtime_properties("non_existing_topic")
    with pytest.raises(ResourceNotFoundError):
        admin.get_subscription_runtime_properties(topic, "non_existing_sub")


def test_list_queues_runtime_properties(sb, admin, new_queue):
    a, b = new_queue(), new_queue()
    send_n(sb, a, 2)
    wait_until(lambda: {r.name: r.active_message_count for r in admin.list_queues_runtime_properties()} == {a: 2, b: 0})


def test_topic_runtime_properties_subscription_count(admin, topic, new_sub):
    assert admin.get_topic_runtime_properties(topic).subscription_count == 0
    new_sub(topic)
    new_sub(topic)
    rp = admin.get_topic_runtime_properties(topic)
    assert rp.name == topic
    assert rp.subscription_count == 2
    assert rp.size_in_bytes is not None
    assert rp.created_at_utc is not None and rp.updated_at_utc is not None and rp.accessed_at_utc is not None


def test_topic_runtime_properties_filter_counts_include_default_rules(admin, topic, new_sub):
    # docs (topic-filters): the counts include each subscription's default rule until it is replaced or removed
    s1, s2 = new_sub(topic), new_sub(topic)
    rp = admin.get_topic_runtime_properties(topic)
    assert (rp.sql_filter_count, rp.correlation_filter_count) == (2, 0)
    admin.create_rule(topic, s1, "corr", filter=CorrelationRuleFilter(correlation_id="x"))
    admin.delete_rule(topic, s2, "$Default")
    rp = admin.get_topic_runtime_properties(topic)
    assert (rp.sql_filter_count, rp.correlation_filter_count) == (1, 1)


def test_topic_scheduled_message_count(sb, admin, topic, new_sub):
    new_sub(topic)
    with sb.get_topic_sender(topic) as s:
        s.schedule_messages(ServiceBusMessage("later"), (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
    wait_until(lambda: admin.get_topic_runtime_properties(topic).scheduled_message_count == 1)


def test_subscription_runtime_properties_follow_messaging(sb, admin, topic, new_sub):
    sub = new_sub(topic)
    send_n(sb, topic, 3, topic=True)
    rp = wait_until(lambda: (r := admin.get_subscription_runtime_properties(topic, sub)) and r.active_message_count == 3 and r)
    assert rp.name == sub and rp.total_message_count == 3
    with sb.get_subscription_receiver(topic, sub, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="x")
    rp = wait_until(lambda: (r := admin.get_subscription_runtime_properties(topic, sub)) and r.dead_letter_message_count == 1 and r)
    assert rp.active_message_count == 2
    assert rp.transfer_message_count == 0 and rp.transfer_dead_letter_message_count == 0
    assert rp.created_at_utc is not None


def test_list_topics_and_subscriptions_runtime_properties(sb, admin, new_topic, new_sub):
    t1, t2 = new_topic(), new_topic()
    s1 = new_sub(t1)
    new_sub(t1)
    new_sub(t2)
    send_n(sb, t1, 2, topic=True)
    assert {r.name: r.subscription_count for r in admin.list_topics_runtime_properties()} == {t1: 2, t2: 1}
    wait_until(lambda: {r.name: r.active_message_count for r in admin.list_subscriptions_runtime_properties(t1)}[s1] == 2)


def test_namespace_properties(admin):
    # recording: messaging_sku == "Standard"
    ns = admin.get_namespace_properties()
    assert ns.messaging_sku == "Standard"
    assert ns.name


def test_namespace_is_selected_by_host_name(emu, admin, new_queue):
    # a client for another namespace (`other.servicebus.localhost`) does not see the queues of `default`
    import sbutil

    name = new_queue()
    other = sbutil.make_admin(emu, "other")
    try:
        assert list(other.list_queues()) == []
        other.create_queue(name)
        assert [q.name for q in other.list_queues()] == [name]
    finally:
        other.close()
    assert [q.name for q in admin.list_queues()] == [name]
