"""Shared helpers for the Service Bus SDK tests.

Connection setup (see docs/sdk-notes.md): one connection string, `UseDevelopmentEmulator=true`, host
`<namespace>.servicebus.localhost:<port>`. The SDK then speaks plain AMQP (with SASL ANONYMOUS and a
`$cbs` token put) to that host and port, and the admin client speaks HTTPS to the same host and port.
"""

import asyncio
import time
import uuid
from datetime import datetime, timedelta, timezone
from urllib.parse import urlparse

from azure.servicebus import (
    ServiceBusClient,
    ServiceBusMessage,
    ServiceBusReceiveMode,
    ServiceBusSubQueue,
)
from azure.servicebus.management import ServiceBusAdministrationClient

KEY_NAME = "RootManageSharedAccessKey"
KEY = "c2VjcmV0c2VjcmV0c2VjcmV0"  # any base64-looking value: the emulator accepts every credential

# Client keyword arguments that keep a broken emulator from hanging a test:
#  retry_total=1 with a short backoff, try_timeout bounds one attempt of send / management / link open.
#  (the receive long poll itself is bounded by max_wait_time, which every receive in the tests sets)
CLIENT_KW = dict(retry_total=1, retry_backoff_factor=0.1, retry_backoff_max=0.5, try_timeout=15)
ADMIN_KW = dict(retry_total=0, connection_timeout=5, read_timeout=20)

PEEK_LOCK = ServiceBusReceiveMode.PEEK_LOCK
RECEIVE_AND_DELETE = ServiceBusReceiveMode.RECEIVE_AND_DELETE
DLQ = ServiceBusSubQueue.DEAD_LETTER


def uniq(prefix: str = "q") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


def host(emu, namespace: str = "default") -> str:
    """`<namespace>.servicebus.localhost:<port>` (what goes after `sb://` and after `https://`)."""
    return urlparse(emu.url("servicebus", namespace)).netloc


def conn_str(emu, namespace: str = "default", entity: str | None = None) -> str:
    s = (
        f"Endpoint=sb://{host(emu, namespace)};SharedAccessKeyName={KEY_NAME};SharedAccessKey={KEY};"
        "UseDevelopmentEmulator=true"
    )
    if entity:
        s += f";EntityPath={entity}"
    return s


def make_client(emu, namespace: str = "default", **kwargs) -> ServiceBusClient:
    kw = {**CLIENT_KW, **kwargs}
    return ServiceBusClient.from_connection_string(conn_str(emu, namespace), **kw)


def make_admin(emu, namespace: str = "default", **kwargs) -> ServiceBusAdministrationClient:
    kw = {**ADMIN_KW, "connection_verify": emu.ca_file, **kwargs}
    return ServiceBusAdministrationClient.from_connection_string(conn_str(emu, namespace), **kw)


def make_async_client(emu, namespace: str = "default", **kwargs):
    from azure.servicebus.aio import ServiceBusClient as AioClient

    kw = {**CLIENT_KW, **kwargs}
    return AioClient.from_connection_string(conn_str(emu, namespace), **kw)


def make_async_admin(emu, namespace: str = "default", **kwargs):
    from azure.servicebus.aio.management import ServiceBusAdministrationClient as AioAdmin

    kw = {**ADMIN_KW, "connection_verify": emu.ca_file, **kwargs}
    return AioAdmin.from_connection_string(conn_str(emu, namespace), **kw)


# -- messages -----------------------------------------------------------------
def body_of(msg) -> str:
    """The body of a sent or received message as one str (works for str, bytes and multi-section bodies)."""
    b = msg.body
    if isinstance(b, (bytes, bytearray)):
        return bytes(b).decode()
    if isinstance(b, str):
        return b
    return b"".join(b).decode()


def bodies(msgs) -> list:
    return [body_of(m) for m in msgs]


def texts(n: int, prefix: str = "m") -> list:
    return [f"{prefix}-{i}" for i in range(n)]


def send_n(client, entity: str, n: int, prefix: str = "m", *, topic: bool = False, **msg_kwargs) -> list:
    """Send n messages (one batch call) to a queue (or topic) and return their bodies in send order."""
    items = texts(n, prefix)
    sender = client.get_topic_sender(entity) if topic else client.get_queue_sender(entity)
    with sender:
        sender.send_messages([ServiceBusMessage(t, **msg_kwargs) for t in items])
    return items


def receive_n(receiver, n: int, *, max_wait: float = 2, settle: str | None = "complete", rounds: int = 6) -> list:
    """Receive until n messages arrived (or `rounds` receive calls ran out). `settle` is the name of a receiver
    method to call on each message ("complete", "abandon", ...), or None to leave them locked."""
    got = []
    for _ in range(rounds):
        if len(got) >= n:
            break
        for m in receiver.receive_messages(max_message_count=n - len(got), max_wait_time=max_wait):
            got.append(m)
            if settle:
                getattr(receiver, f"{settle}_message")(m)
    return got


def drain(receiver, *, max_wait: float = 1, settle: str | None = "complete", limit: int = 1000) -> list:
    """Receive until a receive call returns nothing (an empty queue costs one `max_wait`)."""
    got = []
    while len(got) < limit:
        batch = receiver.receive_messages(max_message_count=50, max_wait_time=max_wait)
        if not batch:
            return got
        for m in batch:
            got.append(m)
            if settle:
                getattr(receiver, f"{settle}_message")(m)
    return got


def drain_bodies(client, queue: str, *, sub_queue=None, max_wait: float = 1, mode=PEEK_LOCK) -> list:
    """Open a receiver, receive everything that is there (completing it) and return the bodies."""
    kw = {"sub_queue": sub_queue} if sub_queue else {}
    with client.get_queue_receiver(queue, receive_mode=mode, max_wait_time=max_wait, **kw) as r:
        got = drain(r, max_wait=max_wait, settle="complete" if mode == PEEK_LOCK else None)
    return bodies(got)


def drain_sub(client, topic: str, sub: str, *, sub_queue=None, max_wait: float = 1) -> list:
    kw = {"sub_queue": sub_queue} if sub_queue else {}
    with client.get_subscription_receiver(topic, sub, max_wait_time=max_wait, **kw) as r:
        got = drain(r, max_wait=max_wait)
    return got


def peek_all(client, queue: str, *, sub_queue=None, count: int = 100) -> list:
    kw = {"sub_queue": sub_queue} if sub_queue else {}
    with client.get_queue_receiver(queue, max_wait_time=1, **kw) as r:
        return r.peek_messages(max_message_count=count)


def utcnow() -> datetime:
    return datetime.now(timezone.utc)


def wait_until(fn, timeout: float = 5.0, interval: float = 0.2):
    """Poll `fn` until it returns something truthy (returned) or fail after `timeout` seconds."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"condition not met within {timeout}s (last value: {last!r})")


async def async_drain(receiver, *, max_wait: float = 1, settle: str | None = "complete", limit: int = 1000) -> list:
    got = []
    while len(got) < limit:
        batch = await receiver.receive_messages(max_message_count=50, max_wait_time=max_wait)
        if not batch:
            return got
        for m in batch:
            got.append(m)
            if settle:
                await getattr(receiver, f"{settle}_message")(m)
    return got


async def async_wait_until(fn, timeout: float = 5.0, interval: float = 0.2):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = await fn()
        if last:
            return last
        await asyncio.sleep(interval)
    raise AssertionError(f"condition not met within {timeout}s (last value: {last!r})")


def td(seconds: float) -> timedelta:
    return timedelta(seconds=seconds)
