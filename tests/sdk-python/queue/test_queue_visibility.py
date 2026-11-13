"""Visibility timeouts that need real time to pass.

Every sleep here is 1.5 seconds or less. Tests that need no waiting are in
test_queue_messages.py.
"""

import time


def test_received_message_reappears_after_visibility_timeout_with_dequeue_count_two(queue):
    queue.send_message("t")
    first = queue.receive_message(visibility_timeout=1)
    assert queue.receive_message() is None

    time.sleep(1.5)
    second = queue.receive_message()

    assert second is not None
    assert second.id == first.id
    assert second.dequeue_count == 2


def test_message_sent_with_visibility_timeout_becomes_visible_after_delay(queue):
    queue.send_message("t", visibility_timeout=1)
    assert queue.receive_message() is None

    time.sleep(1.5)

    assert queue.receive_message().content == "t"


def test_update_visibility_timeout_to_one_second_reveals_message_after_delay(queue):
    queue.send_message("t")
    msg = queue.receive_message(visibility_timeout=30)

    queue.update_message(msg, visibility_timeout=1)
    time.sleep(1.5)

    again = queue.receive_message()
    assert again is not None
    assert again.id == msg.id
