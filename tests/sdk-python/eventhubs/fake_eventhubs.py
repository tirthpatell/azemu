"""A small in-process Event Hubs double, written only to check the tests in this directory.

It is NOT part of the emulator and not a test (no `test_` prefix). It exists because the emulator's Event Hubs service did
not exist while these tests were written: running the suite against this double finds mistakes in the TESTS (wrong
keyword, wrong helper, a deadlock) before the real service has to meet them. It is built from the same sources as the
tests (the SDK's own pyamqp encoder/decoder, the SDK source, Microsoft's docs), so it shares their assumptions: a pass
against it proves the tests are self-consistent and runnable, nothing more.

Use:

    from fake_eventhubs import FakeEventHubs
    srv = FakeEventHubs().start()      # srv.port
    ...
    srv.stop()

or from a shell, to run the suite against it:

    AZEMU_GATEWAY=http://127.0.0.1:PORT .venv/bin/pytest tests/sdk-python/eventhubs -x -q

(`python fake_eventhubs.py [port]` runs it in the foreground.) Plain AMQP and the tiny HTTP admin API share one port, like
the emulator. TLS is not implemented, so the TLS tests are skipped against it (no CA file).

What it models (all from the SDK source / docs): SASL ANONYMOUS, `$cbs` put-token (answered 202), `$management`
`com.microsoft:eventhub` / `com.microsoft:partition` (404 for an unknown hub, 400 for an unknown partition), sender links
(single message and the 0x80013700 batch format, `max-message-size` 1048576, split into events, partition by link /
partition key hash / round robin, messages settled `accepted`), receiver links (selector filter on offset / sequence
number / enqueued time, `@latest`, runtime-metric delivery annotations, epoch / owner level with `amqp:link:stolen`,
non-epoch fan-out), consumer groups (`$Default` plus created ones), retention is not modelled.
"""

import json
import socket
import struct
import sys
import threading
import time
import zlib
from urllib.parse import urlparse

from azure.eventhub._pyamqp import performatives as P
from azure.eventhub._pyamqp._decode import decode_frame, decode_payload
from azure.eventhub._pyamqp._encode import encode_frame, encode_payload, encode_value
from azure.eventhub._pyamqp.types import TYPE, VALUE, AMQPTypes
from azure.eventhub._pyamqp.endpoints import Source, Target
from azure.eventhub._pyamqp.error import AMQPError
from azure.eventhub._pyamqp.message import Message, Properties
from azure.eventhub._pyamqp.outcomes import Accepted, Rejected

SASL_HDR = b"AMQP\x03\x01\x00\x00"
AMQP_HDR = b"AMQP\x00\x01\x00\x00"
BATCH_FORMAT = 0x80013700
MAX_MESSAGE_SIZE = 1048576
EPOCH = b"com.microsoft:epoch"
RUNTIME_METRIC = b"com.microsoft:enable-receiver-runtime-metric"
SENTINEL_TIME_MS = -62135596800000  # AMQP timestamp the service uses for "no time"

_FIXED = {
    0x40: 0, 0x41: 0, 0x42: 0, 0x43: 0, 0x44: 0, 0x45: 0,
    0x50: 1, 0x51: 1, 0x52: 1, 0x53: 1, 0x54: 1, 0x55: 1, 0x56: 1,
    0x60: 2, 0x61: 2,
    0x70: 4, 0x71: 4, 0x72: 4, 0x73: 4, 0x74: 4,
    0x80: 8, 0x81: 8, 0x82: 8, 0x83: 8, 0x84: 8,
    0x94: 16, 0x98: 16,
}


def skip_value(buf, pos):
    """Position just after the AMQP value that starts at `pos`."""
    c = buf[pos]
    if c == 0x00:
        return skip_value(buf, skip_value(buf, pos + 1))
    if c in _FIXED:
        return pos + 1 + _FIXED[c]
    if c in (0xA0, 0xA1, 0xA3, 0xC0, 0xC1, 0xE0):
        return pos + 2 + buf[pos + 1]
    if c in (0xB0, 0xB1, 0xB3, 0xD0, 0xD1, 0xF0):
        return pos + 5 + int.from_bytes(buf[pos + 1 : pos + 5], "big")
    raise ValueError(f"cannot skip constructor {c:#x}")


def split_sections(payload: bytes):
    """[(descriptor_code, raw_bytes)] of an encoded message / batch payload."""
    out, pos = [], 0
    while pos < len(payload):
        end = skip_value(payload, pos)
        out.append((payload[pos + 2], bytes(payload[pos:end])))
        pos = end
    return out


def enc(typ, value) -> bytes:
    b = bytearray()
    encode_value(b, {TYPE: typ, VALUE: value})
    return bytes(b)


def map_pairs(section: bytes):
    """Raw (key, value) byte pairs of a described map section (annotations)."""
    p = 3  # 00 53 xx
    c = section[p]
    if c == 0xC1:
        count, p = section[p + 2], p + 3
    elif c == 0xD1:
        count, p = int.from_bytes(section[p + 5 : p + 9], "big"), p + 9
    else:
        return []
    pairs = []
    for _ in range(count // 2):
        k_end = skip_value(section, p)
        v_end = skip_value(section, k_end)
        pairs.append((section[p:k_end], section[k_end:v_end]))
        p = v_end
    return pairs


def map_section(code: int, pairs) -> bytes:
    body = b"".join(k + v for k, v in pairs)
    return b"\x00\x53" + bytes([code]) + b"\xd1" + struct.pack(">II", len(body) + 4, len(pairs) * 2) + body


def unbatch(payload: bytes):
    """Inner messages (raw bytes) of a batch payload: the content of each data section."""
    inner = []
    for code, raw in split_sections(payload):
        if code != 0x75:
            continue
        c = raw[3]
        if c == 0xA0:
            inner.append(raw[5 : 5 + raw[4]])
        elif c == 0xB0:
            n = int.from_bytes(raw[4:8], "big")
            inner.append(raw[8 : 8 + n])
    return inner


class Event:
    def __init__(self, seq, offset, enq_ms, sections, pkey_pair):
        self.seq, self.offset, self.enq_ms = seq, offset, enq_ms
        self.sections = sections  # [(code, raw)]
        self.pkey_pair = pkey_pair  # (raw key, raw value) of x-opt-partition-key, or None


class Partition:
    def __init__(self, pid):
        self.id = pid
        self.events = []
        self.next_offset = 0
        self.receivers = []  # live receiver links (consumer group, link)

    def append(self, sections, pkey_pair):
        size = sum(len(r) for _, r in sections)
        ev = Event(len(self.events), self.next_offset, int(time.time() * 1000), sections, pkey_pair)
        self.next_offset += max(size, 1)
        self.events.append(ev)
        return ev


class Hub:
    def __init__(self, name, count, groups):
        self.name = name
        self.created_ms = int(time.time() * 1000)
        self.partitions = [Partition(str(i)) for i in range(count)]
        self.groups = {"$default"} | {g.lower() for g in groups}
        self.rr = 0
        self.cond = threading.Condition()


def _free_hdr(typ):
    return {TYPE: typ}


class Link:
    def __init__(self, conn, sess, handle, name, client_is_receiver):
        self.conn, self.sess, self.handle, self.name = conn, sess, handle, name
        self.client_is_receiver = client_is_receiver
        self.kind = None  # cbs | mgmt | send | recv
        self.address = ""
        self.hub = None
        self.part = None
        self.partition_id = None
        self.group = None
        self.epoch = None
        self.credit = 0
        self.delivery_count = 0
        self.closed = False
        self.pending = bytearray()
        self.pending_tag = None
        self.pending_format = 0
        self.runtime = False
        self.start_index = 0
        self.lock = threading.Lock()


class Session:
    def __init__(self, channel):
        self.channel = channel
        self.links = {}
        self.reply = {}  # address -> server-sender Link used for $cbs / $management responses
        self.next_id = 0
        self.in_delivery_id = 0


class FakeEventHubs:
    def __init__(self, port=0, *, auto_create=True, auto_create_groups=True, default_partitions=4, verbose=False, tls=None):
        self.auto_create = auto_create
        self.auto_create_groups = auto_create_groups
        self.default_partitions = default_partitions
        self.verbose = verbose
        self.tls = tls  # (cert.pem, key.pem): connections that start with a TLS ClientHello are wrapped
        self.hubs = {}
        self.lock = threading.RLock()
        self.log_lines = []
        self.hellos = []  # (first 8 bytes, hostname in open) per connection, for the probe
        self._port = port
        self._srv = None
        self._stop = False
        self._conns = []

    # -- lifecycle ------------------------------------------------------------
    @property
    def port(self):
        return self._srv.getsockname()[1]

    def start(self):
        self._srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._srv.bind(("127.0.0.1", self._port))
        self._srv.listen(64)
        threading.Thread(target=self._accept_loop, daemon=True).start()
        return self

    def stop(self):
        self._stop = True
        try:
            self._srv.close()
        except OSError:
            pass
        for c in list(self._conns):
            c.close()

    def log(self, msg):
        self.log_lines.append(msg)
        if self.verbose:
            print(msg, flush=True)

    def _accept_loop(self):
        while not self._stop:
            try:
                s, _ = self._srv.accept()
            except OSError:
                return
            c = Conn(self, s)
            self._conns.append(c)
            threading.Thread(target=c.run, daemon=True).start()

    # -- state ----------------------------------------------------------------
    def reset(self, keep=None):
        with self.lock:
            self.hubs.clear()
        for c in list(self._conns):
            if c is not keep:
                c.close()
        self._conns = [c for c in self._conns if c is keep]

    def create_hub(self, ns, name, count=None, groups=()):
        with self.lock:
            key = (ns.lower(), name.lower())
            if key in self.hubs:
                return self.hubs[key]
            hub = Hub(name, count or self.default_partitions, groups)
            self.hubs[key] = hub
            return hub

    def get_hub(self, ns, name, create=True):
        with self.lock:
            hub = self.hubs.get((ns.lower(), name.lower()))
            if hub is None and create and self.auto_create:
                hub = self.create_hub(ns, name)
            return hub


class Conn:
    def __init__(self, server, sock):
        self.srv = server
        self.sock = sock
        self.wlock = threading.Lock()
        self.sessions = {}
        self.namespace = "default"
        self.max_frame = 65536
        self.alive = True

    def close(self):
        self.alive = False
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass

    # -- io -------------------------------------------------------------------
    def _read(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise EOFError
            buf += chunk
        return buf

    def _write(self, data):
        with self.wlock:
            self.sock.sendall(data)

    def send(self, channel, frame):
        header, body = encode_frame(frame)
        self._write(header + struct.pack(">H", channel) + (body or b""))

    def _sasl_frame(self, frame):
        header, body = encode_frame(frame, frame_type=b"\x01")
        self._write(header + b"\x00\x00" + body)

    # -- main loop --------------------------------------------------------------
    def run(self):
        try:
            if self.srv.tls and self.sock.recv(1, socket.MSG_PEEK) == b"\x16":
                import ssl

                ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                ctx.load_cert_chain(*self.srv.tls)
                self.sock = ctx.wrap_socket(self.sock, server_side=True)
            first = self._read(4)
            if first != b"AMQP":
                return self._http(first)
            hdr = first + self._read(4)
            self.srv.hellos.append(hdr)
            if hdr == SASL_HDR:
                self._write(SASL_HDR)
                self._sasl_frame(P.SASLMechanism(sasl_server_mechanisms=[b"ANONYMOUS"]))
                size = struct.unpack(">I", self._read(4))[0]
                rest = self._read(size - 4)
                _, fields = decode_frame(memoryview(rest[4:]))
                self.srv.log(f"sasl-init mechanism={fields[0]!r} hostname={fields[2] if len(fields) > 2 else None!r}")
                self._sasl_frame(P.SASLOutcome(code=0, additional_data=None))
                hdr = self._read(8)
            if hdr != AMQP_HDR:
                return
            self._write(AMQP_HDR)
            self._frames()
        except (EOFError, OSError, ValueError):
            pass
        finally:
            self.close()

    def _frames(self):
        while self.alive:
            hdr = self._read(8)
            size = struct.unpack(">I", hdr[:4])[0]
            channel = struct.unpack(">H", hdr[6:8])[0]
            if size == 8:
                continue  # keep-alive
            body = self._read(size - 8)
            code, fields = decode_frame(memoryview(body))
            if code != 20:
                fields = [bytes(f) if isinstance(f, memoryview) else f for f in fields]
            h = getattr(self, f"_on_{code:#x}", None)
            self.srv.log(f"<- {code:#x} ch={channel} {fields[:6] if code != 20 else fields[:10]}")
            if h is None:
                continue
            if h(channel, fields) == "stop":
                return

    # -- performatives ------------------------------------------------------------
    def _on_0x10(self, channel, f):  # open
        host = f[1].decode() if isinstance(f[1], (bytes, bytearray)) else f[1]
        self.srv.log(f"open container={f[0]!r} hostname={host!r} idle_timeout={f[4]!r} max_frame={f[2]!r}")
        self.srv.hellos[-1] = (self.srv.hellos[-1], host)
        if host:
            self.namespace = host.split(":")[0].split(".")[0]
        self.max_frame = min(f[2] or 65536, 1 << 20)
        self.send(0, P.OpenFrame(container_id="fake-eventhubs", max_frame_size=f[2] or 65536, channel_max=f[3] or 65535,
                                 idle_timeout=None, outgoing_locales=None, incoming_locales=None,
                                 offered_capabilities=None, desired_capabilities=None, hostname=None, properties=None))

    def _on_0x11(self, channel, f):  # begin
        self.sessions[channel] = Session(channel)
        self.send(channel, P.BeginFrame(remote_channel=channel, next_outgoing_id=0, incoming_window=1 << 30,
                                        outgoing_window=1 << 30, handle_max=4294967295, offered_capabilities=None,
                                        desired_capabilities=None, properties=None))

    def _on_0x12(self, channel, f):  # attach
        sess = self.sessions[channel]
        name, handle, role = f[0], f[1], f[2]
        link = Link(self, sess, handle, name, client_is_receiver=bool(role))
        sess.links[handle] = link
        source = f[5] or []
        target = f[6] or []
        props = f[13] or {}
        desired = f[12] or []
        if isinstance(desired, bytes):
            desired = [desired]
        self.srv.log(f"attach name={name!r} handle={handle} client_role={'receiver' if role else 'sender'} snd_settle={f[3]} rcv_settle={f[4]} "
                     f"source={source!r} target={target!r} max_message_size={f[10]!r} desired={desired!r} properties={props!r}")
        link.runtime = RUNTIME_METRIC in desired
        link.epoch = props.get(EPOCH)
        addr_raw = (source[0] if role and source else (target[0] if target else b"")) or b""
        link.address = addr_raw.decode() if isinstance(addr_raw, bytes) else str(addr_raw)
        filters = source[7] if role and len(source) > 7 and source[7] else {}
        error = self._classify(link, filters)
        reply_src = Source(address=addr_raw, filters=None) if role else Source(address=None)
        reply_tgt = Target(address=None) if role else Target(address=addr_raw)
        selector = None
        for v in filters.values():
            selector = v
        if selector is not None:
            reply_src = Source(address=addr_raw, filters={"apache.org:selector-filter:string": ("apache.org:selector-filter:string", selector.decode() if isinstance(selector, bytes) else selector)})
        self.send(channel, P.AttachFrame(
            name=name, handle=handle, role=not role, send_settle_mode=f[3], rcv_settle_mode=f[4], source=reply_src,
            target=reply_tgt, unsettled=None, incomplete_unsettled=False,
            initial_delivery_count=0 if role else None, max_message_size=MAX_MESSAGE_SIZE,
            offered_capabilities=[RUNTIME_METRIC] if link.runtime else None, desired_capabilities=None, properties=None))
        if error:
            link.closed = True
            self.send(channel, P.DetachFrame(handle=handle, closed=True, error=AMQPError(error[0], error[1])))
            return
        if not role:  # client sends to us: give credit
            link.credit = 1000
            self._flow(link)
        elif link.kind in ("cbs", "mgmt"):
            sess.reply[link.address.lower()] = link
        elif link.kind == "recv":
            self._start_receiver(link, filters)

    def _classify(self, link, filters):
        """Decide what a link is; returns (condition, description) to refuse it."""
        addr = link.address
        low = addr.lower()
        if low == "$cbs":
            link.kind = "cbs"
            return None
        if low.endswith("$management"):
            link.kind = "mgmt"
            return None
        path = urlparse(addr).path.strip("/") if "://" in addr else addr.strip("/")
        parts = path.split("/")
        hub = self.srv.get_hub(self.namespace, parts[0])
        if hub is None:
            return (b"amqp:not-found", f"The messaging entity '{parts[0]}' could not be found.")
        link.hub = hub
        low_parts = [p.lower() for p in parts]
        if link.client_is_receiver:
            link.kind = "recv"
            if len(parts) != 5 or low_parts[1] != "consumergroups" or low_parts[3] != "partitions":
                return (b"amqp:invalid-field", "bad consumer address")
            group, pid = parts[2], parts[4]
            if group.lower() not in hub.groups:
                if self.srv.auto_create_groups:
                    hub.groups.add(group.lower())
                else:
                    return (b"amqp:not-found", f"The messaging entity '{parts[0]}/{group}' could not be found.")
            part = next((p for p in hub.partitions if p.id == pid), None)
            if part is None:
                return (b"amqp:not-found", f"The messaging entity partition '{pid}' could not be found.")
            link.part, link.group, link.partition_id = part, group.lower(), pid
            return self._epoch_check(link)
        link.kind = "send"
        if len(parts) == 3 and low_parts[1] == "partitions":
            part = next((p for p in hub.partitions if p.id == parts[2]), None)
            if part is None:
                return (b"amqp:not-found", f"The messaging entity partition '{parts[2]}' could not be found.")
            link.part = part
        elif len(parts) != 1:
            return (b"amqp:invalid-field", "bad sender address")
        return None

    def _epoch_check(self, link):
        part = link.part
        with part_lock(part):
            live = [l for g, l in part.receivers if g == link.group and not l.closed]
            if link.epoch is None:
                if any(l.epoch is not None for l in live):
                    return (b"amqp:link:stolen", "Receiver with an epoch already exists on this partition; a non-epoch receiver cannot be created.")
                if len(live) >= 5:
                    return (b"amqp:resource-limit-exceeded", "Exceeded the maximum number of allowed receivers per partition in a consumer group which is 5.")
            else:
                for l in live:
                    if l.epoch is not None and l.epoch > link.epoch:
                        return (b"amqp:link:stolen", f"Receiver with epoch '{l.epoch}' exists; epoch '{link.epoch}' is lower.")
                for l in live:
                    l.closed = True
                    l.conn._detach_error(l, b"amqp:link:stolen", "New receiver with higher or equal epoch is created hence current receiver is getting disconnected.")
            part.receivers.append((link.group, link))
        return None

    def _detach_error(self, link, cond, desc):
        try:
            self.send(link.sess.channel, P.DetachFrame(handle=link.handle, closed=True, error=AMQPError(cond, desc)))
        except OSError:
            pass

    def _flow(self, link):
        self.send(link.sess.channel, P.FlowFrame(next_incoming_id=link.sess.in_delivery_id, incoming_window=1 << 30,
                                                 next_outgoing_id=link.sess.next_id, outgoing_window=1 << 30,
                                                 handle=link.handle, delivery_count=link.delivery_count,
                                                 link_credit=link.credit, available=None, drain=False, echo=False,
                                                 properties=None))

    def _on_0x13(self, channel, f):  # flow
        sess = self.sessions.get(channel)
        if not sess or f[4] is None:
            return
        link = sess.links.get(f[4])
        if link is None or not link.client_is_receiver:
            return
        with link.lock:
            link.credit = (f[5] or 0) + (f[6] or 0) - link.delivery_count
        with link.hub.cond if link.hub else threading.Lock():
            if link.hub:
                link.hub.cond.notify_all()

    def _on_0x14(self, channel, f):  # transfer
        sess = self.sessions[channel]
        link = sess.links.get(f[0])
        if link is None or link.closed:
            return
        payload = bytes(f[11])
        if f[1] is not None:
            sess.in_delivery_id = f[1]
            link.pending_id = f[1]
            link.pending_tag = f[2]
            link.pending_format = f[3] or 0
            link.pending_settled = bool(f[4])
        link.pending += payload
        if f[5]:
            if len(link.pending) > MAX_MESSAGE_SIZE:
                link.pending = bytearray()
                link.closed = True
                self.send(channel, P.DetachFrame(handle=link.handle, closed=True, error=AMQPError(b"amqp:link:message-size-exceeded", "The received message exceeds the maximum message size allowed on the link.")))
            return
        data, link.pending = bytes(link.pending), bytearray()
        link.delivery_count += 1
        state = Accepted()
        if len(data) > MAX_MESSAGE_SIZE:
            link.closed = True
            self.send(channel, P.DispositionFrame(role=True, first=link.pending_id, last=None, settled=True,
                                                  state=Rejected(error=AMQPError(b"amqp:link:message-size-exceeded", "too large")), batchable=None))
            self.send(channel, P.DetachFrame(handle=link.handle, closed=True, error=AMQPError(b"amqp:link:message-size-exceeded", "The received message exceeds the maximum message size allowed on the link.")))
            return
        if link.kind == "cbs":
            status, desc = self._cbs_status(data)
            self._respond(sess, "$cbs", data, status, desc, None)
        elif link.kind == "mgmt":
            self._mgmt(sess, link, data)
        elif link.kind == "send":
            self._ingest(link, data, link.pending_format)
        if not link.pending_settled:
            self.send(channel, P.DispositionFrame(role=True, first=link.pending_id, last=None, settled=True, state=state, batchable=None))
        if link.delivery_count % 500 == 0:
            link.credit = 1000
            self._flow(link)

    def _cbs_status(self, data):
        """put-token for a hub or consumer group that does not exist is answered 404, as the real service does (the SDK turns
        any non-2xx status into AuthenticationError; Microsoft's test_client_invalid_credential relies on this for "fakehub")."""
        req = decode_payload(memoryview(data))
        app = {(k.decode() if isinstance(k, bytes) else k): v for k, v in (req.application_properties or {}).items()}
        name = app.get("name")
        name = name.decode() if isinstance(name, bytes) else str(name)
        path = urlparse(name).path.strip("/").split("/") if "://" in name else name.strip("/").split("/")
        hub = self.srv.get_hub(self.namespace, path[0]) if path and path[0] else None
        if path and path[0] and hub is None:
            return 404, f"The messaging entity 'sb://{name.split('://')[-1]}' could not be found."
        if hub is not None and len(path) >= 3 and path[1].lower() == "consumergroups" and path[2].lower() not in hub.groups \
                and not self.srv.auto_create_groups:
            return 404, f"The messaging entity '{name}' could not be found."
        return 202, "Accepted"

    def _on_0x15(self, channel, f):  # disposition
        pass

    def _on_0x16(self, channel, f):  # detach
        sess = self.sessions.get(channel)
        link = sess.links.pop(f[0], None) if sess else None
        if link is not None:
            link.closed = True
            if link.part is not None:
                with part_lock(link.part):
                    link.part.receivers = [(g, l) for g, l in link.part.receivers if l is not link]
            if link.hub:
                with link.hub.cond:
                    link.hub.cond.notify_all()
            if not getattr(link, "_detached_by_us", False):
                self.send(channel, P.DetachFrame(handle=f[0], closed=True, error=None))

    def _on_0x17(self, channel, f):  # end
        sess = self.sessions.pop(channel, None)
        if sess:
            for link in sess.links.values():
                link.closed = True
                if link.part is not None:
                    with part_lock(link.part):
                        link.part.receivers = [(g, l) for g, l in link.part.receivers if l is not link]
        self.send(channel, P.EndFrame(error=None))

    def _on_0x18(self, channel, f):  # close
        self.send(0, P.CloseFrame(error=None))
        return "stop"

    # -- request / response ---------------------------------------------------------
    def _respond(self, sess, node, request_payload, status, desc, body_value_bytes):
        link = sess.reply.get(node)
        if link is None:
            return
        req = decode_payload(memoryview(request_payload))
        msg_id = req.properties.message_id if req.properties else None
        props = Properties(correlation_id=msg_id)
        resp = bytearray()
        encode_payload(resp, Message(properties=props, application_properties={"status-code": status, "status-description": desc}))
        out = bytes(resp) + (b"\x00\x53\x77" + body_value_bytes if body_value_bytes is not None else b"")
        link.sess.next_id += 1
        link.delivery_count += 1
        self.send(sess.channel, P.TransferFrame(handle=link.handle, delivery_id=sess.next_id - 1, delivery_tag=struct.pack(">I", link.delivery_count),
                                                message_format=0, settled=True, more=False, rcv_settle_mode=None, state=None,
                                                resume=None, aborted=None, batchable=None, payload=out))

    def _mgmt(self, sess, link, data):
        req = decode_payload(memoryview(data))
        app = {(k.decode() if isinstance(k, bytes) else k): v for k, v in (req.application_properties or {}).items()}
        s = lambda v: v.decode() if isinstance(v, bytes) else v
        name = s(app.get("name"))
        typ = s(app.get("type"))
        hub = self.srv.get_hub(self.namespace, name) if name else None
        if hub is None:
            return self._respond(sess, link.address.lower(), data, 404, f"The messaging entity '{name}' could not be found.", None)
        if typ == "com.microsoft:eventhub":
            # array32 of str8 elements: constructor once, then len+bytes per element
            elems = b"".join(bytes([len(p.id)]) + p.id.encode() for p in hub.partitions)
            arr = b"\xf0" + struct.pack(">II", len(elems) + 5, len(hub.partitions)) + b"\xa1" + elems
            pairs = [(enc(AMQPTypes.string, "name"), enc(AMQPTypes.string, hub.name)),
                     (enc(AMQPTypes.string, "created_at"), enc(AMQPTypes.timestamp, hub.created_ms)),
                     (enc(AMQPTypes.string, "partition_ids"), arr)]
            return self._respond(sess, link.address.lower(), data, 200, "OK", _map_value(pairs))
        if typ == "com.microsoft:partition":
            pid = s(app.get("partition"))
            part = next((p for p in hub.partitions if p.id == pid), None)
            if part is None:
                return self._respond(sess, link.address.lower(), data, 400, f"The specified partition is invalid for an EventHub partition sender or receiver. Partition '{pid}'.", None)
            with part_lock(part):
                evs = list(part.events)
            last = evs[-1] if evs else None
            pairs = [(enc(AMQPTypes.string, "name"), enc(AMQPTypes.string, hub.name)),
                     (enc(AMQPTypes.string, "partition"), enc(AMQPTypes.string, part.id)),
                     (enc(AMQPTypes.string, "begin_sequence_number"), enc(AMQPTypes.long, evs[0].seq if evs else -1)),
                     (enc(AMQPTypes.string, "last_enqueued_sequence_number"), enc(AMQPTypes.long, last.seq if last else -1)),
                     (enc(AMQPTypes.string, "last_enqueued_offset"), enc(AMQPTypes.string, str(last.offset) if last else "-1")),
                     (enc(AMQPTypes.string, "last_enqueued_time_utc"), enc(AMQPTypes.timestamp, last.enq_ms if last else SENTINEL_TIME_MS)),
                     (enc(AMQPTypes.string, "is_partition_empty"), enc(AMQPTypes.boolean, not evs))]
            return self._respond(sess, link.address.lower(), data, 200, "OK", _map_value(pairs))
        return self._respond(sess, link.address.lower(), data, 400, "unsupported management type", None)

    # -- sending to a hub -------------------------------------------------------------
    def _ingest(self, link, data, fmt):
        hub = link.hub
        outers = split_sections(data)
        outer_pkey = None
        if fmt == BATCH_FORMAT:
            for code, raw in outers:
                if code == 0x72:
                    for k, v in map_pairs(raw):
                        if k.endswith(b"x-opt-partition-key"):
                            outer_pkey = (k, v)
            messages = [split_sections(m) for m in unbatch(data)]
        else:
            messages = [outers]
        # one batch lands on one partition: by link, by key of the first event / outer key, else round robin
        with hub.cond:
            if link.part is not None:
                part = link.part
            else:
                key = outer_pkey
                if key is None:
                    for code, raw in messages[0]:
                        if code == 0x72:
                            for k, v in map_pairs(raw):
                                if k.endswith(b"x-opt-partition-key"):
                                    key = (k, v)
                if key is not None:
                    part = hub.partitions[zlib.crc32(_str_value(key[1])) % len(hub.partitions)]
                else:
                    part = hub.partitions[hub.rr % len(hub.partitions)]
                    hub.rr += 1
            for secs in messages:
                pkey = None
                for code, raw in secs:
                    if code == 0x72:
                        for k, v in map_pairs(raw):
                            if k.endswith(b"x-opt-partition-key"):
                                pkey = (k, v)
                part.append(list(secs), pkey)  # delivery annotations the client wrote last are kept (Microsoft's test_send_amqp_annotated_message)
            hub.cond.notify_all()

    # -- receiving from a hub ------------------------------------------------------------
    def _start_receiver(self, link, filters):
        part = link.part
        selector = None
        for v in filters.values():
            selector = v.decode() if isinstance(v, bytes) else v
        idx = 0
        if selector:
            import re

            m = re.match(r"amqp\.annotation\.x-opt-(offset|sequence-number|enqueued-time)\s*(>=|>)\s*'([^']*)'", selector)
            if m:
                kind, op, val = m.groups()
                with part_lock(part):
                    n = len(part.events)
                    if val == "@latest":
                        idx = n
                    else:
                        num = int(val)
                        attr = {"offset": "offset", "sequence-number": "seq", "enqueued-time": "enq_ms"}[kind]
                        idx = n
                        for i, e in enumerate(part.events):
                            v = getattr(e, attr)
                            if (v >= num) if op == ">=" else (v > num):
                                idx = i
                                break
        link.start_index = idx
        threading.Thread(target=self._pump, args=(link,), daemon=True).start()

    def _pump(self, link):
        part, hub = link.part, link.hub
        idx = link.start_index
        while self.alive and not link.closed:
            with hub.cond:
                while not link.closed and self.alive and not (link.credit > 0 and idx < len(part.events)):
                    hub.cond.wait(0.05)
                if link.closed or not self.alive:
                    return
                ev = part.events[idx]
                last = part.events[-1]
            idx += 1
            payload = self._build(ev, last if link.runtime else None)
            try:
                self._deliver(link, payload)
            except OSError:
                return
            with link.lock:
                link.credit -= 1

    def _build(self, ev, last):
        secs = ev.sections
        out = bytearray()
        for code, raw in secs:
            if code == 0x70:
                out += raw
        dpairs = []
        for code, raw in secs:
            if code == 0x71:
                dpairs += map_pairs(raw)
        if last is not None:
            runtime = [
                (enc(AMQPTypes.symbol, b"last_enqueued_sequence_number"), enc(AMQPTypes.long, last.seq)),
                (enc(AMQPTypes.symbol, b"last_enqueued_offset"), enc(AMQPTypes.string, str(last.offset))),
                (enc(AMQPTypes.symbol, b"last_enqueued_time_utc"), enc(AMQPTypes.timestamp, last.enq_ms)),
                (enc(AMQPTypes.symbol, b"runtime_info_retrieval_time_utc"), enc(AMQPTypes.timestamp, int(time.time() * 1000))),
            ]
            names = {k for k, _ in runtime}
            dpairs = [(k, v) for k, v in dpairs if k not in names] + runtime
        if dpairs:
            out += map_section(0x71, dpairs)
        pairs = []
        for code, raw in secs:
            if code == 0x72:
                pairs += [(k, v) for k, v in map_pairs(raw) if not (k.endswith(b"x-opt-offset") or k.endswith(b"x-opt-sequence-number") or k.endswith(b"x-opt-enqueued-time"))]
        have_key = any(k.endswith(b"x-opt-partition-key") for k, _ in pairs)
        pairs += [(enc(AMQPTypes.symbol, b"x-opt-sequence-number"), enc(AMQPTypes.long, ev.seq)),
                  (enc(AMQPTypes.symbol, b"x-opt-offset"), enc(AMQPTypes.string, str(ev.offset))),
                  (enc(AMQPTypes.symbol, b"x-opt-enqueued-time"), enc(AMQPTypes.timestamp, ev.enq_ms))]
        out += map_section(0x72, pairs)
        for code, raw in secs:
            if code not in (0x70, 0x71, 0x72):
                out += raw
        return bytes(out)

    def _deliver(self, link, payload):
        sess = link.sess
        chunk = max(self.max_frame - 200, 512)
        parts = [payload[i : i + chunk] for i in range(0, len(payload), chunk)] or [b""]
        did = sess.next_id
        sess.next_id += 1
        link.delivery_count += 1
        for i, p in enumerate(parts):
            more = i < len(parts) - 1
            if i == 0:
                self.send(sess.channel, P.TransferFrame(handle=link.handle, delivery_id=did, delivery_tag=struct.pack(">I", link.delivery_count),
                                                        message_format=0, settled=True, more=more, rcv_settle_mode=None, state=None,
                                                        resume=None, aborted=None, batchable=None, payload=p))
            else:
                self.send(sess.channel, P.TransferFrame(handle=link.handle, delivery_id=None, delivery_tag=None, message_format=None,
                                                        settled=None, more=more, rcv_settle_mode=None, state=None, resume=None,
                                                        aborted=None, batchable=None, payload=p))

    # -- HTTP admin ---------------------------------------------------------------------
    def _http(self, first):
        data = first
        while b"\r\n\r\n" not in data:
            chunk = self.sock.recv(4096)
            if not chunk:
                return
            data += chunk
        head, _, rest = data.partition(b"\r\n\r\n")
        lines = head.decode().split("\r\n")
        method, path, _ = lines[0].split(" ", 2)
        headers = {l.split(":", 1)[0].lower(): l.split(":", 1)[1].strip() for l in lines[1:] if ":" in l}
        n = int(headers.get("content-length", 0))
        while len(rest) < n:
            rest += self.sock.recv(4096)
        body = rest[:n]
        status, payload = 404, {"error": "not found"}
        parts = [p for p in path.split("?")[0].split("/") if p]
        if method == "GET" and parts == ["_azemu", "health"]:
            status, payload = 200, {"status": "ok", "services": ["eventhubs"]}
        elif method == "POST" and parts == ["_azemu", "reset"]:
            self.srv.reset(keep=self)
            status, payload = 200, {"status": "reset"}
        elif method == "PUT" and len(parts) == 5 and parts[:2] == ["_azemu", "eventhubs"] and parts[3] == "hubs":
            cfg = json.loads(body or b"{}")
            self.srv.create_hub(parts[2], parts[4], cfg.get("partitionCount"), cfg.get("consumerGroups", []))
            status, payload = 200, {"name": parts[4]}
        resp = json.dumps(payload).encode()
        self._write(f"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {len(resp)}\r\nConnection: close\r\n\r\n".encode() + resp)


_PART_LOCKS = {}


def part_lock(part):
    lock = _PART_LOCKS.get(id(part))
    if lock is None:
        lock = _PART_LOCKS.setdefault(id(part), threading.RLock())
    return lock


def _map_value(pairs):
    body = b"".join(k + v for k, v in pairs)
    return b"\xd1" + struct.pack(">II", len(body) + 4, len(pairs) * 2) + body


def _str_value(raw: bytes) -> bytes:
    """The text of an encoded string / symbol / binary value (for hashing partition keys)."""
    c = raw[0]
    if c in (0xA0, 0xA1, 0xA3):
        return raw[2 : 2 + raw[1]]
    if c in (0xB0, 0xB1, 0xB3):
        return raw[5 : 5 + int.from_bytes(raw[1:5], "big")]
    return raw


if __name__ == "__main__":
    tls = None
    if "--tls-dir" in sys.argv:  # make a throwaway CA + server certificate there; the CA path is printed
        sys.path.insert(0, __file__.rsplit("/", 1)[0])
        from sdk_dial_probe import make_ca

        ca, cert, key = make_ca(sys.argv[sys.argv.index("--tls-dir") + 1])
        tls = (cert, key)
        print(f"AZEMU_CA_FILE={ca}", flush=True)
    srv = FakeEventHubs(int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 0, verbose="-v" in sys.argv, tls=tls,
                       auto_create="--no-autocreate" not in sys.argv, auto_create_groups="--no-autocreate" not in sys.argv).start()
    print(f"fake event hubs on 127.0.0.1:{srv.port}", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        srv.stop()
