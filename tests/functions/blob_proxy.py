"""Diagnostic only: a tiny forwarding proxy in front of the emulator's blob endpoint that clamps `maxresults` to 5000.

Why: the Functions host lists containers with maxresults=10000 (blob trigger scan). Azure documents that a value above
5000 returns up to 5000 items; azemu answers 400 OutOfRangeQueryParameterValue and the host stops. This proxy shows
what else is behind that finding. Used by `run.py --clamp-maxresults`. Not part of any result in the table.
"""
import http.client, os, re, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate"}


def make(upstream_port):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def do(self):
            n = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(n) if n else None
            path = re.sub(r"maxresults=(\d+)", lambda m: "maxresults=%d" % min(int(m.group(1)), 5000), self.path)
            c = http.client.HTTPConnection("127.0.0.1", upstream_port, timeout=60)
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP}
            headers["Host"] = self.headers["Host"].rsplit(":", 1)[0] + f":{upstream_port}"
            c.request(self.command, path, body=body, headers=headers)
            r = c.getresponse()
            data = r.read()
            if os.environ.get("PROXY_DUMP"):
                with open(os.environ["PROXY_DUMP"], "a") as f:
                    f.write(f"{self.command} {path} -> {r.status}\n{data[:3000].decode(errors='replace')}\n\n")
            self.send_response(r.status)
            for k, v in r.getheaders():
                if k.lower() not in HOP and k.lower() != "content-length":
                    self.send_header(k, v)
            self.send_header("Content-Length", str(len(data)) if self.command != "HEAD" else r.getheader("content-length", "0"))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)
            c.close()

        do_GET = do_PUT = do_HEAD = do_DELETE = do_POST = do_OPTIONS = do

    return H


def start(listen_port, upstream_port):
    srv = ThreadingHTTPServer(("127.0.0.1", listen_port), make(upstream_port))
    srv.daemon_threads = True
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv
