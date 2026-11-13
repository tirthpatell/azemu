#!/usr/bin/env python3
"""Smoke test of one azemu binary: start it, check health, one Blob round trip, one Cosmos DB stored procedure, stop it.

    python3 tools/release/smoke_test.py path/to/azemu [--expect-version 0.1.0]

Standard library only, so it runs on every runner (Linux, macOS). What it proves, in this order:
  1. the binary starts and prints its `azemu ready` line;
  2. `azemu --version` and GET /_azemu/health agree on the version, and the services a release has are mounted;
  3. a Blob container and a blob are written and read back over http;
  4. a Cosmos DB stored procedure runs over https (which starts the script worker, the binary running itself again, so
     this is the check that a binary built for a new platform can start its own worker);
  5. SIGTERM ends the process with exit code 0 within a few seconds.
Exit code 0: all of it held; 1: something did not (the reason is printed, then the emulator's log); 2: usage.
It uses a free port, and a temporary certificate directory and HOME, so it touches nothing of the user."""
import argparse
import http.client
import json
import os
import re
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

REQUIRED_SERVICES = {"blob", "queue", "table", "keyvault", "cosmos", "servicebus", "eventhubs", "arm"}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class TlsToLoopback(http.client.HTTPSConnection):
    """https to 127.0.0.1 with the certificate checked for another name (the *.localhost name the request is for), so that no
    name has to resolve."""

    def __init__(self, port, context, name, timeout):
        super().__init__("127.0.0.1", port, context=context, timeout=timeout)
        self._name = name

    def connect(self):
        sock = socket.create_connection((self.host, self.port), self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self._name)


def request(port, method, path, host, headers=None, body=None, tls_ca=None):
    h = {"Host": f"{host}:{port}", "Authorization": "SharedKey smoke:c21va2U=", "x-ms-version": "2023-11-03",
         "x-ms-date": time.strftime("%a, %d %b %Y %H:%M:%S GMT", time.gmtime())}
    h.update(headers or {})
    data = body.encode() if isinstance(body, str) else body
    if tls_ca:
        ctx = ssl.create_default_context(cafile=tls_ca)
        conn = TlsToLoopback(port, ctx, host, 30)
    else:
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        conn.request(method, path, body=data, headers=h)
        r = conn.getresponse()
        return r.status, r.read().decode("utf-8", "replace")
    finally:
        conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--expect-version", help="the version `azemu --version` must print")
    ap.add_argument("--expect-commit", help="the commit (a prefix of at least 7 characters) that `azemu --version` must name; `any` only "
                    "demands that it is not `unknown`")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    if not os.access(binary, os.X_OK):
        print(f"not an executable file: {binary}", file=sys.stderr)
        return 2

    work = tempfile.mkdtemp(prefix="azemu-smoke-")
    steps = []
    proc = None
    log = []

    def ok(msg):
        steps.append(msg)
        print(f"ok   {msg}", flush=True)

    def fail(msg):
        print(f"FAIL {msg}", flush=True)
        if log:
            print("---- emulator log (last 40 lines) ----")
            print("\n".join(log[-40:]))
        return 1

    try:
        v = subprocess.run([binary, "--version"], capture_output=True, text=True, timeout=30)
        version_text = v.stdout.strip()
        if v.returncode != 0 or not version_text:
            return fail(f"`--version` exited {v.returncode}: {v.stdout!r} {v.stderr!r}")
        m = re.search(r"(\d+\.\d+\.\d+\S*)", version_text)
        if args.expect_version and (not m or m.group(1) != args.expect_version):
            return fail(f"`--version` printed {version_text!r}, expected version {args.expect_version}")
        if args.expect_commit:
            cm = re.search(r"commit (\w+)", version_text)
            got = cm.group(1) if cm else ""
            if args.expect_commit == "any":
                good = got not in ("", "unknown")
            else:
                good = got != "" and len(got) >= 7 and args.expect_commit.startswith(got)
            if not good:
                return fail(f"`--version` names commit {got or '(none)'!r}, expected {args.expect_commit}")
        ok(f"--version: {version_text}")

        port = free_port()
        certs = os.path.join(work, "certs")
        env = {k: v for k, v in os.environ.items() if not k.startswith("AZEMU_")}
        env["HOME"] = work
        proc = subprocess.Popen([binary, "--port", str(port), "--cert-dir", certs, "--log-level", "info"], env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        ready = []

        def pump():
            for line in proc.stdout:
                line = line.rstrip("\n")
                log.append(line)
                if line.startswith("azemu ready"):
                    ready.append(line)

        t = threading.Thread(target=pump, daemon=True)
        t.start()
        deadline = time.time() + 60
        while not ready and time.time() < deadline and proc.poll() is None:
            time.sleep(0.1)
        if not ready:
            return fail("the emulator did not print its `azemu ready` line within 60 s" + (f" (exited {proc.returncode})" if proc.poll() is not None else ""))
        ok("started: " + ready[0][:110])

        st, body = request(port, "GET", "/_azemu/health", "127.0.0.1")
        health = json.loads(body) if st == 200 else {}
        if st != 200 or health.get("status") != "ok":
            return fail(f"health answered {st}: {body[:200]}")
        missing = REQUIRED_SERVICES - set(health.get("services", []))
        if missing:
            return fail(f"services not mounted: {sorted(missing)}")
        if args.expect_version and health.get("version") != args.expect_version:
            return fail(f"health says version {health.get('version')}, expected {args.expect_version}")
        ok(f"health: version {health.get('version')}, commit {health.get('commit')}, {len(health['services'])} services")

        host = "smoke.blob.core.localhost"
        st, body = request(port, "PUT", "/demo?restype=container", host, {"Content-Length": "0"})
        if st != 201:
            return fail(f"create container answered {st}: {body[:200]}")
        st, body = request(port, "PUT", "/demo/hello.txt", host, {"x-ms-blob-type": "BlockBlob"}, "hello smoke")
        if st != 201:
            return fail(f"put blob answered {st}: {body[:200]}")
        st, body = request(port, "GET", "/demo/hello.txt", host)
        if st != 200 or body != "hello smoke":
            return fail(f"get blob answered {st}: {body[:200]!r}")
        ok("blob round trip: container, put, get")

        ca = os.path.join(certs, "ca.pem")
        chost = "smoke.documents.localhost"
        js = {"Content-Type": "application/json", "x-ms-version": "2018-12-31"}

        def cosmos(method, path, doc, extra=None):
            return request(port, method, path, chost, {**js, **(extra or {})}, json.dumps(doc), tls_ca=ca)

        st, body = cosmos("POST", "/dbs", {"id": "d"})
        if st != 201:
            return fail(f"create database answered {st}: {body[:200]}")
        st, body = cosmos("POST", "/dbs/d/colls", {"id": "c", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}})
        if st != 201:
            return fail(f"create container answered {st}: {body[:200]}")
        sproc = "function (name) { getContext().getResponse().setBody('hello ' + name); }"
        st, body = cosmos("POST", "/dbs/d/colls/c/sprocs", {"id": "s", "body": sproc})
        if st != 201:
            return fail(f"create stored procedure answered {st}: {body[:200]}")
        st, body = cosmos("POST", "/dbs/d/colls/c/sprocs/s", ["smoke"], {"x-ms-documentdb-partitionkey": '["p"]'})
        if st != 200 or json.loads(body) != "hello smoke":
            return fail(f"stored procedure answered {st}: {body[:300]!r}")
        ok("cosmos stored procedure ran (the script worker started)")

        proc.send_signal(signal.SIGTERM)
        try:
            code = proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            return fail("the emulator did not exit within 15 s of SIGTERM")
        t.join(timeout=5)
        if code != 0:
            return fail(f"the emulator exited with code {code} after SIGTERM")
        ok("SIGTERM: exit code 0")
        leftover = subprocess.run(["pgrep", "-f", f"--cert-dir {certs}"], capture_output=True, text=True) if shutil.which("pgrep") else None
        if leftover is not None and leftover.stdout.strip():
            return fail(f"a process of the emulator is still running: {leftover.stdout.strip()}")
        ok("no process left behind")
        print(f"smoke test passed ({len(steps)} steps)")
        return 0
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
