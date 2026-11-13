"""The same core scenarios through azure.storage.queue.aio (async clients)."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.queue.aio import QueueServiceClient as AsyncQueueServiceClient

from queue_support import assert_storage_error, unique_queue_name


async def test_async_create_queue_then_list_it(emu):
    name = unique_queue_name()
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        await service.create_queue(name)

        names = [q.name async for q in service.list_queues(name_starts_with=name)]

    assert names == [name]


async def test_async_send_then_receive_round_trip(emu):
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        queue = await service.create_queue(unique_queue_name())

        sent = await queue.send_message("async-hello")
        got = await queue.receive_message()

    assert got.id == sent.id
    assert got.content == "async-hello"
    assert got.dequeue_count == 1


async def test_async_receive_from_missing_queue_raises_queue_not_found(emu):
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        queue = service.get_queue_client(unique_queue_name())

        with pytest.raises(ResourceNotFoundError) as exc:
            await queue.receive_message()

    assert_storage_error(exc.value, 404, "QueueNotFound")


async def test_async_peek_then_delete_message(emu):
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        queue = await service.create_queue(unique_queue_name())
        await queue.send_message("peek-then-delete")

        (peeked,) = await queue.peek_messages()
        assert peeked.pop_receipt is None
        got = await queue.receive_message()
        await queue.delete_message(got)

        assert await queue.peek_messages() == []


async def test_async_update_message_returns_new_pop_receipt(emu):
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        queue = await service.create_queue(unique_queue_name())
        await queue.send_message("old")
        msg = await queue.receive_message()

        updated = await queue.update_message(msg, content="new", visibility_timeout=0)

        assert updated.pop_receipt and updated.pop_receipt != msg.pop_receipt
        assert (await queue.receive_message()).content == "new"


async def test_async_delete_with_stale_pop_receipt_is_rejected(emu):
    async with AsyncQueueServiceClient.from_connection_string(emu.storage_conn_str()) as service:
        queue = await service.create_queue(unique_queue_name())
        await queue.send_message("v")
        msg = await queue.receive_message()
        await queue.update_message(msg, visibility_timeout=0)

        with pytest.raises(HttpResponseError) as exc:
            await queue.delete_message(msg.id, msg.pop_receipt)

    assert_storage_error(exc.value, 400, "PopReceiptMismatch")
