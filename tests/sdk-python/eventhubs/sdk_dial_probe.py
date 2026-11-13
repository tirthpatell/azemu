"""Shows what the azure-eventhub SDK dials and sends first. Not a test (no `test_` prefix); run it by hand:

    .venv/bin/python tests/sdk-python/eventhubs/sdk_dial_probe.py

Part 1 starts a bare TCP listener per variant (plain AMQP from a `UseDevelopmentEmulator=true` connection string, plain with
an IP, TLS with `custom_endpoint_address` + `connection_verify`), prints the first bytes the SDK writes, the SASL frames and
the `open` frame, answers just enough (SASL ANONYMOUS, AMQP header, open) to see them, and then closes the socket: the SDK
call ends in an error, which is expected. Part 2 runs one send and one receive against `fake_eventhubs.FakeEventHubs` with
logging on, which prints every attach (addresses, filters, link properties, capabilities) the SDK makes.
"""

import datetime
import socket
import ssl
import struct
import sys
import tempfile
import threading

from azure.core.credentials import AzureNamedKeyCredential
from azure.eventhub import EventData, EventHubConsumerClient, EventHubProducerClient, TransportType
from azure.eventhub._pyamqp._decode import decode_frame

SASL_HDR = b"AMQP\x03\x01\x00\x00"
AMQP_HDR = b"AMQP\x00\x01\x00\x00"
NAMES = {0x10: "open", 0x11: "begin", 0x12: "attach", 0x13: "flow", 0x14: "transfer", 0x15: "disposition", 0x16: "detach",
         0x17: "end", 0x18: "close", 0x41: "sasl-init", 0x40: "sasl-mechanisms", 0x44: "sasl-outcome"}


def _read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def _frame(ftype, body):
    return struct.pack(">IBBH", len(body) + 8, 2, ftype, 0) + body


def _sasl_mechanisms():
    sym = b"ANONYMOUS"
    arr_body = bytes([1, 0xA3, len(sym)]) + sym
    arr = b"\xe0" + bytes([len(arr_body)]) + arr_body
    lst = b"\xc0" + bytes([len(arr) + 1, 1]) + arr
    return _frame(1, b"\x00\x53\x40" + lst)


def _sasl_outcome_ok():
    return _frame(1, b"\x00\x53\x44\xc0\x03\x01\x50\x00")


def _open_frame(container=b"probe"):
    cid = b"\xa1" + bytes([len(container)]) + container
    lst = b"\xc0" + bytes([len(cid) + 1, 1]) + cid
    return _frame(0, b"\x00\x53\x10" + lst)


def serve_one(conn, log):
    try:
        conn.settimeout(1.5)
        first = _read_exact(conn, 8)
        log(f"  first 8 bytes on the wire: {first!r}")
        if first == SASL_HDR:
            conn.sendall(SASL_HDR + _sasl_mechanisms())
            while True:
                hdr = _read_exact(conn, 8)
                size = struct.unpack(">I", hdr[:4])[0]
                body = _read_exact(conn, size - 8)
                if hdr[5:6] == b"\x01":
                    ftype, fields = decode_frame(memoryview(body))
                    log(f"  SASL frame {NAMES.get(ftype, hex(ftype))}: {[bytes(f) if isinstance(f, memoryview) else f for f in fields]}")
                    if ftype == 0x41:
                        conn.sendall(_sasl_outcome_ok())
                        break
        second = _read_exact(conn, 8)
        log(f"  next 8 bytes (after SASL): {second!r}")
        conn.sendall(AMQP_HDR)
        for _ in range(6):
            hdr = _read_exact(conn, 8)
            size = struct.unpack(">I", hdr[:4])[0]
            body = _read_exact(conn, size - 8) if size > 8 else b""
            if not body:
                continue
            ftype, fields = decode_frame(memoryview(body))
            log(f"  AMQP frame {NAMES.get(ftype, hex(ftype))}: {fields[:6]}")
            if ftype == 0x10:
                log(f"    open.container_id={fields[0]!r} open.hostname={fields[1]!r} idle_timeout={fields[4]!r}")
                conn.sendall(_open_frame())
    except Exception as exc:  # noqa: BLE001
        log(f"  (stub stopped: {type(exc).__name__})")
    finally:
        conn.close()


def make_ca(dirname):
    import ipaddress

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    now = datetime.datetime.now(datetime.timezone.utc)
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "probe-ca")])
    ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name).public_key(ca_key.public_key())
          .serial_number(1).not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=30))
          .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
          .add_extension(x509.SubjectKeyIdentifier.from_public_key(ca_key.public_key()), critical=False)
          .add_extension(x509.KeyUsage(digital_signature=True, key_cert_sign=True, crl_sign=True, content_commitment=False,
                                       key_encipherment=False, data_encipherment=False, key_agreement=False,
                                       encipher_only=False, decipher_only=False), critical=True)
          .sign(ca_key, hashes.SHA256()))
    key = ec.generate_private_key(ec.SECP256R1())
    cert = (x509.CertificateBuilder().subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "probe")]))
            .issuer_name(ca_name).public_key(key.public_key()).serial_number(2)
            .not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=30))
            .add_extension(x509.SubjectAlternativeName([
                x509.DNSName("localhost"), x509.DNSName("*.eventhub.localhost"), x509.DNSName("eventhub.localhost"),
                x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), critical=False)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
            .sign(ca_key, hashes.SHA256()))
    paths = []
    for name, obj in (("ca.pem", ca), ("cert.pem", cert)):
        path = f"{dirname}/{name}"
        with open(path, "wb") as fh:
            fh.write(obj.public_bytes(serialization.Encoding.PEM))
        paths.append(path)
    key_path = f"{dirname}/key.pem"
    with open(key_path, "wb") as fh:
        fh.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    return paths[0], paths[1], key_path


def probe(label, call, tls=None):
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    lines = []
    srv.settimeout(6)

    def run():
        try:
            c, _ = srv.accept()
        except OSError:
            lines.append("  (client never connected)")
            return
        if tls:
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(tls[1], tls[2])
            try:
                c = ctx.wrap_socket(c, server_side=True)
            except ssl.SSLError as exc:
                lines.append(f"  TLS handshake failed on the server: {exc}")
                return
            lines.append("  TLS handshake ok")
        serve_one(c, lines.append)

    t = threading.Thread(target=run)
    t.start()
    print(f"== {label} (port {port})")
    try:
        call(port, *([tls[0]] if tls else []))
    except Exception as exc:  # noqa: BLE001
        print(f"  SDK raised {type(exc).__name__}: {str(exc)[:110]!r}")
    t.join(10)
    srv.close()
    print("\n".join(lines))


def main():
    kw = dict(retry_total=0, auth_timeout=2)

    def cs_call(host):
        def call(port):
            cs = f"Endpoint=sb://{host.format(port=port)};SharedAccessKeyName=k;SharedAccessKey=dg==;UseDevelopmentEmulator=true"
            with EventHubProducerClient.from_connection_string(cs, eventhub_name="hub", **kw) as c:
                c.get_partition_ids()
        return call

    def tls_call(endpoint_host):
        def call(port, ca):
            cred = AzureNamedKeyCredential("k", "dg==")
            with EventHubProducerClient("myns.eventhub.localhost", "hub", cred, custom_endpoint_address=f"sb://{endpoint_host}:{port}",
                                        transport_type=TransportType.Amqp, connection_verify=ca, **kw) as c:
                c.get_partition_ids()
        return call

    probe("plain: Endpoint=sb://myns.eventhub.localhost:PORT;UseDevelopmentEmulator=true", cs_call("myns.eventhub.localhost:{port}"))
    probe("plain: Endpoint=sb://127.0.0.1:PORT;UseDevelopmentEmulator=true", cs_call("127.0.0.1:{port}"))
    with tempfile.TemporaryDirectory() as d:
        tls = make_ca(d)
        probe("TLS: fqn myns.eventhub.localhost + custom_endpoint_address=sb://myns.eventhub.localhost:PORT + transport_type=Amqp",
              tls_call("myns.eventhub.localhost"), tls)
        probe("TLS: same with custom_endpoint_address=sb://127.0.0.1:PORT", tls_call("127.0.0.1"), tls)

    print("\n== full session against the in-process double (every attach the SDK makes)")
    sys.path.insert(0, __file__.rsplit("/", 1)[0])
    from fake_eventhubs import FakeEventHubs

    fake = FakeEventHubs(verbose=True).start()
    cs = f"Endpoint=sb://myns.eventhub.localhost:{fake.port};SharedAccessKeyName=k;SharedAccessKey=dg==;UseDevelopmentEmulator=true"
    with EventHubProducerClient.from_connection_string(cs, eventhub_name="hub", **kw) as p:
        p.send_batch([EventData("one")], partition_id="0")
    got = []
    cons = EventHubConsumerClient.from_connection_string(cs, consumer_group="$Default", eventhub_name="hub", **kw)

    def on_event(ctx, ev):
        got.append(ev)

    th = threading.Thread(target=cons.receive, args=(on_event,),
                          kwargs=dict(partition_id="0", starting_position="-1", track_last_enqueued_event_properties=True, owner_level=3))
    th.start()
    import time

    time.sleep(2)
    cons.close()
    th.join(5)
    print("received:", [e.body_as_str() for e in got])
    fake.stop()


if __name__ == "__main__":
    sys.exit(main())
