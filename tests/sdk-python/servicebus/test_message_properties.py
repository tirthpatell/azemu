"""Message round trip: body types, application properties, system properties, broker-set properties.

The SDK decodes AMQP strings as bytes (so a str application property comes back as bytes, and the keys are
bytes): that is how Microsoft's tests assert too (`message.application_properties[b"key"] == b"value"`).
"""

import uuid
from datetime import datetime, timedelta, timezone

import pytest
from azure.servicebus import ServiceBusMessage, ServiceBusMessageState
from azure.servicebus.amqp import (
    AmqpAnnotatedMessage,
    AmqpMessageBodyType,
    AmqpMessageHeader,
    AmqpMessageProperties,
)

from sbutil import body_of, receive_n, send_n, utcnow


def roundtrip(sb, queue, message, *, settle="complete", wait=3):
    """Send one message (ServiceBusMessage, dict or AmqpAnnotatedMessage) and receive it back."""
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(message)
    with sb.get_queue_receiver(queue, max_wait_time=wait) as receiver:
        got = receive_n(receiver, 1, max_wait=wait, settle=settle)
    assert len(got) == 1, "message did not come back"
    return got[0]


# -- bodies -------------------------------------------------------------------------------------
@pytest.mark.parametrize(
    "body",
    ["plain", "", "héllo wörld ✓ 日本語 🚀", "line1\nline2\ttab", "x" * 5000, '{"a": [1, 2, {"b": null}]}'],
    ids=["ascii", "empty", "unicode", "whitespace", "5000-chars", "json-text"],
)
def test_str_body_round_trip(sb, queue, body):
    m = roundtrip(sb, queue, ServiceBusMessage(body))
    assert body_of(m) == body
    assert m.body_type == AmqpMessageBodyType.DATA


@pytest.mark.parametrize(
    "body",
    [b"bytes", b"", bytes(range(256)), b"\x00\x01\x02\xff" * 100],
    ids=["small", "empty", "all-256-byte-values", "binary-pattern"],
)
def test_bytes_body_round_trip(sb, queue, body):
    m = roundtrip(sb, queue, ServiceBusMessage(body))
    assert b"".join(m.body) == body


def test_str_body_is_received_as_utf8_bytes(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("é"))
    assert list(m.body) == ["é".encode()]
    assert str(m) == "é"


def test_body_none_round_trip(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage(None))
    assert m.body is None


def test_data_body_with_several_sections_round_trip(sb, queue):
    sent = AmqpAnnotatedMessage(data_body=[b"aa", b"bb", b"cc"])
    m = roundtrip(sb, queue, sent)
    assert m.raw_amqp_message.body_type == AmqpMessageBodyType.DATA
    assert list(m.raw_amqp_message.body) == [b"aa", b"bb", b"cc"]


def test_value_body_dict_round_trip(sb, queue):
    value = {b"key": [-123, b"data", False]}
    m = roundtrip(sb, queue, AmqpAnnotatedMessage(value_body=value))
    assert m.raw_amqp_message.body_type == AmqpMessageBodyType.VALUE
    assert m.raw_amqp_message.body == value
    assert m.body_type == AmqpMessageBodyType.VALUE


@pytest.mark.parametrize("value", [42, -1, 1.5, True, "text"], ids=["int", "negative", "float", "bool", "str"])
def test_value_body_scalar_round_trip(sb, queue, value):
    m = roundtrip(sb, queue, AmqpAnnotatedMessage(value_body=value))
    assert m.raw_amqp_message.body_type == AmqpMessageBodyType.VALUE
    got = m.raw_amqp_message.body
    assert got == (value.encode() if isinstance(value, str) else value)


def test_sequence_body_round_trip(sb, queue):
    seq = [b"message", 123.456, True]
    m = roundtrip(sb, queue, AmqpAnnotatedMessage(sequence_body=seq))
    assert m.raw_amqp_message.body_type == AmqpMessageBodyType.SEQUENCE
    assert list(m.raw_amqp_message.body) == [seq]


def test_amqp_message_with_value_and_data_body_is_rejected_client_side():
    with pytest.raises(ValueError):
        AmqpAnnotatedMessage(data_body=[b"a"], value_body=1)
    with pytest.raises(ValueError):
        AmqpAnnotatedMessage()


def test_dict_message_is_sent_as_service_bus_message(sb, queue):
    m = roundtrip(sb, queue, {"body": "from a dict", "subject": "s", "application_properties": {"k": 1}})
    assert body_of(m) == "from a dict"
    assert m.subject == "s"
    assert m.application_properties[b"k"] == 1


def test_amqp_header_priority_and_custom_annotations_survive(sb, queue):
    sent = AmqpAnnotatedMessage(
        value_body={b"key": 1}, header=AmqpMessageHeader(priority=10), annotations={"ann_key": "ann_value"}
    )
    m = roundtrip(sb, queue, sent)
    assert m.raw_amqp_message.header.priority == 10
    assert m.raw_amqp_message.annotations[b"ann_key"] == b"ann_value"


def test_amqp_footer_and_properties_subject_survive(sb, queue):
    # Microsoft's test_queue_send_amqp_annotated_message (sequence message part)
    sent = AmqpAnnotatedMessage(
        sequence_body=[b"x"], footer={"footer_key": "footer_value"}, properties=AmqpMessageProperties(subject="seq")
    )
    m = roundtrip(sb, queue, sent)
    assert m.raw_amqp_message.footer[b"footer_key"] == b"footer_value"
    assert m.raw_amqp_message.properties.subject == b"seq"
    assert m.subject == "seq"


def test_amqp_delivery_annotations_survive(sb, queue):
    # asserted by Microsoft's live test_queue_send_amqp_annotated_message (data message part)
    sent = AmqpAnnotatedMessage(data_body=[b"aa"], delivery_annotations={"delann_key": "delann_value"})
    m = roundtrip(sb, queue, sent)
    assert m.raw_amqp_message.delivery_annotations[b"delann_key"] == b"delann_value"


def test_different_body_types_in_one_batch_all_arrive_in_order(sb, queue):
    with sb.get_queue_sender(queue) as sender:
        batch = sender.create_message_batch()
        batch.add_message(AmqpAnnotatedMessage(data_body=[b"d"]))
        batch.add_message(AmqpAnnotatedMessage(value_body={b"k": 1}))
        batch.add_message(AmqpAnnotatedMessage(sequence_body=[1, 2]))
        batch.add_message({"body": "dict"})
        batch.add_message(ServiceBusMessage("sb"))
        sender.send_messages(batch)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        got = receive_n(r, 5)
    assert [m.body_type for m in got] == [
        AmqpMessageBodyType.DATA,
        AmqpMessageBodyType.VALUE,
        AmqpMessageBodyType.SEQUENCE,
        AmqpMessageBodyType.DATA,
        AmqpMessageBodyType.DATA,
    ]


# -- application properties ---------------------------------------------------------------------
@pytest.mark.parametrize(
    "value, expected",
    [
        ("text", b"text"),
        ("", b""),
        ("ünï", "ünï".encode()),
        (5, 5),
        (0, 0),
        (-7, -7),
        (2**31 - 1, 2**31 - 1),
        (-(2**31), -(2**31)),
        (1.5, 1.5),
        (-0.25, -0.25),
        (True, True),
        (False, False),
        (b"by\x00te", b"by\x00te"),
        (uuid.UUID(int=0x1234567890ABCDEF), uuid.UUID(int=0x1234567890ABCDEF)),
        (datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc), 1767323045000),
    ],
    ids=[
        "str",
        "empty-str",
        "unicode-str",
        "int",
        "zero",
        "negative-int",
        "int32-max",
        "int32-min",
        "float",
        "negative-float",
        "true",
        "false",
        "bytes",
        "uuid",
        "datetime-as-epoch-ms",
    ],
)
def test_application_property_value_types_round_trip(sb, queue, value, expected):
    m = roundtrip(sb, queue, ServiceBusMessage("b", application_properties={"p": value}))
    got = m.application_properties[b"p"]
    assert got == expected
    assert type(got) is type(expected)


def test_application_property_bool_stays_bool_not_int(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("b", application_properties={"t": True, "one": 1}))
    assert m.application_properties[b"t"] is True
    assert m.application_properties[b"one"] == 1 and m.application_properties[b"one"] is not True


def test_many_application_properties_round_trip(sb, queue):
    props = {f"key{i}": i for i in range(50)}
    m = roundtrip(sb, queue, ServiceBusMessage("b", application_properties=props))
    assert {k.decode(): v for k, v in m.application_properties.items()} == props


def test_application_property_keys_are_case_sensitive(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("b", application_properties={"Key": 1, "key": 2}))
    assert m.application_properties[b"Key"] == 1
    assert m.application_properties[b"key"] == 2


def test_no_application_properties_gives_empty(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("b"))
    assert not m.application_properties


def test_application_properties_with_bytes_keys(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("b", application_properties={b"bk": "v"}))
    assert m.application_properties[b"bk"] == b"v"


# -- system properties ---------------------------------------------------------------------------
@pytest.mark.parametrize(
    "prop, value",
    [
        ("message_id", "msg-id-1"),
        ("message_id", "x" * 100),
        ("correlation_id", "cid-1"),
        ("subject", "my label"),
        ("content_type", "application/json"),
        ("to", "queue-b"),
        ("reply_to", "reply-queue"),
        ("subject", "ünï ✓"),
    ],
    ids=["message_id", "message_id-100", "correlation_id", "subject", "content_type", "to", "reply_to", "unicode-subject"],
)
def test_system_property_round_trip(sb, queue, prop, value):
    m = roundtrip(sb, queue, ServiceBusMessage("b", **{prop: value}))
    assert getattr(m, prop) == value


def test_message_id_uuid_round_trip(sb, queue):
    mid = uuid.uuid4()
    m = roundtrip(sb, queue, ServiceBusMessage("b", message_id=mid))
    assert m.message_id == mid


def test_all_system_properties_together(sb, queue):
    # Microsoft's receive_handler_peeklock sets the same set
    m = roundtrip(
        sb,
        queue,
        ServiceBusMessage(
            "Handler message no. 0",
            application_properties={"key": "value"},
            subject="label",
            content_type="application/text",
            correlation_id="cid",
            message_id="0",
            to="to",
            reply_to="reply_to",
        ),
    )
    assert m.application_properties[b"key"] == b"value"
    assert (m.subject, m.content_type, m.correlation_id) == ("label", "application/text", "cid")
    assert (m.message_id, m.to, m.reply_to) == ("0", "to", "reply_to")
    assert not m.session_id and not m.reply_to_session_id
    assert not m.scheduled_enqueue_time_utc
    assert not m.time_to_live


def test_properties_not_set_are_empty_on_receive(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("plain"))
    assert not m.subject and not m.content_type and not m.correlation_id
    assert not m.to and not m.reply_to
    assert not m.session_id and not m.reply_to_session_id
    assert not m.partition_key
    assert not m.time_to_live
    assert not m.scheduled_enqueue_time_utc


def test_message_with_partition_key_is_accepted(sb, queue):
    # Whether a non-partitioned entity returns the partition key is not stated in the docs: only delivery is asserted.
    m = roundtrip(sb, queue, ServiceBusMessage("b", partition_key="pk"))
    assert body_of(m) == "b"


def test_session_id_and_reply_to_session_id_on_non_session_queue_round_trip(sb, queue):
    # Microsoft's ..._retrieve_deferred_receiver_complete sends session_id to a queue without sessions
    m = roundtrip(sb, queue, ServiceBusMessage("b", session_id="sess-1", reply_to_session_id="reply-sess"))
    assert m.reply_to_session_id == "reply-sess"
    assert m.session_id == "sess-1"


def test_time_to_live_round_trip_and_expires_at(sb, queue):
    # Microsoft's test_queue_message_batch: expires_at_utc == enqueued_time_utc + time_to_live
    m = roundtrip(sb, queue, ServiceBusMessage("b", time_to_live=timedelta(seconds=90)))
    assert m.time_to_live == timedelta(seconds=90)
    assert m.expires_at_utc == m.enqueued_time_utc + timedelta(seconds=90)


def test_message_without_ttl_has_no_time_to_live_or_expiry(sb, queue):
    # Microsoft's receive_handler_peeklock: `assert not message.time_to_live` for messages sent without a TTL
    m = roundtrip(sb, queue, ServiceBusMessage("b"))
    assert not m.time_to_live
    assert m.expires_at_utc is None


def test_received_message_can_be_resent_as_new_message(sb, queue):
    # Microsoft's browse test passes a peeked ServiceBusReceivedMessage to send_messages
    with sb.get_queue_sender(queue) as sender:
        sender.send_messages(ServiceBusMessage("orig", subject="s", application_properties={"k": "v"}))
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (peeked,) = r.peek_messages()
        with sb.get_queue_sender(queue) as sender:
            sender.send_messages(peeked)
        got = receive_n(r, 2)
    assert [body_of(m) for m in got] == ["orig", "orig"]
    assert {m.subject for m in got} == {"s"}
    assert got[0].sequence_number != got[1].sequence_number


# -- broker-set properties -----------------------------------------------------------------------
def test_enqueued_time_utc_is_recent_and_timezone_aware(sb, queue):
    before = utcnow()
    m = roundtrip(sb, queue, ServiceBusMessage("b"))
    after = utcnow()
    assert m.enqueued_time_utc.tzinfo is not None
    assert before - timedelta(seconds=5) <= m.enqueued_time_utc <= after + timedelta(seconds=5)


def test_sequence_numbers_strictly_increase_in_send_order(sb, queue):
    send_n(sb, queue, 10)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        got = receive_n(r, 10)
    seqs = [m.sequence_number for m in got]
    assert all(isinstance(s, int) and s > 0 for s in seqs)
    assert seqs == sorted(set(seqs))


def test_sequence_numbers_are_unique_across_separate_sends(sb, queue):
    for i in range(3):
        with sb.get_queue_sender(queue) as s:
            s.send_messages(ServiceBusMessage(f"x{i}"))
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        seqs = [m.sequence_number for m in receive_n(r, 3)]
    assert len(set(seqs)) == 3 and seqs == sorted(seqs)


def test_lock_token_is_a_uuid_and_differs_per_message(sb, queue):
    send_n(sb, queue, 3)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        got = receive_n(r, 3, settle=None)
        tokens = [m.lock_token for m in got]
        assert all(isinstance(t, uuid.UUID) for t in tokens)
        assert len(set(tokens)) == 3
        for m in got:
            r.complete_message(m)
        assert got[0].lock_token is None  # settled messages have no lock token


def test_locked_until_utc_is_lock_duration_from_now(sb, new_queue):
    queue = new_queue(lock_duration=timedelta(seconds=30))
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        remaining = (m.locked_until_utc - utcnow()).total_seconds()
        assert 15 <= remaining <= 32
        r.complete_message(m)


def test_default_lock_duration_is_60_seconds(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = receive_n(r, 1, settle=None, max_wait=3)
        remaining = (m.locked_until_utc - utcnow()).total_seconds()
        assert 45 <= remaining <= 62
        r.complete_message(m)


def test_peeked_message_has_no_lock_token_or_locked_until(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (p,) = r.peek_messages()
        assert p.locked_until_utc is None
        assert p.sequence_number and p.enqueued_time_utc
        assert p.delivery_count == 0
        assert p.state == ServiceBusMessageState.ACTIVE


def test_delivery_count_of_peeked_message_follows_abandons(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        receive_n(r, 1, settle="abandon")
        receive_n(r, 1, settle="abandon")
        # sequence_number=1: by default the SDK peeks from after the last received message.
        (p,) = r.peek_messages(sequence_number=1)
        assert p.delivery_count == 2


def test_enqueued_sequence_number_is_zero_or_unset_for_direct_sends(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("b"))
    assert not m.enqueued_sequence_number


def test_dead_letter_source_is_unset_for_a_plain_dead_letter(sb, queue):
    send_n(sb, queue, 1)
    with sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = receive_n(r, 1, settle=None)
        r.dead_letter_message(m, reason="x")
    with sb.get_queue_receiver(queue, sub_queue="deadletter", max_wait_time=3) as dlq:
        (d,) = receive_n(dlq, 1, max_wait=3)
        assert d.dead_letter_source is None


def test_message_repr_and_str_do_not_raise(sb, queue):
    m = roundtrip(sb, queue, ServiceBusMessage("printable"), settle=None)
    assert "printable" in str(m)
    assert repr(m)
