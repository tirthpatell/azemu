"""Helpers for the Event Grid subscription tests: a local webhook and polling."""

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WAIT = 10.0


def wait_until(fn, timeout=WAIT, what="condition", interval=0.05):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    raise AssertionError(f"timed out after {timeout}s waiting for {what} (last value: {last!r})")


class _Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _reply(self, status=200, body=b"", headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_OPTIONS(self):
        owner = self.server.owner
        owner.handshakes.append(("OPTIONS", {k.lower(): v for k, v in self.headers.items()}))
        if owner.refuse_validation:
            return self._reply(500)
        self._reply(200, headers=[("WebHook-Allowed-Origin", "eventgrid.azure.net")])

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        headers = {k.lower(): v for k, v in self.headers.items()}
        owner = self.server.owner
        if headers.get("aeg-event-type") == "SubscriptionValidation":
            owner.handshakes.append(("POST", headers))
            if owner.refuse_validation:
                return self._reply(500)
            code = json.loads(raw)[0]["data"]["validationCode"]
            return self._reply(200, json.dumps({"validationResponse": code}).encode(), [("Content-Type", "application/json")])
        with owner.lock:
            owner.requests.append((headers, raw))
        self._reply(200)


class Webhook:
    """A local HTTP endpoint on a free port that answers the Event Grid validation handshake.
    `events` is every delivered event (the Event Grid schema arrives as an array of one, CloudEvents as the event)."""

    def __init__(self, refuse_validation=False):
        self.refuse_validation = refuse_validation
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self.server.owner = self
        self.requests = []
        self.handshakes = []
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/hook?secret=s3"

    def stop(self):
        self.server.shutdown()
        self.server.server_close()

    @property
    def events(self):
        out = []
        with self.lock:
            for _, raw in self.requests:
                body = json.loads(raw)
                out.extend(body if isinstance(body, list) else [body])
        return out

    def wait_events(self, n, timeout=WAIT):
        wait_until(lambda: len(self.events) >= n, timeout, f"{n} events at the webhook")
        return self.events
