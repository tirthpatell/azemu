"""Topics that do not exist. A real namespace makes the event hub when a producer or an admin client names it (the metadata request allows
auto-creation): one partition, 7 days; a consumer's request does not allow it and creates nothing (November 2026). The emulator does the
same whatever AZEMU_EVENTHUBS_AUTOCREATE says; that setting is about AMQP."""

from confluent_kafka import KafkaError

import kutil
from kutil import consumer, plain, producer


def test_producing_to_a_missing_topic_creates_an_event_hub_and_delivers_the_record(emu, track):
    name = kutil.uniq("auto")
    kutil.CREATED.append(name)  # on Azure the hub is removed when the test ends
    p = track(producer(plain(emu)))
    reports = []
    p.produce(name, value=b"first", on_delivery=lambda e, m: reports.append((e, m)))
    p.flush(30)
    assert reports and reports[0][0] is None
    assert reports[0][1].offset() == 0
    hub = kutil.admin_get(emu, name)
    assert hub is not None and hub["partitionCount"] == 1 and hub["retentionSeconds"] == 7 * 86400


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


def test_asking_the_admin_client_for_a_missing_topic_creates_it(emu):
    name = kutil.uniq("missingmd")
    kutil.CREATED.append(name)
    md = kutil.admin(plain(emu)).list_topics(name, timeout=30)
    assert md.topics[name].error is None and len(md.topics[name].partitions) == 1
    hub = kutil.admin_get(emu, name)
    assert hub is not None and hub["partitionCount"] == 1 and hub["retentionSeconds"] == 7 * 86400


def test_listing_all_topics_creates_nothing(emu):
    before = kutil.admin(plain(emu)).list_topics(timeout=10).topics
    after = kutil.admin(plain(emu)).list_topics(timeout=10).topics
    assert set(before) == set(after)


def test_a_consumer_cannot_create_a_topic(emu, track):
    name = kutil.uniq("missingc")
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"allow.auto.create.topics": True}))
    c.subscribe([name])
    for _ in range(6):
        c.poll(0.5)
    assert kutil.admin_get(emu, name) is None


def test_creating_the_topic_explicitly_works(emu):
    from confluent_kafka.admin import NewTopic

    name = kutil.uniq("explicit")
    a = kutil.admin(plain(emu))
    a.create_topics([NewTopic(name, num_partitions=2)])[name].result(10)
    assert kutil.admin_get(emu, name)["partitionCount"] == 2
