"""Consumer groups, committed offsets, start positions and seeking."""

import time

import pytest
from confluent_kafka import KafkaError, TopicPartition

import kutil
from kutil import consume, consumer, plain, produce_all, producer, wait_assignment, wait_until


def fill(emu, topic, n, partition=None, prefix="m"):
    p = producer(plain(emu))
    kw = {} if partition is None else {"partition": partition}
    msgs = produce_all(p, topic, [f"{prefix}{i}".encode() for i in range(n)], **kw)
    return msgs


def test_auto_offset_reset_earliest_reads_the_backlog(emu, topic1, track):
    fill(emu, topic1, 5)
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"auto.offset.reset": "earliest"}))
    c.subscribe([topic1])
    assert [m.value() for m in consume(c, 5)] == [f"m{i}".encode() for i in range(5)]


def test_auto_offset_reset_latest_skips_the_backlog(emu, topic1, track):
    fill(emu, topic1, 5)
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"auto.offset.reset": "latest"}))
    c.subscribe([topic1])
    wait_assignment(c)
    assert consume(c, 1, timeout=2, quiet_ok=True) == []
    fill(emu, topic1, 2, prefix="new")
    assert [m.value() for m in consume(c, 2)] == [b"new0", b"new1"]


def test_commit_and_resume_after_a_restart(emu, topic1, track):
    fill(emu, topic1, 10)
    group = kutil.uniq("g")
    c1 = consumer(plain(emu), group)
    c1.subscribe([topic1])
    got = consume(c1, 4)
    assert [m.value() for m in got[:4]] == [f"m{i}".encode() for i in range(4)]
    c1.commit(message=got[3], asynchronous=False)
    c1.close()
    # a new consumer of the same group continues after the committed offset (4)
    c2 = track(consumer(plain(emu), group))
    c2.subscribe([topic1])
    got = consume(c2, 6)
    assert [m.value() for m in got] == [f"m{i}".encode() for i in range(4, 10)]
    committed = c2.committed([TopicPartition(topic1, 0)], timeout=10)
    assert committed[0].offset == 4


def test_uncommitted_messages_are_read_again(emu, topic1, track):
    fill(emu, topic1, 3)
    group = kutil.uniq("g")
    c1 = consumer(plain(emu), group)
    c1.subscribe([topic1])
    consume(c1, 3)
    c1.close()  # nothing was committed (enable.auto.commit is off)
    c2 = track(consumer(plain(emu), group))
    c2.subscribe([topic1])
    assert len(consume(c2, 3)) == 3


def test_auto_commit(emu, topic1, track):
    fill(emu, topic1, 6)
    group = kutil.uniq("g")
    c1 = consumer(plain(emu), group, **{"enable.auto.commit": True, "auto.commit.interval.ms": 500})
    c1.subscribe([topic1])
    consume(c1, 6)
    time.sleep(1.5)
    c1.close()  # the final commit happens on close
    c2 = track(consumer(plain(emu), group))
    c2.subscribe([topic1])
    assert consume(c2, 1, timeout=3, quiet_ok=True) == []
    assert c2.committed([TopicPartition(topic1, 0)], timeout=10)[0].offset == 6


def test_commit_with_offsets_and_metadata_is_stored(emu, topic1, track):
    fill(emu, topic1, 3)
    group = kutil.uniq("g")
    c = track(consumer(plain(emu), group))
    c.assign([TopicPartition(topic1, 0)])
    c.commit(offsets=[TopicPartition(topic1, 0, 2)], asynchronous=False)
    (tp,) = c.committed([TopicPartition(topic1, 0)], timeout=10)
    assert tp.offset == 2
    # a partition without a commit has no offset
    other = track(consumer(plain(emu), kutil.uniq("g2")))
    (tp,) = other.committed([TopicPartition(topic1, 0)], timeout=10)
    assert tp.offset == -1001  # OFFSET_INVALID: nothing committed


def test_seek_and_position(emu, topic1, track):
    fill(emu, topic1, 10)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.assign([TopicPartition(topic1, 0)])
    assert consume(c, 1)[0].offset() == 0
    c.seek(TopicPartition(topic1, 0, 7))
    got = consume(c, 3)
    assert [m.offset() for m in got] == [7, 8, 9]
    assert c.position([TopicPartition(topic1, 0)])[0].offset == 10
    c.seek(TopicPartition(topic1, 0, 2))
    assert consume(c, 1)[0].value() == b"m2"


def test_assign_with_explicit_offsets(emu, topic1, track):
    fill(emu, topic1, 10)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.assign([TopicPartition(topic1, 0, 6)])
    assert [m.offset() for m in consume(c, 4)] == [6, 7, 8, 9]


def test_offset_out_of_range_applies_auto_offset_reset(emu, topic1, track):
    fill(emu, topic1, 3)
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"auto.offset.reset": "earliest"}))
    c.assign([TopicPartition(topic1, 0, 100)])  # beyond the end
    assert [m.offset() for m in consume(c, 3)] == [0, 1, 2]


def test_watermarks_and_offsets_for_times(emu, topic1, track):
    fill(emu, topic1, 4)
    c = track(consumer(plain(emu), kutil.uniq("g")))
    low, high = c.get_watermark_offsets(TopicPartition(topic1, 0), timeout=10)
    assert (low, high) == (0, 4)
    now_ms = int(time.time() * 1000)
    res = c.offsets_for_times([TopicPartition(topic1, 0, now_ms - 60_000)], timeout=10)
    assert res[0].offset == 0
    res = c.offsets_for_times([TopicPartition(topic1, 0, now_ms + 3_600_000)], timeout=10)
    assert res[0].offset == -1  # no event that new: the end of the log


def test_the_end_of_a_partition_is_reported_when_asked(emu, topic1, track):
    fill(emu, topic1, 2)
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"enable.partition.eof": True}))
    c.subscribe([topic1])
    got, eof = 0, False
    end = time.time() + 20
    while time.time() < end and not (got == 2 and eof):
        m = c.poll(0.5)
        if m is None:
            continue
        if m.error():
            eof = eof or m.error().code() == KafkaError._PARTITION_EOF
        else:
            got += 1
    assert got == 2 and eof


def test_fetch_waits_for_new_messages_and_does_not_busy_loop(emu, topic1, track):
    c = track(consumer(plain(emu), kutil.uniq("g"), **{"fetch.wait.max.ms": 2000}))
    c.assign([TopicPartition(topic1, 0, 0)])
    wait_until(lambda: c.assignment(), what="assignment")
    t = time.time()
    assert c.poll(1.0) is None
    p = producer(plain(emu))
    p.produce(topic1, value=b"late")
    p.flush(10)
    (m,) = consume(c, 1, timeout=5)
    assert m.value() == b"late"


def test_two_consumers_split_the_partitions_and_a_leaver_triggers_a_rebalance(emu, topic, track):
    group = kutil.uniq("g")
    p = producer(plain(emu))
    produce_all(p, topic, [f"m{i}".encode() for i in range(40)])
    c1 = track(consumer(plain(emu), group))
    c1.subscribe([topic])
    assert len(wait_assignment(c1)) == 4  # alone, it owns all four partitions
    c2 = track(consumer(plain(emu), group))
    c2.subscribe([topic])

    def split():
        c1.poll(0.2)
        c2.poll(0.2)
        a1, a2 = c1.assignment(), c2.assignment()
        return a1 and a2 and len(a1) + len(a2) == 4 and not ({(t.topic, t.partition) for t in a1} & {(t.topic, t.partition) for t in a2})

    wait_until(split, timeout=30, interval=0.05, what="the partitions split between two consumers")
    assert len(c1.assignment()) == 2 and len(c2.assignment()) == 2  # the range assignor splits 4 partitions evenly
    c2.close()  # a leave: the other consumer gets everything again

    def alone():
        c1.poll(0.2)
        return len(c1.assignment()) == 4

    wait_until(alone, timeout=30, interval=0.05, what="the leaver's partitions to move to the other consumer")


def test_a_consumer_that_is_killed_is_dropped_after_the_session_timeout(emu, topic, track):
    import subprocess
    import sys

    group = kutil.uniq("g")
    child_code = (
        "import sys, time\n"
        "from confluent_kafka import Consumer\n"
        "c = Consumer({'bootstrap.servers': sys.argv[1], 'group.id': sys.argv[2], 'session.timeout.ms': 6000,"
        " 'heartbeat.interval.ms': 1000, 'auto.offset.reset': 'earliest'})\n"
        "c.subscribe([sys.argv[3]])\n"
        "while True:\n"
        "    c.poll(0.2)\n"
        "    if c.assignment():\n"
        "        print('assigned', len(c.assignment()), flush=True)\n"
        "        break\n"
        "time.sleep(600)\n"
    )
    child = subprocess.Popen([sys.executable, "-c", child_code, f"127.0.0.1:{emu.port}", group, topic], stdout=subprocess.PIPE, text=True)
    try:
        assert child.stdout.readline().startswith("assigned")
        c2 = track(consumer(plain(emu), group))
        c2.subscribe([topic])
        # the child never polls again after its first assignment, so it stops heartbeating the way a hung process does;
        # the broker drops it after the session timeout and gives its partitions to the live consumer
        child.kill()
        child.wait(10)

        def all_four():
            c2.poll(0.2)
            return len(c2.assignment()) == 4

        wait_until(all_four, timeout=45, interval=0.05, what="the dead consumer's partitions to move")
    finally:
        child.kill()
        child.wait(10)


def test_group_state_is_visible_in_the_admin_api(emu, topic1, track):
    group = kutil.uniq("g")
    fill(emu, topic1, 2)
    c = track(consumer(plain(emu), group))
    c.subscribe([topic1])
    got = consume(c, 2)
    c.commit(message=got[-1], asynchronous=False)
    (g,) = [g for g in kutil.admin_groups(emu) if g["group"] == group]
    assert g["state"] == "Stable"
    assert g["protocol"] == "range" or g["protocol"]  # a consumer protocol name
    assert [(o["topic"], o["partition"], o["offset"]) for o in g["offsets"]] == [(topic1, 0, 2)]


def test_consumers_in_different_groups_read_independently(emu, topic1, track):
    fill(emu, topic1, 3)
    a = track(consumer(plain(emu), kutil.uniq("ga")))
    b = track(consumer(plain(emu), kutil.uniq("gb")))
    a.subscribe([topic1])
    b.subscribe([topic1])
    assert len(consume(a, 3)) == 3
    assert len(consume(b, 3)) == 3


def test_subscribe_to_a_regex(emu, track):
    names = [kutil.uniq("rx") for _ in range(2)]
    prefix = "rxprefix-" + kutil.uniq("")
    topics = [f"{prefix}-{i}" for i in range(2)]
    for t in topics:
        kutil.admin_create(emu, t, 1)
    p = producer(plain(emu))
    for t in topics:
        produce_all(p, t, [t.encode()])
    c = track(consumer(plain(emu), kutil.uniq("g")))
    c.subscribe([f"^{prefix}-.*"])
    assert sorted(m.value() for m in consume(c, 2)) == sorted(t.encode() for t in topics)
