"""The admin client: topics, configs, groups; and what Event Hubs does not have."""

import time

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
    # the address the connection came in on (Azure: the namespace's own host name and port 9093)
    assert (broker.host, broker.port) == ((kutil.azure_eh.fqdn(), 9093) if emu.azure else ("127.0.0.1", emu.port))
    assert len(md.topics[names[0]].partitions) == 3
    assert len(md.topics[names[1]].partitions) == 1
    assert all(p.leader == broker.id for p in md.topics[names[0]].partitions.values())


def test_partitions_list_no_replicas_and_no_in_sync_replicas(emu, topic):
    # Azure leaves both lists empty (a real namespace, November 2026)
    md = admin(plain(emu)).list_topics(topic, timeout=10)
    assert all(p.replicas == [] and p.isrs == [] for p in md.topics[topic].partitions.values())


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


def test_create_topic_without_a_count_or_a_retention_makes_one_partition_and_seven_days(emu):
    a = admin(plain(emu))
    name = kutil.uniq("ret")
    kutil.CREATED.append(name)
    a.create_topics([NewTopic(name, num_partitions=-1, replication_factor=-1)])[name].result(30)
    hub = kutil.admin_get(emu, name)
    assert hub["partitionCount"] == 1 and hub["retentionSeconds"] == 7 * 86400
    res = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, name)])
    cfg = list(res.values())[0].result(10)
    # the four settings Azure lists (a real namespace, November 2026): no max.message.bytes
    assert sorted(cfg) == ["cleanup.policy", "message.timestamp.difference.max.ms", "message.timestamp.type", "retention.ms"]
    assert cfg["retention.ms"].value == str(7 * 86400 * 1000)
    assert cfg["cleanup.policy"].value == "delete"
    assert cfg["message.timestamp.type"].value == "LogAppendTime"
    assert cfg["message.timestamp.difference.max.ms"].value == "7776000000"


def test_create_topic_with_a_retention_in_whole_days(emu):
    # Azure Standard rounds a shorter retention up to whole days (2 hours became 1 day) and refuses more than 7 days; the emulator keeps what
    # it is given, so only whole days are compared here
    a = admin(plain(emu))
    name = kutil.uniq("ret3")
    kutil.CREATED.append(name)
    a.create_topics([NewTopic(name, num_partitions=2, replication_factor=3, config={"retention.ms": str(3 * 86400 * 1000), "no.such.config": "1"})])[name].result(30)
    hub = kutil.admin_get(emu, name)
    assert hub["partitionCount"] == 2 and hub["retentionSeconds"] == 3 * 86400
    cfg = list(a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, name)]).values())[0].result(10)
    assert cfg["retention.ms"].value == str(3 * 86400 * 1000)


def test_a_retention_of_minus_one_is_refused(emu):
    a = admin(plain(emu))
    name = kutil.uniq("retinf")
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(name, num_partitions=1, config={"retention.ms": "-1"})])[name].result(30)
    assert e.value.args[0].code() == KafkaError.INVALID_CONFIG
    assert kutil.admin_get(emu, name) is None


@pytest.mark.azure_differs("by design: the emulator does not compact, so it refuses cleanup.policy=compact with INVALID_CONFIG; Azure Standard creates a compacted event hub (Microsoft documents compaction for Premium and Dedicated)")
def test_compaction_is_refused(emu):
    a = admin(plain(emu))
    name = kutil.uniq("compact")
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(name, num_partitions=1, config={"cleanup.policy": "compact"})])[name].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_CONFIG
    assert kutil.admin_get(emu, name) is None


def test_compact_and_delete_together_is_refused(emu):
    a = admin(plain(emu))
    name = kutil.uniq("both")
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(name, num_partitions=1, config={"cleanup.policy": "compact,delete"})])[name].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_CONFIG
    assert kutil.admin_get(emu, name) is None


@pytest.mark.parametrize("bad", ["bad name!", "x" * 300, "..", "z\u00fcrich"], ids=["space", "300-chars", "dots", "non-ascii"])
def test_bad_topic_names_are_refused(emu, bad):
    # INVALID_CONFIG on a real namespace (November 2026), not INVALID_TOPIC_EXCEPTION
    a = admin(plain(emu))
    with pytest.raises(KafkaException) as e:
        a.create_topics([NewTopic(bad, num_partitions=1)])[bad].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_CONFIG


def test_deleted_topic_is_gone_for_amqp_too_and_can_be_created_again(emu):
    a = admin(plain(emu))
    name = kutil.uniq("again")
    a.create_topics([NewTopic(name, num_partitions=2)])[name].result(10)
    a.delete_topics([name])[name].result(10)
    a.create_topics([NewTopic(name, num_partitions=1)])[name].result(10)
    assert kutil.admin_get(emu, name)["partitionCount"] == 1


def test_changing_the_partition_count_is_refused(emu, topic):
    a = admin(plain(emu))
    with pytest.raises(KafkaException) as e:
        a.create_partitions([NewPartitions(topic, 8)])[topic].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_REQUEST
    assert kutil.admin_get(emu, topic)["partitionCount"] == 4


def test_describe_topic_configs(emu, topic):
    a = admin(plain(emu))
    res = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, topic)])
    cfg = list(res.values())[0].result(10)
    assert cfg["retention.ms"].value == str(24 * 3600 * 1000)  # the fixture makes the hub with one day
    assert cfg["cleanup.policy"].value == "delete"


def test_describe_configs_of_a_missing_topic(emu):
    # no error and no settings, and nothing is made (a real namespace, November 2026)
    a = admin(plain(emu))  # keep the client: a destroyed admin client never completes its futures
    name = kutil.uniq("nosuch")
    missing = a.describe_configs([ConfigResource(ConfigResource.Type.TOPIC, name)])
    assert list(missing.values())[0].result(10) == {}
    assert kutil.admin_get(emu, name) is None


def test_describe_broker_configs_is_not_supported(emu, topic):
    a = admin(plain(emu))
    res = a.describe_configs([ConfigResource(ConfigResource.Type.BROKER, "0")])
    with pytest.raises(KafkaException) as e:
        list(res.values())[0].result(10)
    assert e.value.args[0].code() == KafkaError.INVALID_REQUEST  # "Broker configuration reads are not supported on this cluster."


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


def test_a_group_whose_last_member_left_is_forgotten_but_its_commit_can_still_be_read(emu, topic1, track):
    # a real namespace (November 2026): two seconds after the member left the group is not listed, describes as DEAD, and the request for
    # every partition returns nothing; the commit is still there for a request that names the partition
    from confluent_kafka import ConsumerGroupTopicPartitions

    group = kutil.uniq("g")
    produce_all(producer(plain(emu)), topic1, [b"a", b"b"])
    c = consumer(plain(emu), group)
    c.subscribe([topic1])
    got = consume(c, 2)
    c.commit(message=got[-1], asynchronous=False)
    a = admin(plain(emu))
    assert group in [g.group_id for g in a.list_consumer_groups().result(10).valid]  # while the member is there
    c.close()
    time.sleep(4)
    assert group not in [g.group_id for g in a.list_consumer_groups().result(10).valid]
    assert str(a.describe_consumer_groups([group])[group].result(10).state).endswith("DEAD")
    res = a.list_consumer_group_offsets([ConsumerGroupTopicPartitions(group)])[group].result(10)
    assert res.topic_partitions == []
    other = track(consumer(plain(emu), group))
    (tp,) = other.committed([TopicPartition(topic1, 0)], timeout=10)
    assert tp.offset == 2


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
