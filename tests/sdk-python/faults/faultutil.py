"""The admin API of the fault rules (`/_azemu/faults`) for the tests of this directory."""

import json
import time
import urllib.error
import urllib.request


class Faults:
    def __init__(self, emu):
        self.base = f"{emu.gateway}/_azemu/faults"

    def _call(self, method: str, url: str, body=None):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(url, method=method, data=data, headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                return r.status, json.loads(r.read() or b"{}")
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"{}")

    def add(self, **rule) -> dict:
        """Add one rule (`add(service="blob", fault={"kind": "throttle"}, times=2)`); returns it, with its id."""
        status, body = self._call("POST", self.base, rule)
        assert status == 201, (status, body)
        return body

    def rules(self) -> list:
        status, body = self._call("GET", self.base)
        assert status == 200, (status, body)
        return body["rules"]

    def fired(self, rule_id: int = 1) -> int:
        return next(r["fired"] for r in self.rules() if r["id"] == rule_id)

    def clear(self) -> None:
        self._call("DELETE", self.base)


class Stopwatch:
    """`with Stopwatch() as t: ...` then `t.seconds`."""

    def __enter__(self):
        self.start = time.monotonic()
        return self

    def __exit__(self, *exc):
        self.seconds = time.monotonic() - self.start
        return False
