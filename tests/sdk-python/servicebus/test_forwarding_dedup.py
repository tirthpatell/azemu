"""Auto-forwarding (forward_to / forward_dead_lettered_messages_to) and duplicate detection.

Sources: docs "Chaining Service Bus entities with autoforwarding", "Duplicate detection" (the second send with the
same MessageId is accepted but dropped; the id is remembered for the detection window, also after the first message was
consumed; scheduled messages take part), Microsoft's test_queue_message_duplicate_detection.
"""

from datetime import timedelta

from azure.servicebus import ServiceBusMessage

from sbutil import DLQ, bodies, body_of, drain_bodies, drain_sub, receive_n, send_n, utcnow, wait_until


# -- auto-forwarding ----------------------------------------------------------------------------
def test_queue_forwards_messages_to_target_queue(sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    sent = send_n(sb, source, 5)
    assert drain_bodies(sb, target) == sent
    assert drain_bodies(sb, source) == []


def test_forwarded_message_keeps_body_and_properties(sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    with sb.get_queue_sender(source) as s:
        s.send_messages(
            ServiceBusMessage("payload", application_properties={"k": 1}, subject="sub", message_id="mid", correlation_id="cid")
        )
    with sb.get_queue_receiver(target, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, max_wait=3)
        assert body_of(m) == "payload"
        assert m.application_properties[b"k"] == 1
        assert (m.subject, m.message_id, m.correlation_id) == ("sub", "mid", "cid")
        assert m.delivery_count == 0


def test_forwarded_message_has_enqueued_sequence_number_of_its_first_queue(sb, new_queue):
    # SDK docs: enqueued_sequence_number reflects the sequence number first assigned, for auto-forwarded messages
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    send_n(sb, source, 1)
    with sb.get_queue_receiver(target, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, max_wait=3)
        assert m.enqueued_sequence_number


def test_forwarding_keeps_order(sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target)
    sent = send_n(sb, source, 12)
    assert drain_bodies(sb, target) == sent


def test_forwarding_chain_of_three_queues(sb, new_queue):
    c = new_queue("c")
    b = new_queue("b", forward_to=c)
    a = new_queue("a", forward_to=b)
    sent = send_n(sb, a, 3)
    assert drain_bodies(sb, c) == sent
    assert drain_bodies(sb, b) == [] and drain_bodies(sb, a) == []


def test_forwarding_set_by_update_applies_to_new_messages(sb, admin, new_queue):
    target = new_queue("target")
    source = new_queue("source")
    send_n(sb, source, 1, prefix="before")
    q = admin.get_queue(source)
    q.forward_to = target
    admin.update_queue(q)
    send_n(sb, source, 1, prefix="after")
    assert drain_bodies(sb, target) == ["after-0"]


def test_dead_lettered_messages_are_forwarded_to_the_dlq_target(sb, new_queue):
    target = new_queue("dlq-target")
    source = new_queue("source", forward_dead_lettered_messages_to=target)
    send_n(sb, source, 1, prefix="dl")
    with sb.get_queue_receiver(source, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="why", error_description="because")
    with sb.get_queue_receiver(target, max_wait_time=3) as t:
        (d,) = receive_n(t, 1, max_wait=3)
        assert body_of(d) == "dl-0"
        assert d.dead_letter_reason == "why"
        assert d.dead_letter_error_description == "because"
        assert d.dead_letter_source == source
    assert drain_bodies(sb, source, sub_queue=DLQ) == []


def test_max_delivery_count_dead_letters_are_forwarded_too(sb, new_queue):
    target = new_queue("dlq-target")
    source = new_queue("source", max_delivery_count=2, forward_dead_lettered_messages_to=target)
    send_n(sb, source, 1)
    with sb.get_queue_receiver(source, max_wait_time=3) as r:
        for _ in range(2):
            receive_n(r, 1, settle="abandon", max_wait=3)
    with sb.get_queue_receiver(target, max_wait_time=3) as t:
        (d,) = receive_n(t, 1, max_wait=3)
        assert d.dead_letter_reason == "MaxDeliveryCountExceeded"
        assert d.dead_letter_source == source


def test_subscription_forwards_to_queue(sb, admin, new_topic, new_sub, new_queue):
    topic = new_topic()
    target = new_queue("target")
    sub = new_sub(topic, forward_to=target)
    sent = send_n(sb, topic, 4, topic=True)
    assert drain_bodies(sb, target) == sent
    assert drain_sub(sb, topic, sub) == []


def test_subscription_forward_only_gets_filtered_messages(sb, admin, new_topic, new_sub, new_queue):
    from azure.servicebus.management import SqlRuleFilter

    topic = new_topic()
    target = new_queue("target")
    sub = new_sub(topic, remove_default=True, forward_to=target)
    admin.create_rule(topic, sub, "only-red", filter=SqlRuleFilter("color = 'red'"))
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("r", application_properties={"color": "red"}))
        s.send_messages(ServiceBusMessage("b", application_properties={"color": "blue"}))
    assert drain_bodies(sb, target) == ["r"]


def test_subscription_dead_letters_are_forwarded_to_queue(sb, new_topic, new_sub, new_queue):
    topic = new_topic()
    target = new_queue("target")
    sub = new_sub(topic, forward_dead_lettered_messages_to=target)
    send_n(sb, topic, 1, prefix="dl", topic=True)
    with sb.get_subscription_receiver(topic, sub, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="why")
    with sb.get_queue_receiver(target, max_wait_time=3) as t:
        (d,) = receive_n(t, 1, max_wait=3)
        assert d.dead_letter_reason == "why"
        assert d.dead_letter_source == sub


def test_topic_to_subscription_to_queue_to_queue(sb, new_topic, new_sub, new_queue):
    last = new_queue("last")
    mid = new_queue("mid", forward_to=last)
    topic = new_topic()
    new_sub(topic, forward_to=mid)
    sent = send_n(sb, topic, 2, topic=True)
    assert drain_bodies(sb, last) == sent


# -- duplicate detection ----------------------------------------------------------------------------
def test_same_message_id_twice_gives_one_message(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("first", message_id="dup-1"))
        s.send_messages(ServiceBusMessage("second", message_id="dup-1"))
    assert drain_bodies(sb, queue) == ["first"]


def test_same_message_id_five_times_gives_one_message(sb, new_queue):
    # Microsoft's test_queue_message_duplicate_detection
    queue = new_queue(requires_duplicate_detection=True, dead_lettering_on_message_expiration=True)
    with sb.get_queue_sender(queue) as s:
        for i in range(5):
            s.send_messages(ServiceBusMessage(str(i), message_id="same-id"))
    assert drain_bodies(sb, queue) == ["0"]


def test_different_message_ids_are_all_delivered(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        for i in range(4):
            s.send_messages(ServiceBusMessage(f"m{i}", message_id=f"id-{i}"))
    assert drain_bodies(sb, queue) == ["m0", "m1", "m2", "m3"]


def test_duplicates_inside_one_batch_are_dropped(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages([ServiceBusMessage("a", message_id="x"), ServiceBusMessage("b", message_id="x"), ServiceBusMessage("c", message_id="y")])
    assert drain_bodies(sb, queue) == ["a", "c"]


def test_without_duplicate_detection_same_ids_are_all_delivered(sb, queue):
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("a", message_id="x"))
        s.send_messages(ServiceBusMessage("b", message_id="x"))
    assert drain_bodies(sb, queue) == ["a", "b"]


def test_message_id_is_remembered_after_the_first_message_was_consumed(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("one", message_id="seen"))
    assert drain_bodies(sb, queue) == ["one"]
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("two", message_id="seen"))
    assert drain_bodies(sb, queue) == []


def test_duplicate_send_is_accepted_without_error(sb, new_queue):
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("a", message_id="x"))
        s.send_messages(ServiceBusMessage("a", message_id="x"))  # no exception: the broker accepts and drops


def test_scheduled_duplicate_of_sent_message_is_dropped(sb, admin, new_queue):
    # docs: scheduled messages take part in duplicate detection
    queue = new_queue(requires_duplicate_detection=True)
    with sb.get_queue_sender(queue) as s:
        s.send_messages(ServiceBusMessage("now", message_id="dd"))
        s.schedule_messages(ServiceBusMessage("later", message_id="dd"), (utcnow() + timedelta(seconds=120)).replace(microsecond=0))
    wait_until(lambda: admin.get_queue_runtime_properties(queue).scheduled_message_count == 0)
    assert drain_bodies(sb, queue) == ["now"]


def test_duplicate_detection_on_topic(sb, new_topic, new_sub):
    topic = new_topic(requires_duplicate_detection=True)
    sub = new_sub(topic)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("a", message_id="t1"))
        s.send_messages(ServiceBusMessage("b", message_id="t1"))
    assert bodies(drain_sub(sb, topic, sub)) == ["a"]


def test_duplicate_detection_flag_and_window_are_returned_by_admin(admin):
    from sbutil import uniq

    name = uniq("q")
    admin.create_queue(name, requires_duplicate_detection=True, duplicate_detection_history_time_window=timedelta(minutes=1))
    q = admin.get_queue(name)
    assert q.requires_duplicate_detection is True
    assert q.duplicate_detection_history_time_window == timedelta(minutes=1)
