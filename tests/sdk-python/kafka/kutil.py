"""Helpers for the Kafka suite."""

import json
import sys
import time
import urllib.request
import uuid

import emubin

from pathlib import Path

# the Event Hubs suite's helpers (AMQP side of the cross-protocol tests)
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "eventhubs"))

from confluent_kafka import Consumer, KafkaError, Producer
from confluent_kafka.admin import AdminClient

DEFAULT_NS = "default"
NS = "myns"
# a connection string as the Event Hubs Kafka documentation shows it; any key is accepted
KEY_NAME = "RootManageSharedAccessKey"
KEY = "c2VjcmV0c2VjcmV0c2VjcmV0"


def uniq(prefix="t") -> str:
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


def conn_string(emu, namespace=NS) -> str:
    return f"Endpoint=sb://{namespace}.eventhub.localhost:{emu.port}/;SharedAccessKeyName={KEY_NAME};SharedAccessKey={KEY}"


def admin_create(emu, hub, partitions=4, namespace=DEFAULT_NS, groups=(), retention_seconds=None):
    spec = {"partitionCount": partitions, "consumerGroups": list(groups)}
    if retention_seconds:
        spec["retentionSeconds"] = retention_seconds
    body = json.dumps(spec).encode()
    req = urllib.request.Request(
        f"{emu.gateway}/_azemu/eventhubs/{namespace}/hubs/{hub}", data=body, method="PUT", headers={"Content-Type": "application/json"}
    )
    urllib.request.urlopen(req, timeout=10).read()


def admin_get(emu, hub, namespace=DEFAULT_NS):
    try:
        with urllib.request.urlopen(f"{emu.gateway}/_azemu/eventhubs/{namespace}/hubs/{hub}", timeout=10) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def admin_groups(emu, namespace=DEFAULT_NS):
    with urllib.request.urlopen(f"{emu.gateway}/_azemu/kafka/{namespace}/groups", timeout=10) as r:
        return json.loads(r.read())["groups"]


def plain(emu) -> dict:
    """Plain text, no SASL, namespace `default`."""
    return {"bootstrap.servers": f"127.0.0.1:{emu.port}", "socket.timeout.ms": 10000, "log.connection.close": False}


def sasl_plain(emu, namespace=NS) -> dict:
    """SASL_PLAINTEXT with the connection string as the password (the namespace comes from its Endpoint)."""
    return {
        **plain(emu),
        "security.protocol": "SASL_PLAINTEXT",
        "sasl.mechanism": "PLAIN",
        "sasl.username": "$ConnectionString",
        "sasl.password": conn_string(emu, namespace),
    }


def tls(emu, namespace=NS) -> dict:
    """TLS, the emulator's CA, and the host name that carries the namespace (SNI)."""
    return {
        "bootstrap.servers": f"{namespace}.eventhub.localhost:{emu.port}",
        "security.protocol": "ssl",
        "ssl.ca.location": emu.ca_file,
        "socket.timeout.ms": 10000,
        "log.connection.close": False,
    }


def sasl_ssl(emu, namespace=NS) -> dict:
    return {
        **tls(emu, namespace),
        "security.protocol": "SASL_SSL",
        "sasl.mechanism": "PLAIN",
        "sasl.username": "$ConnectionString",
        "sasl.password": conn_string(emu, namespace),
    }


def producer(conf, **extra) -> Producer:
    return Producer({**conf, **extra})


def consumer(conf, group, **extra) -> Consumer:
    base = {
        "group.id": group,
        "auto.offset.reset": "earliest",
        "enable.auto.commit": False,
        "session.timeout.ms": 6000,
        "heartbeat.interval.ms": 1000,
        "max.poll.interval.ms": 20000,
    }
    return Consumer({**conf, **base, **extra})


def produce_all(p, topic, items, **kw):
    """Produce `items` (value bytes, or (key, value) pairs) and wait for the delivery reports. Returns them."""
    reports = []

    def cb(err, msg):
        reports.append((err, msg))

    for it in items:
        key, value = it if isinstance(it, tuple) else (None, it)
        p.produce(topic, value=value, key=key, on_delivery=cb, **kw)
    left = p.flush(15)
    assert left == 0, f"{left} messages were not delivered"
    errs = [e for e, _ in reports if e]
    assert not errs, errs
    return [m for _, m in reports]


def consume(c, n, timeout=20.0, quiet_ok=False):
    """Poll until `n` messages arrive (or `timeout`); errors other than end-of-partition fail the test."""
    out = []
    end = time.time() + timeout
    while len(out) < n and time.time() < end:
        m = c.poll(0.5)
        if m is None:
            continue
        if m.error():
            if m.error().code() == KafkaError._PARTITION_EOF:
                continue
            raise AssertionError(f"consumer error: {m.error()}")
        out.append(m)
    if not quiet_ok:
        assert len(out) >= n, f"got {len(out)} of {n} messages"
    return out


def wait_assignment(c, timeout=20.0):
    end = time.time() + timeout
    while time.time() < end:
        c.poll(0.2)
        a = c.assignment()
        if a:
            return a
    raise AssertionError("no partitions were assigned")


def admin(conf) -> AdminClient:
    return AdminClient(conf)


def wait_until(fn, timeout=10.0, interval=0.1, what="condition"):
    end = time.time() + timeout
    last = None
    while time.time() < end:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {what}")


def decode_amqp(b: bytes):
    """Decode one AMQP 1.0 encoded value (the few primitive types Python's pyamqp writes for application properties)."""
    import struct

    code = b[0]
    body = b[1:]
    if code == 0x40:
        return None
    if code == 0x41:
        return True
    if code == 0x42:
        return False
    if code == 0x56:  # boolean, one byte
        return body[0] != 0
    if code == 0x54:  # smallint
        return struct.unpack(">b", body[:1])[0]
    if code == 0x55:  # smalllong
        return struct.unpack(">b", body[:1])[0]
    if code == 0x71:  # int
        return struct.unpack(">i", body[:4])[0]
    if code == 0x81:  # long
        return struct.unpack(">q", body[:8])[0]
    if code == 0x82:  # double... (0x82 is double)
        return struct.unpack(">d", body[:8])[0]
    if code == 0xA1:  # str8-utf8
        return body[1 : 1 + body[0]].decode()
    if code == 0xB1:  # str32-utf8
        n = struct.unpack(">I", body[:4])[0]
        return body[4 : 4 + n].decode()
    if code == 0xA0:  # vbin8
        return bytes(body[1 : 1 + body[0]])
    raise AssertionError(f"unexpected AMQP type code {code:#x}")


class Extra:
    """A second emulator process started with its own arguments and environment (for `--pin` and the settings)."""

    def __init__(self, emu, args=(), env=None):
        import os
        import pathlib
        import socket
        import subprocess
        import tempfile

        self.tmp = tempfile.TemporaryDirectory(prefix="azemu-kafka-extra-")

        def free():
            with socket.socket() as s:
                s.bind(("127.0.0.1", 0))
                return s.getsockname()[1]

        self.port = free()
        self.pinned = free()
        binary = os.environ.get("AZEMU_BIN") or str(pathlib.Path(emu.proc.args[0]) if emu.proc else (emubin.unified() or "target/debug/azemu"))
        argv = [binary, "--port", str(self.port), "--cert-dir", self.tmp.name, "--log-level", "info"]
        argv += [a.replace("{pinned}", str(self.pinned)) for a in args]
        self.proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env={**os.environ, **(env or {})})
        self.ca_file = os.path.join(self.tmp.name, "ca.pem")
        end = time.time() + 20
        while time.time() < end:
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_azemu/health", timeout=1).read()
                return
            except Exception:
                time.sleep(0.1)
        raise RuntimeError("the extra emulator did not start")

    def create_hub(self, hub, namespace=DEFAULT_NS, partitions=4):
        """Create an event hub in this emulator through its admin API (it does not create hubs by being named)."""
        body = json.dumps({"partitionCount": partitions}).encode()
        req = urllib.request.Request(
            f"http://127.0.0.1:{self.port}/_azemu/eventhubs/{namespace}/hubs/{hub}", data=body, method="PUT", headers={"Content-Type": "application/json"}
        )
        urllib.request.urlopen(req, timeout=10).read()

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(5)
        except Exception:
            self.proc.kill()
        self.tmp.cleanup()
