"""Events cross between the protocols: what Kafka writes AMQP reads, and the other way round.

The rules are those of Microsoft Learn, "Exchange events between consumers and producers that use different protocols":
the record value is the event body, the key is the partition key, headers are application properties (a Kafka header
value is raw bytes; an AMQP property reaches Kafka AMQP-encoded, type information included, except the listed immutable
properties, which arrive as UTF-8 strings), and a Kafka offset is the event's sequence number.
"""

import time

import pytest
from azure.eventhub import EventData

import ehutil
import kutil
from kutil import consume, consumer, decode_amqp, produce_all, producer, sasl_plain


def text(v):
    """The SDK hands back bytes for strings it read off the wire (pyamqp); compare as text."""
    return v.decode() if isinstance(v, bytes) else v


def props_of(event):
    return {text(k): v for k, v in (event.properties or {}).items()}


def kafka_conf(emu):
    """Kafka in the namespace `myns` (from the Endpoint of the connection string), the namespace the AMQP side uses."""
    return sasl_plain(emu, ehutil.NAMESPACE)


@pytest.fixture
def hub(emu):
    name = kutil.uniq("xp")
    ehutil.admin_create_hub(emu, name, 2)
    return name


def test_kafka_to_amqp_body_key_headers_and_offsets(emu, hub, track):
    p = track(producer(kafka_conf(emu)))
    headers = [("trace-id", b"abc-123"), ("count", (7).to_bytes(4, "big")), ("empty", b""), ("nothing", None)]
    sent = produce_all(p, hub, [(b"user-42", b'{"n": 1}'), (b"user-42", b'{"n": 2}')], headers=headers)
    events = ehutil.read_from_start(emu, hub, 2, partition_id=str(sent[0].partition()))
    assert [e.body_as_str() for e in events] == ['{"n": 1}', '{"n": 2}']
    assert all(text(e.partition_key) == "user-42" for e in events)  # the record key is the partition key
    assert [e.sequence_number for e in events] == [m.offset() for m in sent]  # the offset is the sequence number
    props = props_of(events[0])
    assert props["trace-id"] == b"abc-123" and props["count"] == (7).to_bytes(4, "big") and props["empty"] == b""
    assert props["nothing"] is None
    assert events[0].enqueued_time is not None


def test_kafka_to_amqp_binary_key_and_null_value(emu, hub, track):
    p = track(producer(kafka_conf(emu)))
    produce_all(p, hub, [(None, b"no key")], partition=0)
    reports = []
    p.produce(hub, key=b"\xff\xfe", value=None, partition=0, on_delivery=lambda e, m: reports.append(e))
    p.flush(10)
    assert reports == [None]
    events = ehutil.read_from_start(emu, hub, 2, partition_id="0")
    assert events[0].body_as_str() == "no key" and not events[0].partition_key
    assert events[1].partition_key is not None  # a key that is not UTF-8 is still kept


def test_kafka_batch_with_gzip_is_read_event_by_event_through_amqp(emu, hub, track):
    p = track(producer(kafka_conf(emu), **{"compression.type": "gzip", "linger.ms": 50}))
    items = [f"event-{i:03d}".encode() for i in range(100)]
    produce_all(p, hub, items, partition=1)
    events = ehutil.read_from_start(emu, hub, 100, partition_id="1")
    assert [b"".join(e.body) for e in events] == items


def test_amqp_to_kafka_body_partition_key_and_properties(emu, hub, track):
    prod = track(ehutil.make_producer(emu, hub))
    batch = prod.create_batch(partition_id="1")
    ev = EventData("amqp body")
    ev.properties = {"MyStringProperty": "hello", "MyIntegerProperty": 1234, "flag": True}
    ev.content_type = "text/plain"
    ev.correlation_id = "corr-1"
    batch.add(ev)
    prod.send_batch(batch)
    kp = track(producer(kafka_conf(emu)))
    kp.flush(1)
    c = track(consumer(kafka_conf(emu), kutil.uniq("g")))
    c.subscribe([hub])
    (m,) = consume(c, 1)
    assert (m.partition(), m.offset()) == (1, 0)
    assert m.value() == b"amqp body"
    h = dict(m.headers())
    # the property values are AMQP encoded: the consumer must decode them (and the best practice is to look for a
    # property that says so); the content type is one of the properties Event Hubs turns into a plain string
    assert decode_amqp(h["MyStringProperty"]) == "hello"
    assert decode_amqp(h["MyIntegerProperty"]) == 1234
    assert decode_amqp(h["flag"]) is True
    assert h["content-type"] == b"text/plain"


def test_amqp_partition_key_becomes_the_kafka_key(emu, hub, track):
    prod = track(ehutil.make_producer(emu, hub))
    batch = prod.create_batch(partition_key="customer-7")
    batch.add(EventData("one"))
    batch.add(EventData("two"))
    prod.send_batch(batch)
    c = track(consumer(kafka_conf(emu), kutil.uniq("g")))
    c.subscribe([hub])
    got = consume(c, 2)
    assert sorted(m.value() for m in got) == [b"one", b"two"]
    assert {m.key() for m in got} == {b"customer-7"}
    assert len({m.partition() for m in got}) == 1  # one partition key, one partition


def test_a_hub_created_by_amqp_is_a_topic_and_a_topic_created_by_kafka_is_a_hub(emu, track):
    from confluent_kafka.admin import NewTopic

    name = kutil.uniq("shared")
    ehutil.admin_create_hub(emu, name, 4)  # the emulator does not create hubs by being named
    prod = track(ehutil.make_producer(emu, name))
    prod.send_batch(prod.create_batch(partition_id="0"))
    a = kutil.admin(kafka_conf(emu))
    md = a.list_topics(timeout=10)
    assert name in md.topics and len(md.topics[name].partitions) == 4
    other = kutil.uniq("kcreated")
    a.create_topics([NewTopic(other, num_partitions=2)])[other].result(10)
    assert ehutil.make_producer(emu, other).get_partition_ids() == ["0", "1"]


def test_both_protocols_can_write_to_the_same_partition_and_both_read_everything(emu, hub, track):
    kp = track(producer(kafka_conf(emu)))
    produce_all(kp, hub, [b"k1"], partition=0)
    ap = track(ehutil.make_producer(emu, hub))
    ehutil.send_each(ap, ["a1"], partition_id="0")
    produce_all(kp, hub, [b"k2"], partition=0)
    events = ehutil.read_from_start(emu, hub, 3, partition_id="0")
    assert [e.body_as_str() for e in events] == ["k1", "a1", "k2"]
    assert [e.sequence_number for e in events] == [0, 1, 2]
    c = track(consumer(kafka_conf(emu), kutil.uniq("g")))
    c.assign([kutil.TopicPartition(hub, 0, 0)] if hasattr(kutil, "TopicPartition") else [__import__("confluent_kafka").TopicPartition(hub, 0, 0)])
    got = consume(c, 3)
    assert [m.value() for m in got] == [b"k1", b"a1", b"k2"]
    assert [m.offset() for m in got] == [0, 1, 2]


def test_kafka_consumer_groups_are_not_amqp_consumer_groups(emu, hub, track):
    # a Kafka group name does not create or need an AMQP consumer group, and the AMQP groups are not Kafka groups
    p = track(producer(kafka_conf(emu)))
    produce_all(p, hub, [b"x"], partition=0)
    group = kutil.uniq("kgroup")
    c = track(consumer(kafka_conf(emu), group))
    c.subscribe([hub])
    consume(c, 1)
    c.commit(asynchronous=False)
    info = kutil.admin_get(emu, hub, ehutil.NAMESPACE)
    assert group not in [g.lower() for g in info.get("consumerGroups", [])]
    assert "$Default" in info["consumerGroups"] or "$default" in [g.lower() for g in info["consumerGroups"]]
    assert group in [g["group"] for g in kutil.admin_groups(emu, ehutil.NAMESPACE)]


def test_retention_applies_to_kafka_readers(emu, track):
    name = kutil.uniq("short")
    ehutil.admin_create_hub(emu, name, 1)
    kutil.admin_create(emu, name, 1, namespace=ehutil.NAMESPACE, retention_seconds=2)
    p = track(producer(kafka_conf(emu)))
    produce_all(p, name, [b"old1", b"old2", b"old3"], partition=0)
    c = track(consumer(kafka_conf(emu), kutil.uniq("g")))
    low, high = c.get_watermark_offsets(kutil.TopicPartition(name, 0) if hasattr(kutil, "TopicPartition") else __import__("confluent_kafka").TopicPartition(name, 0), timeout=10)
    assert (low, high) == (0, 3)
    time.sleep(3)
    low, high = c.get_watermark_offsets(__import__("confluent_kafka").TopicPartition(name, 0), timeout=10)
    assert (low, high) == (3, 3)  # the expired events are gone, the sequence numbers go on
    produce_all(p, name, [b"new"], partition=0)
    c.assign([__import__("confluent_kafka").TopicPartition(name, 0, 0)])  # offset 0 is gone: the reset policy applies
    (m,) = consume(c, 1)
    assert (m.offset(), m.value()) == (3, b"new")


def test_reset_clears_topics_groups_and_offsets(emu, hub, track):
    import urllib.request

    p = track(producer(kafka_conf(emu)))
    produce_all(p, hub, [b"x"], partition=0)
    group = kutil.uniq("g")
    c = track(consumer(kafka_conf(emu), group))
    c.subscribe([hub])
    got = consume(c, 1)
    c.commit(message=got[0], asynchronous=False)
    assert any(g["group"] == group for g in kutil.admin_groups(emu, ehutil.NAMESPACE))
    urllib.request.urlopen(urllib.request.Request(f"{emu.gateway}/_azemu/reset", method="POST", data=b""), timeout=10).read()
    assert kutil.admin_groups(emu, ehutil.NAMESPACE) == []
    assert kutil.admin_get(emu, hub, ehutil.NAMESPACE) is None
    md = kutil.admin(kafka_conf(emu)).list_topics(timeout=10)
    assert hub not in md.topics
