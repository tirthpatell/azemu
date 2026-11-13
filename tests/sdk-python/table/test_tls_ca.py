"""The emulator's local CA is limited to localhost, names under it and loopback addresses.

The checks use Python's `ssl` (OpenSSL), the library behind `requests` and every Azure Python SDK:

- a name inside the limits gets a certificate that verifies against `ca.pem`;
- a name outside gets a TLS alert, not a certificate;
- a certificate for another name that is signed with the CA's key (what someone who stole `ca.key` could make)
  is rejected by the client because of the CA's name constraints ("permitted subtree violation");
- `ca.key` is readable by its owner only.
"""

import os
import shutil
import socket
import ssl
import stat
import subprocess
import tempfile

import pytest


@pytest.fixture
def cert_dir(emu):
    if emu.storage_is_external or not emu.ca_file:
        pytest.skip("needs the emulator the fixture started")
    return os.path.dirname(emu.ca_file)


def handshake(port, name, ca_file):
    ctx = ssl.create_default_context(cafile=ca_file)
    with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
        with ctx.wrap_socket(s, server_hostname=name) as t:
            return t.version()


@pytest.mark.parametrize("name", ["acct.table.localhost", "localhost", "x.y.localhost", "127.0.0.1"])
def test_names_inside_the_limits_verify(emu, name):
    assert handshake(emu.port, name, emu.ca_file).startswith("TLS")


@pytest.mark.parametrize("name", ["example.com", "acct.table.core.windows.net", "login.microsoftonline.com", "azemu"])
def test_names_outside_the_limits_are_refused_with_an_alert(emu, name):
    with pytest.raises(ssl.SSLError) as e:
        handshake(emu.port, name, emu.ca_file)
    assert "ALERT" in str(e.value).upper()


def test_ca_key_is_private_and_the_root_key_is_not_kept(cert_dir):
    assert stat.S_IMODE(os.stat(os.path.join(cert_dir, "ca.key")).st_mode) == 0o600
    # ca.pem is the root clients trust; ca.key belongs to the issuing certificate next to it
    names = {n for n in os.listdir(cert_dir) if n.startswith("ca.")}
    assert {"ca.pem", "ca.key", "ca.issuing.pem"} <= names


@pytest.mark.skipif(shutil.which("openssl") is None, reason="needs the openssl command")
@pytest.mark.parametrize("san,cn", [("DNS:example.com", "example.com"), ("DNS:acct.table.core.windows.net", "x"), ("IP:10.0.0.5", "10.0.0.5")])
def test_a_certificate_signed_with_the_stolen_key_for_another_name_is_rejected(emu, cert_dir, san, cn):
    with tempfile.TemporaryDirectory() as d:
        def run(*args, **kw):
            subprocess.run(args, check=True, capture_output=True, **kw)

        run("openssl", "genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:P-256", "-out", f"{d}/k.pem")
        run("openssl", "req", "-new", "-key", f"{d}/k.pem", "-subj", f"/CN={cn}", "-out", f"{d}/r.csr")
        with open(f"{d}/ext", "w") as f:
            f.write(f"subjectAltName={san}\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n")
        run("openssl", "x509", "-req", "-in", f"{d}/r.csr", "-CA", f"{cert_dir}/ca.issuing.pem", "-CAkey", f"{cert_dir}/ca.key",
            "-CAcreateserial", "-CAserial", f"{d}/serial", "-days", "2", "-extfile", f"{d}/ext", "-out", f"{d}/leaf.pem")
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        srv = subprocess.Popen(
            ["openssl", "s_server", "-accept", str(port), "-cert", f"{d}/leaf.pem", "-cert_chain", f"{cert_dir}/ca.issuing.pem",
             "-key", f"{d}/k.pem", "-quiet", "-www"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            import time

            for _ in range(50):
                try:
                    socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
                    break
                except OSError:
                    time.sleep(0.1)
            host = san.split(":", 1)[1]
            with pytest.raises(ssl.SSLCertVerificationError) as e:
                handshake(port, host, emu.ca_file)
            assert "permitted subtree violation" in str(e.value)
        finally:
            srv.terminate()
            srv.wait(timeout=5)
