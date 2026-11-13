"""The same scenarios through azure.servicebus.aio (async clients) - queues, settlement, sessions, topics, admin.

pytest-asyncio runs in auto mode (tests/sdk-python/pytest.ini). Entities are created with the sync admin fixtures; the
async clients `async_sb` / `async_admin` come from conftest.py.
"""

from datetime import timedelta

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.servicebus import NEXT_AVAILABLE_SESSION, ServiceBusMessage, ServiceBusMessageState
from azure.servicebus.aio import AutoLockRenewer
from azure.servicebus.exceptions import (
    MessageAlreadySettled,
    MessageSizeExceededError,
    OperationTimeoutError,
    ServiceBusError,
    SessionCannotBeLockedError,
)
from azure.servicebus.management import CorrelationRuleFilter, SqlRuleAction, SqlRuleFilter

import sbutil
from sbutil import DLQ, async_drain, bodies, body_of, uniq, utcnow


async def asend(client, queue, n, prefix="m", **kw):
    items = [f"{prefix}-{i}" for i in range(n)]
    async with client.get_queue_sender(queue) as s:
        await s.send_messages([ServiceBusMessage(t, **kw) for t in items])
    return items


async def arecv(receiver, n, *, settle="complete", max_wait=2, rounds=6):
    got = []
    for _ in range(rounds):
        if len(got) >= n:
            break
        for m in await receiver.receive_messages(max_message_count=n - len(got), max_wait_time=max_wait):
            got.append(m)
            if settle:
                await getattr(receiver, f"{settle}_message")(m)
    return got


async def adrain_bodies(client, queue, **kw):
    async with client.get_queue_receiver(queue, max_wait_time=1, **kw) as r:
        return bodies(await async_drain(r))


# -- queues ---------------------------------------------------------------------------------------
async def test_async_send_and_receive_one(async_sb, queue):
    async with async_sb.get_queue_sender(queue) as s:
        await s.send_messages(ServiceBusMessage("hello"))
    async with async_sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = await arecv(r, 1, max_wait=3)
        assert body_of(m) == "hello"


async def test_async_send_many_and_order(async_sb, queue):
    sent = await asend(async_sb, queue, 15)
    assert await adrain_bodies(async_sb, queue) == sent


async def test_async_message_batch(async_sb, queue):
    async with async_sb.get_queue_sender(queue) as s:
        batch = await s.create_message_batch()
        for i in range(4):
            batch.add_message(ServiceBusMessage(f"b{i}"))
        await s.send_messages(batch)
    assert await adrain_bodies(async_sb, queue) == ["b0", "b1", "b2", "b3"]


async def test_async_message_too_large(async_sb, queue):
    async with async_sb.get_queue_sender(queue) as s:
        with pytest.raises(MessageSizeExceededError):
            await s.send_messages(ServiceBusMessage("A" * 256 * 1024))


async def test_async_iterator_receive(async_sb, queue):
    sent = await asend(async_sb, queue, 5)
    got = []
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        async for m in r:
            got.append(body_of(m))
            await r.complete_message(m)
    assert got == sent


async def test_async_empty_queue_returns_empty_list(async_sb, queue):
    async with async_sb.get_queue_receiver(queue, max_wait_time=1) as r:
        assert await r.receive_messages(max_message_count=3, max_wait_time=1) == []


async def test_async_peek_does_not_remove(async_sb, queue):
    sent = await asend(async_sb, queue, 3)
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert bodies(await r.peek_messages(max_message_count=5)) == sent
        assert bodies(await arecv(r, 3)) == sent


async def test_async_receive_and_delete(async_sb, queue):
    await asend(async_sb, queue, 3)
    async with async_sb.get_queue_receiver(queue, receive_mode="receiveanddelete", max_wait_time=2) as r:
        assert len(await arecv(r, 3, settle=None)) == 3
    assert await adrain_bodies(async_sb, queue) == []


async def test_async_message_properties_round_trip(async_sb, queue):
    async with async_sb.get_queue_sender(queue) as s:
        await s.send_messages(
            ServiceBusMessage(
                "p", application_properties={"k": 1, "s": "v"}, subject="sub", message_id="mid", correlation_id="cid",
                content_type="ct", to="to", reply_to="rt", time_to_live=timedelta(seconds=90),
            )
        )
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = await arecv(r, 1)
        assert m.application_properties[b"k"] == 1 and m.application_properties[b"s"] == b"v"
        assert (m.subject, m.message_id, m.correlation_id) == ("sub", "mid", "cid")
        assert (m.content_type, m.to, m.reply_to) == ("ct", "to", "rt")
        assert m.time_to_live == timedelta(seconds=90)
        assert m.sequence_number and m.enqueued_time_utc


# -- settlement ----------------------------------------------------------------------------------------
async def test_async_abandon_increments_delivery_count(async_sb, queue):
    await asend(async_sb, queue, 1)
    async with async_sb.get_queue_receiver(queue, max_wait_time=3) as r:
        (m,) = await arecv(r, 1, settle="abandon", max_wait=3)
        (again,) = await arecv(r, 1, max_wait=3)
        assert (m.delivery_count, again.delivery_count) == (0, 1)


async def test_async_complete_twice_raises_already_settled(async_sb, queue):
    await asend(async_sb, queue, 1)
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = await arecv(r, 1)
        with pytest.raises(MessageAlreadySettled):
            await r.complete_message(m)


async def test_async_dead_letter_with_reason(async_sb, queue):
    await asend(async_sb, queue, 1, prefix="dl")
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = await arecv(r, 1, settle=None)
        await r.dead_letter_message(m, reason="Testing reason", error_description="Testing description")
    async with async_sb.get_queue_receiver(queue, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = await arecv(dlq, 1, max_wait=3)
        assert body_of(d) == "dl-0"
        assert (d.dead_letter_reason, d.dead_letter_error_description) == ("Testing reason", "Testing description")


async def test_async_defer_and_receive_deferred(async_sb, queue):
    await asend(async_sb, queue, 2)
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        seqs = [m.sequence_number for m in await arecv(r, 2, settle="defer")]
        deferred = await r.receive_deferred_messages(seqs)
        assert len(deferred) == 2
        assert all(d.state == ServiceBusMessageState.DEFERRED for d in deferred)
        for d in deferred:
            await r.complete_message(d)


async def test_async_renew_message_lock(async_sb, queue):
    await asend(async_sb, queue, 1)
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        (m,) = await arecv(r, 1, settle=None)
        before = m.locked_until_utc
        new = await r.renew_message_lock(m)
        assert new >= before
        await r.complete_message(m)


@pytest.mark.slow
async def test_async_auto_lock_renewer_keeps_lock_past_lock_duration(async_sb, new_queue):
    import asyncio

    queue = new_queue(lock_duration=timedelta(seconds=5))
    await asend(async_sb, queue, 1)
    renewer = AutoLockRenewer(max_lock_renewal_duration=30)
    try:
        async with async_sb.get_queue_receiver(queue, max_wait_time=3) as r:
            (m,) = await arecv(r, 1, settle=None, max_wait=3)
            original = m.locked_until_utc
            renewer.register(r, m)
            await asyncio.sleep(max(0.0, (original - utcnow()).total_seconds()) + 1.5)
            assert m.locked_until_utc > utcnow()
            await r.complete_message(m)
    finally:
        await renewer.close()


# -- scheduled ------------------------------------------------------------------------------------------
async def test_async_schedule_peek_and_cancel(async_sb, queue):
    when = (utcnow() + timedelta(seconds=120)).replace(microsecond=0)
    async with async_sb.get_queue_sender(queue) as s:
        seqs = await s.schedule_messages([ServiceBusMessage("a"), ServiceBusMessage("b")], when)
        assert len(seqs) == 2
        async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
            peeked = await r.peek_messages(max_message_count=5)
            assert sorted(m.sequence_number for m in peeked) == sorted(seqs)
            assert all(m.state == ServiceBusMessageState.SCHEDULED for m in peeked)
        await s.cancel_scheduled_messages(seqs)
    async with async_sb.get_queue_receiver(queue, max_wait_time=2) as r:
        assert await r.peek_messages() == []


@pytest.mark.slow
async def test_async_scheduled_message_arrives(async_sb, queue):
    when = (utcnow() + timedelta(seconds=3)).replace(microsecond=0)
    async with async_sb.get_queue_sender(queue) as s:
        await s.schedule_messages(ServiceBusMessage("soon"), when)
    async with async_sb.get_queue_receiver(queue, max_wait_time=8) as r:
        (m,) = await arecv(r, 1, max_wait=8)
        assert body_of(m) == "soon"


# -- sessions -------------------------------------------------------------------------------------------
async def test_async_session_send_receive_and_state(async_sb, session_queue):
    async with async_sb.get_queue_sender(session_queue) as s:
        await s.send_messages([ServiceBusMessage(f"s{i}", session_id="A") for i in range(3)])
        await s.send_messages(ServiceBusMessage("other", session_id="B"))
    async with async_sb.get_queue_receiver(session_queue, session_id="A", max_wait_time=2) as r:
        assert await r.session.get_state() is None
        await r.session.set_state("st")
        assert await r.session.get_state() == b"st"
        assert bodies(await arecv(r, 3)) == ["s0", "s1", "s2"]
        assert r.session.locked_until_utc > utcnow()
        new = await r.session.renew_lock()
        assert new >= r.session.locked_until_utc


async def test_async_next_available_session(async_sb, session_queue):
    async with async_sb.get_queue_sender(session_queue) as s:
        await s.send_messages(ServiceBusMessage("x", session_id="only"))
    async with async_sb.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=3) as r:
        assert r.session.session_id == "only"


async def test_async_next_available_session_times_out(emu, session_queue):
    client = sbutil.make_async_client(emu, retry_total=0)
    try:
        with pytest.raises(OperationTimeoutError):
            async with client.get_queue_receiver(session_queue, session_id=NEXT_AVAILABLE_SESSION, max_wait_time=2):
                pass
    finally:
        await client.close()


async def test_async_session_cannot_be_locked_twice(async_sb, session_queue):
    async with async_sb.get_queue_sender(session_queue) as s:
        await s.send_messages(ServiceBusMessage("x", session_id="busy"))
    async with async_sb.get_queue_receiver(session_queue, session_id="busy", max_wait_time=2):
        with pytest.raises(SessionCannotBeLockedError):
            async with async_sb.get_queue_receiver(session_queue, session_id="busy", max_wait_time=2) as second:
                await second.receive_messages(max_wait_time=1)


async def test_async_non_session_send_to_session_queue_fails(async_sb, session_queue):
    async with async_sb.get_queue_sender(session_queue) as s:
        with pytest.raises(ServiceBusError):
            await s.send_messages(ServiceBusMessage("no session"))


async def test_async_list_queue_sessions(async_sb, session_queue):
    async with async_sb.get_queue_sender(session_queue) as s:
        await s.send_messages([ServiceBusMessage("a", session_id="s1"), ServiceBusMessage("b", session_id="s2")])
    assert {sid async for sid in async_sb.list_queue_sessions(session_queue)} == {"s1", "s2"}


# -- topics and rules ---------------------------------------------------------------------------------------
async def test_async_topic_fan_out(async_sb, topic, new_sub):
    s1, s2 = new_sub(topic), new_sub(topic)
    async with async_sb.get_topic_sender(topic) as s:
        await s.send_messages([ServiceBusMessage(f"t{i}") for i in range(3)])
    for sub in (s1, s2):
        async with async_sb.get_subscription_receiver(topic, sub, max_wait_time=2) as r:
            assert bodies(await arecv(r, 3)) == ["t0", "t1", "t2"]


async def test_async_subscription_dead_letter(async_sb, topic_sub):
    topic, sub = topic_sub
    async with async_sb.get_topic_sender(topic) as s:
        await s.send_messages(ServiceBusMessage("dl"))
    async with async_sb.get_subscription_receiver(topic, sub, max_wait_time=2) as r:
        (m,) = await arecv(r, 1, settle=None)
        await r.dead_letter_message(m, reason="r")
    async with async_sb.get_subscription_receiver(topic, sub, sub_queue=DLQ, max_wait_time=3) as dlq:
        (d,) = await arecv(dlq, 1, max_wait=3)
        assert d.dead_letter_reason == "r"


async def test_async_sql_and_correlation_rules_filter_messages(async_sb, admin, new_topic, new_sub):
    topic = new_topic()
    red = new_sub(topic, remove_default=True)
    cid = new_sub(topic, remove_default=True)
    admin.create_rule(topic, red, "r", filter=SqlRuleFilter("color = 'red'"), action=SqlRuleAction("SET seen = 1"))
    admin.create_rule(topic, cid, "r", filter=CorrelationRuleFilter(correlation_id="c1"))
    async with async_sb.get_topic_sender(topic) as s:
        await s.send_messages(ServiceBusMessage("a", application_properties={"color": "red"}, correlation_id="c1"))
        await s.send_messages(ServiceBusMessage("b", application_properties={"color": "blue"}, correlation_id="c2"))
    async with async_sb.get_subscription_receiver(topic, red, max_wait_time=1) as r:
        got = await async_drain(r)
        assert bodies(got) == ["a"] and got[0].application_properties[b"seen"] == 1
    async with async_sb.get_subscription_receiver(topic, cid, max_wait_time=1) as r:
        assert bodies(await async_drain(r)) == ["a"]


async def test_async_forwarding_and_duplicate_detection(async_sb, new_queue):
    target = new_queue("target")
    source = new_queue("source", forward_to=target, requires_duplicate_detection=True)
    async with async_sb.get_queue_sender(source) as s:
        await s.send_messages(ServiceBusMessage("one", message_id="d"))
        await s.send_messages(ServiceBusMessage("dup", message_id="d"))
    assert await adrain_bodies(async_sb, target) == ["one"]


# -- async administration client -----------------------------------------------------------------------------
async def test_async_admin_queue_lifecycle(async_admin):
    name = uniq("q")
    q = await async_admin.create_queue(name, max_delivery_count=7, lock_duration=timedelta(seconds=13))
    assert (q.name, q.max_delivery_count, q.lock_duration) == (name, 7, timedelta(seconds=13))
    got = await async_admin.get_queue(name)
    got.max_delivery_count = 9
    await async_admin.update_queue(got)
    assert (await async_admin.get_queue(name)).max_delivery_count == 9
    assert name in [x.name async for x in async_admin.list_queues()]
    await async_admin.delete_queue(name)
    with pytest.raises(ResourceNotFoundError):
        await async_admin.get_queue(name)


async def test_async_admin_errors(async_admin):
    name = uniq("q")
    await async_admin.create_queue(name)
    with pytest.raises(ResourceExistsError):
        await async_admin.create_queue(name)
    with pytest.raises(ResourceNotFoundError):
        await async_admin.delete_queue("non_existing_queue")
    q = await async_admin.get_queue(name)
    q.requires_session = True
    with pytest.raises(HttpResponseError):
        await async_admin.update_queue(q)


async def test_async_admin_topic_subscription_rule(async_admin):
    t, s = uniq("t"), uniq("s")
    await async_admin.create_topic(t)
    await async_admin.create_subscription(t, s, max_delivery_count=14)
    assert (await async_admin.get_subscription(t, s)).max_delivery_count == 14
    await async_admin.create_rule(t, s, "r", filter=SqlRuleFilter("a = 1"))
    assert [r.name async for r in async_admin.list_rules(t, s)] == ["$Default", "r"]
    assert (await async_admin.get_rule(t, s, "r")).filter.sql_expression == "a = 1"
    await async_admin.delete_rule(t, s, "r")
    await async_admin.delete_subscription(t, s)
    await async_admin.delete_topic(t)
    with pytest.raises(ResourceNotFoundError):
        await async_admin.get_topic(t)


async def test_async_admin_runtime_properties_and_namespace(async_sb, async_admin, queue):
    await asend(async_sb, queue, 3)
    rp = await async_admin.get_queue_runtime_properties(queue)
    assert rp.active_message_count == 3 and rp.total_message_count == 3
    assert (await async_admin.get_namespace_properties()).messaging_sku == "Standard"
