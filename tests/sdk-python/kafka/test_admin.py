"""The admin client: topics, configs, groups; and what Event Hubs does not have."""

import pytest
from confluent_kafka import KafkaError, KafkaException, TopicPartition
from confluent_kafka.admin import ConfigResource, NewPartitions, NewTopic

import kutil
from kutil import admin, consume, consumer, plain, produce_all, producer


def test_list_topics_shows_event_hubs_with_their_partitions(emu, track):
    names = [kutil.uniq("lt") for _ in range(2)]
    kutil.admin_create(emu, names[0], 3)
    kutil.admin_create(emu, names[1], 1)
    md = admin(plain(emu)).list_topics(timeout=10)
    assert len(md.brokers) == 1
    (broker,) = md.brokers.values()
    assert (broker.host, broker.port) == ("127.0.0.1", emu.port)  # the address the connection came in on
    assert len(md.topics[names[0]].partitions) == 3
    assert len(md.topics[names[1]].partitions) == 1
    assert all(p.leader == broker.id and p.replicas == [broker.id] and p.isrs == [broker.id] for p in md.topics[names[0]].partitions.values())


def test_list_topics_for_one_topic(emu, topic):
    md = admin(plain(emu)).list_topics(topic, timeout=10)
    assert list(md.topics) == [topic] and md.topics[topic].error is None


def test_create_and_delete_topic(emu):
    a = admin(plain(emu))
    name = kutil.uniq("created")
    futures = a.create_topics([NewTopic(name, num_partitions=3, replication_factor=1)])
    futures[name].result(10)
    hub = kutil.admin_get(emu, name)  # it is an event hub
    assert hub is not None and hub["partitionCount"] == 3
    assert len(a.list_topics(name, timeout=10).topics[name].partitions) == 3
    # a second create is refused
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(name, num_partitions=3)])[name].result(10)
    assert e.value.args[0].code() == KafkaError.TOPIC_ALREADY_EXISTS
    a.delete_topics([name])[name].result(10)
    assert kutil.admin_get(emu, name) is None
    with pytest.raises(KafkaException) as e:
        a.delete_topics([name])[name].result(10)
    assert e.value.args[0].code() == KafkaError.UNKNOWN_TOPIC_OR_PART


def test_create_topic_with_retention_and_default_partitions(emu):
    a = admin(plain(emu))
    name = kutil.uniq("ret")
    a.create_topics([NewTopic(name, num_partitions=-1, replication_factor=-1, config={"retention.ms": "7200000"})])[name].result(10)
    hub = kutil.admin_get(emu, name)
    assert hub["partitionCount"] == 4 and hub["retentionSeconds"] == 7200
    res = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, name)])
    cfg = list(res.values())[0].result(10)
    assert cfg["retention.ms"].value == "7200000"
    assert cfg["cleanup.policy"].value == "delete"
    assert int(cfg["max.message.bytes"].value) == 1048576


def test_compaction_and_bad_names_are_refused(emu):
    a = admin(plain(emu))
    name = kutil.uniq("compact")
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(name, num_partitions=1, config={"cleanup.policy": "compact"})])[name].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_CONFIG
    assert kutil.admin_get(emu, name) is None
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic("bad name!", num_partitions=1)])["bad name!"].result(10)
    assert e.value.args[0].code() == KafkaError.TOPIC_EXCEPTION


def test_deleted_topic_is_gone_for_amqp_too_and_can_be_created_again(emu):
    a = admin(plain(emu))
    name = kutil.uniq("again")
    a.create_topics([NewTopic(name, num_partitions=2)])[name].result(10)
    a.delete_topics([name])[name].result(10)
    a.create_topics([NewTopic(name, num_partitions=1)])[name].result(10)
    assert kutil.admin_get(emu, name)["partitionCount"] == 1


def test_changing_the_partition_count_is_refused(emu, topic):
    a = admin(plain(emu))
    with pytest.raises(KafkaException):
        a.create_partitions([NewPartitions(topic, 8)])[topic].result(10)
    assert kutil.admin_get(emu, topic)["partitionCount"] == 4


def test_describe_topic_configs_and_broker_configs(emu, topic):
    a = admin(plain(emu))
    res = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, topic), ConfigResource(ConfigResource.Type.BROKER, "0")])
    results = {r.restype: f.result(10) for r, f in res.items()}
    assert results[ConfigResource.Type.TOPIC]["retention.ms"].value == str(24 * 3600 * 1000)
    assert "num.partitions" in results[ConfigResource.Type.BROKER]
    missing = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, "no-such-topic-xyz")])
    with pytest.raises(KafkaException) as e:
        list(missing.values())[0].result(10)
    assert e.value.args[0].code() == KafkaError.UNKNOWN_TOPIC_OR_PART


def test_list_and_describe_consumer_groups(emu, topic1, track):
    group = kutil.uniq("g")
    p = producer(plain(emu))
    produce_all(p, topic1, [b"a", b"b"])
    c = track(consumer(plain(emu), group, **{"client.id": "describe-me"}))
    c.subscribe([topic1])
    got = consume(c, 2)
    c.commit(message=got[-1], asynchronous=False)
    a = admin(plain(emu))
    listing = a.list_consumer_groups().result(10)
    assert group in [g.group_id for g in listing.valid]
    desc = a.describe_consumer_groups([group])[group].result(10)
    assert desc.group_id == group
    assert str(desc.state).endswith("STABLE")
    assert len(desc.members) == 1
    m = desc.members[0]
    assert m.client_id == "describe-me"
    assert [(tp.topic, tp.partition) for tp in m.assignment.topic_partitions] == [(topic1, 0)]


def test_group_offsets_can_be_listed(emu, topic1, track):
    from confluent_kafka import ConsumerGroupTopicPartitions

    group = kutil.uniq("g")
    produce_all(producer(plain(emu)), topic1, [b"a", b"b", b"c"])
    c = track(consumer(plain(emu), group))
    c.subscribe([topic1])
    got = consume(c, 3)
    c.commit(message=got[-1], asynchronous=False)
    a = admin(plain(emu))
    res = a.list_consumer_group_offsets([ConsumerGroupTopicPartitions(group)])[group].result(10)
    assert [(tp.topic, tp.partition, tp.offset) for tp in res.topic_partitions] == [(topic1, 0, 3)]


def test_an_unknown_group_describes_as_dead(emu):
    a = admin(plain(emu))  # keep the client: a destroyed admin client never completes its futures
    desc = a.describe_consumer_groups(["nobody-" + kutil.uniq()])
    (f,) = desc.values()
    d = f.result(10)
    assert str(d.state).endswith("DEAD") and d.members == []


def test_metadata_is_the_event_hub_with_its_partition_count(emu, topic):
    c = consumer(plain(emu), kutil.uniq("g"))
    md = c.list_topics(topic, timeout=10)
    assert sorted(md.topics[topic].partitions) == [0, 1, 2, 3]
    c.close()
