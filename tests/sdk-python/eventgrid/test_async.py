"""The async clients (azure.eventgrid.aio): a subset of the publish and pull tests."""

import asyncio
import time

import pytest
from azure.core.credentials import AzureKeyCredential
from azure.core.exceptions import HttpResponseError
from azure.core.messaging import CloudEvent
from azure.eventgrid import EventGridEvent
from azure.eventgrid.aio import EventGridConsumerClient, EventGridPublisherClient


def ce(data="payload", **kw):
    return CloudEvent(source="/test", type="Test.Event", data=data, **kw)


# -- Event Grid Basic -------------------------------------------------------------------

async def test_async_send_event_grid_event(topic_endpoint, key):
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send(EventGridEvent(subject="s", event_type="t", data={"a": 1}, data_version="1"))


async def test_async_send_event_grid_event_list(topic_endpoint, key):
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send([EventGridEvent(subject=f"s{i}", event_type="t", data=i, data_version="1") for i in range(5)])


@pytest.mark.parametrize("data", ["text", {"a": 1}, None, b"bytes"])
async def test_async_send_cloud_event(topic_endpoint, key, data):
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send(CloudEvent(source="/s", type="t", data=data))


async def test_async_send_cloud_event_dict_and_custom_schema(topic_endpoint, key):
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send({"id": "id", "source": "/s", "specversion": "1.0", "type": "t", "data": "x"})
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send([{"customSubject": "a", "customId": "1"}, {"customSubject": "b", "customId": "2"}])


async def test_async_partner_channel(topic_endpoint, key):
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        await c.send(CloudEvent(source="/s", type="t"), channel_name="chan")


async def test_async_large_payload_is_413(topic_endpoint, key):
    big = {"features": ["x" * 1000] * 2100}
    async with EventGridPublisherClient(topic_endpoint, key, retry_total=0) as c:
        with pytest.raises(HttpResponseError) as e:
            await c.send(EventGridEvent(subject="s", event_type="t", data=big, data_version="1"))
    assert e.value.status_code == 413


async def test_async_send_and_close_session(topic_endpoint, key):
    c = EventGridPublisherClient(topic_endpoint, key, retry_total=0)
    await c.send(EventGridEvent(subject="s", event_type="t", data="x", data_version="1"))
    await c.close()


# -- namespaces -------------------------------------------------------------------------

@pytest.fixture
def aio_producer(ns_endpoint, key, ns_topic):
    return EventGridPublisherClient(ns_endpoint, key, namespace_topic=ns_topic, retry_total=0)


@pytest.fixture
def aio_consumer(ns_endpoint, key, ns_topic):
    return EventGridConsumerClient(ns_endpoint, key, namespace_topic=ns_topic, subscription="sub", retry_total=0)


async def test_async_send_receive_acknowledge(aio_producer, aio_consumer):
    async with aio_producer, aio_consumer:
        await aio_producer.send(ce({"hello": "async"}))
        got = await aio_consumer.receive(max_events=1, max_wait_time=3)
        assert len(got) == 1
        d = got[0]
        assert d.event.data == {"hello": "async"}
        assert d.broker_properties.delivery_count == 1
        res = await aio_consumer.acknowledge(lock_tokens=[d.broker_properties.lock_token])
        assert res.succeeded_lock_tokens == [d.broker_properties.lock_token]
        assert await aio_consumer.receive(max_events=1, max_wait_time=1) == []


async def test_async_release_reject_renew(aio_producer, aio_consumer):
    async with aio_producer, aio_consumer:
        await aio_producer.send([ce("a"), ce("b")])
        a, b = await aio_consumer.receive(max_events=2, max_wait_time=3)
        renewed = await aio_consumer.renew_locks(lock_tokens=[a.broker_properties.lock_token])
        assert renewed.succeeded_lock_tokens == [a.broker_properties.lock_token]
        released = await aio_consumer.release(lock_tokens=[a.broker_properties.lock_token], release_delay=0)
        assert released.succeeded_lock_tokens == [a.broker_properties.lock_token]
        rejected = await aio_consumer.reject(lock_tokens=[b.broker_properties.lock_token])
        assert rejected.succeeded_lock_tokens == [b.broker_properties.lock_token]
        (again,) = await aio_consumer.receive(max_events=5, max_wait_time=2)
        assert again.event.data == "a"
        assert again.broker_properties.delivery_count == 2


async def test_async_bad_token_is_reported_in_the_result(aio_consumer):
    async with aio_consumer:
        res = await aio_consumer.acknowledge(lock_tokens=["CgMKATESCQoHdG9rZW4tMQ=="])
        assert res.succeeded_lock_tokens == []
        assert res.failed_lock_tokens[0].error.code == "BadToken"


async def test_async_long_poll_wakes_on_publish(aio_producer, aio_consumer):
    async with aio_producer, aio_consumer:
        poll = asyncio.create_task(aio_consumer.receive(max_events=1, max_wait_time=20))
        await asyncio.sleep(0.5)
        t0 = time.time()
        await aio_producer.send(ce("wake"))
        got = await asyncio.wait_for(poll, 10)
        assert got[0].event.data == "wake"
        assert time.time() - t0 < 5


@pytest.mark.emulator_only
async def test_async_lock_expiry_redelivers(admin, ns, ns_endpoint, key, ns_topic):
    admin.put_sub(ns_topic, "short", {"lockDurationSeconds": 1}, namespace=ns)
    p = EventGridPublisherClient(ns_endpoint, key, namespace_topic=ns_topic, retry_total=0)
    c = EventGridConsumerClient(ns_endpoint, key, namespace_topic=ns_topic, subscription="short", retry_total=0)
    async with p, c:
        await p.send(ce("x"))
        (d1,) = await c.receive(max_events=1, max_wait_time=3)
        (d2,) = await c.receive(max_events=1, max_wait_time=10)
        assert d2.broker_properties.delivery_count == 2
        assert d2.event.id == d1.event.id
