"""Helpers shared by the Azure Files tests (not a test module). Works against the emulator and, with AZEMU_AZURE=1, against Azure.

Nothing here calls the management plane: the account name and key of Azure come from `AZ_STORAGE_ACCOUNT` and `AZ_STORAGE_KEY`.
"""

import base64
import datetime
import hashlib
import hmac
import os
import urllib.parse
import uuid

import requests

DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
VERSION = "2025-11-05"


def unique(prefix="t"):
    return f"{prefix}{uuid.uuid4().hex[:12]}"


def account_name(emu):
    return os.environ["AZ_STORAGE_ACCOUNT"] if emu.azure else DEV_ACCOUNT


def account_key(emu):
    return os.environ["AZ_STORAGE_KEY"] if emu.azure else DEV_KEY


def endpoint(emu):
    """The file endpoint without a trailing slash."""
    if emu.azure:
        return f"https://{account_name(emu)}.file.core.windows.net"
    return emu.url("file", DEV_ACCOUNT)


def conn_str(emu):
    scheme = "https" if emu.azure else "http"
    return (
        f"DefaultEndpointsProtocol={scheme};AccountName={account_name(emu)};AccountKey={account_key(emu)};"
        f"FileEndpoint={endpoint(emu)}"
    )


def delete_share(client):
    """Delete a share and what hangs on it, whatever state the test left it in."""
    try:
        client.delete_share(delete_snapshots=True)
    except Exception:  # the share may be gone already
        pass


def expect(exc_info, status, code):
    """Check an HttpResponseError for its status and Azure's error code."""
    e = exc_info.value
    assert e.status_code == status, f"status {e.status_code}, expected {status}: {e}"
    assert e.error_code == code, f"error code {e.error_code}, expected {code}: {e}"


class Raw:
    """Raw REST calls signed with the account key (Shared Key), for what an SDK hides."""

    def __init__(self, emu):
        self.emu = emu
        self.account = account_name(emu)
        self.key = base64.b64decode(account_key(emu))
        self.base = endpoint(emu)
        self.session = requests.Session()
        if not emu.azure and emu.ca_file:
            self.session.verify = emu.ca_file

    def _sign(self, method, path, query, headers):
        h = {k.lower(): v for k, v in headers.items()}
        length = h.get("content-length", "")
        if length == "0":
            length = ""
        std = [method, h.get("content-encoding", ""), h.get("content-language", ""), length, h.get("content-md5", ""),
               h.get("content-type", ""), "", h.get("if-modified-since", ""), h.get("if-match", ""), h.get("if-none-match", ""),
               h.get("if-unmodified-since", ""), h.get("range", "")]
        canon_headers = "".join(f"{k}:{v}\n" for k, v in sorted((k, v) for k, v in h.items() if k.startswith("x-ms-")))
        resource = f"/{self.account}{urllib.parse.quote(path, safe='/')}"
        values = {}
        for k, v in query:
            values.setdefault(k.lower(), []).append(v)
        for k in sorted(values):
            resource += f"\n{k}:{','.join(sorted(values[k]))}"
        to_sign = "\n".join(std) + "\n" + canon_headers + resource
        return base64.b64encode(hmac.new(self.key, to_sign.encode(), hashlib.sha256).digest()).decode()

    def call(self, method, path, query=(), headers=None, body=b"", version=VERSION, sign=True):
        query = list(query)
        headers = dict(headers or {})
        headers.setdefault("x-ms-version", version)
        headers["x-ms-date"] = datetime.datetime.now(datetime.timezone.utc).strftime("%a, %d %b %Y %H:%M:%S GMT")
        if isinstance(body, str):
            body = body.encode()
        if body or method in ("PUT", "POST"):
            headers["Content-Length"] = str(len(body))
        if sign:
            headers["Authorization"] = f"SharedKey {self.account}:{self._sign(method, path, query, headers)}"
        url = self.base + urllib.parse.quote(path, safe="/")
        if query:
            url += "?" + urllib.parse.urlencode(query)
        return self.session.request(method, url, headers=headers, data=body or None, timeout=60)


def as_utc(value):
    """A timestamp the SDK hands back (a datetime, or a string with seven fraction digits) as an aware UTC datetime."""
    import re

    if isinstance(value, datetime.datetime):
        return value if value.tzinfo else value.replace(tzinfo=datetime.timezone.utc)
    text = re.sub(r"(\.\d{6})\d+", r"\1", value.replace("Z", "+00:00"))
    return datetime.datetime.fromisoformat(text).astimezone(datetime.timezone.utc)


def text(response):
    """The body of a response as sent, with the byte order mark Azure puts in front of XML (`requests` drops it from `.text`)."""
    return response.content.decode("utf-8")


def eventually(call, exc_type, code, seconds=180, every=5):
    """Call until it stops failing with `code`: Azure answers ShareBeingDeleted for a while after a share is deleted (a restore, or a
    new share of the same name, has to wait)."""
    import time

    deadline = time.time() + seconds
    while True:
        try:
            return call()
        except exc_type as e:
            if getattr(e, "error_code", None) != code or time.time() > deadline:
                raise
            time.sleep(every)
