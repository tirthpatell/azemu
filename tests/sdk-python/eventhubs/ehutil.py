"""Shared helpers for the Event Hubs SDK tests (azure-eventhub 5.15.1, pure-Python pyamqp transport).

Connection setup (see docs/sdk-notes.md): one connection string with `UseDevelopmentEmulator=true`; the SDK then dials plain AMQP
(SASL ANONYMOUS first) at `<namespace>.eventhub.localhost:<port>`.

Everything that waits for the emulator is bounded. `receive` / `receive_batch` of the SDK run until told to stop, so they are
always run through `Collector` (a thread) or `AsyncCollector` (a task): "collect N events or fail after a few seconds".
"""

import asyncio
import json
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import defaultdict
from urllib.parse import urlparse

from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient

NAMESPACE = "myns"
KEY_NAME = "RootManageSharedAccessKey"
KEY = "c2VjcmV0c2VjcmV0c2VjcmV0"  # base64-looking; the emulator accepts every credential
DEFAULT_GROUP = "$Default"
MARKER = b"__azemu_ready_marker__"

# Keep a broken service from hanging a test: one retry with a tiny backoff (the SDK default would sleep 0.8 s per retry).
CLIENT_KW = dict(retry_total=1, retry_backoff_factor=0.05, retry_backoff_max=0.2, auth_timeout=10)

# How long a "collect N events" wait may take before the test fails.
WAIT = 8.0


def uniq(prefix: str = "hub") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


# -- connection strings -------------------------------------------------------------------------------------------
def host(emu, namespace: str = NAMESPACE) -> str:
    """`<namespace>.eventhub.localhost:<port>` (what goes after `Endpoint=sb://`)."""
    return urlparse(emu.url("eventhub", namespace)).netloc


def conn_str(emu, hub: str | None = None, namespace: str = NAMESPACE, *, key_name: str = KEY_NAME, key: str = KEY,
             emulator: bool = True) -> str:
    s = f"Endpoint=sb://{host(emu, namespace)};SharedAccessKeyName={key_name};SharedAccessKey={key}"
    if emulator:
        s += ";UseDevelopmentEmulator=true"
    if hub:
        s += f";EntityPath={hub}"
    return s


# -- the one place that knows the emulator's admin API ----------------------------------------------------------------
HUB_ADMIN_ROUTE = "/_azemu/eventhubs/{namespace}/hubs/{hub}"  # PUT, JSON {"partitionCount": N, "consumerGroups": [...]}


def admin_create_hub(emu, hub: str, partitions: int = 4, groups=(), namespace: str = NAMESPACE) -> None:
    """Create an event hub with a chosen partition count (and consumer groups) through the emulator's admin API."""
    path = HUB_ADMIN_ROUTE.format(namespace=namespace, hub=hub)
    body = json.dumps({"partitionCount": partitions, "consumerGroups": list(groups)}).encode()
    req = urllib.request.Request(emu.gateway + path, data=body, method="PUT", headers={"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=10).read()
    except urllib.error.HTTPError as e:  # surface the body: it says what the emulator did not like
        raise AssertionError(f"admin create hub failed: {e.code} {e.read()[:300]!r}") from None


# -- clients ------------------------------------------------------------------------------------------------------------
def make_producer(emu, hub: str, namespace: str = NAMESPACE, **kwargs) -> EventHubProducerClient:
    kw = {**CLIENT_KW, **kwargs}
    return EventHubProducerClient.from_connection_string(conn_str(emu, namespace=namespace), eventhub_name=hub, **kw)


def make_consumer(emu, hub: str, group: str = DEFAULT_GROUP, namespace: str = NAMESPACE, **kwargs) -> EventHubConsumerClient:
    kw = {**CLIENT_KW, **kwargs}
    return EventHubConsumerClient.from_connection_string(
        conn_str(emu, namespace=namespace), consumer_group=group, eventhub_name=hub, **kw
    )


def make_async_producer(emu, hub: str, namespace: str = NAMESPACE, **kwargs):
    from azure.eventhub.aio import EventHubProducerClient as AioProducer

    kw = {**CLIENT_KW, **kwargs}
    return AioProducer.from_connection_string(conn_str(emu, namespace=namespace), eventhub_name=hub, **kw)


def make_async_consumer(emu, hub: str, group: str = DEFAULT_GROUP, namespace: str = NAMESPACE, **kwargs):
    from azure.eventhub.aio import EventHubConsumerClient as AioConsumer

    # load_balancing_interval: the async receive loop sleeps one interval (default 30 s) between rounds and only notices a close
    # after that sleep, so a short interval keeps `await receive(...)` from outliving `close()` by half a minute
    kw = {**CLIENT_KW, "load_balancing_interval": 1, **kwargs}
    return AioConsumer.from_connection_string(
        conn_str(emu, namespace=namespace), consumer_group=group, eventhub_name=hub, **kw
    )


# -- events -------------------------------------------------------------------------------------------------------------
def body_of(event) -> str:
    return event.body_as_str()


def is_marker(event) -> bool:
    """True for the ready markers `wait_live` sends (compares raw bytes; works for any body type)."""
    try:
        return b"".join(event.body) == MARKER
    except Exception:  # noqa: BLE001 - sequence / value bodies are never markers
        return False


def bodies(events) -> list:
    return [body_of(e) for e in events]


def _safe_body(event):
    try:
        return event.body_as_str()[:60]
    except Exception:  # noqa: BLE001
        return "<binary>"


def texts(n: int, prefix: str = "m") -> list:
    return [f"{prefix}-{i}" for i in range(n)]


def send_n(producer, n: int, prefix: str = "m", *, partition_id=None, partition_key=None, **ed_kwargs) -> list:
    """Send n events as ONE batch (so they land together, in order) and return their bodies in send order."""
    items = texts(n, prefix)
    kw = {}
    if partition_id is not None:
        kw["partition_id"] = partition_id
    if partition_key is not None:
        kw["partition_key"] = partition_key
    batch = producer.create_batch(**kw)
    for t in items:
        batch.add(EventData(t))
    producer.send_batch(batch)
    return items


def send_each(producer, items, **kw) -> None:
    """Send every item as its own `send_event` call."""
    for t in items:
        producer.send_event(EventData(t), **kw)


async def async_send_n(producer, n: int, prefix: str = "m", *, partition_id=None, partition_key=None) -> list:
    items = texts(n, prefix)
    kw = {}
    if partition_id is not None:
        kw["partition_id"] = partition_id
    if partition_key is not None:
        kw["partition_key"] = partition_key
    batch = await producer.create_batch(**kw)
    for t in items:
        batch.add(EventData(t))
    await producer.send_batch(batch)
    return items


def wait_until(fn, timeout: float = 5.0, interval: float = 0.1, what: str = "condition"):
    """Poll `fn` until it returns something truthy (returned) or fail after `timeout` seconds."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"{what} not met within {timeout}s (last value: {last!r})")


# -- collecting received events (sync) ------------------------------------------------------------------------------------
class Collector:
    """Runs `client.receive_batch` (or `receive`) on a thread and records what arrives.

    `items` is a list of (partition_id, EventData) in arrival order; `events` the events only (the ready markers sent by
    `wait_live` are kept apart in `markers`). `wait_events(n)` blocks until n events arrived or `WAIT` seconds passed and then
    fails with what it saw. Stop with `stop()` (closes the client, which ends the receive loop) or use it as a context manager.
    """

    def __init__(self, client, *, batch: bool = True, partition_id=None, **receive_kwargs):
        self.client = client
        self.batch = batch
        self.items = []
        self.markers = []
        self.errors = []  # (partition_id or None, exception)
        self.initialized = []  # partition ids, in order of on_partition_initialize
        self.closed = []  # (partition_id, CloseReason)
        self.contexts = {}  # partition_id -> last PartitionContext seen
        self.empty_callbacks = 0
        self.batch_sizes = []  # length of every non-empty batch handed to the callback (receive_batch only)
        self._lock = threading.Lock()
        self._thread = None
        self._kwargs = dict(receive_kwargs)
        if partition_id is not None:
            self._kwargs["partition_id"] = partition_id
        self._user_init = self._kwargs.pop("on_partition_initialize", None)
        self._user_close = self._kwargs.pop("on_partition_close", None)
        self._user_error = self._kwargs.pop("on_error", None)
        self._user_event = self._kwargs.pop("on_event_hook", None)
        self.thread_error = None

    # callbacks run on the receive thread
    def _record(self, ctx, event):
        self.contexts[ctx.partition_id] = ctx
        with self._lock:
            if is_marker(event):
                self.markers.append((ctx.partition_id, event))
            else:
                self.items.append((ctx.partition_id, event))
        if self._user_event:
            self._user_event(ctx, event)

    def _on_event(self, ctx, event):
        if event is None:
            self.empty_callbacks += 1
            self.contexts[ctx.partition_id] = ctx
            return
        self._record(ctx, event)

    def _on_batch(self, ctx, events):
        if not events:
            self.empty_callbacks += 1
            self.contexts[ctx.partition_id] = ctx
            return
        self.batch_sizes.append(len(events))
        for e in events:
            self._record(ctx, e)

    def _on_error(self, ctx, error):
        with self._lock:
            self.errors.append((ctx.partition_id if ctx else None, error))
        if self._user_error:
            self._user_error(ctx, error)

    def _on_init(self, ctx):
        self.contexts[ctx.partition_id] = ctx
        with self._lock:
            self.initialized.append(ctx.partition_id)
        if self._user_init:
            self._user_init(ctx)

    def _on_close(self, ctx, reason):
        with self._lock:
            self.closed.append((ctx.partition_id, reason))
        if self._user_close:
            self._user_close(ctx, reason)

    def start(self):
        kw = dict(on_error=self._on_error, on_partition_initialize=self._on_init, on_partition_close=self._on_close)
        kw.update(self._kwargs)
        fn = self.client.receive_batch if self.batch else self.client.receive
        cb = self._on_batch if self.batch else self._on_event

        def run():
            try:
                fn(cb, **kw)
            except Exception as exc:  # noqa: BLE001 - reported through thread_error
                self.thread_error = exc

        self._thread = threading.Thread(target=run, daemon=True)
        self._thread.start()
        return self

    def stop(self, timeout: float = 5.0):
        try:
            self.client.close()
        finally:
            if self._thread:
                self._thread.join(timeout)

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()

    # -- reading what arrived
    @property
    def events(self) -> list:
        with self._lock:
            return [e for _, e in self.items]

    def by_partition(self) -> dict:
        out = defaultdict(list)
        with self._lock:
            for pid, e in self.items:
                out[pid].append(e)
        return dict(out)

    def count(self) -> int:
        with self._lock:
            return len(self.items)

    def wait_events(self, n: int, timeout: float = WAIT, what: str = "events") -> list:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.count() >= n:
                return self.events
            if self.thread_error:
                raise AssertionError(f"receive loop died: {self.thread_error!r}") from self.thread_error
            time.sleep(0.05)
        raise AssertionError(
            f"expected {n} {what} within {timeout}s, got {self.count()}: {[_safe_body(e) for e in self.events][:20]}; "
            f"errors={self.errors!r} thread_error={self.thread_error!r}"
        )

    def wait_until(self, pred, timeout: float = WAIT, what: str = "condition"):
        return wait_until(lambda: pred(self), timeout, 0.05, what)

    def settle(self, seconds: float = 0.6) -> None:
        """Give extra events a moment to show up (used before asserting that NO more arrive)."""
        time.sleep(seconds)

    def wait_initialized(self, n: int = 1, timeout: float = WAIT):
        return self.wait_until(lambda c: len(c.initialized) >= n, timeout, f"{n} partition(s) initialized")

    def wait_live(self, producer, partition_ids, timeout: float = 10.0):
        """Block until a receiver started at "@latest" really has its links open on `partition_ids`.

        `on_partition_initialize` fires BEFORE the SDK attaches the link (the attach happens inside the first receive loop
        iteration), so it cannot be used to know when "@latest" is fixed. Instead send a marker event to each partition
        every 0.3 s until a marker has come back from every one. Markers never show up in `events`.
        """
        pending = set(partition_ids)
        deadline = time.monotonic() + timeout
        while pending and time.monotonic() < deadline:
            seen = {pid for pid, _ in list(self.markers)}
            pending -= seen
            if not pending:
                break
            for pid in sorted(pending):
                producer.send_event(EventData(MARKER), partition_id=pid)
            time.sleep(0.3)
        seen = {pid for pid, _ in list(self.markers)}
        missing = set(partition_ids) - seen
        assert not missing, f"receiver not live on partitions {sorted(missing)} after {timeout}s (errors={self.errors!r})"
        return self


def live_receiver(emu, hub, producer, partition_ids, *, group=DEFAULT_GROUP, batch=True, client_kw=None, **recv_kw) -> Collector:
    """A started Collector reading "@latest" (the SDK default) on `partition_ids`, confirmed live with markers.

    One partition: `partition_ids=["0"]` (the receive is limited to it). Several: all partitions of the hub are read, so
    pass them all.
    """
    client = make_consumer(emu, hub, group, **(client_kw or {}))
    kw = dict(recv_kw)
    if len(partition_ids) == 1:
        kw["partition_id"] = partition_ids[0]
    kw.setdefault("max_wait_time", 1)
    col = Collector(client, batch=batch, **kw).start()
    try:
        col.wait_live(producer, partition_ids)
    except BaseException:
        col.stop()
        raise
    return col


def read_from_start(emu, hub, expected: int, *, partition_id=None, group=DEFAULT_GROUP, timeout: float = WAIT, **kw) -> list:
    """Read from the beginning ("-1") until `expected` events arrived; returns them (in arrival order). Closes the client."""
    client = make_consumer(emu, hub, group)
    kw.setdefault("max_wait_time", 1)
    with Collector(client, partition_id=partition_id, starting_position="-1", **kw) as col:
        return col.wait_events(expected, timeout)


def read_all_from_start(emu, hub, expected: int, *, group=DEFAULT_GROUP, timeout: float = WAIT) -> dict:
    """Read every partition from "-1" until `expected` events arrived in total; returns {partition id: [events]}."""
    client = make_consumer(emu, hub, group)
    with Collector(client, starting_position="-1", max_wait_time=1) as col:
        col.wait_events(expected, timeout)
        col.settle(0.3)
        return col.by_partition()


def read_exact(emu, hub, expected: int, *, partition_id=None, group=DEFAULT_GROUP, quiet: float = 0.7, **kw) -> list:
    """Like read_from_start, then wait `quiet` seconds and assert that nothing more arrived (exactly `expected` events)."""
    client = make_consumer(emu, hub, group)
    kw.setdefault("max_wait_time", 1)
    with Collector(client, partition_id=partition_id, starting_position="-1", **kw) as col:
        col.wait_events(expected) if expected else None
        col.settle(quiet)
        got = col.events
        assert len(got) == expected, f"expected exactly {expected} events, got {len(got)}: {bodies(got)[:20]}"
        return got


def partition_sizes(emu, hub, partition_ids) -> dict:
    """{partition id: number of events in it} by reading each partition from "-1" until it is quiet (1.2 s)."""
    out = {}
    client = make_consumer(emu, hub)
    with Collector(client, starting_position="-1", max_wait_time=1) as col:
        col.settle(1.2)
        by = col.by_partition()
    for pid in partition_ids:
        out[pid] = len(by.get(pid, []))
    return out


# -- the same for asyncio -----------------------------------------------------------------------------------------------
class AsyncCollector:
    """Runs `await client.receive_batch(...)` (or `receive`) as a task and records what arrives. All callbacks are coroutines."""

    def __init__(self, client, *, batch: bool = True, partition_id=None, **receive_kwargs):
        self.client = client
        self.batch = batch
        self.items = []
        self.markers = []
        self.errors = []
        self.initialized = []
        self.closed = []
        self.empty_callbacks = 0
        self._kwargs = dict(receive_kwargs)
        if partition_id is not None:
            self._kwargs["partition_id"] = partition_id
        self._task = None
        self.task_error = None

    async def _record(self, ctx, event):
        if is_marker(event):
            self.markers.append((ctx.partition_id, event))
        else:
            self.items.append((ctx.partition_id, event))

    async def _on_event(self, ctx, event):
        if event is None:
            self.empty_callbacks += 1
            return
        await self._record(ctx, event)

    async def _on_batch(self, ctx, events):
        if not events:
            self.empty_callbacks += 1
            return
        for e in events:
            await self._record(ctx, e)

    async def _on_error(self, ctx, error):
        self.errors.append((ctx.partition_id if ctx else None, error))

    async def _on_init(self, ctx):
        self.initialized.append(ctx.partition_id)

    async def _on_close(self, ctx, reason):
        self.closed.append((ctx.partition_id, reason))

    def start(self):
        kw = dict(on_error=self._on_error, on_partition_initialize=self._on_init, on_partition_close=self._on_close)
        kw.update(self._kwargs)
        fn = self.client.receive_batch if self.batch else self.client.receive
        cb = self._on_batch if self.batch else self._on_event

        async def run():
            try:
                await fn(cb, **kw)
            except Exception as exc:  # noqa: BLE001
                self.task_error = exc

        self._task = asyncio.ensure_future(run())
        return self

    async def stop(self, timeout: float = 3.0):
        try:
            await self.client.close()
        finally:
            if self._task:
                try:
                    await asyncio.wait_for(asyncio.shield(self._task), timeout)
                except (asyncio.TimeoutError, asyncio.CancelledError):
                    self._task.cancel()
                    try:
                        await self._task
                    except (asyncio.CancelledError, Exception):  # noqa: BLE001
                        pass

    @property
    def events(self) -> list:
        return [e for _, e in self.items]

    def by_partition(self) -> dict:
        out = defaultdict(list)
        for pid, e in self.items:
            out[pid].append(e)
        return dict(out)

    async def wait_events(self, n: int, timeout: float = WAIT) -> list:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if len(self.items) >= n:
                return self.events
            if self.task_error:
                raise AssertionError(f"receive task died: {self.task_error!r}") from self.task_error
            await asyncio.sleep(0.05)
        raise AssertionError(
            f"expected {n} events within {timeout}s, got {len(self.items)}: {[_safe_body(e) for e in self.events][:20]}; "
            f"errors={self.errors!r} task_error={self.task_error!r}"
        )

    async def wait_live(self, producer, partition_ids, timeout: float = 10.0):
        pending = set(partition_ids)
        deadline = time.monotonic() + timeout
        while pending and time.monotonic() < deadline:
            pending -= {pid for pid, _ in self.markers}
            if not pending:
                break
            for pid in sorted(pending):
                await producer.send_event(EventData(MARKER), partition_id=pid)
            await asyncio.sleep(0.3)
        missing = set(partition_ids) - {pid for pid, _ in self.markers}
        assert not missing, f"receiver not live on partitions {sorted(missing)} after {timeout}s (errors={self.errors!r})"
        return self


async def async_read_from_start(emu, hub, expected: int, *, partition_id=None, group=DEFAULT_GROUP, **kw) -> list:
    client = make_async_consumer(emu, hub, group)
    kw.setdefault("max_wait_time", 1)
    kw.setdefault("starting_position", "-1")
    col = AsyncCollector(client, partition_id=partition_id, **kw).start()
    try:
        return await col.wait_events(expected)
    finally:
        await col.stop()


# -- time ----------------------------------------------------------------------------------------------------------------
def ms(dt) -> float:
    """datetime -> epoch milliseconds."""
    return dt.timestamp() * 1000
