"""Event Grid state across restarts, through the official SDK: namespace topics with pull subscriptions (events,
delivery counts, locks, dead letters), custom topics with push subscriptions, and a delivery that was waiting for a retry."""

import pathlib
import sys
import time

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "eventgrid"))
from egutil import Admin, Hook  # noqa: E402

from azure.core.credentials import AzureKeyCredential  # noqa: E402
from azure.core.messaging import CloudEvent  # noqa: E402
from azure.eventgrid import EventGridConsumerClient, EventGridEvent, EventGridPublisherClient  # noqa: E402

KEY = AzureKeyCredential("c2VjcmV0")
NS = "persistns"


def ce(data, **kw):
    return CloudEvent(source="/test", type="Test.Event", data=data, **kw)


def consumer(p, topic="jobs", sub="workers"):
    return EventGridConsumerClient(p.emu.url("eventgrid", NS), KEY, namespace_topic=topic, subscription=sub, retry_total=0)


def producer(p, topic="jobs"):
    return EventGridPublisherClient(p.emu.url("eventgrid", NS), KEY, namespace_topic=topic, retry_total=0)


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_pull_queues_locks_and_dead_letters_survive(persistent, how):
    p = persistent()
    admin = Admin(p.emu)
    admin.put_topic("jobs", namespace=NS)
    admin.put_sub("jobs", "workers", {"lockDurationSeconds": 120, "maxDeliveryCount": 3, "eventTimeToLiveSeconds": 7200}, namespace=NS)
    with producer(p) as pub:
        pub.send([ce(f"e{i}", subject=f"s{i}") for i in range(5)])
    with consumer(p) as c:
        got = c.receive(max_events=3, max_wait_time=3)
        assert [d.event.data for d in got] == ["e0", "e1", "e2"]
        c.acknowledge(lock_tokens=[got[0].broker_properties.lock_token])
        c.reject(lock_tokens=[got[1].broker_properties.lock_token])
        locked = got[2]
    sub_before = admin.get_sub("jobs", "workers", namespace=NS)
    dead_before = admin.inspect("jobs", "workers", "deadletters", namespace=NS)
    assert len(dead_before) == 1

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    admin = Admin(p.emu)
    assert admin.get_sub("jobs", "workers", namespace=NS) == sub_before
    assert admin.inspect("jobs", "workers", "deadletters", namespace=NS) == dead_before
    with consumer(p) as c:
        # the lock of before the restart is still good until it ends
        res = c.acknowledge(lock_tokens=[locked.broker_properties.lock_token])
        assert res.succeeded_lock_tokens == [locked.broker_properties.lock_token]
        rest = c.receive(max_events=10, max_wait_time=3)
        assert [d.event.data for d in rest] == ["e3", "e4"]
        assert all(d.broker_properties.delivery_count == 1 for d in rest)
        # a token that was used before the restart is not good any more
        res = c.acknowledge(lock_tokens=[got[0].broker_properties.lock_token])
        assert res.succeeded_lock_tokens == []
        c.release(lock_tokens=[d.broker_properties.lock_token for d in rest])
        again = c.receive(max_events=10, max_wait_time=3)
        assert [d.broker_properties.delivery_count for d in again] == [2, 2]


def test_a_push_subscription_and_a_delivery_waiting_for_a_retry_survive(persistent):
    p = persistent(extra_args=[])
    hook = Hook(statuses=[500])  # every notification fails until it is told otherwise
    try:
        admin = Admin(p.emu)
        admin.put_topic("orders")
        admin.put_sub("orders", "to-hook", {"webhookUrl": hook.url, "filter": {"includedEventTypes": ["Order.Created"]}, "retryPolicy": {"maxDeliveryAttempts": 20}})
        pub = EventGridPublisherClient(p.emu.url("eventgrid", "orders") + "/api/events", KEY, retry_total=0)
        with pub:
            pub.send([EventGridEvent(subject="/orders/1", event_type="Order.Created", data={"n": 1}, data_version="1")])
            pub.send([EventGridEvent(subject="/orders/2", event_type="Other", data={"n": 2}, data_version="1")])
        first = hook.wait_for(1, timeout=10)
        assert len(first) >= 1 and first[0]["body"][0]["id"]
        sent_id = first[0]["body"][0]["id"]
        attempts_before = len(hook.notifications)
        assert p.stop() == 0
        hook._statuses[:] = [200]  # the webhook works again
        p.start()
        # the subscription is still there with its filter, and the event that was waiting is delivered after the restart
        sub = Admin(p.emu).get_sub("orders", "to-hook")
        assert sub["validation"]["status"] == "Succeeded"
        deadline = time.time() + 20
        while time.time() < deadline and not any(r["headers"].get("aeg-delivery-count") not in (None, "0") and r["body"][0]["id"] == sent_id and True for r in hook.notifications[attempts_before:]):
            time.sleep(0.2)
        later = [r for r in hook.notifications[attempts_before:] if r["body"][0]["id"] == sent_id]
        assert later, "the event that was waiting for a retry was delivered after the restart"
        assert int(later[0]["headers"]["aeg-delivery-count"]) >= 1
        # a new event reaches the webhook, and the filter still holds
        pub = EventGridPublisherClient(p.emu.url("eventgrid", "orders") + "/api/events", KEY, retry_total=0)
        with pub:
            pub.send([EventGridEvent(subject="/orders/3", event_type="Order.Created", data={"n": 3}, data_version="1")])
            pub.send([EventGridEvent(subject="/orders/4", event_type="Other", data={"n": 4}, data_version="1")])
        time.sleep(1.5)
        subjects = {r["body"][0]["subject"] for r in hook.notifications[attempts_before:]}
        assert "/orders/3" in subjects and "/orders/4" not in subjects and "/orders/2" not in subjects
    finally:
        hook.close()
