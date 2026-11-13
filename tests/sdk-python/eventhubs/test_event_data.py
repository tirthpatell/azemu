"""What an event looks like after a round trip: body types, application properties, AMQP properties, system properties.

Sources: the SDK's own encode/decode (every expected value below was first produced by sending a message through
`encode_payload` / `decode_payload` locally), `EventData.system_properties` docstring (which AMQP properties are surfaced),
Microsoft's `test_send_amqp_annotated_message`, `test_send_and_receive_small_body`, `test_send_non_ascii` and
`test_receive_end_of_stream`. The service must hand back what was sent with the same AMQP types; strings reach Python as bytes.
"""

import uuid
from datetime import datetime, timedelta, timezone

import pytest
from azure.eventhub import EventData
from azure.eventhub._pyamqp.utils import amqp_long_value
from azure.eventhub.amqp import AmqpAnnotatedMessage, AmqpMessageBodyType, AmqpMessageProperties

from ehutil import read_from_start, send_n


def roundtrip(emu, hub, producer, event, *, batch=False):
    """Send one event to partition 0 (alone or inside a batch) and return it as received."""
    if batch:
        b = producer.create_batch(partition_id="0")
        b.add(event)
        producer.send_batch(b)
    else:
        producer.send_event(event, partition_id="0")
    (got,) = read_from_start(emu, hub, 1, partition_id="0")
    return got


def raw_body(event) -> bytes:
    return b"".join(event.body)


# -- bodies ------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("batch", [False, True], ids=["single", "in-batch"])
@pytest.mark.parametrize("payload", [b"", b"A single event"], ids=["empty", "small"])
def test_bytes_body(emu, hub, producer, payload, batch):
    # Microsoft's test_send_and_receive_small_body
    ev = roundtrip(emu, hub, producer, EventData(payload), batch=batch)
    assert list(ev.body) == [payload]
    assert ev.body_type == AmqpMessageBodyType.DATA


@pytest.mark.parametrize("batch", [False, True], ids=["single", "in-batch"])
def test_str_body(emu, hub, producer, batch):
    ev = roundtrip(emu, hub, producer, EventData("hello world"), batch=batch)
    assert ev.body_as_str() == "hello world"
    assert raw_body(ev) == b"hello world"


def test_non_ascii_body(emu, hub, producer):
    # Microsoft's test_send_non_ascii
    text = "é,è,à,ù,â,ê,î,ô,û"
    ev = roundtrip(emu, hub, producer, EventData(text))
    assert ev.body_as_str() == text
    assert raw_body(ev) == text.encode("utf-8")


def test_cjk_json_body(emu, hub, producer):
    import json

    ev = roundtrip(emu, hub, producer, EventData(json.dumps({"foo": "漢字"})))
    assert ev.body_as_json() == {"foo": "漢字"}


def test_json_body_round_trip(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, EventData('{"json_key": "json_val", "n": [1, 2, 3]}'))
    assert ev.body_as_json() == {"json_key": "json_val", "n": [1, 2, 3]}


def test_binary_body_with_every_byte_value(emu, hub, producer):
    data = bytes(range(256)) * 4
    assert raw_body(roundtrip(emu, hub, producer, EventData(data))) == data


def test_list_of_bytes_is_a_multi_section_data_body(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, EventData([b"aa", b"bb", b"cc"]))
    assert list(ev.body) == [b"aa", b"bb", b"cc"]
    assert ev.body_as_str() == "aabbcc"


def test_data_body_via_amqp_annotated_message(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(data_body=[b"aa", b"bb", b"cc"]))
    assert ev.body_type == AmqpMessageBodyType.DATA
    assert list(ev.raw_amqp_message.body) == [b"aa", b"bb", b"cc"]


def test_sequence_body(emu, hub, producer):
    # Microsoft: body == [sequence_body], body_as_str == "['message', 123.456, True]"
    ev = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(sequence_body=[b"message", 123.456, True]))
    assert ev.body_type == AmqpMessageBodyType.SEQUENCE
    assert list(ev.raw_amqp_message.body) == [[b"message", 123.456, True]]
    assert ev.body_as_str() == "['message', 123.456, True]"


def test_sequence_body_with_several_sequences(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(sequence_body=[[1, 2], [3]]))
    assert ev.body_type == AmqpMessageBodyType.SEQUENCE
    assert list(ev.raw_amqp_message.body) == [[1, 2], [3]]


def test_value_body_map(emu, hub, producer):
    # Microsoft: raw body == value_body, body_as_str == "{'key': [-123, 'data', False]}"
    value = {b"key": [-123, b"data", False]}
    ev = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(value_body=value))
    assert ev.body_type == AmqpMessageBodyType.VALUE
    assert ev.raw_amqp_message.body == value
    assert ev.body_as_str() == "{'key': [-123, 'data', False]}"


@pytest.mark.parametrize("value,expected", [("text", b"text"), (42, 42), ([1, "a"], [1, b"a"]), (True, True)],
                         ids=["str", "int", "list", "bool"])
def test_value_body_scalars_and_lists(emu, hub, producer, value, expected):
    ev = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(value_body=value))
    assert ev.body_type == AmqpMessageBodyType.VALUE
    assert ev.raw_amqp_message.body == expected


def test_all_three_body_types_in_one_batch_keep_their_types(emu, hub, producer):
    batch = producer.create_batch(partition_id="0")
    batch.add(AmqpAnnotatedMessage(data_body=[b"d"]))
    batch.add(AmqpAnnotatedMessage(sequence_body=[1, 2]))
    batch.add(AmqpAnnotatedMessage(value_body="v"))
    producer.send_batch(batch)
    got = read_from_start(emu, hub, 3, partition_id="0")
    assert [e.body_type for e in got] == [AmqpMessageBodyType.DATA, AmqpMessageBodyType.SEQUENCE, AmqpMessageBodyType.VALUE]


# -- application properties ---------------------------------------------------------------------------------------------
def test_string_application_properties(emu, hub, producer):
    # Microsoft's test_send_multiple_partitions_with_app_prop: properties[b"raw_prop"] == b"raw_value"
    ev = EventData("p")
    ev.properties = {"raw_prop": "raw_value"}
    got = roundtrip(emu, hub, producer, ev)
    assert got.properties == {b"raw_prop": b"raw_value"}


def test_application_properties_of_every_supported_type(emu, hub, producer):
    ev = EventData("types")
    uid = uuid.UUID("12345678-1234-5678-1234-567812345678")
    ev.properties = {
        "s": "str", "i": 5, "neg": -3, "big": 2_000_000, "long": amqp_long_value(2**40), "f": 1.5, "t": True, "fa": False,
        "none": None, "bytes": b"by", "uuid": uid, "dt": datetime(2020, 1, 2, 3, 4, 5, tzinfo=timezone.utc),
    }
    got = roundtrip(emu, hub, producer, ev).properties
    assert got == {
        b"s": b"str", b"i": 5, b"neg": -3, b"big": 2_000_000, b"long": 2**40, b"f": 1.5, b"t": True, b"fa": False,
        b"none": None, b"bytes": b"by", b"uuid": uid, b"dt": 1577934245000,
    }
    assert type(got[b"t"]) is bool and type(got[b"i"]) is int and type(got[b"f"]) is float


def test_application_property_types_survive_inside_a_batch(emu, hub, producer):
    ev = EventData("types")
    ev.properties = {"i": 7, "f": 0.25, "b": True, "s": "x"}
    got = roundtrip(emu, hub, producer, ev, batch=True).properties
    assert got == {b"i": 7, b"f": 0.25, b"b": True, b"s": b"x"}


def test_unicode_application_property_names_and_values(emu, hub, producer):
    ev = EventData("u")
    ev.properties = {"ключ": "значение", "漢": "字"}
    got = roundtrip(emu, hub, producer, ev).properties
    assert {k.decode(): v.decode() for k, v in got.items()} == {"ключ": "значение", "漢": "字"}


def test_empty_string_property_value(emu, hub, producer):
    ev = EventData("e")
    ev.properties = {"empty": ""}
    assert roundtrip(emu, hub, producer, ev).properties == {b"empty": b""}


def test_many_application_properties(emu, hub, producer):
    ev = EventData("many")
    ev.properties = {f"k{i:02d}": i for i in range(60)}
    got = roundtrip(emu, hub, producer, ev).properties
    assert got == {f"k{i:02d}".encode(): i for i in range(60)}


def test_large_application_property_value(emu, hub, producer):
    ev = EventData("big")
    ev.properties = {"blob": "x" * 20_000}
    assert roundtrip(emu, hub, producer, ev).properties == {b"blob": b"x" * 20_000}


def test_event_without_properties_has_none_or_empty(emu, hub, producer):
    got = roundtrip(emu, hub, producer, EventData("plain"))
    assert not got.properties


def test_properties_stay_with_their_own_event_in_a_batch(emu, hub, producer):
    batch = producer.create_batch(partition_id="0")
    for i in range(5):
        ev = EventData(f"e{i}")
        ev.properties = {"idx": i}
        batch.add(ev)
    producer.send_batch(batch)
    got = read_from_start(emu, hub, 5, partition_id="0")
    assert [(e.body_as_str(), e.properties[b"idx"]) for e in got] == [(f"e{i}", i) for i in range(5)]


# -- AMQP properties -----------------------------------------------------------------------------------------------------
def test_content_type_correlation_id_message_id(emu, hub, producer):
    # Microsoft's test_send_amqp_annotated_message / test_receive_over_websocket_sync
    ev = EventData("{}")
    ev.content_type = "text/plain"
    ev.correlation_id = "corr_id"
    ev.message_id = "mess_id"
    got = roundtrip(emu, hub, producer, ev)
    assert (got.content_type, got.correlation_id, got.message_id) == ("text/plain", "corr_id", "mess_id")


@pytest.mark.parametrize("batch", [False, True], ids=["single", "in-batch"])
def test_message_ids_are_unique_per_event_in_a_batch(emu, hub, producer, batch):
    b = producer.create_batch(partition_id="0")
    for i in range(4):
        ev = EventData(f"e{i}")
        ev.message_id = f"mess_id_sample_{i}"
        ev.content_type = "application/json"
        b.add(ev)
    producer.send_batch(b)
    got = read_from_start(emu, hub, 4, partition_id="0")
    assert [e.message_id for e in got] == [f"mess_id_sample_{i}" for i in range(4)]
    assert {e.content_type for e in got} == {"application/json"}


def test_uuid_message_id(emu, hub, producer):
    uid = uuid.uuid4()
    ev = EventData("u")
    ev.message_id = uid
    assert roundtrip(emu, hub, producer, ev).message_id == uid


def test_event_without_ids_has_none(emu, hub, producer):
    got = roundtrip(emu, hub, producer, EventData("plain"))
    assert got.message_id is None and got.correlation_id is None and got.content_type is None


def test_all_amqp_properties_round_trip(emu, hub, producer):
    props = AmqpMessageProperties(
        message_id="m1", correlation_id="c1", content_type="text/csv", content_encoding="gzip", subject="subj", to="dest",
        reply_to="back", group_id="grp", group_sequence=5, reply_to_group_id="rgrp", user_id=b"user",
        creation_time=1600000000000, absolute_expiry_time=4100000000000,
    )
    got = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(data_body=b"x", properties=props)).raw_amqp_message.properties
    assert got.message_id == b"m1" and got.correlation_id == b"c1" and got.content_type == b"text/csv"
    assert got.content_encoding == b"gzip" and got.subject == b"subj" and got.to == b"dest" and got.reply_to == b"back"
    assert got.group_id == b"grp" and got.group_sequence == 5 and got.reply_to_group_id == b"rgrp" and got.user_id == b"user"
    assert got.creation_time == 1600000000000 and got.absolute_expiry_time == 4100000000000


def test_amqp_properties_appear_in_system_properties(emu, hub, producer):
    props = AmqpMessageProperties(subject="subj", to="dest", message_id="mid")
    got = roundtrip(emu, hub, producer, AmqpAnnotatedMessage(data_body=b"x", properties=props))
    sp = got.system_properties
    assert sp[b"subject"] == b"subj" and sp[b"to"] == b"dest" and sp[b"message-id"] == b"mid"


def test_amqp_annotated_message_application_properties_annotations_and_footer(emu, hub, producer):
    # Microsoft's test_send_amqp_annotated_message (sequence / value / data messages)
    msg = AmqpAnnotatedMessage(
        value_body={b"k": 1}, annotations={"ann_key": "ann_value"}, application_properties={"body_type": "value"},
        footer={"footer_key": "footer_value"},
    )
    raw = roundtrip(emu, hub, producer, msg).raw_amqp_message
    assert raw.annotations[b"ann_key"] == b"ann_value"
    assert raw.application_properties[b"body_type"] == b"value"
    assert raw.footer[b"footer_key"] == b"footer_value"


def test_delivery_annotations_sent_by_the_client_come_back(emu, hub, producer):
    # Microsoft's test_send_amqp_annotated_message asserts `delivery_annotations[b"delann_key"] == b"delann_value"` against
    # the real service (the SDK deliberately encodes this section last; see _pyamqp/_encode.py).
    msg = AmqpAnnotatedMessage(data_body=[b"aa"], delivery_annotations={"delann_key": "delann_value"})
    raw = roundtrip(emu, hub, producer, msg).raw_amqp_message
    assert raw.delivery_annotations[b"delann_key"] == b"delann_value"


def test_amqp_annotated_message_in_a_batch_and_as_single_event_are_equal(emu, hub, producer):
    def make():
        return AmqpAnnotatedMessage(data_body=[b"x"], application_properties={"a": 1}, annotations={"x-opt-mine": "v"})

    producer.send_event(make(), partition_id="0")
    b = producer.create_batch(partition_id="0")
    b.add(make())
    producer.send_batch(b)
    one, two = read_from_start(emu, hub, 2, partition_id="0")
    assert one.raw_amqp_message.application_properties == two.raw_amqp_message.application_properties == {b"a": 1}
    assert one.raw_amqp_message.annotations[b"x-opt-mine"] == two.raw_amqp_message.annotations[b"x-opt-mine"] == b"v"


# -- system properties set by the service ---------------------------------------------------------------------------------
def test_sequence_numbers_start_somewhere_and_increase_by_one(emu, hub, producer):
    send_n(producer, 10, partition_id="0")
    seqs = [e.sequence_number for e in read_from_start(emu, hub, 10, partition_id="0")]
    assert seqs[0] >= 0
    assert seqs == list(range(seqs[0], seqs[0] + 10))


def test_sequence_numbers_are_per_partition(emu, hub, producer):
    send_n(producer, 3, partition_id="0")
    send_n(producer, 3, partition_id="1")
    a = [e.sequence_number for e in read_from_start(emu, hub, 3, partition_id="0")]
    b = [e.sequence_number for e in read_from_start(emu, hub, 3, partition_id="1")]
    assert a == b == [a[0], a[0] + 1, a[0] + 2]


def test_sequence_numbers_continue_across_batches_and_single_events(emu, hub, producer):
    send_n(producer, 2, partition_id="0")
    producer.send_event(EventData("single"), partition_id="0")
    send_n(producer, 2, prefix="again", partition_id="0")
    seqs = [e.sequence_number for e in read_from_start(emu, hub, 5, partition_id="0")]
    assert seqs == list(range(seqs[0], seqs[0] + 5))


def test_sequence_number_is_an_int(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, EventData("x"))
    assert isinstance(ev.sequence_number, int) and not isinstance(ev.sequence_number, bool)


def test_offsets_are_strings_that_increase_strictly(emu, hub, producer):
    send_n(producer, 8, partition_id="2")
    offsets = [e.offset for e in read_from_start(emu, hub, 8, partition_id="2")]
    assert all(isinstance(o, str) and o.isdigit() for o in offsets)
    numbers = [int(o) for o in offsets]
    assert numbers == sorted(set(numbers)), "offsets must be unique and increasing"


def test_enqueued_time_is_utc_recent_and_non_decreasing(emu, hub, producer):
    before = datetime.now(timezone.utc) - timedelta(seconds=5)
    send_n(producer, 5, partition_id="0")
    times = [e.enqueued_time for e in read_from_start(emu, hub, 5, partition_id="0")]
    after = datetime.now(timezone.utc) + timedelta(seconds=5)
    assert all(t.utcoffset() == timedelta(0) for t in times)
    assert all(before <= t <= after for t in times)
    assert times == sorted(times)


def test_events_sent_at_different_moments_have_increasing_enqueued_times(emu, hub, producer):
    import time

    producer.send_event(EventData("first"), partition_id="0")
    time.sleep(0.3)
    producer.send_event(EventData("second"), partition_id="0")
    a, b = read_from_start(emu, hub, 2, partition_id="0")
    assert b.enqueued_time - a.enqueued_time >= timedelta(milliseconds=200)


def test_system_properties_hold_the_service_annotations(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, EventData("sys"))
    sp = ev.system_properties
    assert sp[b"x-opt-sequence-number"] == ev.sequence_number
    assert sp[b"x-opt-offset"].decode() == ev.offset
    assert isinstance(sp[b"x-opt-enqueued-time"], int)
    assert b"x-opt-partition-key" not in sp


def test_partition_key_is_in_system_properties_when_sent(emu, hub, producer):
    producer.send_event(EventData("k"), partition_key="pk-1")
    from ehutil import Collector, make_consumer

    with Collector(make_consumer(emu, hub), starting_position="-1", max_wait_time=1) as col:
        (ev,) = col.wait_events(1)
    assert ev.system_properties[b"x-opt-partition-key"] == b"pk-1"
    assert ev.partition_key == b"pk-1"


def test_str_and_repr_of_a_received_event_show_the_metadata(emu, hub, producer):
    # Microsoft's test_receive_end_of_stream checks these fragments of str(event)
    ev = roundtrip(emu, hub, producer, EventData("shown"))
    s = str(ev)
    assert "shown" in s and ", offset: " in s and ", enqueued_time=" in s
    assert "sequence_number" in repr(ev) and "offset" in repr(ev)


def test_enqueued_time_in_system_properties_matches_the_property(emu, hub, producer):
    ev = roundtrip(emu, hub, producer, EventData("t"))
    ms = ev.system_properties[b"x-opt-enqueued-time"]
    assert datetime.fromtimestamp(ms / 1000, tz=timezone.utc) == ev.enqueued_time
