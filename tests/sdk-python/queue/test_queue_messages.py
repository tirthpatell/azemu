"""Messages: send, receive, peek, update, delete, clear and size limits.

Tests here avoid sleeps. A visibility timeout of 0 makes a message visible again
at once, so re-delivery is tested without waiting. Timing tests are in
test_queue_visibility.py.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError

from queue_support import assert_storage_error

ZERO_GUID = "00000000-0000-0000-0000-000000000000"
TWO_SECONDS = timedelta(seconds=2)


# -- send ---------------------------------------------------------------------


def test_send_message_returns_id_pop_receipt_and_times(queue):
    msg = queue.send_message("hello")

    assert isinstance(msg.id, str) and msg.id
    assert msg.pop_receipt
    assert msg.inserted_on is not None
    assert msg.expires_on is not None
    assert msg.next_visible_on is not None


def test_send_message_default_time_to_live_is_seven_days(queue):
    msg = queue.send_message("x")

    assert abs((msg.expires_on - msg.inserted_on) - timedelta(days=7)) <= TWO_SECONDS


def test_send_message_time_to_live_sets_expiry(queue):
    msg = queue.send_message("x", time_to_live=3600)

    assert abs((msg.expires_on - msg.inserted_on) - timedelta(seconds=3600)) <= TWO_SECONDS


def test_send_message_time_to_live_minus_one_never_expires(queue):
    # Recording test_put_message_infinite_time_to_live: ExpirationTime 31 Dec 9999 23:59:59 GMT.
    msg = queue.send_message("x", time_to_live=-1)

    assert msg.expires_on == datetime(9999, 12, 31, 23, 59, 59, tzinfo=timezone.utc)


def test_send_message_with_visibility_timeout_hides_it_from_receive(queue):
    queue.send_message("hidden", visibility_timeout=60)

    assert queue.receive_message() is None


def test_message_over_64_kib_is_rejected_with_413(queue):
    # Recording test_encryption_add_encrypted_64k_message: 413 RequestBodyTooLarge.
    with pytest.raises(HttpResponseError) as exc:
        queue.send_message("a" * (64 * 1024 + 1))

    assert_storage_error(exc.value, 413, "RequestBodyTooLarge")


# -- receive ------------------------------------------------------------------


def test_receive_message_returns_sent_content_with_receipt(queue):
    sent = queue.send_message("one")

    got = queue.receive_message()

    assert got.id == sent.id
    assert got.content == "one"
    assert got.dequeue_count == 1
    assert got.pop_receipt
    assert got.next_visible_on is not None


def test_receive_message_default_visibility_is_thirty_seconds(queue):
    queue.send_message("x")
    before = datetime.now(timezone.utc)

    got = queue.receive_message()

    after = datetime.now(timezone.utc)
    thirty = timedelta(seconds=30)
    assert before + thirty - TWO_SECONDS <= got.next_visible_on <= after + thirty + TWO_SECONDS


def test_receive_message_from_empty_queue_returns_none(queue):
    assert queue.receive_message() is None


def test_receive_message_from_missing_queue_raises_queue_not_found(service, queue_name):
    client = service.get_queue_client(queue_name)

    with pytest.raises(ResourceNotFoundError) as exc:
        client.receive_message()

    assert_storage_error(exc.value, 404, "QueueNotFound")


def test_receive_messages_stops_at_max_messages(queue):
    for i in range(5):
        queue.send_message(f"m{i}")

    got = list(queue.receive_messages(max_messages=3))

    assert len(got) == 3


def test_receive_messages_messages_per_page_splits_results(queue):
    for i in range(5):
        queue.send_message(f"m{i}")

    pages = queue.receive_messages(messages_per_page=2, max_messages=5).by_page()
    sizes = [len(list(page)) for page in pages]

    assert sizes and all(size <= 2 for size in sizes)
    assert sum(sizes) == 5


def test_visible_messages_come_back_in_send_order_for_one_client(queue):
    # Not a documented Azure guarantee (Microsoft Learn does not state FIFO for
    # Queue Storage). Pinned because the brief asks for it for a single sequential client.
    for i in range(5):
        queue.send_message(f"m{i}")

    received = [queue.receive_message().content for _ in range(5)]

    assert received == [f"m{i}" for i in range(5)]


def test_dequeue_count_increments_each_time_the_message_is_received(queue):
    queue.send_message("again")
    first = queue.receive_message()
    queue.update_message(first, visibility_timeout=0)

    second = queue.receive_message()

    assert second.dequeue_count == 2


# -- peek ---------------------------------------------------------------------


def test_peek_returns_content_without_pop_receipt_or_visibility(queue):
    sent = queue.send_message("peek me")

    (peeked,) = queue.peek_messages()

    assert peeked.id == sent.id
    assert peeked.content == "peek me"
    assert peeked.pop_receipt is None
    assert peeked.next_visible_on is None


def test_peek_does_not_change_dequeue_count(queue):
    # Recording test_peek_messages: a never-received message peeks with DequeueCount 0.
    # Peek only returns visible messages, so the received message is made visible
    # again with update (visibility_timeout=0) before the final peek.
    queue.send_message("p")
    assert queue.peek_messages()[0].dequeue_count == 0
    assert queue.peek_messages()[0].dequeue_count == 0

    msg = queue.receive_message()
    queue.update_message(msg, visibility_timeout=0)

    assert queue.peek_messages()[0].dequeue_count == 1


def test_peek_does_not_hide_messages(queue):
    queue.send_message("a")
    queue.send_message("b")

    assert len(queue.peek_messages(max_messages=2)) == 2
    assert len(list(queue.receive_messages(max_messages=2))) == 2


# -- update -------------------------------------------------------------------


def test_update_message_content_returns_new_pop_receipt(queue):
    queue.send_message("old")
    msg = queue.receive_message()

    updated = queue.update_message(msg, content="new", visibility_timeout=0)

    assert updated.pop_receipt
    assert updated.pop_receipt != msg.pop_receipt
    assert updated.next_visible_on is not None
    again = queue.receive_message()
    assert again.content == "new"


def test_update_message_visibility_timeout_hides_then_reveals(queue):
    queue.send_message("v")
    msg = queue.receive_message()
    assert queue.receive_message() is None

    queue.update_message(msg, visibility_timeout=0)

    assert queue.receive_message().content == "v"


def test_old_pop_receipt_stops_working_after_update(queue):
    queue.send_message("v")
    msg = queue.receive_message()
    updated = queue.update_message(msg, content="v2", visibility_timeout=0)

    with pytest.raises(HttpResponseError) as exc:
        queue.delete_message(msg.id, msg.pop_receipt)
    assert_storage_error(exc.value, 400, "PopReceiptMismatch")

    queue.delete_message(msg.id, updated.pop_receipt)
    assert queue.receive_message() is None


# -- delete -------------------------------------------------------------------


def test_delete_message_with_valid_pop_receipt_removes_it(queue):
    queue.send_message("gone")
    msg = queue.receive_message()

    queue.delete_message(msg)

    assert queue.peek_messages() == []


def test_delete_message_with_another_messages_pop_receipt_is_rejected(queue):
    # Pop receipt belongs to a different dequeued message: 400 PopReceiptMismatch.
    queue.send_message("a")
    queue.send_message("b")
    first = queue.receive_message()
    second = queue.receive_message()

    with pytest.raises(HttpResponseError) as exc:
        queue.delete_message(first.id, second.pop_receipt)

    assert_storage_error(exc.value, 400, "PopReceiptMismatch")


def test_delete_unknown_message_id_raises_message_not_found(queue):
    # A message id that does not exist: 404 MessageNotFound. The receipt is a real one
    # so that only the id is wrong.
    queue.send_message("x")
    msg = queue.receive_message()

    with pytest.raises(HttpResponseError) as exc:
        queue.delete_message(ZERO_GUID, msg.pop_receipt)

    assert_storage_error(exc.value, 404, "MessageNotFound")


# -- clear --------------------------------------------------------------------


def test_clear_messages_removes_visible_and_hidden_messages(queue):
    queue.send_message("a")
    queue.send_message("b")
    queue.send_message("c")
    queue.receive_message()  # one is now hidden

    queue.clear_messages()

    assert queue.peek_messages() == []
    assert queue.receive_message() is None

