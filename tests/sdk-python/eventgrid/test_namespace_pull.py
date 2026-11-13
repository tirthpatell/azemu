"""Namespace topics with pull delivery: EventGridPublisherClient(namespace_topic=...) and
EventGridConsumerClient (receive, acknowledge, release, reject, renew_locks).

The topic `jobs` and the subscription `sub` are declared through the emulator's admin API
(on Azure ARM creates them): see the `ns_topic` fixture.
"""

import threading
import time

import pytest
from azure.core.exceptions import HttpResponseError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent

from conftest import make_consumer, make_producer


def ce(data="payload", **kw):
    return CloudEvent(source="/test/source", type="Test.Event", data=data, **kw)


def receive_all(consumer, n=1, wait=3):
    return consumer.receive(max_events=n, max_wait_time=wait)


# -- send and receive -------------------------------------------------------------------

def test_send_receive_acknowledge(producer, consumer):
    producer.send(ce({"hello": "world"}, subject="s1"))
    got = receive_all(consumer)
    assert len(got) == 1
    d = got[0]
    assert d.event.type == "Test.Event"
    assert d.event.source == "/test/source"
    assert d.event.data == {"hello": "world"}
    assert d.event.subject == "s1"
    assert d.event.specversion == "1.0"
    assert d.event.id
    assert d.broker_properties.delivery_count == 1
    assert d.broker_properties.lock_token
    res = consumer.acknowledge(lock_tokens=[d.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == [d.broker_properties.lock_token]
    assert res.failed_lock_tokens == []
    assert consumer.receive(max_events=1, max_wait_time=1) == []


def test_receive_default_returns_one_event(producer, consumer):
    producer.send([ce("a"), ce("b"), ce("c")])
    got = consumer.receive(max_wait_time=3)
    assert len(got) == 1


def test_events_are_received_in_publish_order(producer, consumer):
    producer.send([ce(str(i)) for i in range(10)])
    got = consumer.receive(max_events=10, max_wait_time=3)
    assert [d.event.data for d in got] == [str(i) for i in range(10)]


def test_max_events_limits_the_batch_and_the_rest_stays_available(producer, consumer):
    producer.send([ce(str(i)) for i in range(5)])
    first = consumer.receive(max_events=2, max_wait_time=3)
    rest = consumer.receive(max_events=10, max_wait_time=3)
    assert [d.event.data for d in first] == ["0", "1"]
    assert [d.event.data for d in rest] == ["2", "3", "4"]


def test_a_locked_event_is_not_returned_again(producer, consumer):
    producer.send(ce())
    assert len(receive_all(consumer)) == 1
    assert consumer.receive(max_events=1, max_wait_time=1) == []


@pytest.mark.parametrize("data", ["text", {"k": [1, 2, {"n": None}]}, 42, [1, 2, 3], True])
def test_data_types_round_trip(producer, consumer, data):
    producer.send(ce(data))
    (d,) = receive_all(consumer)
    assert d.event.data == data


def test_bytes_data_round_trips_as_data_base64(producer, consumer):
    producer.send(ce(b"\x00\x01binary\xff"))
    (d,) = receive_all(consumer)
    assert d.event.data == b"\x00\x01binary\xff"


def test_event_without_data(producer, consumer):
    producer.send(CloudEvent(source="/s", type="t"))
    (d,) = receive_all(consumer)
    assert d.event.data is None


def test_cloud_event_attributes_and_extensions_round_trip(producer, consumer):
    producer.send(
        CloudEvent(
            source="/s",
            type="t",
            data={"a": 1},
            subject="subj",
            dataschema="https://example.com/schema.json",
            datacontenttype="application/json",
            extensions={"customext": "value", "reasoncode": 204},
        )
    )
    (d,) = receive_all(consumer)
    assert d.event.subject == "subj"
    assert d.event.dataschema == "https://example.com/schema.json"
    assert d.event.datacontenttype == "application/json"
    assert d.event.extensions["customext"] == "value"
    assert d.event.extensions["reasoncode"] == 204


def test_event_id_and_time_are_kept(producer, consumer):
    sent = ce()
    producer.send(sent)
    (d,) = receive_all(consumer)
    assert d.event.id == sent.id
    assert d.event.time == sent.time


def test_send_dict_cloud_event(producer, consumer):
    producer.send({"id": "my-id", "source": "/s", "specversion": "1.0", "type": "dict.type", "data": {"x": 1}})
    (d,) = receive_all(consumer)
    assert d.event.id == "my-id"
    assert d.event.type == "dict.type"


def test_send_list_of_dicts(producer, consumer):
    producer.send([{"id": f"i{i}", "source": "/s", "specversion": "1.0", "type": "t"} for i in range(3)])
    got = consumer.receive(max_events=5, max_wait_time=3)
    assert [d.event.id for d in got] == ["i0", "i1", "i2"]


def test_event_grid_events_are_not_supported_for_namespaces(producer):
    with pytest.raises(TypeError):
        producer.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1"))


def test_channel_name_is_not_supported_for_namespaces(producer):
    with pytest.raises(ValueError):
        producer.send(ce(), channel_name="chan")


def test_each_subscription_gets_its_own_copy(admin, ns, ns_endpoint, key, producer, consumer):
    admin.put_sub("jobs", "second", {}, namespace=ns)
    other = make_consumer(ns_endpoint, key, "jobs", "second")
    producer.send(ce("shared"))
    a = receive_all(consumer)
    b = receive_all(other)
    assert a[0].event.data == b[0].event.data == "shared"
    assert a[0].broker_properties.lock_token != b[0].broker_properties.lock_token
    # settling in one subscription does not affect the other
    consumer.acknowledge(lock_tokens=[a[0].broker_properties.lock_token])
    res = other.release(lock_tokens=[b[0].broker_properties.lock_token], release_delay=0)
    assert res.succeeded_lock_tokens == [b[0].broker_properties.lock_token]
    assert len(receive_all(other)) == 1


def test_topics_are_independent(admin, ns, ns_endpoint, key, producer, consumer):
    admin.put_topic("other", namespace=ns)
    admin.put_sub("other", "sub", {}, namespace=ns)
    other_c = make_consumer(ns_endpoint, key, "other", "sub")
    producer.send(ce("only-jobs"))
    assert other_c.receive(max_events=1, max_wait_time=1) == []
    assert len(receive_all(consumer)) == 1


# -- settlement -------------------------------------------------------------------------

def test_release_returns_the_event_with_a_higher_delivery_count(producer, consumer):
    producer.send(ce("again"))
    (d1,) = receive_all(consumer)
    res = consumer.release(lock_tokens=[d1.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == [d1.broker_properties.lock_token]
    assert res.failed_lock_tokens == []
    (d2,) = receive_all(consumer)
    assert d2.event.data == "again"
    assert d2.broker_properties.delivery_count == 2
    assert d2.broker_properties.lock_token != d1.broker_properties.lock_token


def test_release_with_zero_delay(producer, consumer):
    producer.send(ce())
    (d,) = receive_all(consumer)
    res = consumer.release(lock_tokens=[d.broker_properties.lock_token], release_delay=0)
    assert len(res.succeeded_lock_tokens) == 1
    assert len(receive_all(consumer)) == 1


def test_reject_removes_the_event_for_good(producer, consumer):
    producer.send(ce("rejected"))
    (d,) = receive_all(consumer)
    res = consumer.reject(lock_tokens=[d.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == [d.broker_properties.lock_token]
    assert consumer.receive(max_events=1, max_wait_time=1) == []


def test_acknowledge_several_tokens_at_once(producer, consumer):
    producer.send([ce(str(i)) for i in range(4)])
    got = consumer.receive(max_events=4, max_wait_time=3)
    tokens = [d.broker_properties.lock_token for d in got]
    res = consumer.acknowledge(lock_tokens=tokens)
    assert sorted(res.succeeded_lock_tokens) == sorted(tokens)
    assert res.failed_lock_tokens == []


def test_partial_failure_is_reported_per_token(producer, consumer):
    producer.send(ce())
    (d,) = receive_all(consumer)
    bad = "CgMKATESCQoHdG9rZW4tMQ=="
    res = consumer.acknowledge(lock_tokens=[d.broker_properties.lock_token, bad])
    assert res.succeeded_lock_tokens == [d.broker_properties.lock_token]
    assert len(res.failed_lock_tokens) == 1
    f = res.failed_lock_tokens[0]
    assert f.lock_token == bad
    assert f.error.code == "BadToken"


@pytest.mark.parametrize("op", ["acknowledge", "release", "reject", "renew_locks"])
def test_unknown_lock_token_fails_in_the_result_not_with_an_http_error(consumer, op):
    res = getattr(consumer, op)(lock_tokens=["CgMKATESCQoHdG9rZW4tMQ=="])
    assert res.succeeded_lock_tokens == []
    assert [f.error.code for f in res.failed_lock_tokens] == ["BadToken"]


def test_acknowledging_twice_fails_the_second_time(producer, consumer):
    producer.send(ce())
    (d,) = receive_all(consumer)
    t = d.broker_properties.lock_token
    assert consumer.acknowledge(lock_tokens=[t]).succeeded_lock_tokens == [t]
    again = consumer.acknowledge(lock_tokens=[t])
    assert again.succeeded_lock_tokens == []
    assert len(again.failed_lock_tokens) == 1


def test_renew_locks_succeeds_for_a_held_lock(producer, consumer):
    producer.send(ce())
    (d,) = receive_all(consumer)
    res = consumer.renew_locks(lock_tokens=[d.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == [d.broker_properties.lock_token]
    assert res.failed_lock_tokens == []
    # still locked, still ours
    assert consumer.receive(max_events=1, max_wait_time=1) == []
    assert consumer.acknowledge(lock_tokens=[d.broker_properties.lock_token]).succeeded_lock_tokens


def test_lock_tokens_must_not_be_empty(consumer):
    with pytest.raises(HttpResponseError) as e:
        consumer.acknowledge(lock_tokens=[])
    assert e.value.status_code == 400


# -- long polling -----------------------------------------------------------------------

def test_receive_returns_as_soon_as_an_event_is_published(ns_endpoint, key, producer, consumer):
    result = {}

    def poll():
        t0 = time.time()
        result["events"] = consumer.receive(max_events=1, max_wait_time=20)
        result["took"] = time.time() - t0

    th = threading.Thread(target=poll)
    th.start()
    time.sleep(0.5)
    producer.send(ce("late"))
    th.join(15)
    assert not th.is_alive()
    assert result["events"][0].event.data == "late"
    assert 0.3 < result["took"] < 8


@pytest.mark.emulator_only
def test_receive_waits_for_max_wait_time_when_empty(consumer):
    """Azure's smallest max_wait_time is 10 s; the emulator accepts shorter ones."""
    t0 = time.time()
    assert consumer.receive(max_events=1, max_wait_time=2) == []
    took = time.time() - t0
    assert 1.5 < took < 6


# -- emulator-only: shortened locks, expiry, dead letters, filters ----------------------------

@pytest.mark.emulator_only
def test_lock_expiry_redelivers_with_increasing_delivery_count(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "short", {"lockDurationSeconds": 1}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "short")
    producer.send(ce("x"))
    (d1,) = receive_all(c)
    assert d1.broker_properties.delivery_count == 1
    (d2,) = c.receive(max_events=1, max_wait_time=10)  # blocks until the first lock lapses
    assert d2.event.id == d1.event.id
    assert d2.broker_properties.delivery_count == 2
    assert d2.broker_properties.lock_token != d1.broker_properties.lock_token
    # the first lock token is dead
    res = c.acknowledge(lock_tokens=[d1.broker_properties.lock_token, d2.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == [d2.broker_properties.lock_token]
    assert [f.lock_token for f in res.failed_lock_tokens] == [d1.broker_properties.lock_token]


@pytest.mark.emulator_only
def test_renewing_the_lock_keeps_the_event_past_the_lock_duration(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "renew", {"lockDurationSeconds": 1}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "renew")
    producer.send(ce("x"))
    (d,) = receive_all(c)
    token = d.broker_properties.lock_token
    for _ in range(4):
        time.sleep(0.5)
        assert c.renew_locks(lock_tokens=[token]).succeeded_lock_tokens == [token]
    # two seconds passed with a one second lock: the event was never offered again
    assert c.receive(max_events=1, max_wait_time=0) == []
    assert c.acknowledge(lock_tokens=[token]).succeeded_lock_tokens == [token]


@pytest.mark.emulator_only
def test_renewing_an_expired_lock_fails(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "expired", {"lockDurationSeconds": 1}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "expired")
    producer.send(ce("x"))
    (d,) = receive_all(c)
    time.sleep(1.4)
    res = c.renew_locks(lock_tokens=[d.broker_properties.lock_token])
    assert res.succeeded_lock_tokens == []
    assert len(res.failed_lock_tokens) == 1


@pytest.mark.emulator_only
def test_max_delivery_count_moves_the_event_to_the_dead_letters(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "limited", {"lockDurationSeconds": 1, "maxDeliveryCount": 2}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "limited")
    producer.send(ce("poison"))
    (d1,) = receive_all(c)
    (d2,) = c.receive(max_events=1, max_wait_time=10)
    assert (d1.broker_properties.delivery_count, d2.broker_properties.delivery_count) == (1, 2)
    # not delivered a third time
    assert c.receive(max_events=1, max_wait_time=3) == []
    dead = admin.inspect("jobs", "limited", "deadletters", namespace=ns)
    assert len(dead) == 1
    assert dead[0]["event"]["data"] == "poison"
    assert dead[0]["deliveryCount"] == 2
    assert dead[0]["reason"] == "MaxDeliveryCountExceeded"


@pytest.mark.emulator_only
def test_released_event_goes_to_the_dead_letters_at_the_max_delivery_count(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "one", {"maxDeliveryCount": 1}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "one")
    producer.send(ce("once"))
    (d,) = receive_all(c)
    c.release(lock_tokens=[d.broker_properties.lock_token])
    assert c.receive(max_events=1, max_wait_time=1) == []
    assert len(admin.inspect("jobs", "one", "deadletters", namespace=ns)) == 1


@pytest.mark.emulator_only
def test_rejected_events_are_kept_as_dead_letters(admin, ns, producer, consumer):
    producer.send(ce("bad"))
    (d,) = receive_all(consumer)
    consumer.reject(lock_tokens=[d.broker_properties.lock_token])
    dead = admin.inspect("jobs", "sub", "deadletters", namespace=ns)
    assert [x["reason"] for x in dead] == ["Rejected"]


@pytest.mark.emulator_only
def test_events_expire_after_the_time_to_live(admin, ns, ns_endpoint, key, producer):
    admin.put_sub("jobs", "ttl", {"eventTimeToLiveSeconds": 1}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "jobs", "ttl")
    producer.send(ce("short-lived"))
    time.sleep(1.5)
    assert c.receive(max_events=1, max_wait_time=0) == []
    dead = admin.inspect("jobs", "ttl", "deadletters", namespace=ns)
    assert dead and dead[0]["reason"] == "TimeToLiveExpired"


@pytest.mark.emulator_only
def test_filters_on_a_pull_subscription(admin, ns, ns_endpoint, key, producer):
    admin.put_sub(
        "jobs",
        "filtered",
        {
            "filter": {
                "includedEventTypes": ["Order.Created", "Order.Updated"],
                "subjectBeginsWith": "/orders/eu/",
                "advancedFilters": [
                    {"operatorType": "NumberGreaterThanOrEquals", "key": "data.total", "value": 100},
                    {"operatorType": "StringNotIn", "key": "data.status", "values": ["draft"]},
                ],
            }
        },
        namespace=ns,
    )
    c = make_consumer(ns_endpoint, key, "jobs", "filtered")

    def ev(i, type_, subject, total, status):
        return CloudEvent(source="/s", type=type_, subject=subject, data={"total": total, "status": status}, id=i)

    producer.send(
        [
            ev("1", "Order.Created", "/orders/eu/1", 150, "paid"),  # match
            ev("2", "Order.Deleted", "/orders/eu/2", 150, "paid"),  # wrong type
            ev("3", "Order.Created", "/orders/us/3", 150, "paid"),  # wrong subject
            ev("4", "Order.Updated", "/orders/eu/4", 99, "paid"),  # too small
            ev("5", "Order.Updated", "/orders/eu/5", 500, "draft"),  # excluded status
            ev("6", "Order.Updated", "/orders/EU/6", 100, "PAID"),  # match (case-insensitive subject)
        ]
    )
    got = c.receive(max_events=10, max_wait_time=2)
    assert [d.event.id for d in got] == ["1", "6"]


@pytest.mark.emulator_only
def test_a_new_subscription_only_sees_events_published_after_it_exists(admin, ns, ns_endpoint, key, producer, consumer):
    producer.send(ce("before"))
    admin.put_sub("jobs", "late", {}, namespace=ns)
    late = make_consumer(ns_endpoint, key, "jobs", "late")
    producer.send(ce("after"))
    assert [d.event.data for d in late.receive(max_events=10, max_wait_time=2)] == ["after"]
    assert [d.event.data for d in consumer.receive(max_events=10, max_wait_time=2)] == ["before", "after"]


@pytest.mark.emulator_only
def test_publishing_to_a_topic_without_subscriptions_drops_the_event(admin, ns, ns_endpoint, key):
    admin.put_topic("empty", namespace=ns)
    make_producer(ns_endpoint, key, "empty").send(ce("nobody listens"))
    admin.put_sub("empty", "s", {}, namespace=ns)
    c = make_consumer(ns_endpoint, key, "empty", "s")
    assert c.receive(max_events=1, max_wait_time=0) == []


@pytest.mark.emulator_only
def test_subscription_stats_in_the_admin_api(admin, ns, producer, consumer):
    producer.send([ce("1"), ce("2"), ce("3")])
    (d,) = receive_all(consumer)
    stats = admin.get_sub("jobs", "sub", namespace=ns)["stats"]
    assert (stats["available"], stats["locked"], stats["enqueued"]) == (2, 1, 3)
    consumer.acknowledge(lock_tokens=[d.broker_properties.lock_token])
    assert admin.get_sub("jobs", "sub", namespace=ns)["stats"]["acknowledged"] == 1
