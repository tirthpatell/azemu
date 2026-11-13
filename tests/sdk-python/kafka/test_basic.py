"""Produce and consume with librdkafka: keys, partitions, headers, batches, compression, the idempotent producer."""

import pytest
from confluent_kafka import KafkaError, TopicPartition

import kutil
from kutil import consume, consumer, plain, produce_all, producer


def test_produce_and_consume(emu, topic, track):
    p = track(producer(plain(emu)))
    sent = produce_all(p, topic, [b"hello", b"world", b"!"], partition=0)
    assert [m.offset() for m in sent] == [0, 1, 2]
    assert {m.partition() for m in sent} == {0}
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic])
    got = consume(c, 3)
    assert [m.value() for m in got] == [b"hello", b"world", b"!"]
    assert [m.offset() for m in got] == [0, 1, 2]
    assert all(m.topic() == topic for m in got)


def test_keys_pick_partitions_and_come_back(emu, topic, track):
    p = track(producer(plain(emu)))
    items = [(f"user-{i % 5}".encode(), f"v{i}".encode()) for i in range(40)]
    sent = produce_all(p, topic, items)
    by_key = {}
    for m in sent:
        by_key.setdefault(m.key(), set()).add(m.partition())
    assert all(len(parts) == 1 for parts in by_key.values()), by_key  # one key, one partition
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic])
    got = consume(c, 40)
    assert sorted((m.key(), m.value()) for m in got) == sorted(items)


def test_explicit_partitions_and_per_partition_offsets(emu, topic, track):
    p = track(producer(plain(emu)))
    for part in (0, 1, 2, 3):
        sent = produce_all(p, topic, [b"a", b"b"], partition=part)
        assert [(m.partition(), m.offset()) for m in sent] == [(part, 0), (part, 1)]
    produce_all(p, topic, [b"c"], partition=2)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.assign([TopicPartition(topic, 2, 0)])
    assert [m.value() for m in consume(c, 3)] == [b"a", b"b", b"c"]


def test_headers_round_trip_including_null_and_repeated(emu, topic1, track):
    p = track(producer(plain(emu)))
    headers = [("trace", b"abc"), ("empty", b""), ("nothing", None), ("trace", b"def"), ("bin", bytes(range(256)))]
    produce_all(p, topic1, [b"x"], headers=headers)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    (m,) = consume(c, 1)
    assert m.headers() == headers


def test_null_key_and_null_value_and_empty_value(emu, topic1, track):
    p = track(producer(plain(emu)))
    reports = []
    p.produce(topic1, value=None, key=b"k", on_delivery=lambda e, m: reports.append(e))
    p.produce(topic1, value=b"", key=None, on_delivery=lambda e, m: reports.append(e))
    assert p.flush(10) == 0 and reports == [None, None]
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    a, b = consume(c, 2)
    assert (a.key(), a.value()) == (b"k", None)
    assert (b.key(), b.value()) in {(None, b""), (None, None)}


def test_large_batches_keep_order(emu, topic1, track):
    p = track(producer(plain(emu), **{"linger.ms": 50, "batch.num.messages": 500}))
    items = [f"msg-{i:05d}".encode() * 10 for i in range(3000)]
    sent = produce_all(p, topic1, items)
    assert [m.offset() for m in sent] == list(range(3000))
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    got = consume(c, 3000, timeout=40)
    assert [m.value() for m in got] == items


def test_gzip_compression(emu, topic1, track):
    p = track(producer(plain(emu), **{"compression.type": "gzip", "linger.ms": 20}))
    items = [(b"k", b"compress me " * 200 + str(i).encode()) for i in range(50)]
    produce_all(p, topic1, items)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    got = consume(c, 50)
    assert [m.value() for m in got] == [v for _, v in items]


@pytest.mark.parametrize("codec", ["snappy", "lz4", "zstd"])
def test_other_compression_codecs_are_refused_as_event_hubs_does(emu, topic1, track, codec):
    # Event Hubs for Kafka supports gzip only: other codecs get the "message format version" error
    p = track(producer(plain(emu), **{"compression.type": codec, "message.timeout.ms": 8000, "retries": 0}))
    errors = []
    p.produce(topic1, value=b"x" * 1000, on_delivery=lambda e, m: errors.append(e))
    p.flush(15)
    assert len(errors) == 1 and errors[0] is not None
    assert errors[0].code() == KafkaError.UNSUPPORTED_FOR_MESSAGE_FORMAT, errors[0]


def test_idempotent_producer_sends_in_order_without_gaps(emu, topic1, track):
    p = track(producer(plain(emu), **{"enable.idempotence": True, "linger.ms": 5, "max.in.flight.requests.per.connection": 5}))
    items = [f"i{i}".encode() for i in range(500)]
    sent = produce_all(p, topic1, items)
    assert [m.offset() for m in sent] == list(range(500))
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    assert [m.value() for m in consume(c, 500)] == items


def test_two_idempotent_producers_have_separate_sequences(emu, topic1, track):
    a = track(producer(plain(emu), **{"enable.idempotence": True}))
    b = track(producer(plain(emu), **{"enable.idempotence": True}))
    produce_all(a, topic1, [b"a1", b"a2"])
    produce_all(b, topic1, [b"b1"])
    produce_all(a, topic1, [b"a3"])
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([topic1])
    assert [m.value() for m in consume(c, 4)] == [b"a1", b"a2", b"b1", b"a3"]


def test_a_message_over_the_event_size_limit_is_refused(emu, topic1, track):
    # the broker limit is 1 MiB (Event Hubs Standard); raise the client limits so the broker answers
    p = track(producer(plain(emu), **{"message.max.bytes": 5_000_000, "message.timeout.ms": 8000}))
    errors = []
    p.produce(topic1, value=b"x" * 2_000_000, on_delivery=lambda e, m: errors.append(e))
    p.flush(15)
    assert errors and errors[0].code() == KafkaError.MSG_SIZE_TOO_LARGE, errors


def test_produce_to_a_missing_partition_fails(emu, topic1, track):
    p = track(producer(plain(emu), **{"message.timeout.ms": 6000}))
    errors = []
    p.produce(topic1, value=b"x", partition=9, on_delivery=lambda e, m: errors.append(e))
    p.flush(10)
    assert errors and errors[0] is not None


def test_transactions_are_not_available(emu, topic1, track):
    from confluent_kafka import KafkaException

    p = track(producer(plain(emu), **{"transactional.id": kutil.uniq("tx"), "transaction.timeout.ms": 10000}))
    with pytest.raises(KafkaException):
        p.init_transactions(10)
