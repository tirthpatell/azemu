"""Helpers for the Event Grid tests: admin API wrapper and a small local webhook server."""

import json
import os
import emubin
import pathlib
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import requests
from azure.core.credentials import AccessToken

ROOT = pathlib.Path(__file__).resolve().parents[3]


class FakeToken:
    """A token credential the SDK accepts (it never leaves the machine)."""

    def get_token(self, *scopes, **kwargs):
        return AccessToken("fake-token", 4102444800)


class AsyncFakeToken:
    async def get_token(self, *scopes, **kwargs):
        return AccessToken("fake-token", 4102444800)

    async def close(self):
        pass


class EmuProc:
    """A second emulator process with its own environment (autocreate off, a config file, ...)."""

    def __init__(self, env=None):
        binary = emubin.for_service("eventgrid") or str(ROOT / "target" / "debug" / "azemu-eventgrid")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self.dir = tempfile.mkdtemp(prefix="azemu-eg-")
        self.ca_file = os.path.join(self.dir, "ca.pem")
        e = dict(os.environ)
        e.update(env or {})
        self.proc = subprocess.Popen(
            [binary, "--port", str(self.port), "--cert-dir", self.dir, "--host", "127.0.0.1"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=e,
        )
        end = time.time() + 20
        while time.time() < end:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/_azemu/health", timeout=1):
                    return
            except Exception:
                time.sleep(0.05)
        self.close()
        raise RuntimeError("emulator did not start")

    @property
    def gateway(self):
        return f"http://127.0.0.1:{self.port}"

    def url(self, label, account, scheme="http"):
        return f"{scheme}://{account}.{label}.localhost:{self.port}"

    def reset(self):
        requests.post(f"{self.gateway}/_azemu/reset", timeout=10).raise_for_status()

    def close(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


class Admin:
    """The emulator's admin API (`/_azemu/eventgrid`). Emulator only."""

    def __init__(self, emu):
        self.base = f"{emu.gateway}/_azemu/eventgrid"

    def _params(self, namespace=None, domain=None):
        p = {}
        if namespace:
            p["namespace"] = namespace
        if domain:
            p["domain"] = domain
        return p

    def put_topic(self, name, body=None, namespace=None, domain=None, expect=(200, 201)):
        r = requests.put(f"{self.base}/topics/{name}", json=body or {}, params=self._params(namespace, domain), timeout=10)
        assert r.status_code in expect, r.text
        return r.json()

    def put_sub(self, topic, sub, body, namespace=None, domain=None, expect=(200, 201)):
        r = requests.put(
            f"{self.base}/topics/{topic}/subscriptions/{sub}", json=body, params=self._params(namespace, domain), timeout=10
        )
        assert r.status_code in expect, r.text
        return r.json()

    def get_sub(self, topic, sub, namespace=None, domain=None):
        r = requests.get(f"{self.base}/topics/{topic}/subscriptions/{sub}", params=self._params(namespace, domain), timeout=10)
        r.raise_for_status()
        return r.json()

    def inspect(self, topic, sub, what, namespace=None, domain=None):
        r = requests.get(
            f"{self.base}/topics/{topic}/subscriptions/{sub}/{what}", params=self._params(namespace, domain), timeout=10
        )
        r.raise_for_status()
        return r.json()["value"]

    def request(self, method, path, **kw):
        return requests.request(method, f"{self.base}{path}", timeout=10, **kw)


class Hook:
    """A webhook endpoint that records requests.

    `statuses`: answers for notifications in order (the last repeats); handshakes are answered
    properly unless `handshake=False` (then they get 200 with an empty body).
    """

    def __init__(self, statuses=None, handshake=True):
        self.requests = []
        self._statuses = list(statuses or [200])
        self._lock = threading.Lock()
        self.handshake = handshake
        hook = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def log_message(self, *a):
                pass

            def _record(self):
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n) if n else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = raw.decode("utf-8", "replace")
                rec = {
                    "method": self.command,
                    "path": self.path,
                    "headers": {k.lower(): v for k, v in self.headers.items()},
                    "body": body,
                    "time": time.time(),
                }
                with hook._lock:
                    hook.requests.append(rec)
                return rec

            def _reply(self, status, body=None, headers=None):
                data = json.dumps(body).encode() if body is not None else b""
                self.send_response(status)
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_OPTIONS(self):
                self._record()
                if hook.handshake:
                    self._reply(200, None, {"WebHook-Allowed-Origin": "eventgrid.azure.net", "WebHook-Allowed-Rate": "*"})
                else:
                    self._reply(200)

            def do_POST(self):
                rec = self._record()
                if rec["headers"].get("aeg-event-type") == "SubscriptionValidation":
                    if hook.handshake:
                        code = rec["body"][0]["data"]["validationCode"]
                        self._reply(200, {"validationResponse": code}, {"Content-Type": "application/json"})
                    else:
                        self._reply(200)
                    return
                with hook._lock:
                    n = hook._statuses.pop(0) if len(hook._statuses) > 1 else hook._statuses[0]
                self._reply(n)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}/hook"
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.02), daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    @property
    def handshakes(self):
        with self._lock:
            return [r for r in self.requests if r["method"] == "OPTIONS" or r["headers"].get("aeg-event-type") == "SubscriptionValidation"]

    @property
    def notifications(self):
        with self._lock:
            return [r for r in self.requests if r["method"] == "POST" and r["headers"].get("aeg-event-type") == "Notification"]

    def wait_for(self, n, timeout=10.0):
        """Wait until at least `n` notifications arrived; returns them (possibly fewer)."""
        end = time.time() + timeout
        while time.time() < end:
            got = self.notifications
            if len(got) >= n:
                return got
            time.sleep(0.02)
        return self.notifications
