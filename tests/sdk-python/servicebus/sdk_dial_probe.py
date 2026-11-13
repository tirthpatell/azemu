"""Tiny AMQP 1.0 listener that shows what the azure-servicebus SDK dials and sends first.

Not a test (the name does not match test_*.py). Run it by hand:

    .venv/bin/python tests/sdk-python/servicebus/sdk_dial_probe.py

It starts a listener on a free local port, points the SDK at it in several
variants (connection string with UseDevelopmentEmulator, TLS with a custom
endpoint) and prints for each variant the raw first bytes the SDK writes and
the performatives it sends (SASL init, open, begin, ...). The listener answers
just enough (SASL ANONYMOUS, then the AMQP header and an open frame) to see the
`open` frame; then it closes the socket, so the SDK call ends in an error. That
error is expected: only the printed first bytes matter.
"""

import datetime
import socket
import ssl
import struct
import tempfile
import sys
import threading

from azure.servicebus import ServiceBusClient, ServiceBusMessage
from azure.servicebus._pyamqp._decode import decode_frame

SASL_HDR = b"AMQP\x03\x01\x00\x00"
AMQP_HDR = b"AMQP\x00\x01\x00\x00"
TLS_HDR = b"AMQP\x02\x01\x00\x00"

NAMES = {0x10: "open", 0x11: "begin", 0x12: "attach", 0x13: "flow", 0x14: "transfer",
         0x15: "disposition", 0x16: "detach", 0x17: "end", 0x18: "close",
         0x41: "sasl-init", 0x40: "sasl-mechanisms", 0x44: "sasl-outcome"}


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
    arr_body = bytes([1, 0xA3, len(sym)]) + sym  # count, element constructor, one symbol
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
    """Read what the client sends, answering SASL and the open frame."""
    try:
        conn.settimeout(1.5)  # give up (and close) when the SDK has said all it will say
        first = _read_exact(conn, 8)
        log(f"  first 8 bytes on the wire: {first!r}")
        if first == SASL_HDR:
            conn.sendall(SASL_HDR + _sasl_mechanisms())
            while True:
                hdr = _read_exact(conn, 8)
                size = struct.unpack(">I", hdr[:4])[0]
                body = _read_exact(conn, size - 8)
                if hdr[5:6] == b"\x01":
                    # SASL frame (type 1); skip 2 channel bytes already in hdr
                    ftype, fields = decode_frame(memoryview(body))
                    log(f"  SASL frame {NAMES.get(ftype, hex(ftype))}: {[f if not isinstance(f, memoryview) else bytes(f) for f in fields]}")
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
            shown = fields[:7] if ftype == 0x12 else fields[:4]
            log(f"  AMQP frame {NAMES.get(ftype, hex(ftype))}: {shown}")
            if ftype == 0x10:
                log(f"    open.container_id={fields[0]!r} open.hostname={fields[1]!r}")
                conn.sendall(_open_frame())
    except Exception as exc:  # noqa: BLE001
        log(f"  (stub stopped: {type(exc).__name__})")
    finally:
        conn.close()


def make_ca(dirname):
    """Make a throwaway CA and a server certificate for localhost names. Returns (ca_pem, cert_pem, key_pem)."""
    import ipaddress

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    now = datetime.datetime.now(datetime.timezone.utc)
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "probe-ca")])
    ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name)
          .public_key(ca_key.public_key()).serial_number(1)
          .not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=30))
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
                x509.DNSName("localhost"), x509.DNSName("*.servicebus.localhost"),
                x509.DNSName("servicebus.localhost"), x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]),
                critical=False)
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
        fh.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                   serialization.NoEncryption()))
    return paths[0], paths[1], key_path


def probe(label, make_client_and_call, tls=None):
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    port = srv.getsockname()[1]
    lines = []
    srv.settimeout(5)

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
            lines.append(f"  TLS handshake ok, SNI/server_hostname seen by the server: {getattr(c, 'server_hostname', None)}")
        serve_one(c, lines.append)

    t = threading.Thread(target=run)
    t.start()
    print(f"== {label} (port {port})")
    try:
        make_client_and_call(port, *( [tls[0]] if tls else []))
    except Exception as exc:  # noqa: BLE001
        print(f"  SDK raised {type(exc).__name__}: {str(exc)[:100]}")
    t.join(8)
    srv.close()
    print("\n".join(lines))


def main():
    from azure.core.credentials import AzureNamedKeyCredential
    from azure.servicebus._common.constants import TransportType

    def tls_custom_endpoint(port, ca):
        cred = AzureNamedKeyCredential("k", "v")
        with ServiceBusClient("default.servicebus.localhost", cred, retry_total=0,
                              custom_endpoint_address=f"sb://default.servicebus.localhost:{port}",
                              transport_type=TransportType.Amqp, connection_verify=ca) as c:
            with c.get_queue_sender("q") as s:
                s.send_messages(ServiceBusMessage("x"))

    def tls_custom_endpoint_ip(port, ca):
        cred = AzureNamedKeyCredential("k", "v")
        with ServiceBusClient("default.servicebus.localhost", cred, retry_total=0,
                              custom_endpoint_address=f"sb://127.0.0.1:{port}",
                              transport_type=TransportType.Amqp, connection_verify=ca) as c:
            with c.get_queue_sender("q") as s:
                s.send_messages(ServiceBusMessage("x"))

    def plain_kwarg(port):
        cred = AzureNamedKeyCredential("k", "v")
        with ServiceBusClient(f"127.0.0.1:{port}", cred, retry_total=0, use_tls=False) as c:
            with c.get_queue_sender("q") as s:
                s.send_messages(ServiceBusMessage("x"))

    def plain_ip(port):
        cs = f"Endpoint=sb://127.0.0.1:{port};SharedAccessKeyName=k;SharedAccessKey=v;UseDevelopmentEmulator=true"
        with ServiceBusClient.from_connection_string(cs, retry_total=0) as c:
            with c.get_queue_sender("q") as s:
                s.send_messages(ServiceBusMessage("x"))

    def plain_name(port):
        cs = f"Endpoint=sb://default.servicebus.localhost:{port};SharedAccessKeyName=k;SharedAccessKey=v;UseDevelopmentEmulator=true"
        with ServiceBusClient.from_connection_string(cs, retry_total=0) as c:
            with c.get_queue_sender("q") as s:
                s.send_messages(ServiceBusMessage("x"))

    probe("plain, Endpoint=sb://127.0.0.1:PORT;UseDevelopmentEmulator=true", plain_ip)
    probe("plain, Endpoint=sb://default.servicebus.localhost:PORT;UseDevelopmentEmulator=true", plain_name)
    probe("plain, ServiceBusClient('127.0.0.1:PORT', cred, use_tls=False)", plain_kwarg)
    with tempfile.TemporaryDirectory() as d:
        tls = make_ca(d)
        probe("TLS, fqn default.servicebus.localhost + custom_endpoint_address=sb://default.servicebus.localhost:PORT",
              tls_custom_endpoint, tls)
        probe("TLS, fqn default.servicebus.localhost + custom_endpoint_address=sb://127.0.0.1:PORT",
              tls_custom_endpoint_ip, tls)


if __name__ == "__main__":
    sys.exit(main())
