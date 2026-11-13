#!/usr/bin/env python3
"""A tiny stand-in for the emulator, used to test replay.py.

    mockserver.py --perfect PATH            answer every request with the recorded response
    mockserver.py --mutate PATH             same, but change server-generated values on the
                                            way out and fail if the original comes back
    mockserver.py ... --wrong-rate 0.1      also answer that fraction with a wrong status

PATH is a recording file or a directory searched recursively.  Entries that replay.py
does not send (auth-dependent errors, sanitized copy sources with no candidate) are
left out of the mock too, unless --keep-unsent is given (then run replay.py --no-classify).  The mock does not
know which recording a client is replaying: after /_azemu/reset it keeps the set
of recordings whose requests so far (method + path + query) match what arrived,
and answers with the next recorded response of the first one left.

Endpoints besides the recorded ones:
    POST /_azemu/reset    forget the position and all mutations
    GET  /_azemu/health   200 {"status":"ok","services":[]}
    GET  /_mock/stats     counters as JSON
"""
from __future__ import annotations

import argparse
import html
import json
import os
import random
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Optional
from urllib.parse import parse_qsl, quote, unquote, urlsplit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import replay  # noqa: E402

#: response headers whose values are replaced in mutate mode
MUTATE_HEADERS = {
    "etag", "x-ms-popreceipt", "x-ms-copy-id", "x-ms-snapshot", "x-ms-version-id",
    "x-ms-continuation", "x-ms-continuation-nextpartitionkey", "x-ms-continuation-nextrowkey",
    "x-ms-continuation-nexttablename",
}
#: body leaf names (lower case) whose values are replaced in mutate mode
MUTATE_KEYS = {
    "messageid", "popreceipt", "etag", "odata.etag", "snapshot", "versionid", "nextmarker",
    "continuationtoken", "id", "kid", "nextlink",
}
_KV_VERSION_RE = re.compile(r"^[0-9a-f]{32}$")
SKIP_QUERY = {"api-version", "maxresults", "$top", "top"}


def rotate(s: str) -> str:
    """Change every letter and digit (bijectively, same length, still URL-safe)."""
    out = []
    for c in s:
        if "0" <= c <= "9":
            out.append(chr((ord(c) - 48 + 1) % 10 + 48))
        elif "a" <= c <= "z":
            out.append(chr((ord(c) - 97 + 1) % 26 + 97))
        elif "A" <= c <= "Z":
            out.append(chr((ord(c) - 65 + 1) % 26 + 65))
        else:
            out.append(c)
    return "".join(out)


def mutate_value(v: str) -> Optional[str]:
    """New value for a server-generated string, or None to leave it alone."""
    if v.startswith(("http://", "https://")):
        u = urlsplit(v)
        if u.query:
            parts = []
            changed = False
            for part in u.query.split("&"):
                k, eq, val = part.partition("=")
                if eq and unquote(k).lower() not in SKIP_QUERY and any(ch.isalnum() for ch in val):
                    parts.append(f"{k}={rotate(val)}")
                    changed = True
                else:
                    parts.append(part)
            if not changed:
                return None
            return v[: v.index("?") + 1] + "&".join(parts)
        head, _, last = v.rpartition("/")
        if _KV_VERSION_RE.match(last):
            return f"{head}/{rotate(last)}"
        return None
    if len(v) < replay.MIN_MAP_LEN or not any(c.isdigit() for c in v) or any(c.isspace() for c in v):
        return None
    m = re.fullmatch(r'((?:W/)?")(.*)(")', v)
    if m:
        return m.group(1) + rotate(m.group(2)) + m.group(3)
    return rotate(v)


_PLAN_SETTINGS = replay.Settings(
    gateway="http://127.0.0.1:0", gateway_host="127.0.0.1", gateway_port=0, force_label=None,
    auth_defaults={}, timeout=1.0, reset=False, dry_run=False)


class RecordedRun:
    def __init__(self, name: str, entries: list) -> None:
        self.name = name
        self.entries = entries


def key_of(prep: "replay.PreparedEntry") -> tuple[str, str]:
    return (prep.method, replay.sanitize_target(prep.target))


class MockState:
    def __init__(self, paths: list[str], mutate: bool = False, wrong_rate: float = 0.0, seed: int = 1,
                 drop_unsent: bool = True) -> None:
        self.mutate = mutate
        self.services: list[str] = []  # what /_azemu/health reports as mounted
        self.wrong_rate = wrong_rate
        self.rng = random.Random(seed)
        self.lock = threading.Lock()
        self.runs: list[RecordedRun] = []
        self.first_index: dict[tuple, list[RecordedRun]] = {}
        self.load_problems: list[str] = []
        for name, path in replay.find_recordings(paths):
            try:
                entries = []
                writes = replay.BlobWriteLog()
                for i, raw in enumerate(replay.load_recording(path)):
                    p = replay.prepare_entry(raw, i)
                    if isinstance(p, replay.Skipped):
                        continue
                    if drop_unsent:
                        # entries replay.py (classification on) never sends: do not expect them
                        _cs, unsent = replay.plan_request(p, writes, _PLAN_SETTINGS)
                        if unsent:
                            continue
                    writes.note(p)
                    entries.append(p)
            except Exception as exc:  # noqa: BLE001
                self.load_problems.append(f"{name}: {exc}")
                continue
            if entries:
                run = RecordedRun(name, entries)
                self.runs.append(run)
                self.first_index.setdefault(key_of(entries[0]), []).append(run)
        self.stats = {"requests": 0, "unmatched": 0, "unmutated_hits": 0, "wrong_injected": 0, "resets": 0,
                      "mutated_values": 0}
        self.reset()

    def reset(self) -> None:
        self.candidates: list[RecordedRun] = []
        self.step = 0
        self.orig_to_mut = replay.ValueMap()  # used to find originals in requests
        self.mut_to_orig = replay.ValueMap()  # used to undo mutations before matching
        self.mutated: dict[str, str] = {}
        self.stats["resets"] += 1

    # -- mutation ----------------------------------------------------------
    def _mut(self, v: str) -> str:
        if v in self.mutated:
            return self.mutated[v]
        new = mutate_value(v)
        if new is None or new == v:
            return v
        self.mutated[v] = new
        self.orig_to_mut.remember(v, new)
        self.mut_to_orig.remember(new, v)
        # also the bare value inside ETag quotes, and URL pieces
        m = re.fullmatch(r'(?:W/)?"(.*)"', v)
        if m:
            nm = re.fullmatch(r'(?:W/)?"(.*)"', new)
            if nm:
                self.orig_to_mut.remember(m.group(1), nm.group(1))
                self.mut_to_orig.remember(nm.group(1), m.group(1))
        if v.startswith(("http://", "https://")):
            for x, y in replay._url_pairs(v, new):
                self.orig_to_mut.remember(x, y)
                self.mut_to_orig.remember(y, x)
        if not replay._SIMPLE_TOKEN_RE.match(v):
            # a composite value: ids inside it may be reused on their own later
            for x, y in zip(replay._TOKEN_RE.findall(v), replay._TOKEN_RE.findall(new)):
                if x != y and replay._good_token(x):  # words such as "datetime" are not ids
                    self.orig_to_mut.remember(x, y)
                    self.mut_to_orig.remember(y, x)
        self.stats["mutated_values"] += 1
        return new

    def _mutate_json(self, obj, key=""):
        if isinstance(obj, dict):
            return {k: self._mutate_json(v, k.lower()) for k, v in obj.items()}
        if isinstance(obj, list):
            return [self._mutate_json(v, key) for v in obj]
        if isinstance(obj, str) and key in MUTATE_KEYS:
            return self._mut(obj)
        return obj

    def _mutate_body(self, prep, body: bytes) -> bytes:
        fmt = replay.detect_format(body, prep.rec_ct)
        if fmt == "json":
            try:
                return json.dumps(self._mutate_json(replay.parse_json(body)), ensure_ascii=False,
                                  separators=(",", ":")).encode("utf-8")
            except ValueError:
                return body
        if fmt == "xml":
            text = body.decode("utf-8")
            names = "|".join(["MessageId", "PopReceipt", "Etag", "ETag", "Snapshot", "VersionId", "NextMarker"])
            return re.sub(rf"(<(?:\w+:)?({names})>)([^<]*)(</(?:\w+:)?\2>)",
                          lambda m: f"{m.group(1)}{html.escape(self._mut(html.unescape(m.group(3))), quote=False)}{m.group(4)}",
                          text).encode("utf-8")
        return body

    def _fingerprint(self, headers: dict, body: bytes) -> tuple:
        lh = {k.lower(): v for k, v in headers.items()}
        un = self.mut_to_orig.apply if self.mutate else (lambda x: x)
        return (
            lh.get("x-ms-date"), lh.get("date"), lh.get("x-ms-client-request-id"),
            un(body.decode("utf-8", "replace")) if body else "",
        )

    @staticmethod
    def _recorded_fingerprint(prep) -> tuple:
        lh = {k.lower(): v for k, v in prep.req_headers}
        return (
            lh.get("x-ms-date"), lh.get("date"), lh.get("x-ms-client-request-id"),
            prep.req_body.decode("utf-8", "replace") if prep.req_body else "",
        )

    # -- request handling -----------------------------------------------------
    def handle(self, method: str, target: str, headers: dict, body: bytes):
        """-> (status, [(name, value)], body bytes)"""
        with self.lock:
            self.stats["requests"] += 1
            texts = [target, unquote(target)] + [v for k, v in headers.items()
                                                   if k.lower() not in ("authorization", "user-agent", "host")]
            if body and len(body) < 1_000_000:
                texts.append(body.decode("utf-8", "replace"))
            hit = None
            if self.mutate and self.orig_to_mut.pairs:
                for t in texts:
                    hit = self.orig_to_mut.search(t)
                    if hit:
                        break
            key = (method, self.mut_to_orig.apply(target) if self.mutate else target)
            step = self.step
            if step == 0:
                cands = list(self.first_index.get(key, []))
            else:
                cands = [c for c in self.candidates if len(c.entries) > step and key_of(c.entries[step]) == key]
            if len(cands) > 1:
                # several recordings share this prefix: prefer the one whose
                # timestamps / request id / body equal what arrived
                fp = self._fingerprint(headers, body)
                same = [c for c in cands if self._recorded_fingerprint(c.entries[step]) == fp]
                if same:
                    cands = same
            if not cands:
                self.stats["unmatched"] += 1
                msg = json.dumps({"mock": "no recorded entry matches", "method": method, "target": target,
                                  "step": step}).encode()
                return 500, [("Content-Type", "application/json")], msg
            self.candidates = cands
            self.step += 1
            prep = cands[0].entries[step]
            if hit is not None:
                self.stats["unmutated_hits"] += 1
                msg = json.dumps({"mock": "request carried an original (unmutated) recorded value",
                                  "value": hit, "target": target}).encode()
                return 500, [("Content-Type", "application/json")], msg
            status = prep.rec_status
            if self.wrong_rate and self.rng.random() < self.wrong_rate:
                self.stats["wrong_injected"] += 1
                status = 404 if status < 400 else 200
            out_headers = []
            resp_body = prep.rec_body
            if self.mutate:
                resp_body = self._mutate_body(prep, resp_body)
            for k, v in prep.rec_headers.items():
                lk = k.lower()
                if lk in ("transfer-encoding", "connection", "content-encoding", "keep-alive"):
                    continue
                if lk == "content-length":
                    continue
                if self.mutate and lk in MUTATE_HEADERS:
                    v = self._mut(v)
                out_headers.append((k, v))
            if prep.method == "HEAD":
                cl = prep.rec_headers.get("content-length")
                if cl is not None:
                    out_headers.append(("Content-Length", cl))
                resp_body = b""
            else:
                out_headers.append(("Content-Length", str(len(resp_body))))
            return status, out_headers, resp_body


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    wbufsize = 1 << 16  # one write per response (avoids Nagle / delayed-ACK stalls)
    state: MockState = None  # set by MockServer

    def log_message(self, *a, **k):  # quiet
        pass

    def _serve(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        path = self.path
        if path.split("?")[0] == "/_azemu/health" and self.command == "GET":
            data = json.dumps({"status": "ok", "services": list(self.state.services)}).encode()
            return self._send(200, [("Content-Type", "application/json")], data)
        if path.split("?")[0] == "/_azemu/reset" and self.command == "POST":
            with self.state.lock:
                self.state.reset()
            return self._send(200, [("Content-Type", "text/plain")], b"reset")
        if path.split("?")[0] == "/_mock/stats":
            with self.state.lock:
                data = json.dumps(self.state.stats).encode()
            return self._send(200, [("Content-Type", "application/json")], data)
        status, headers, resp = self.state.handle(self.command, path, dict(self.headers.items()), body)
        self._send(status, headers, resp)

    def _send(self, status, headers, body):
        self.send_response_only(status)
        have_cl = False
        for k, v in headers:
            if k.lower() == "content-length":
                have_cl = True
            self.send_header(k, v)
        if not have_cl and self.command != "HEAD" and status not in (204, 304):
            self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if self.command != "HEAD" and status not in (204, 304) and not (100 <= status < 200):
            self.wfile.write(body)
        self.wfile.flush()

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = do_PATCH = do_OPTIONS = _serve


class MockServer:
    """In-process mock; `with MockServer(...) as m: m.url`."""

    def __init__(self, paths, mutate=False, wrong_rate=0.0, seed=1, host="127.0.0.1", port=0, drop_unsent=True):
        if isinstance(paths, str):
            paths = [paths]
        self.state = MockState(paths, mutate=mutate, wrong_rate=wrong_rate, seed=seed, drop_unsent=drop_unsent)
        handler = type("H", (Handler,), {"state": self.state})
        ThreadingHTTPServer.daemon_threads = True
        ThreadingHTTPServer.request_queue_size = 64
        self.httpd = ThreadingHTTPServer((host, port), handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    @property
    def port(self) -> int:
        return self.httpd.server_address[1]

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def start(self) -> "MockServer":
        if not self.thread.is_alive() and not self.thread.ident:
            self.thread.start()
        return self

    def stop(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=5)

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--perfect", metavar="FILE_OR_DIR", action="append",
                   help="answer with the recorded response of the matching entry")
    g.add_argument("--mutate", metavar="FILE_OR_DIR", action="append",
                   help="like --perfect but change generated values and require them back")
    ap.add_argument("--wrong-rate", type=float, default=0.0, help="fraction of requests answered with a wrong status")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--keep-unsent", action="store_true",
                    help="also expect the entries that replay.py does not send (use with replay.py --no-classify)")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=0, help="0 = pick a free port")
    args = ap.parse_args(argv)
    paths = args.perfect or args.mutate
    srv = MockServer(paths, mutate=bool(args.mutate), wrong_rate=args.wrong_rate, seed=args.seed,
                     host=args.host, port=args.port, drop_unsent=not args.keep_unsent)
    print(f"loaded {len(srv.state.runs)} recordings ({len(srv.state.load_problems)} unreadable)", flush=True)
    print(f"listening on http://{args.host}:{srv.port}", flush=True)
    try:
        srv.httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
