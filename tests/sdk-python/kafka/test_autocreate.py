"""Topics that do not exist: by default they are an error, as on Event Hubs (the `no_autocreate` tests); with
`AZEMU_EVENTHUBS_AUTOCREATE=1` Metadata creates them, as Kafka brokers do with `auto.create.topics.enable` (the `autocreate`
test, run in the runner's second pass). Two librdkafka behaviours that hold in both modes are not marked."""

import pytest
from confluent_kafka import KafkaError

import kutil
from kutil import consumer, plain, producer


@pytest.mark.autocreate
def test_producing_to_a_missing_topic_creates_an_event_hub(emu, track):
    name = kutil.uniq("auto")
    p = track(producer(plain(emu)))
    reports = []
    p.produce(name, value=b"first", on_delivery=lambda e, m: reports.append((e, m)))
    p.flush(15)
    assert reports and reports[0][0] is None
    hub = kutil.admin_get(emu, name)
    assert hub is not None and hub["partitionCount"] == 4  # the default a client-created hub gets


def test_a_librdkafka_consumer_does_not_create_a_missing_topic(emu, track):
    name = kutil.uniq("noauto")
    # librdkafka's consumer asks for the topic's metadata without the auto-creation flag, whatever allow.auto.create.topics
    # says (the Java consumer sets it; see the Java suite), so a missing topic stays missing
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"allow.auto.create.topics": True}))
    c.subscribe([name])
    errors = []
    for _ in range(12):
        m = c.poll(0.5)
        if m is not None and m.error():
            errors.append(m.error().code())
    assert kutil.admin_get(emu, name) is None
    assert KafkaError.UNKNOWN_TOPIC_OR_PART in errors or errors == []  # librdkafka reports the unknown topic as an error event


def test_listing_all_topics_creates_nothing(emu):
    before = kutil.admin(plain(emu)).list_topics(timeout=10).topics
    after = kutil.admin(plain(emu)).list_topics(timeout=10).topics
    assert set(before) == set(after)


@pytest.mark.no_autocreate
def test_producing_to_a_missing_topic_fails_without_auto_creation(emu, track):
    name = kutil.uniq("missing")
    p = track(producer(plain(emu), **{"message.timeout.ms": 6000}))
    reports = []
    p.produce(name, value=b"x", on_delivery=lambda e, m: reports.append(e))
    p.flush(15)
    assert reports and reports[0] is not None and reports[0].code() in (KafkaError._MSG_TIMED_OUT, KafkaError.UNKNOWN_TOPIC_OR_PART, KafkaError._UNKNOWN_TOPIC)
    assert kutil.admin_get(emu, name) is None


@pytest.mark.no_autocreate
def test_a_consumer_cannot_create_a_topic_without_auto_creation(emu, track):
    name = kutil.uniq("missingc")
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"allow.auto.create.topics": True}))
    c.subscribe([name])
    for _ in range(6):
        c.poll(0.5)
    assert kutil.admin_get(emu, name) is None


@pytest.mark.no_autocreate
def test_creating_the_topic_explicitly_works_without_auto_creation(emu):
    from confluent_kafka.admin import NewTopic

    name = kutil.uniq("explicit")
    a = kutil.admin(plain(emu))
    a.create_topics([NewTopic(name, num_partitions=2)])[name].result(10)
    assert kutil.admin_get(emu, name)["partitionCount"] == 2
