#!/usr/bin/env python3
"""Replay Azure SDK recordings against an azemu emulator and score fidelity.

Run `replay.py --help` for the command line and see README.md for what the
numbers mean.  Standard library only.

Layout of this file (top to bottom):
  1. constants you may want to tune (noise headers, placeholders, labels)
  2. recording parsing: body decoding, URI/host handling
  3. value mapping (server-generated values reused by later requests)
  4. comparison: error codes, body shape, headers
  5. operation patterns and grouping keys
  6. HTTP client, per-recording replay, aggregation, report, CLI
"""
from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import http.client
import json
import os
import re
import socket
import ssl
import sys
import time
import traceback
import xml.etree.ElementTree as ET
from collections import Counter, OrderedDict, defaultdict
from dataclasses import dataclass, field
from email.utils import format_datetime, parsedate_to_datetime
from typing import Any, Optional
from urllib.parse import parse_qsl, quote, unquote, urlsplit
from xml.sax.saxutils import escape as xml_escape

try:  # run as a script or imported from this directory
    import valuecheck
except ImportError:  # pragma: no cover - imported as part of a package
    from . import valuecheck

# One recording has a 5,000 digit integer as a body.
sys.set_int_max_str_digits(0)

# ---------------------------------------------------------------------------
# 1. Constants
# ---------------------------------------------------------------------------

#: Hosts are mapped to the emulator's service label by looking for one of these
#: DNS labels after the first label (the first label is the account).
LABEL_ALIASES = {
    "blob": "blob",
    "queue": "queue",
    "table": "table",  # also covers x.table.cosmos.azure.com
    "dfs": "dfs",
    "vault": "vault",
    "managedhsm": "vault",  # x.managedhsm.azure.net
    "azconfig": "azconfig",
    "azconfig-test": "azconfig",  # fake_app_config.azconfig-test.io in the recordings
    "documents": "documents",
    "servicebus": "servicebus",
    "eventgrid": "eventgrid",  # x.eastus-1.eventgrid.azure.net
}
#: The Resource Manager host of the `azure-mgmt-*` recordings. It has no account; the emulator's name for it is the host
#: `management.localhost`.
ARM_HOST = "management.azure.com"
ARM_LABEL = "management"
#: What the sanitizer wrote in place of `management.azure.com` in newer recordings (the path is still a Resource Manager path).
SANITIZED_ARM_HOST = "sanitized.azure.com"
STORAGE_LABELS = ("blob", "queue", "table", "dfs")

#: Default Authorization header per label.  `{account}` is filled in.
#: Key Vault: the recordings strip Authorization from the request that follows a
#: 401 challenge, so "none" would make every replayed request 401.  We send a
#: bearer token on everything except requests whose recorded answer is the 401
#: challenge itself (see `choose_auth`).
DEFAULT_AUTH = {
    "blob": "SharedKey {account}:replay",
    "queue": "SharedKey {account}:replay",
    "table": "SharedKey {account}:replay",
    "dfs": "SharedKey {account}:replay",
    "azconfig": "HMAC-SHA256 Credential=replay&SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature=replay",
    "vault": "Bearer replay",
    "eventgrid": "SharedAccessKey replay",
    "servicebus": "SharedAccessSignature sr=replay&sig=replay&se=9999999999&skn=replay",
    "management": "Bearer replay",
}

#: Request headers that are not forwarded (lower case).
DROP_REQUEST_HEADERS = {
    "host", "content-length", "connection", "accept-encoding", "transfer-encoding",
    "expect", "keep-alive", "proxy-connection", "upgrade",
}

#: Response headers ignored by the soft header check (lower case).  This is the
#: single place to edit.  `x-ms-request-id` is deliberately NOT here.
NOISE_HEADERS = {
    "server", "vary", "strict-transport-security", "x-content-type-options",
    "transfer-encoding", "connection", "keep-alive", "cache-control", "pragma",
    "expires", "x-powered-by", "x-xss-protection", "x-frame-options", "p3p",
    "x-cache", "x-msedge-ref", "x-ms-server-latency", "x-ms-correlation-request-id",
    "x-ms-routing-request-id", "x-ms-build-version", "x-ms-clitelem", "x-ms-srs",
    "x-ms-gateway-slice", "set-cookie", "report-to", "nel", "alt-svc", "age",
    "via", "x-azure-ref", "x-ms-apihub-cached-response",
}
NOISE_HEADER_PREFIXES = (
    "x-ms-keyvault-", "x-ms-ests-", "access-control-", "content-security-policy",
    "x-ms-ratelimit-",
)

#: Values that the sanitizer writes in place of real ones.  Never used as value
#: mapping keys, and a recorded body equal to one of them has no shape.
PLACEHOLDER_VALUES = {
    "sanitized", "redacted", "fake_token_value", "fakeresource", "fake_resource",
    "storagename", "vaultname", "fake_endpoint", "fakeendpoint", "fake_app_config",
    "faketableaccount", "00000000-0000-0000-0000-000000000000",
}
_PLACEHOLDER_RE = re.compile(
    r"^(?:0{8}-0{4}-0{4}-0{4}-0{12}|(.)\1+|sanitized.*|redacted.*|fake[_-]?.*)$", re.I
)

#: Response header names never used for value mapping (lower case).
MAP_BLOCK_HEADERS = {
    "content-length", "content-type", "content-md5", "content-encoding", "content-language",
    "content-disposition", "content-range", "cache-control", "server", "date", "last-modified",
    "expires", "set-cookie", "www-authenticate", "x-ms-version", "retry-after", "retry-after-ms",
    "x-ms-server-latency", "x-ms-request-id", "x-ms-client-request-id", "client-request-id",
    "x-ms-content-crc64", "x-ms-blob-content-md5", "x-ms-correlation-request-id",
    "x-ms-routing-request-id", "x-ms-creation-time", "x-ms-copy-completion-time",
    "x-ms-last-access-time", "x-ms-time-next-visible", "x-ms-access-tier-change-time",
    "memento-datetime", "access-control-allow-origin", "strict-transport-security",
    "x-ms-copy-progress", "link",
}
MAP_BLOCK_PREFIXES = ("x-ms-keyvault-", "x-ms-ests-")

#: Query keys that select which operation a request is (value is kept).
SELECTOR_QUERY_KEYS = {"comp", "restype", "resource", "action"}
#: Query keys left out of operation patterns (SAS parameters, versions, timeouts).
IGNORED_QUERY_KEYS = {
    "api-version", "timeout", "sv", "ss", "srt", "sp", "se", "st", "spr", "sig", "sr", "sdd",
    "si", "sip", "skoid", "sktid", "skt", "ske", "sks", "skv", "saoid", "suoid", "scid", "ses",
    "rscc", "rscd", "rsce", "rscl", "rsct", "tn", "spk", "srk", "epk", "erk", "client-request-id",
}

MIN_MAP_LEN = 6
#: Longest value mapped when it comes from a header or a URL.
MAX_MAP_LEN = 4096
#: Longest value mapped when it is a leaf of a JSON/XML body.  Key Vault backup blobs
#: are 5 KB to 45 KB of base64 and come back in the restore request.
MAX_BODY_MAP_LEN = 4 * 1024 * 1024
REQUEST_HISTORY_LIMIT = 1 << 20  # bytes of any one request body kept for the "client chose it" check

#: Request headers whose value is an absolute point in time that the recording made
#: relative to the clock of the day it was recorded (HTTP-date format only).  They are
#: moved by the difference between the emulator's clock and the recorded clock.
#: Matches If-Modified-Since / If-Unmodified-Since with any `x-ms-*-` prefix
#: (source, blob, access-tier, ...), plus the two other absolute-time headers found.
TIME_SHIFT_HEADER_RE = re.compile(
    r"^(?:x-ms-(?:[a-z]+-)*)?if-(?:un)?modified-since$"
    r"|^x-ms-immutability-policy-until-date$|^x-ms-expiry-time$"
)

#: Credential headers other than Authorization that the sanitizer also blanks.
CREDENTIAL_HEADERS = {"x-ms-copy-source-authorization"}
#: Labels whose real clients send `Authorization: Bearer <token>`.
BEARER_LABELS = {"vault", "azconfig", "eventgrid", "servicebus", "management"}
PLACEHOLDER_BEARER = "Bearer sanitized"

#: Error codes (lower case) that real Azure returned because of the credential the
#: request carried (missing, expired, wrong audience, wrong signature, no permission).
#: The recording no longer contains that credential, so the emulator cannot be asked
#: the same question.  Entries with one of these recorded codes are classified
#: `unreplayable:auth-dependent` and are not sent.
AUTH_ERROR_CODES = {
    "authenticationfailed", "invalidauthenticationinfo", "noauthenticationinformation",
    "invalidtoken", "invalidauthenticationtoken", "expiredauthenticationtoken",
    "authorizationfailure", "authorizationfailed", "authorizationpermissionmismatch",
    "authorizationservicemismatch", "authorizationresourcetypemismatch",
    "authorizationprotocolmismatch", "invalid_token",
}
#: A recorded error whose message names the token audience came from a wrong-audience
#: credential whatever its code is (`Unauthorized`, `InvalidAuthenticationInfo`, ...).
AUTH_AUDIENCE_RE = re.compile(rb"audience", re.I)
#: `CannotVerifyCopySource` is an auth failure only as a 401 (the copy source needed a
#: credential the recording lacks).  As 404 / 409 / 412 it means the source blob is missing,
#: public access is off, or a source condition failed: the emulator can answer those.
AUTH_COPY_SOURCE_CODE = "cannotverifycopysource"

_HTTP_DATE_RE = re.compile(r"^[A-Z][a-z]{2}, \d{1,2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$")
_ISO_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?)?(?:Z|[+-]\d{2}:?\d{2})?$")
_ID_NAME_RE = re.compile(r"snapshot|version|versionid", re.I)


class RecordingFormatError(Exception):
    """The recording file is malformed (counted as 'errored')."""


# ---------------------------------------------------------------------------
# 2. Recording parsing
# ---------------------------------------------------------------------------

def is_textual_content_type(ct: Optional[str]) -> bool:
    """Does the recorder store a string body of this content type as plain text?

    The recorder (azure-sdk-tools test-proxy) keeps text-ish bodies as text and
    base64-encodes everything else.  An empty/missing content type is binary.
    """
    if not ct:
        return False
    main = ct.split(";")[0].strip().lower()
    if main.startswith("text/"):
        return True
    sub = main.split("/", 1)[-1]
    return (
        "json" in sub or "xml" in sub or sub.endswith("+json") or sub.endswith("+xml")
        or main in ("application/x-www-form-urlencoded", "application/javascript", "application/x-ndjson")
    )


B64_PREFIX = "b64:"
_B64_RE = re.compile(r"^[A-Za-z0-9+/]*={0,2}$")


def try_base64(s: str) -> Optional[bytes]:
    if len(s) % 4 or not _B64_RE.match(s):
        return None
    try:
        return base64.b64decode(s, validate=True)
    except (binascii.Error, ValueError):
        return None


def decode_body(body: Any, content_type: Optional[str] = None) -> tuple[bytes, str]:
    """Turn a recorded RequestBody / ResponseBody into the bytes that went on the wire.

    Returns (bytes, how) where `how` is one of: none, text, list, json, base64,
    text-fallback.  Encodings found in the corpus:
      null                       no body
      list of strings            text split into pieces; concatenate.  A piece that starts
                                 with `b64:` is base64 of raw bytes (table $batch sub-requests)
      dict / list / int / float  JSON embedded directly (a JSON number is possible)
      string, textual type       plain text (json, xml, text/*, form-urlencoded)
      string, any other type     base64 of the raw bytes (octet-stream, png, avro,
                                 arrow, multipart/mixed, and an absent content type)
    """
    if body is None:
        return b"", "none"
    if isinstance(body, bool) or isinstance(body, (int, float)):
        return json.dumps(body).encode(), "json"
    if isinstance(body, dict) or (isinstance(body, list) and not all(isinstance(x, str) for x in body)):
        return json.dumps(body, ensure_ascii=False, separators=(",", ":")).encode("utf-8"), "json"
    if isinstance(body, list):
        # The recorder splits a text body into pieces and stores a piece that is not
        # valid UTF-8 text (an embedded binary part) as `b64:<base64>`.
        parts = []
        for piece in body:
            if piece.startswith(B64_PREFIX):
                raw = try_base64(piece[len(B64_PREFIX):])
                if raw is not None:
                    parts.append(raw)
                    continue
            parts.append(piece.encode("utf-8"))
        return b"".join(parts), "list"
    if isinstance(body, str):
        if body == "":
            return b"", "text"
        if is_textual_content_type(content_type):
            return body.encode("utf-8"), "text"
        raw = try_base64(body)
        if raw is not None:
            return raw, "base64"
        return body.encode("utf-8"), "text-fallback"
    raise RecordingFormatError(f"unsupported body type {type(body).__name__}")


def header_get(headers: dict, name: str) -> Optional[str]:
    name = name.lower()
    for k, v in headers.items():
        if k.lower() == name:
            return v if isinstance(v, str) else (", ".join(map(str, v)) if isinstance(v, list) else str(v))
    return None


def lower_headers(headers: Optional[dict]) -> dict[str, str]:
    out: dict[str, str] = {}
    for k, v in (headers or {}).items():
        if isinstance(v, list):
            v = ", ".join(str(x) for x in v)
        elif not isinstance(v, str):
            v = str(v)
        lk = str(k).lower()
        out[lk] = f"{out[lk]}, {v}" if lk in out else v
    return out


_URI_RE = re.compile(r"^(?P<scheme>[A-Za-z][A-Za-z0-9+.\-]*)://(?P<netloc>[^/?#]*)(?P<rest>[^#]*)")


def split_request_uri(uri: str) -> tuple[str, str, Optional[int], str]:
    """-> (scheme, host, port, target) with target = path + query exactly as recorded."""
    m = _URI_RE.match(uri or "")
    if not m:
        raise RecordingFormatError(f"cannot parse RequestUri {uri!r}")
    netloc = m.group("netloc").rsplit("@", 1)[-1]
    port: Optional[int] = None
    host = netloc
    if netloc.startswith("["):
        host = netloc.split("]")[0] + "]"
    elif ":" in netloc:
        host, _, p = netloc.rpartition(":")
        port = int(p) if p.isdigit() else None
    rest = m.group("rest")
    if not rest:
        rest = "/"
    elif rest[0] == "?":
        rest = "/" + rest
    return m.group("scheme").lower(), host, port, rest


def resolve_service(host: str, force_label: Optional[str] = None, arm: bool = True) -> Optional[tuple[str, str]]:
    """-> (account, label), or None when the host is not one of ours (login, ...). `arm` says whether Resource Manager's
    host counts: it does for the `azure-mgmt-*` recordings only. The recordings of the data-plane packages hold some
    requests to `management.azure.com` too (a test that makes its storage account); they stay skipped as before, so the
    score of those packages is what it was."""
    h = host.strip().lower().rstrip(".")
    if h == ARM_HOST:
        return ("", (force_label or ARM_LABEL)) if arm else None
    if not h or h.startswith("[") or re.fullmatch(r"[\d.]+", h):
        return None
    labels = h.split(".")
    if len(labels) < 2:
        return None
    found = None
    for lab in labels[1:]:
        if lab in LABEL_ALIASES:
            found = LABEL_ALIASES[lab]
            break
    if found is None:
        return None
    # keep the recorded case of the account (hosts are case-insensitive)
    account = host.strip().rstrip(".").split(".")[0]
    return account, (force_label or found)


def sanitize_target(target: str) -> str:
    """Percent-encode only what http.client refuses (space, control chars, non-ASCII)."""
    if re.search(r"[\x00-\x20\x7f-\U0010ffff]", target):
        return "".join(
            c if (0x20 < ord(c) < 0x7f) else "".join("%%%02X" % b for b in c.encode("utf-8"))
            for c in target
        )
    return target


@dataclass
class PreparedEntry:
    index: int
    method: str
    recorded_uri: str
    scheme: str
    host: str
    account: str
    label: str
    target: str  # path + query, byte for byte as recorded
    req_headers: list  # [(name, value)] without the dropped ones
    req_body_kind: str  # none | text | list | json | base64 | text-fallback
    req_body_raw: Any  # original RequestBody (kept for mapping JSON structurally)
    req_body: bytes
    rec_status: int
    rec_headers: dict  # lower-case -> str
    rec_body: bytes
    rec_body_kind: str
    rec_ct: str
    has_auth: bool  # the recording has a usable Authorization (not a placeholder)
    req_ct: str = ""
    notes: list = field(default_factory=list)
    auth_placeholder: bool = False  # recorded Authorization was only a sanitizer placeholder


@dataclass
class Skipped:
    index: int
    method: str
    uri: str
    reason: str


def prepare_entry(raw: Any, index: int, force_label: Optional[str] = None, arm: bool = True) -> "PreparedEntry | Skipped":
    if not isinstance(raw, dict):
        raise RecordingFormatError(f"entry {index} is not an object")
    try:
        uri = raw["RequestUri"]
        method = str(raw["RequestMethod"]).upper()
        status = int(raw["StatusCode"])
    except (KeyError, ValueError, TypeError) as exc:
        raise RecordingFormatError(f"entry {index}: missing/invalid field {exc!r}") from exc
    if not isinstance(uri, str):
        raise RecordingFormatError(f"entry {index}: RequestUri is not a string")
    scheme, host, _port, target = split_request_uri(uri)
    svc = resolve_service(host, force_label, arm)
    if svc is None and arm and host.lower() == SANITIZED_ARM_HOST and re.match(r"^/(?:subscriptions|providers|tenants)(?:/|\?|$)", target, re.I):
        # newer azure-mgmt recordings write the sanitizer's word in place of management.azure.com
        svc = ("", force_label or ARM_LABEL)
    if svc is None:
        return Skipped(index, method, uri, f"unknown host {host}")
    account, label = svc
    rh = raw.get("RequestHeaders") or {}
    if not isinstance(rh, dict):
        raise RecordingFormatError(f"entry {index}: RequestHeaders is not an object")
    headers = []
    has_auth = False
    auth_placeholder = False
    for k, v in rh.items():
        lk = str(k).lower()
        if isinstance(v, list):
            v = ", ".join(map(str, v))
        if lk == "authorization":
            if is_placeholder_credential(str(v)):
                auth_placeholder = True
                continue  # replaced by `choose_auth`
            has_auth = True
        if lk in DROP_REQUEST_HEADERS:
            continue
        headers.append((str(k), str(v)))
    req_ct = header_get(rh, "content-type")
    body, kind = decode_body(raw.get("RequestBody"), req_ct)
    resp_headers = raw.get("ResponseHeaders") or {}
    if not isinstance(resp_headers, dict):
        raise RecordingFormatError(f"entry {index}: ResponseHeaders is not an object")
    rec_ct = header_get(resp_headers, "content-type") or ""
    rbody, rkind = decode_body(raw.get("ResponseBody"), rec_ct)
    notes = [f"req-body:{kind}"] if kind == "text-fallback" else []
    if rkind == "text-fallback":
        notes.append("resp-body:text-fallback")
    return PreparedEntry(
        index=index, method=method, recorded_uri=uri, scheme=scheme, host=host, account=account,
        label=label, target=target, req_headers=headers, req_body_kind=kind,
        req_body_raw=raw.get("RequestBody"), req_body=body, rec_status=status,
        rec_headers=lower_headers(resp_headers), rec_body=rbody, rec_body_kind=rkind,
        rec_ct=rec_ct, has_auth=has_auth, req_ct=(req_ct or "").lower(), notes=notes,
        auth_placeholder=auth_placeholder,
    )


def load_recording(path: str) -> list:
    with open(path, "r", encoding="utf-8") as fh:
        data = json.load(fh)
    if not isinstance(data, dict) or not isinstance(data.get("Entries"), list):
        raise RecordingFormatError("no 'Entries' list")
    return data["Entries"]


def find_recordings(paths: list[str]) -> list[tuple[str, str]]:
    """-> sorted [(relative_name, absolute_path)] for every .json file below `paths`."""
    out = []
    for p in paths:
        if os.path.isfile(p):
            out.append((os.path.basename(p), p))
            continue
        for root, _dirs, files in os.walk(p):
            for fn in files:
                if fn.endswith(".json"):
                    full = os.path.join(root, fn)
                    out.append((os.path.relpath(full, p), full))
    out.sort(key=lambda t: t[0])
    return out


def package_of(path: str) -> str:
    parts = os.path.abspath(path).split(os.sep)
    for part in reversed(parts):
        if part.startswith("azure-"):
            return part
    return parts[-3] if len(parts) >= 3 else "?"


def is_placeholder_credential(v: str) -> bool:
    """Is this Authorization-style value only a sanitizer placeholder?  `Sanitized`,
    `Bearer Sanitized`, `SharedKey account:Sanitized` ... (the whole value, or the
    scheme followed by nothing but placeholder text)."""
    v = v.strip()
    if not v:
        return True
    if is_placeholder(v):
        return True
    scheme, _, rest = v.partition(" ")
    if rest and re.fullmatch(r"[A-Za-z][A-Za-z0-9\-]*", scheme):
        rest = rest.strip()
        tail = rest.rsplit(":", 1)[-1]
        return is_placeholder(rest) or is_placeholder(tail)
    return False


def placeholder_auth(label: str, account: str, auth_defaults: dict[str, str]) -> Optional[str]:
    """Credential to send for a request whose recorded Authorization was a placeholder:
    the real request had a valid credential of unknown kind.  Bearer-style APIs get a
    bearer token, storage gets the default storage authorization."""
    if not auth_defaults:
        return None  # `--default-auth none`
    if label in BEARER_LABELS:
        return PLACEHOLDER_BEARER
    tmpl = auth_defaults.get(label)
    return tmpl.replace("{account}", account) if tmpl else PLACEHOLDER_BEARER


def choose_auth(prep: PreparedEntry, auth_defaults: dict[str, str]) -> Optional[str]:
    """Authorization value to add, or None.  Never added when the recording has a real
    one or when the query carries a SAS signature.  A recorded placeholder is replaced
    by `placeholder_auth`.  Nothing is added when the recorded answer is a 401 (the
    request that triggers a challenge carries no credential)."""
    if prep.has_auth:
        return None
    if re.search(r"(?:^|[?&])sig=", prep.target):
        return None
    if prep.rec_status == 401 and not prep.auth_placeholder:
        return None
    if prep.auth_placeholder:
        return placeholder_auth(prep.label, prep.account, auth_defaults)
    tmpl = auth_defaults.get(prep.label)
    if not tmpl:
        return None
    return tmpl.replace("{account}", prep.account)


# ---------------------------------------------------------------------------
# 3. Value mapping
# ---------------------------------------------------------------------------

def is_placeholder(v: str) -> bool:
    return v.lower() in PLACEHOLDER_VALUES or bool(_PLACEHOLDER_RE.match(v))


def is_date_like(v: str) -> bool:
    return bool(_HTTP_DATE_RE.match(v) or _ISO_DATE_RE.match(v))


def looks_generated(v: Any, name: str = "", max_len: int = MAX_MAP_LEN) -> bool:
    """Does this recorded value look like something the server made up (and so
    something the emulator will answer with a different value)?  `max_len` is
    `MAX_MAP_LEN` for headers and URLs and `MAX_BODY_MAP_LEN` for body leaves."""
    if not isinstance(v, str) or not (MIN_MAP_LEN <= len(v) <= max_len):
        return False
    lname = name.lower()
    if lname in MAP_BLOCK_HEADERS or lname.startswith(MAP_BLOCK_PREFIXES):
        return False
    if is_placeholder(v):
        return False
    if v.isdigit() or any(c.isspace() for c in v):
        return False
    if is_date_like(v):
        # a date is an identifier only for snapshots / version ids
        return bool(_ID_NAME_RE.search(lname)) and lname not in ("x-ms-version", "api-version")
    if v.startswith(("http://", "https://")):
        return True
    return any(c.isdigit() for c in v)


def _variants(rec: str, act: str):
    yield rec, act
    if len(rec) > MAX_MAP_LEN or len(act) > MAX_MAP_LEN:
        return  # a long body value is never part of a URL; skip the encoded forms
    for safe in ("", "/", ":/@"):
        er, ea = quote(rec, safe=safe), quote(act, safe=safe)
        if er != rec:
            yield er, ea
    if any(c in rec for c in "&<>\"'"):
        ent = {'"': "&quot;", "'": "&apos;"}
        yield xml_escape(rec, ent), xml_escape(act, ent)


class ValueMap:
    """recorded -> actual replacements, applied in a single pass, longest key first,
    so a short value can never corrupt a longer one and a substituted value is never
    substituted again."""

    def __init__(self) -> None:
        self.pairs: dict[str, str] = {}
        self._keys: Optional[list[str]] = None

    def __len__(self) -> int:
        return len(self.pairs)

    def remember(self, rec: str, act: str) -> None:
        if not rec or rec == act or is_placeholder(rec):
            return
        for k, v in _variants(rec, act):
            if k and not is_placeholder(k):
                self.pairs[k] = v
        self._keys = None

    def _sorted_keys(self) -> list[str]:
        if self._keys is None:
            self._keys = sorted(self.pairs, key=len, reverse=True)
        return self._keys

    def _spans(self, s: str) -> list[tuple[int, int, str]]:
        spans = []
        for k in self._sorted_keys():
            i = s.find(k)
            while i >= 0:
                spans.append((i, i + len(k), k))
                i = s.find(k, i + len(k))
        # leftmost first, and at the same start the longest: a single left-to-right pass
        spans.sort(key=lambda t: (t[0], -(t[1] - t[0])))
        return spans

    def search(self, s: str) -> Optional[str]:
        """First recorded key found in s (or None)."""
        spans = self._spans(s) if (self.pairs and s) else []
        return spans[0][2] if spans else None

    def apply(self, s: str) -> str:
        if not self.pairs or not s:
            return s
        spans = self._spans(s)
        if not spans:
            return s
        out, pos = [], 0
        for a, b, k in spans:
            if a < pos:  # overlaps something already replaced
                continue
            out.append(s[pos:a])
            out.append(self.pairs[k])
            pos = b
        out.append(s[pos:])
        return "".join(out)

    def apply_json(self, obj: Any) -> Any:
        if isinstance(obj, str):
            return self.apply(obj)
        if isinstance(obj, list):
            return [self.apply_json(x) for x in obj]
        if isinstance(obj, dict):
            return {k: self.apply_json(v) for k, v in obj.items()}
        return obj


class AccountSettings:
    """What this recording itself did to the account's configuration, so that `valuecheck` can tell a value the recording
    produced from one that is the recorded account's own (rules `settings the recording never set` and `delete type of an
    account the recording did not set up`)."""

    #: the parts of the service properties document (Set Service Properties changes the parts it names and leaves the others)
    SECTIONS = ("Logging", "HourMetrics", "MinuteMetrics", "Cors", "StaticWebsite", "DeleteRetentionPolicy", "DefaultServiceVersion")

    def __init__(self, prepared: list) -> None:
        entries = [p for p in prepared if isinstance(p, PreparedEntry)]
        #: some answer of the recording carries a version id: the recorded account had blob versioning on
        self.versioned = any("x-ms-version-id" in p.rec_headers for p in entries)
        self.sections_set: set[str] = set()
        self.soft_delete_set = False
        #: some request of the recording names an access tier; otherwise the tiers in the answers are the account's default
        self.tier_set = any(k.lower() == "x-ms-access-tier" for p in entries for k, _v in p.req_headers)

    @staticmethod
    def _service_call(p: "PreparedEntry", comp: str) -> bool:
        parts = urlsplit(p.target)
        q = parts.query.lower()
        return parts.path in ("", "/") and f"comp={comp}" in q and "restype=service" in q

    def note(self, p: "PreparedEntry") -> None:
        """Call after the entry was sent."""
        if p.method == "PUT" and self._service_call(p, "properties") and 200 <= p.rec_status < 300:
            body = (p.req_body or b"").decode("utf-8", "replace")
            for sec in self.SECTIONS:
                if re.search(rf"<{sec}[\s/>]", body):
                    self.sections_set.add(sec)
            if re.search(r"<DeleteRetentionPolicy>\s*<Enabled>\s*true", body, re.I):
                self.soft_delete_set = True

    def skip_paths(self, p: "PreparedEntry") -> tuple:
        """Rule `settings the recording never set`: path prefixes of an answer that only reports the account's configuration."""
        if p.method != "GET":
            return ()
        if self._service_call(p, "stats"):
            return ("/StorageServiceStats",)
        if self._service_call(p, "properties"):
            return tuple(f"/StorageServiceProperties/{sec}" for sec in self.SECTIONS if sec not in self.sections_set)
        return ()

    def skip_endings(self) -> tuple:
        """Rule `tier the recording never set`: paths (without the position) of the access tier in a body."""
        return () if self.tier_set else ("/AccessTier",)

    def skip_headers(self, p: "PreparedEntry") -> frozenset:
        """Rules `tier the recording never set` and `delete type of an account the recording did not set up`."""
        out = set()
        if not self.tier_set:
            out.add("x-ms-access-tier")
        if p.method == "DELETE" and not self.soft_delete_set:
            header_names = {k.lower() for k, _v in p.req_headers}
            q = urlsplit(p.target).query.lower()
            plain = "versionid=" not in q and "snapshot=" not in q and "x-ms-delete-snapshots" not in header_names
            # versioning alone decides a plain delete of the current blob; any other delete also depends on the account's soft delete
            if not (self.versioned and plain):
                out.add("x-ms-delete-type-permanent")
        return frozenset(out)


class RequestHistory:
    """Text of the requests already sent in this recording.  A recorded value that
    already appeared in a request was chosen by the client (a name, a lease id it
    proposed), not generated by the server, so it is never mapped."""

    def __init__(self) -> None:
        self.parts: list[str] = []

    def add(self, prep: PreparedEntry) -> None:
        self.parts.append(prep.target)
        self.parts.append(unquote(prep.target))
        self.parts.extend(v for k, v in prep.req_headers if k.lower() not in ("authorization", "user-agent"))
        if prep.req_body_kind in ("text", "list", "json", "text-fallback"):
            self.parts.append(prep.req_body[:REQUEST_HISTORY_LIMIT].decode("utf-8", "replace"))

    def contains(self, v: str) -> bool:
        enc = quote(v, safe="")
        return any(v in p or enc in p for p in self.parts)


def _url_pairs(rec: str, act: str):
    """For two URLs, yield differing (recorded, actual) path segments and query values."""
    try:
        r, a = urlsplit(rec), urlsplit(act)
    except ValueError:
        return
    rs, as_ = r.path.split("/"), a.path.split("/")
    if len(rs) == len(as_):
        for x, y in zip(rs, as_):
            if x != y:
                yield x, y
    rq = dict(parse_qsl(r.query, keep_blank_values=True))
    aq = dict(parse_qsl(a.query, keep_blank_values=True))
    for k, x in rq.items():
        y = aq.get(k)
        if y is not None and x != y:
            yield x, y


_SIMPLE_TOKEN_RE = re.compile(r"^[A-Za-z0-9_\-.~=+/%]+$")
_TOKEN_RE = re.compile(r"[A-Za-z0-9_\-.~]{8,}")


def _good_token(t: str) -> bool:
    return (any(c.isdigit() for c in t) and any(c.isalpha() for c in t)
            and not re.match(r"^\d{4}-\d\d", t) and not is_placeholder(t))


def consider_pair(vmap: ValueMap, history: RequestHistory, rec: str, act: str, name: str, depth: int = 0,
                  max_len: int = MAX_MAP_LEN) -> None:
    if not isinstance(rec, str) or not isinstance(act, str) or rec == act or not act:
        return
    if looks_generated(rec, name, max_len):
        if rec in vmap.pairs or not history.contains(rec):
            vmap.remember(rec, act)
    # ETag style: also the bare value without W/ and quotes
    m = re.fullmatch(r'(?:W/)?"(.*)"', rec)
    ma = re.fullmatch(r'(?:W/)?"(.*)"', act)
    if m and ma and depth == 0:
        consider_pair(vmap, history, m.group(1), ma.group(1), name, depth + 1, max_len)
    if depth == 0 and rec.startswith(("http://", "https://")) and act.startswith(("http://", "https://")):
        for x, y in _url_pairs(rec, act):
            consider_pair(vmap, history, x, y, "url", depth + 1)
    elif (depth == 0 and len(rec) <= MAX_MAP_LEN and not _SIMPLE_TOKEN_RE.match(rec)
          and not name.lower() in MAP_BLOCK_HEADERS):
        # a composite value (JSON-ish continuation token, "key=value;sn=12"): the SDK may
        # reuse only a piece of it later, so pair the id-like tokens inside by position
        rt, at = _TOKEN_RE.findall(rec), _TOKEN_RE.findall(act)
        if len(rt) == len(at) and rt:
            for x, y in zip(rt, at):
                if x != y and _good_token(x):
                    consider_pair(vmap, history, x, y, "token", depth + 1)


# --- leaves of XML / JSON bodies (for pairing and shapes) --------------------

def _local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def parse_xml(raw: bytes) -> ET.Element:
    if raw.startswith(b"\xef\xbb\xbf"):
        raw = raw[3:]
    return ET.fromstring(raw.lstrip())


def parse_json(raw: bytes) -> Any:
    return json.loads(raw.decode("utf-8-sig"))


def detect_format(raw: bytes, ct: str) -> Optional[str]:
    if not raw:
        return None
    head = raw[:8].lstrip(b"\xef\xbb\xbf \t\r\n")[:1]
    lct = (ct or "").lower()
    if "xml" in lct or head == b"<":
        return "xml" if head == b"<" else None
    if "json" in lct or head in (b"{", b"["):
        return "json"
    return None


def xml_leaves(root: ET.Element) -> dict[str, str]:
    out: dict[str, str] = {}

    def walk(el: ET.Element, path: str) -> None:
        kids = list(el)
        if not kids:
            text = (el.text or "").strip()
            if text:
                out[path] = text
            return
        seen: Counter = Counter()
        for k in kids:
            n = _local(k.tag)
            walk(k, f"{path}/{n}[{seen[n]}]")
            seen[n] += 1

    walk(root, "/" + _local(root.tag))
    return out


def json_leaves(obj: Any) -> dict[str, str]:
    out: dict[str, str] = {}

    def walk(o: Any, path: str) -> None:
        if isinstance(o, dict):
            for k, v in o.items():
                walk(v, f"{path}/{k}")
        elif isinstance(o, list):
            for i, v in enumerate(o):
                walk(v, f"{path}[{i}]")
        elif isinstance(o, str):
            out[path] = o

    walk(obj, "")
    return out


def body_leaves(raw: bytes, ct: str) -> dict[str, str]:
    fmt = detect_format(raw, ct)
    try:
        if fmt == "xml":
            return xml_leaves(parse_xml(raw))
        if fmt == "json":
            return json_leaves(parse_json(raw))
    except (ET.ParseError, ValueError, RecursionError):
        pass
    return {}


def _scalar_leaves(obj: Any) -> dict[str, Any]:
    """Every string, number and boolean of a JSON document by path (null is left out: an absent member and a null mean the
    same to nearly every client)."""
    out: dict[str, Any] = {}

    def walk(o: Any, path: str) -> None:
        if isinstance(o, dict):
            for k, v in o.items():
                walk(v, f"{path}/{k}")
        elif isinstance(o, list):
            for i, v in enumerate(o):
                walk(v, f"{path}[{i}]")
        elif isinstance(o, (str, bool, int, float)):
            out[path] = o

    walk(obj, "")
    return out


def value_leaves(raw: bytes, ct: str) -> dict[str, Any]:
    """The values `valuecheck.compare_values` looks at: `body_leaves` for XML; for JSON the strings, numbers and booleans."""
    fmt = detect_format(raw, ct)
    try:
        if fmt == "xml":
            return xml_leaves(parse_xml(raw))
        if fmt == "json":
            return _scalar_leaves(parse_json(raw))
    except (ET.ParseError, ValueError, RecursionError):
        pass
    return {}


def update_value_map(vmap: ValueMap, history: RequestHistory, prep: PreparedEntry,
                     act_headers: dict[str, str], act_body: bytes, rec_leaves: dict[str, str]) -> None:
    for name, rv in prep.rec_headers.items():
        av = act_headers.get(name)
        if av is not None and av != rv:
            consider_pair(vmap, history, rv, av, name)
    if rec_leaves:
        act_leaves = body_leaves(act_body, act_headers.get("content-type", ""))
        for path, rv in rec_leaves.items():
            av = act_leaves.get(path)
            if av is not None and av != rv:
                consider_pair(vmap, history, rv, av, path.rsplit("/", 1)[-1], max_len=MAX_BODY_MAP_LEN)


# ---------------------------------------------------------------------------
# 4. Comparison
# ---------------------------------------------------------------------------

def error_codes(headers: dict[str, str], body: bytes, ct: str, status: int) -> tuple[Optional[str], Optional[str]]:
    """-> (x-ms-error-code header, code found in the body).  Body codes are looked for
    only on status >= 400: JSON {"error":{"code"}}, {"odata.error":{"code"}},
    problem+json (last segment of "type"), XML <Error><Code> / <m:error><m:code>."""
    hdr = headers.get("x-ms-error-code")
    code = None
    if status >= 400 and body:
        fmt = detect_format(body, ct)
        try:
            if fmt == "json":
                obj = parse_json(body)
                if isinstance(obj, dict):
                    for key in ("error", "odata.error"):
                        err = obj.get(key)
                        if isinstance(err, dict) and isinstance(err.get("code"), (str, int)):
                            code = str(err["code"])
                            break
                    else:
                        t = obj.get("type")
                        if isinstance(t, str) and "problem" in (ct or "").lower():
                            code = t.rstrip("/").rsplit("/", 1)[-1]
            elif fmt == "xml":
                root = parse_xml(body)
                if _local(root.tag).lower() == "error":
                    for ch in root:
                        if _local(ch.tag).lower() == "code" and (ch.text or "").strip():
                            code = ch.text.strip()
                            break
        except (ET.ParseError, ValueError, RecursionError):
            pass
    return hdr, code


def shape_paths(raw: bytes, fmt: str) -> set[str]:
    """XML: element paths (namespaces dropped).  JSON: key paths, arrays as []."""
    paths: set[str] = set()
    if fmt == "xml":
        def walk(el: ET.Element, p: str) -> None:
            p = f"{p}/{_local(el.tag)}"
            paths.add(p)
            for k in el:
                walk(k, p)
        walk(parse_xml(raw), "")
    else:
        def walkj(o: Any, p: str) -> None:
            if isinstance(o, dict):
                for k, v in o.items():
                    q = f"{p}/{k}"
                    paths.add(q)
                    walkj(v, q)
            elif isinstance(o, list):
                q = p + "[]"
                if p:
                    paths.add(q)
                for v in o:
                    walkj(v, q)
        walkj(parse_json(raw), "")
    return paths


def avro_messages(raw: bytes):
    """Decode an Avro object container file of the quick query answer (Query Blob Contents) into
    ``(data, errors, last_progress, end)``: the result data joined, the error records as tuples, the last
    progress record and the end record. The random sync marker and the way the data is cut into records do not
    matter for the comparison. ``None`` when ``raw`` is not such a file."""
    if raw[:4] != b"Obj\x01":
        return None
    try:
        pos = 4

        def long() -> int:
            nonlocal pos
            n = shift = 0
            while True:
                c = raw[pos]
                pos += 1
                n |= (c & 0x7F) << shift
                shift += 7
                if not c & 0x80:
                    return (n >> 1) ^ -(n & 1)

        def blob() -> bytes:
            nonlocal pos
            n = long()
            pos += n
            return raw[pos - n:pos]

        while True:  # the metadata map
            n = long()
            if n == 0:
                break
            if n < 0:
                long()
                n = -n
            for _ in range(n):
                blob()
                blob()
        sync = raw[pos:pos + 16]
        pos += 16
        data, errors, progress, end = [], [], None, None
        while pos < len(raw):
            count = long()
            size = long()
            block_end = pos + size
            for _ in range(count):
                idx = long()
                if idx == 0:
                    data.append(blob())
                elif idx == 1:
                    fatal = raw[pos] != 0
                    pos += 1
                    name = blob().decode("utf-8", "replace")
                    blob()
                    errors.append((fatal, name, long()))
                elif idx == 2:
                    progress = (long(), long())
                elif idx == 3:
                    end = long()
                else:
                    return None
            if pos != block_end or raw[pos:pos + 16] != sync:
                return None
            pos += 16
        return b"".join(data), errors, progress, end
    except IndexError:
        return None


def compare_avro(rec: bytes, act: bytes) -> list:
    """Soft differences between two quick query answers (see ``avro_messages``)."""
    r, a = avro_messages(rec), avro_messages(act)
    if r is None:
        return []
    if a is None:
        return [("avro-format", "actual body is not an Avro object container file")]
    out = []
    if r[0] != a[0]:
        out.append(("avro-data", f"result data differs ({len(r[0])} bytes recorded, {len(a[0])} actual)"))
    if r[1] != a[1]:
        out.append(("avro-errors", f"error records differ: recorded {r[1]}, actual {a[1]}"))
    if r[2] != a[2] or r[3] != a[3]:
        out.append(("avro-progress", f"closing progress/end differ: recorded {r[2]}/{r[3]}, actual {a[2]}/{a[3]}"))
    return out


def is_placeholder_body(raw: bytes) -> bool:
    t = raw.decode("utf-8-sig", "replace").strip().strip('"')
    return bool(t) and is_placeholder(t) and len(t) < 64 and not t.startswith(("<", "{", "["))


def is_noise_header(name: str, method: str) -> bool:
    if name == "content-length":
        return method != "HEAD"
    return name in NOISE_HEADERS or name.startswith(NOISE_HEADER_PREFIXES)


@dataclass
class EntryResult:
    index: int
    method: str
    url: str  # path+query actually sent
    host: str  # Host header sent
    recorded_url: str
    expected_status: int
    actual_status: Optional[int]
    expected_code: Optional[str]
    actual_code: Optional[str]
    hard: list  # hard failure reasons
    soft: list  # [(kind, detail)]
    pattern: str
    actual_body: Optional[str] = None
    error: Optional[str] = None  # connection/timeouts
    skipped: Optional[str] = None
    status_match: bool = False
    #: pass | fail | ahead-async | unreplayable:<reason>  (see "5b. Classification")
    klass: str = "pass"
    class_detail: str = ""
    #: how many values were compared (`valuecheck`), and the ones that differ; only for an answer with the recorded status
    values_compared: int = 0
    value_diffs: list = field(default_factory=list)

    @property
    def failed(self) -> bool:
        """A hard failure that no classifier explained."""
        return self.klass == "fail"

    @property
    def is_unreplayable(self) -> bool:
        return self.klass.startswith("unreplayable:")


def compare_entry(prep: PreparedEntry, status: int, act_headers: dict[str, str], act_body: bytes,
                  rec_codes: tuple, rec_leaves_unused=None) -> tuple[list, list, Optional[str], Optional[str]]:
    """-> (hard reasons, soft diffs, expected_code, actual_code)."""
    hard: list = []
    soft: list = []
    act_ct = act_headers.get("content-type", "")
    rec_hdr_code, rec_body_code = rec_codes
    act_hdr_code, act_body_code = error_codes(act_headers, act_body, act_ct, status)
    if status != prep.rec_status:
        hard.append(("status", f"{prep.rec_status}->{status}"))
    if rec_hdr_code is not None and act_hdr_code != rec_hdr_code:
        hard.append(("error-code-header", f"{rec_hdr_code}->{act_hdr_code}"))
    if rec_body_code is not None:
        got = act_body_code if act_body_code is not None else act_hdr_code
        if got != rec_body_code:
            hard.append(("error-code-body", f"{rec_body_code}->{got}"))
    expected_code = rec_hdr_code or rec_body_code
    actual_code = act_hdr_code or act_body_code

    # soft: missing headers
    for name in prep.rec_headers:
        if name not in act_headers and not is_noise_header(name, prep.method):
            soft.append(("header-missing", name))

    # soft: body presence and shape
    rec_body = prep.rec_body
    if prep.method != "HEAD":
        if rec_body and not act_body:
            soft.append(("body-empty", "recorded non-empty, actual empty"))
        elif act_body and not rec_body:
            soft.append(("body-extra", "recorded empty, actual non-empty"))
        elif rec_body and act_body and rec_body[:4] == b"Obj\x01":
            soft.extend(compare_avro(rec_body, act_body))
        elif rec_body and act_body and not is_placeholder_body(rec_body):
            fmt = detect_format(rec_body, prep.rec_ct)
            if fmt:
                try:
                    rp = shape_paths(rec_body, fmt)
                except (ET.ParseError, ValueError, RecursionError):
                    rp = None  # recorded body itself unparseable: nothing to compare
                if rp is not None:
                    try:
                        ap = shape_paths(act_body, fmt)
                    except (ET.ParseError, ValueError, RecursionError):
                        soft.append(("body-format", f"actual body is not valid {fmt}"))
                    else:
                        for p in sorted(rp - ap):
                            soft.append(("shape-missing", p))
                        for p in sorted(ap - rp):
                            soft.append(("shape-extra", p))
    return hard, soft, expected_code, actual_code


# ---------------------------------------------------------------------------
# 5. Operation patterns
# ---------------------------------------------------------------------------

VAULT_LITERALS = {
    "secrets", "keys", "certificates", "deletedsecrets", "deletedkeys", "deletedcertificates",
    "storage", "create", "import", "backup", "restore", "sign", "verify", "encrypt", "decrypt",
    "wrapkey", "unwrapkey", "rotate", "rotationpolicy", "pending", "merge", "issuers", "contacts",
    "policy", "recover", "versions", "getrandombytes", "release", "attestation", "operation",
    "deletedstorage", "sas", "setsas", "completed", ".well-known", "hsm", "rolebackup",
    "restore", "pre-backup", "pre-restore", "securitydomain", "upload", "download", "transferkey",
}
DOCUMENTS_KEYWORDS = {"dbs", "colls", "docs", "sprocs", "udfs", "triggers", "users", "permissions",
                      "attachments", "offers", "pkranges", "conflicts"}
_ID_SEG_RE = re.compile(r"^(?:[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}|[0-9a-fA-F]{16,}|.*\d{4,}.*)$")


def _normalize_path(label: str, path: str) -> str:
    segs = [s for s in path.split("/") if s != ""]
    if not segs:
        return "/"
    if label in ("blob", "dfs"):
        first = "{container}" if label == "blob" else "{filesystem}"
        rest = "{blob}" if label == "blob" else "{path}"
        return "/" + first + ("/" + rest if len(segs) > 1 else "")
    if label == "queue":
        out = ["{queue}"]
        if len(segs) > 1:
            out.append(segs[1])  # "messages"
        if len(segs) > 2:
            out.append("{id}")
        return "/" + "/".join(out)
    if label == "table":
        s0 = unquote(segs[0])
        m = re.match(r"^(?i:tables)\((.*)\)$", s0)
        if m:
            return "/Tables('{table}')"
        if s0.lower() == "tables" or s0.startswith("$"):
            return "/" + s0
        m = re.match(r"^(.*?)\((.*)\)$", s0)
        if m:
            inner = m.group(2)
            keys = ",".join(k for k in ("PartitionKey", "RowKey") if k in inner)
            return "/{table}(" + keys + ")"
        return "/{table}"
    if label == "vault":
        out = [segs[0]]
        names = iter(["{name}", "{version}", "{id}", "{id2}", "{id3}"])
        for s in segs[1:]:
            out.append(s if s in VAULT_LITERALS else next(names, "{id}"))
        return "/" + "/".join(out)
    if label == "azconfig":
        names = iter(["{name}", "{id}", "{id2}"])
        return "/" + "/".join([segs[0]] + [next(names, "{id}") for _ in segs[1:]])
    if label == "servicebus":
        out = []
        prev = ""
        for i, s in enumerate(segs):
            if i == 0:
                out.append(s if s.startswith("$") else "{entity}")
            elif s in ("subscriptions", "rules", "messages", "queues", "topics") or prev == "$Resources":
                out.append(s)
            elif prev == "subscriptions":
                out.append("{subscription}")
            elif prev == "rules":
                out.append("{rule}")
            else:
                out.append("{id}")
            prev = s
        return "/" + "/".join(out)
    if label == "documents":
        out = []
        prev = ""
        for s in segs:
            out.append(s if (s.lower() in DOCUMENTS_KEYWORDS or prev.lower() not in DOCUMENTS_KEYWORDS) else "{id}")
            prev = s
        return "/" + "/".join(out)
    if label == "eventgrid":
        return "/" + "/".join(segs)
    if label == ARM_LABEL:
        # /subscriptions/{sub}/resourceGroups/{rg}/providers/{Namespace}/{type}/{name}/{type}/{name}[/{action}]
        out = []
        i = 0
        while i < len(segs):
            low = segs[i].lower()
            if low == "subscriptions" and i + 1 < len(segs):
                out += ["subscriptions", "{sub}"]
                i += 2
            elif low == "resourcegroups" and i + 1 < len(segs):
                out += ["resourceGroups", "{rg}"]
                i += 2
            elif low == "providers" and i + 1 < len(segs):
                out += ["providers", segs[i + 1]]
                rest = segs[i + 2:]
                # types are the even positions and names the odd ones; operation status ids and tag names are names too
                out += [(s if j % 2 == 0 else "{name}") for j, s in enumerate(rest)]
                break
            else:
                out.append(segs[i] if not _ID_SEG_RE.match(segs[i]) else "{id}")
                i += 1
        return "/" + "/".join(out)
    return "/" + "/".join("{id}" if _ID_SEG_RE.match(s) else s for s in segs)


def operation_pattern(method: str, label: str, target: str) -> str:
    """`PUT /{queue}/messages/{id}?popreceipt&visibilitytimeout`: variable path segments
    become placeholders; selector query keys keep their value, others appear by name."""
    path, _, query = target.partition("?")
    path = _normalize_path(label, path)
    keys: list[str] = []
    for part in query.split("&") if query else []:
        if not part:
            continue
        k, _, v = part.partition("=")
        k = unquote(k)
        lk = k.lower()
        if lk in IGNORED_QUERY_KEYS:
            continue
        keys.append(f"{lk}={unquote(v)}" if lk in SELECTOR_QUERY_KEYS else k)
    q = "&".join(sorted(set(keys), key=lambda x: (not x.partition("=")[2] and "=" not in x, x)))
    return f"{method} {path}" + (f"?{q}" if q else "")


def group_key(pattern: str, exp_status: Any, act_status: Any, exp_code: Optional[str], act_code: Optional[str]) -> str:
    codes = f" ({exp_code or ''}→{act_code or ''})" if (exp_code or act_code) else ""
    return f"{pattern}  {exp_status}→{act_status if act_status is not None else 'ERR'}{codes}"


# ---------------------------------------------------------------------------
# 5b. Request rewriting (copy sources) and classification
# ---------------------------------------------------------------------------
#
# Result classes of one replayed entry:
#   pass                              no hard failure
#   fail                              hard failure (status code or error code differs)
#   unreplayable:auth-dependent       recorded error was caused by the credential (not sent)
#   unreplayable:sanitized-copy-source  copy source is `Sanitized` and no source blob can be
#                                     guessed (not sent)
#   unreplayable:leftover-state       "already exists" on a first create, emulator created it
#   unreplayable:generated-key        decrypt / unwrap of Azure's ciphertext with a key the vault generated in
#                                     the recording (a different key on the emulator) (not sent)
#   unreplayable:throttled            recorded status is 429 (Azure throttled it, the SDK retried;
#                                     the emulator never throttles and would apply it) (not sent)
#   ahead-async                       Azure finished later than the emulator (404 poll answered 2xx)
#
# Each classifier only ever looks at entries that would otherwise be hard failures,
# except the two "not sent" ones whose request cannot be built.  With `--no-classify`
# none of the classes is assigned and every such entry is sent as raw behaviour.

CLASS_AUTH = "unreplayable:auth-dependent"
CLASS_COPY = "unreplayable:sanitized-copy-source"
CLASS_LEFTOVER = "unreplayable:leftover-state"
CLASS_THROTTLED = "unreplayable:throttled"
CLASS_AZURE_ERROR = "unreplayable:azure-server-error"
CLASS_GENERATED_KEY = "unreplayable:generated-key"
CLASS_ASYNC = "ahead-async"


def _is_2xx(status: Optional[int]) -> bool:
    return status is not None and 200 <= status < 300


def _path_of(target: str) -> str:
    return target.partition("?")[0]


def _query_of(target: str) -> dict[str, str]:
    q: dict[str, str] = {}
    for part in target.partition("?")[2].split("&"):
        if part:
            k, _, v = part.partition("=")
            q.setdefault(unquote(k).lower(), unquote(v))
    return q


def is_sanitized_value(v: str) -> bool:
    return bool(v.strip()) and is_placeholder(v.strip())


# --- unreplayable: auth-dependent -------------------------------------------

def auth_failure_reason(prep: PreparedEntry) -> Optional[str]:
    """Rule `auth-dependent`: the recorded answer is an error (status >= 400) whose error
    code is in AUTH_ERROR_CODES, or whose body names a wrong token audience.  The
    key-vault 401 challenge (`Unauthorized`, AKV10000, "request is missing a Bearer
    token") is NOT one of these: it is what a request without a credential gets."""
    if prep.rec_status < 400:
        return None
    hdr, body_code = error_codes(prep.rec_headers, prep.rec_body, prep.rec_ct, prep.rec_status)
    for code in (hdr, body_code):
        if code and code.lower() in AUTH_ERROR_CODES:
            return code
        if code and code.lower() == AUTH_COPY_SOURCE_CODE and prep.rec_status == 401:
            return code
    if AUTH_AUDIENCE_RE.search(prep.rec_body[:8192]):
        return "audience"
    return None


# --- sanitized copy sources ---------------------------------------------------

def blob_write_path(prep: PreparedEntry) -> Optional[str]:
    """Path (`/container/blob`, as recorded) of the blob that this entry wrote
    successfully, or None.  A write is Put Blob / Copy Blob / Put Blob From URL (a PUT
    without `comp` that carries x-ms-blob-type or x-ms-copy-source), Put Block List,
    Put Page and Append Block.  Recorded status must be 2xx."""
    if prep.label != "blob" or prep.method != "PUT" or not _is_2xx(prep.rec_status):
        return None
    segs = [x for x in _path_of(prep.target).split("/") if x]
    if len(segs) < 2:
        return None
    q = _query_of(prep.target)
    comp = q.get("comp", "").lower()
    if comp not in ("", "blocklist", "page", "appendblock"):
        return None
    if comp == "":
        names = {k.lower() for k, _v in prep.req_headers}
        if "restype" in q or not ({"x-ms-blob-type", "x-ms-copy-source"} & names):
            return None
    return "/" + "/".join(segs)


class BlobWriteLog:
    """Blobs written so far in one recording, most recently written last."""

    def __init__(self) -> None:
        self.order: list[str] = []

    def note(self, prep: PreparedEntry) -> None:
        path = blob_write_path(prep)
        if path:
            key = unquote(path)
            self.order = [p for p in self.order if unquote(p) != key]
            self.order.append(path)

    def pick(self, destination: str) -> Optional[str]:
        """Most recently written blob that is not `destination`."""
        dest = unquote(destination)
        for path in reversed(self.order):
            if unquote(path) != dest:
                return path
        return None


def copy_source_placeholder(prep: PreparedEntry) -> bool:
    return prep.label == "blob" and any(
        k.lower() == "x-ms-copy-source" and is_sanitized_value(v) for k, v in prep.req_headers)


def guess_copy_source(prep: PreparedEntry, writes: BlobWriteLog, settings: "Settings") -> Optional[str]:
    """Rule `copy source`: URL, on the emulator, of the most recently written blob of this
    recording other than the destination of this request.  None when there is none."""
    segs = [x for x in _path_of(prep.target).split("/") if x]
    dest = "/" + "/".join(segs)
    src = writes.pick(dest)
    if src is None:
        return None
    return f"{settings.gateway_scheme}://{prep.account}.blob.localhost:{settings.gateway_port}{src}"


class GeneratedKeys:
    """Rule `generated-key`: the keys that a recording made the vault generate (`POST /keys/{name}/create`, `.../rotate`, recorded
    2xx), per vault.  Azure's recorded ciphertext was made for Azure's key and the emulator generates another one, so decrypting
    it with that key can never give what Azure gave.  The emulator answers 400 in nearly every case and 200 in a few (measured: 3 to 4
    of these entries changed between two runs, never the same ones); which one it is is chance, so such an entry is not sent and
    not counted either way.  It only applies when the ciphertext in the request is still Azure's: when an earlier answer of the
    emulator held the ciphertext that the recording shows (an `encrypt` or `wrapkey` just before), the value mapping has put the
    emulator's own ciphertext into the request, the emulator can decrypt it, and the entry is sent and compared like any other.
    A key that was imported (`PUT /keys/{name}`) carries its own material and is not in the list."""

    def __init__(self) -> None:
        self.names: set[tuple[str, str]] = set()

    @staticmethod
    def _segments(p: PreparedEntry) -> list[str]:
        return [unquote(x) for x in _path_of(p.target).split("/") if x]

    def note(self, p: PreparedEntry) -> None:
        if p.label != "vault" or p.method != "POST" or not _is_2xx(p.rec_status):
            return
        segs = self._segments(p)
        if len(segs) == 3 and segs[0].lower() == "keys" and segs[2].lower() in ("create", "rotate"):
            self.names.add((p.account.lower(), segs[1]))

    def reason(self, p: PreparedEntry, body: Optional[bytes]) -> Optional[str]:
        """`body` is the request body as it will be sent (after value mapping)."""
        if p.label != "vault" or p.method != "POST" or not _is_2xx(p.rec_status):
            return None
        segs = self._segments(p)
        if not (len(segs) in (3, 4) and segs[0].lower() == "keys" and segs[-1].lower() in ("decrypt", "unwrapkey")
                and (p.account.lower(), segs[1]) in self.names):
            return None
        try:
            raw = p.req_body_raw
            recorded = (json.loads(raw) if isinstance(raw, (str, bytes)) else raw).get("value")
            sent = json.loads(body).get("value") if body else None
        except (ValueError, AttributeError):
            return None
        if not isinstance(recorded, str) or sent != recorded:
            return None
        return (f"{segs[-1]} of Azure's own ciphertext with the key '{segs[1]}', which the vault generated in this recording "
                "(a different key on the emulator); not sent")


def plan_request(p: PreparedEntry, writes: BlobWriteLog, settings: "Settings") -> tuple[Optional[str], Optional[tuple]]:
    """-> (copy_source_url_or_None, unsent) where `unsent` is (class, detail) when the
    entry must not be sent (only with classification on): auth-dependent, or a sanitized
    copy source with no candidate.  Used by the replay, the dry run and the mock server."""
    if settings.classify:
        if p.rec_status == 429:
            return None, (CLASS_THROTTLED, "recorded 429 (throttled by Azure; the SDK retried); not sent")
        if p.rec_status in (500, 502, 503, 504):
            return None, (CLASS_AZURE_ERROR, f"recorded {p.rec_status}: Azure itself failed (it is not an answer to compare with); not sent")
        reason = auth_failure_reason(p)
        if reason:
            return None, (CLASS_AUTH, f"recorded error code {reason}; not sent")
    copy_source = None
    if copy_source_placeholder(p):
        copy_source = guess_copy_source(p, writes, settings)
        if copy_source is None and settings.classify:
            return None, (CLASS_COPY, "x-ms-copy-source is Sanitized and no earlier blob write exists; not sent")
    return copy_source, None


# --- polling (ahead-async) --------------------------------------------------------

def resource_key(prep: PreparedEntry) -> Optional[tuple]:
    """Identity of the thing a request is about, for the async rule.  Key Vault: the
    object name, `deleted<coll>` folded into `<coll>` (a deleted secret is polled at
    /deletedsecrets/n and recovered at /secrets/n).  Other labels: the path."""
    segs = [unquote(x) for x in _path_of(prep.target).split("/") if x]
    if not segs:
        return None
    if prep.label == "vault":
        if len(segs) < 2:
            return None
        coll = segs[0].lower()
        coll = coll[len("deleted"):] if coll.startswith("deleted") else coll
        return (prep.account.lower(), coll, segs[1])
    return (prep.label, prep.account.lower(), "/".join(segs).lower())


def _starts_async_work(q: PreparedEntry) -> bool:
    """A request that, on Azure, starts work that finishes later: a delete, or a recover."""
    if not _is_2xx(q.rec_status):
        return False
    return q.method == "DELETE" or (
        q.method == "POST" and _path_of(q.target).rstrip("/").lower().endswith("/recover"))


def classify_async(i: int, prepared: list, entries: list) -> Optional[str]:
    """Rule `ahead-async`.  All of:
      1. the entry is a GET, recorded 404, and the emulator answered 2xx;
      2. the same method and recorded URL occurs LATER in the same recording with a 2xx;
      3. no write (not GET/HEAD) to the same resource lies between them;
      4. an earlier 2xx DELETE or POST .../recover on the same resource started the work.
    Returns a detail string, or None."""
    p, e = prepared[i], entries[i]
    if p.label == ARM_LABEL:
        return classify_async_arm(i, prepared, entries)
    if p.method != "GET" or p.rec_status != 404 or not _is_2xx(e.actual_status):
        return None
    key = resource_key(p)
    if key is None:
        return None
    later = None
    for j in range(i + 1, len(prepared)):
        q = prepared[j]
        if (not isinstance(q, Skipped) and q.method == p.method and q.recorded_uri == p.recorded_uri
                and _is_2xx(q.rec_status)):
            later = j
            break
    if later is None:
        return None
    for j in range(i + 1, later):
        q = prepared[j]
        if not isinstance(q, Skipped) and q.method not in ("GET", "HEAD", "OPTIONS") and resource_key(q) == key:
            return None
    for j in range(i):
        q = prepared[j]
        if not isinstance(q, Skipped) and _starts_async_work(q) and resource_key(q) == key:
            return f"recorded 404, later 2xx at entry {later}; started by entry {j} ({q.method})"
    return None


def classify_async_arm(i: int, prepared: list, entries: list) -> Optional[str]:
    """Rule `ahead-async` for Resource Manager. All of: the entry is a GET recorded 202 (the operation was still
    running), the emulator answered 2xx (it finishes every operation at once); the same URL occurs LATER in the same
    recording with a 2xx; and an earlier write (PUT, PATCH, POST or DELETE) of the recording was answered 200, 201,
    202 or 204 (it started the work)."""
    p, e = prepared[i], entries[i]
    if p.method != "GET" or p.rec_status != 202 or not _is_2xx(e.actual_status):
        return None
    later = None
    for j in range(i + 1, len(prepared)):
        q = prepared[j]
        if not isinstance(q, Skipped) and q.method == "GET" and q.recorded_uri == p.recorded_uri and _is_2xx(q.rec_status):
            later = j
            break
    if later is None:
        return None
    for j in range(i):
        q = prepared[j]
        if not isinstance(q, Skipped) and q.method in ("PUT", "PATCH", "POST", "DELETE") and q.rec_status in (200, 201, 202, 204):
            return f"recorded 202, later 2xx at entry {later}; the work was started by entry {j} ({q.method})"
    return None


# --- leftover state -----------------------------------------------------------------

def create_key(prep: PreparedEntry) -> Optional[tuple]:
    """Resource a create request makes, or None when the entry is not a create request.
    Create requests: blob Create Container (PUT /c?restype=container), Put Blob / Copy
    Blob (PUT /c/b without comp), queue Create Queue (PUT /q), table Create Table
    (POST /Tables, the name is in the body), and Key Vault set/create/import."""
    segs = [unquote(x) for x in _path_of(prep.target).split("/") if x]
    q = _query_of(prep.target)
    lab = prep.label
    if not segs:
        return None
    if lab == "blob" and prep.method == "PUT" and "comp" not in q:
        if len(segs) == 1 and q.get("restype") == "container":
            return ("path", "/" + segs[0])
        if len(segs) >= 2 and "restype" not in q:
            return ("path", "/" + "/".join(segs))
    elif lab == "queue" and prep.method == "PUT" and len(segs) == 1 and "comp" not in q:
        return ("path", "/" + segs[0])
    elif lab == "table" and prep.method == "POST" and len(segs) == 1 and segs[0].lower() == "tables":
        try:
            name = json.loads(prep.req_body.decode("utf-8")).get("TableName")
        except (ValueError, AttributeError, UnicodeDecodeError):
            return None
        return ("table", str(name).lower()) if isinstance(name, str) else None
    elif lab == "vault":
        if prep.method == "PUT" and len(segs) == 2 and segs[0].lower() in ("secrets", "keys"):
            return ("path", "/" + "/".join(segs[:2]).lower())
        if prep.method == "POST" and len(segs) == 3 and segs[0].lower() in ("keys", "certificates") \
                and segs[2].lower() in ("create", "import"):
            return ("path", "/" + "/".join(segs[:2]).lower())
    return None


def touches(q: PreparedEntry, key: tuple) -> bool:
    """Does an earlier request q concern the resource `key` (same path or something below it)?"""
    segs = [unquote(x) for x in _path_of(q.target).split("/") if x]
    if key[0] == "table":
        if q.label != "table" or not segs:
            return False
        s0 = segs[0]
        m = re.match(r"^(?i:tables)\('?([^')]*)'?\)$", s0)
        if m:
            return m.group(1).lower() == key[1]
        if s0.lower() == "tables":
            ck = create_key(q)
            return bool(ck and ck == key)
        return s0.split("(")[0].lower() == key[1]
    path = "/" + "/".join(segs)
    if q.label == "vault":
        path = path.lower()
    return path == key[1] or path.startswith(key[1] + "/")


def classify_leftover(i: int, prepared: list, entries: list) -> Optional[str]:
    """Rule `leftover-state`.  All of:
      1. the entry is a create request (see `create_key`), recorded 409 whose error code
         ends in `AlreadyExists` or is `Conflict`, and the emulator answered 2xx;
      2. it is the first request in the recording for that resource: no earlier entry
         (other than a recorded 401 challenge) concerns the same path or table.
    Returns a detail string, or None."""
    p, e = prepared[i], entries[i]
    if p.rec_status != 409 or not _is_2xx(e.actual_status):
        return None
    code = (e.expected_code or "").lower()
    if not (code.endswith("alreadyexists") or code == "conflict"):
        return None
    key = create_key(p)
    if key is None:
        return None
    for j in range(i):
        q = prepared[j]
        if isinstance(q, Skipped) or q.rec_status == 401:
            continue
        if touches(q, key):
            return None
    return f"first request for {key[1]} answered {e.expected_code} on Azure; emulator created it"


def apply_classifiers(prepared: list, entries: list) -> None:
    """Assign `ahead-async` / `unreplayable:leftover-state` to hard failures that match."""
    for i, (p, e) in enumerate(zip(prepared, entries)):
        if isinstance(p, Skipped) or e.klass != "fail":
            continue
        detail = classify_async(i, prepared, entries)
        if detail:
            e.klass, e.class_detail = CLASS_ASYNC, detail
            continue
        detail = classify_leftover(i, prepared, entries)
        if detail:
            e.klass, e.class_detail = CLASS_LEFTOVER, detail


# ---------------------------------------------------------------------------
# 6. HTTP client, replay, aggregation, CLI
# ---------------------------------------------------------------------------

class Client:
    """One persistent connection to the gateway; raw request targets (no re-encoding)."""

    def __init__(self, gateway: str, timeout: float) -> None:
        u = urlsplit(gateway)
        self.scheme = u.scheme or "http"
        self.host = u.hostname or "127.0.0.1"
        self.port = u.port or (443 if self.scheme == "https" else 80)
        self.timeout = timeout
        self.conn: Optional[http.client.HTTPConnection] = None
        self._used = False

    def _connect(self) -> http.client.HTTPConnection:
        if self.conn is None:
            if self.scheme == "https":
                ctx = ssl.create_default_context()
                if self.host in ("127.0.0.1", "localhost", "::1"):
                    # a local emulator with a self-signed certificate
                    ctx.check_hostname = False
                    ctx.verify_mode = ssl.CERT_NONE
                self.conn = http.client.HTTPSConnection(self.host, self.port, timeout=self.timeout, context=ctx)
            else:
                self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
            self._used = False
            self.conn.connect()
            try:
                self.conn.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except OSError:
                pass
        return self.conn

    def close(self) -> None:
        if self.conn is not None:
            try:
                self.conn.close()
            finally:
                self.conn = None

    def request(self, method: str, target: str, host_header: str, headers: list, body: Optional[bytes]):
        """-> (status, [(name, value)], body bytes).  Raises on connection problems."""
        for attempt in (0, 1):
            conn = self._connect()
            reused = self._used
            try:
                conn.putrequest(method, sanitize_target(target), skip_host=True, skip_accept_encoding=True)
                conn.putheader("Host", host_header)
                conn.putheader("Accept-Encoding", "identity")
                for k, v in headers:
                    v = re.sub(r"[\r\n]+", " ", v)
                    try:
                        v.encode("latin-1")
                        conn.putheader(k, v)
                    except UnicodeEncodeError:
                        conn.putheader(k, v.encode("utf-8"))
                if body:
                    conn.putheader("Content-Length", str(len(body)))
                elif method in ("POST", "PUT", "PATCH"):
                    conn.putheader("Content-Length", "0")
                conn.endheaders(body if body else None)
                resp = conn.getresponse()
                data = resp.read()
                self._used = True
                result = (resp.status, resp.getheaders(), data)
                if resp.will_close:
                    self.close()
                return result
            except (ConnectionError, http.client.BadStatusLine, http.client.CannotSendRequest) as exc:
                self.close()
                if attempt == 0 and reused:
                    continue  # the server closed an idle keep-alive connection; retry once
                raise exc
            except Exception:
                self.close()
                raise
        raise RuntimeError("unreachable")


@dataclass
class RecordingResult:
    name: str
    path: str
    package: str
    status: str  # raw: pass | fail | error   (pass = every entry passed)
    error: Optional[str] = None
    entries: list = field(default_factory=list)  # EntryResult
    skipped: list = field(default_factory=list)  # Skipped

    @property
    def status_adjusted(self) -> str:
        """pass | fail | error.  Adjusted: no hard failure left after classification
        (unreplayable and ahead-async entries do not count as failures)."""
        if self.status == "error":
            return "error"
        return "fail" if any(e.failed for e in self.entries) else "pass"

    @property
    def verified_entries(self) -> int:
        return sum(1 for e in self.entries if e.klass in ("pass", CLASS_ASYNC))


@dataclass
class Settings:
    gateway: str
    gateway_host: str
    gateway_port: int
    force_label: Optional[str]
    auth_defaults: dict
    timeout: float
    reset: bool
    dry_run: bool
    time_shift: bool = True
    classify: bool = True
    gateway_scheme: str = "http"


def _fmt_http_date(dt) -> str:
    return format_datetime(dt, usegmt=True)


def shift_http_date(value: str, offset_seconds: float) -> str:
    try:
        dt = parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return value
    from datetime import timedelta
    return _fmt_http_date(dt + timedelta(seconds=offset_seconds))


#: A fixed AES-256 key and its SHA-256 hash (base64), sent where a recording has "Sanitized" key headers.
_CPK_KEY = bytes(range(32))
CPK_PLACEHOLDERS = {
    "x-ms-encryption-key": base64.b64encode(_CPK_KEY).decode(),
    "x-ms-encryption-key-sha256": base64.b64encode(hashlib.sha256(_CPK_KEY).digest()).decode(),
    "x-ms-source-encryption-key": base64.b64encode(_CPK_KEY).decode(),
    "x-ms-source-encryption-key-sha256": base64.b64encode(hashlib.sha256(_CPK_KEY).digest()).decode(),
}


def build_request(prep: PreparedEntry, vmap: ValueMap, settings: Settings, time_offset: float,
                  copy_source: Optional[str] = None):
    """-> (target, host_header, headers, body bytes) with value mapping applied.
    `copy_source` replaces a sanitized x-ms-copy-source (see `guess_copy_source`)."""
    target = vmap.apply(prep.target)
    if prep.label == ARM_LABEL:
        host_header = f"{ARM_LABEL}.localhost:{settings.gateway_port}"
    else:
        host_header = f"{prep.account}.{prep.label}.localhost:{settings.gateway_port}"
    headers = []
    for k, v in prep.req_headers:
        lk = k.lower()
        if lk == "x-ms-copy-source" and copy_source is not None and is_sanitized_value(v):
            headers.append((k, copy_source))
            continue
        if lk in CREDENTIAL_HEADERS and is_placeholder_credential(v):
            headers.append((k, PLACEHOLDER_BEARER))
            continue
        if lk in CPK_PLACEHOLDERS and is_sanitized_value(v):
            # the recordings replace the customer-provided key and its hash with "Sanitized"; the emulator checks that
            # the key, the hash and the algorithm agree, so a real key (always the same one) and its hash are sent
            headers.append((k, CPK_PLACEHOLDERS[lk]))
            continue
        if lk not in ("authorization", "user-agent", "x-ms-date", "date"):
            v = vmap.apply(v)
        if settings.time_shift and time_offset and TIME_SHIFT_HEADER_RE.match(lk):
            v = shift_http_date(v, time_offset)
        headers.append((k, v))
    auth = choose_auth(prep, settings.auth_defaults)
    if auth:
        headers.append(("Authorization", auth))
    body = prep.req_body
    if len(vmap) and body:
        if prep.req_body_kind == "json":
            body = json.dumps(vmap.apply_json(prep.req_body_raw), ensure_ascii=False, separators=(",", ":")).encode()
        elif prep.req_body_kind in ("text", "list", "text-fallback") or (
                prep.req_body_kind == "base64" and prep.req_ct.startswith("multipart/")):
            # (multipart batches are stored as base64 but are text: sub-request URLs carry ids)
            try:
                body = vmap.apply(body.decode("utf-8")).encode("utf-8")
            except UnicodeDecodeError:
                pass
    return target, host_header, headers, (body if body else None)


def _parse_date(h: dict[str, str]):
    v = h.get("date")
    if not v:
        return None
    try:
        return parsedate_to_datetime(v)
    except (TypeError, ValueError):
        return None


class SecondGate:
    """Keep the order of the seconds that a recording lived through.

    A recording took seconds (a test that sleeps, a slow network); the replay takes milliseconds. Dates on the wire have one-second
    resolution, so two things that were seconds apart in the recording can land in the same second in the replay, and a request
    with `If-Modified-Since` or `If-Unmodified-Since` (the date of the first thing) then gets another answer than Azure gave,
    depending only on whether the replay happened to cross a second boundary. So this waits for the next second before an entry
    that was answered in a later second than the one before it, when the clock has not moved on by itself, up to the last entry of
    the recording that carries such a header. Nothing else in the replay waits, and a wait is at most a little over a second.
    """

    def __init__(self, prepared: list, now=time.time, sleep=time.sleep) -> None:
        self.now, self.sleep = now, sleep
        self.last = -1
        for i, p in enumerate(prepared):
            if not isinstance(p, Skipped) and any(TIME_SHIFT_HEADER_RE.match(k.lower()) for k, _v in p.req_headers):
                self.last = i
        self.prev_recorded = None
        self.prev_wall: Optional[float] = None
        self.waits = 0

    def before(self, position: int, p: PreparedEntry) -> None:
        """Call before an entry is sent; `position` is its place in the recording."""
        if position > self.last or self.prev_recorded is None or self.prev_wall is None:
            return
        recorded = _parse_date(p.rec_headers)
        if recorded is None or recorded <= self.prev_recorded:
            return
        now = self.now()
        if int(now) <= int(self.prev_wall):
            self.sleep(int(now) + 1 - now + 0.02)
            self.waits += 1

    def after(self, p: PreparedEntry) -> None:
        """Call after the answer to an entry has come."""
        recorded = _parse_date(p.rec_headers)
        if recorded is not None:
            self.prev_recorded = recorded
        self.prev_wall = self.now()


def _unsent_result(p: PreparedEntry, klass: str, detail: str) -> EntryResult:
    codes = error_codes(p.rec_headers, p.rec_body, p.rec_ct, p.rec_status)
    return EntryResult(p.index, p.method, "", "", p.recorded_uri, p.rec_status, None, codes[0] or codes[1], None,
                       [], [], operation_pattern(p.method, p.label, p.target), klass=klass, class_detail=detail)


_ARM_GROUP_RE = re.compile(r"^/subscriptions/([^/?]+)/resourcegroups/([^/?]+)(/|\?|$)", re.I)


def prerequisite_groups(prepared: list) -> list[tuple[str, str]]:
    """The resource groups an ARM recording works in without ever creating them. The `azure-mgmt-*` tests get their group
    (`rgname`, the sanitizer's word) from a test fixture whose requests are not in the recording, so without the group
    every answer would be `ResourceGroupNotFound` and nothing about the service could be compared. A group that a
    successful `PUT .../resourceGroups/<name>` earlier in the recording creates is not one of them."""
    created: set = set()
    needed: list = []
    for p in prepared:
        if isinstance(p, Skipped) or p.label != ARM_LABEL:
            continue
        path = _path_of(p.target)
        m = _ARM_GROUP_RE.match(path)
        if not m:
            continue
        key = (m.group(1).lower(), m.group(2).lower())
        if p.method == "PUT" and not path.rstrip("/").lower().split("resourcegroups/", 1)[1].count("/") and _is_2xx(p.rec_status):
            created.add(key)
            continue
        if key not in created and key not in [(s.lower(), g.lower()) for s, g in needed]:
            needed.append((m.group(1), m.group(2)))
    return needed


def make_prerequisite_groups(prepared: list, client: Optional[Client], settings: Settings) -> None:
    """Creates `prerequisite_groups` before the first request (not scored, not part of the report)."""
    if client is None:
        return
    for sub, rg in prerequisite_groups(prepared):
        body = json.dumps({"location": "eastus"}).encode()
        client.request("PUT", f"/subscriptions/{sub}/resourcegroups/{rg}?api-version=2021-04-01",
                       f"{ARM_LABEL}.localhost:{settings.gateway_port}",
                       [("Authorization", "Bearer replay"), ("Content-Type", "application/json")], body)


def run_recording(name: str, path: str, settings: Settings, client: Optional[Client]) -> RecordingResult:
    result = RecordingResult(name=name, path=path, package=package_of(path), status="pass")
    try:
        raw_entries = load_recording(path)
        prepared: list = []
        for i, raw in enumerate(raw_entries):
            prepared.append(prepare_entry(raw, i, settings.force_label, arm=package_of(path).startswith("azure-mgmt-")))
    except (RecordingFormatError, ValueError, OSError, RecursionError, UnicodeDecodeError) as exc:
        result.status, result.error = "error", f"{type(exc).__name__}: {exc}"
        return result

    if settings.dry_run:
        return _dry_run(result, prepared, settings)

    assert client is not None
    if settings.reset:
        try:
            st, _h, _b = client.request("POST", "/_azemu/reset", f"127.0.0.1:{settings.gateway_port}", [], None)
            if st >= 300:
                result.status, result.error = "error", f"reset returned {st}"
                return result
        except Exception as exc:  # noqa: BLE001
            result.status, result.error = "error", f"reset failed: {type(exc).__name__}: {exc}"
            return result

    if settings.classify:
        try:
            make_prerequisite_groups(prepared, client, settings)
        except Exception as exc:  # noqa: BLE001
            result.status, result.error = "error", f"prerequisite groups failed: {type(exc).__name__}: {exc}"
            return result

    vmap = ValueMap()
    history = RequestHistory()
    writes = BlobWriteLog()
    account_settings = AccountSettings(prepared)
    time_offset = 0.0
    offset_known = False
    gate = SecondGate(prepared)
    generated_keys = GeneratedKeys()
    try:
        for position, p in enumerate(prepared):
            if isinstance(p, Skipped):
                result.skipped.append(p)
                result.entries.append(EntryResult(
                    p.index, p.method, "", "", p.uri, 0, None, None, None, [], [], "", skipped=p.reason,
                    klass="skipped"))
                continue
            copy_source, unsent = plan_request(p, writes, settings)
            if unsent:
                result.entries.append(_unsent_result(p, *unsent))
                continue
            target, host_header, headers, body = build_request(p, vmap, settings, time_offset, copy_source)
            unsent_key = generated_keys.reason(p, body) if settings.classify else None
            generated_keys.note(p)
            if unsent_key:
                result.entries.append(_unsent_result(p, CLASS_GENERATED_KEY, unsent_key))
                continue
            pattern = operation_pattern(p.method, p.label, p.target)
            history.add(p)
            rec_codes = error_codes(p.rec_headers, p.rec_body, p.rec_ct, p.rec_status)
            gate.before(position, p)
            try:
                status, raw_headers, act_body = client.request(p.method, target, host_header, headers, body)
                gate.after(p)
            except Exception as exc:  # noqa: BLE001 - connection errors/timeouts are failures, not crashes
                is_timeout = isinstance(exc, (socket.timeout, TimeoutError))
                kind = "timeout" if is_timeout else "connection"
                er = EntryResult(
                    p.index, p.method, target, host_header, p.recorded_uri, p.rec_status, None,
                    rec_codes[0] or rec_codes[1], kind, [(kind, f"{type(exc).__name__}: {exc}")], [], pattern,
                    error=f"{type(exc).__name__}: {exc}", klass="fail")
                result.entries.append(er)
                writes.note(p)
                continue
            act_headers = lower_headers({k: v for k, v in raw_headers})
            hard, soft, exp_code, act_code = compare_entry(p, status, act_headers, act_body, rec_codes)
            rec_leaves = body_leaves(p.rec_body, p.rec_ct) if p.rec_body else {}
            values_compared, value_diffs = 0, []
            if not hard:
                values_compared, value_diffs = valuecheck.compare_values(
                    p.account, p.rec_headers, act_headers,
                    value_leaves(p.rec_body, p.rec_ct) if p.rec_body else {},
                    value_leaves(act_body, act_headers.get("content-type", "")) if act_body else {},
                    skip_headers=account_settings.skip_headers(p), skip_paths=account_settings.skip_paths(p),
                    skip_endings=account_settings.skip_endings())
            er = EntryResult(
                p.index, p.method, target, host_header, p.recorded_uri, p.rec_status, status, exp_code,
                act_code, hard, soft, pattern, status_match=(status == p.rec_status),
                klass="fail" if hard else "pass", values_compared=values_compared, value_diffs=value_diffs)
            if hard:
                er.actual_body = act_body[:2000].decode("utf-8", "replace")
            result.entries.append(er)
            writes.note(p)
            account_settings.note(p)
            if not offset_known:
                ad, rd = _parse_date(act_headers), _parse_date(p.rec_headers)
                if ad and rd:
                    time_offset = (ad - rd).total_seconds()
                    offset_known = True
            update_value_map(vmap, history, p, act_headers, act_body, rec_leaves)
        if settings.classify:
            apply_classifiers(prepared, result.entries)
    except Exception as exc:  # noqa: BLE001 - a bug in this tool must not kill the run
        result.status, result.error = "error", f"internal error: {type(exc).__name__}: {exc}\n{traceback.format_exc(limit=4)}"
        return result
    if any(e.klass != "pass" and e.klass != "skipped" for e in result.entries):
        result.status = "fail"
    return result


def _dry_run(result: RecordingResult, prepared: list, settings: Settings) -> RecordingResult:
    """Build every request without sending anything.  Entries come back as `pass`
    except those that `run_recording` would not send."""
    vmap = ValueMap()
    writes = BlobWriteLog()
    generated_keys = GeneratedKeys()
    for p in prepared:
        if isinstance(p, Skipped):
            result.skipped.append(p)
            continue
        try:
            copy_source, unsent = plan_request(p, writes, settings)
            if unsent:
                result.entries.append(_unsent_result(p, *unsent))
                continue
            _t, _h, hdrs, _b = build_request(p, vmap, settings, 0.0, copy_source)
            unsent_key = generated_keys.reason(p, _b) if settings.classify else None
            generated_keys.note(p)
            if unsent_key:
                result.entries.append(_unsent_result(p, CLASS_GENERATED_KEY, unsent_key))
                continue
            writes.note(p)
            operation_pattern(p.method, p.label, p.target)
            body_leaves(p.rec_body, p.rec_ct)
            soft = []
            if copy_source is not None:
                soft.append(("copy-source-rewritten", copy_source))
            shape = detect_format(p.rec_body, p.rec_ct)
            if shape and not is_placeholder_body(p.rec_body):
                try:
                    shape_paths(p.rec_body, shape)
                except (ET.ParseError, ValueError, RecursionError):
                    soft.append(("recorded-body-unparseable", shape))
            result.entries.append(EntryResult(
                p.index, p.method, p.target, "", p.recorded_uri, p.rec_status, p.rec_status, None, None,
                [], soft, "", status_match=True))
        except Exception as exc:  # noqa: BLE001 - dry run must report, never crash
            result.status, result.error = "error", f"entry {p.index}: {type(exc).__name__}: {exc}"
            return result
    return result


# --- aggregation --------------------------------------------------------------

@dataclass
class Group:
    key: str
    method: str
    pattern: str
    expected_status: Any
    actual_status: Any
    expected_code: Optional[str]
    actual_code: Optional[str]
    count: int = 0
    first_in_recording: int = 0
    files: list = field(default_factory=list)


def _pct(a: int, b: int) -> float:
    return round(100.0 * a / b, 2) if b else 100.0


def _generic_path(path: str) -> str:
    """`/value[3]/properties/x` -> `/value[]/properties/x`: array positions and XML element numbers do not make a new group."""
    return re.sub(r"\[\d+\]", "[]", path)


@dataclass
class ValueGroup:
    """Value differences of one kind in one operation: `GET /{queue}/messages` 200, body value `/QueueMessagesList/...`."""
    pattern: str
    status: Any
    kind: str  # header | body | text
    key: str
    count: int = 0
    #: (recorded, actual, recording, entry index) of the first one
    example: tuple = ()
    files: list = field(default_factory=list)

    @property
    def label(self) -> str:
        what = {"header": "header", "body": "value", "text": "error text"}.get(self.kind, self.kind)
        return f"{self.pattern}  {self.status}  {what} {self.key}"


def aggregate(results: list[RecordingResult]) -> dict:
    """Counts raw (everything that is not a pass counts against) and adjusted
    (unreplayable entries leave the denominator, ahead-async counts as passing)."""
    rec_total = len(results)
    rec_pass = sum(r.status == "pass" for r in results)
    rec_pass_adj = sum(r.status_adjusted == "pass" for r in results)
    rec_fail = sum(r.status == "fail" for r in results)
    rec_err = sum(r.status == "error" for r in results)
    rec_vacuous = sum(1 for r in results if r.status_adjusted == "pass" and r.entries and r.verified_entries == 0)
    entries_total = skipped = evaluated = 0
    classes: Counter = Counter()
    class_examples: dict[str, list] = defaultdict(list)
    status_ok = status_ok_adj = 0
    groups: dict[str, Group] = {}
    soft_header: Counter = Counter()
    soft_shape: Counter = Counter()
    soft_other: Counter = Counter()
    soft_examples: dict = {}
    per_package: dict[str, Counter] = defaultdict(Counter)
    soft_entries = 0
    values_compared = values_differ = entries_with_value_diffs = 0
    value_groups: dict[tuple, ValueGroup] = {}
    for r in results:
        pk = per_package[r.package]
        pk["recordings"] += 1
        pk[f"recordings_{r.status}"] += 1
        if r.status_adjusted == "pass":
            pk["recordings_pass_adjusted"] += 1
        first_failure_seen = False
        for e in r.entries:
            entries_total += 1
            pk["entries"] += 1
            if e.skipped:
                skipped += 1
                pk["skipped"] += 1
                continue
            evaluated += 1
            pk["evaluated"] += 1
            classes[e.klass] += 1
            pk[f"class:{e.klass}"] += 1
            if len(class_examples[e.klass]) < 3:
                class_examples[e.klass].append((r.name, e.index))
            if e.status_match:
                status_ok += 1
                status_ok_adj += 1
                pk["status_ok"] += 1
            elif e.klass == CLASS_ASYNC:
                status_ok_adj += 1
            if e.klass == "fail":
                key = group_key(e.pattern, e.expected_status, e.actual_status, e.expected_code, e.actual_code)
                g = groups.get(key)
                if g is None:
                    g = groups[key] = Group(key, e.method, e.pattern, e.expected_status,
                                            e.actual_status if e.actual_status is not None else "ERR",
                                            e.expected_code, e.actual_code)
                g.count += 1
                if not first_failure_seen:
                    g.first_in_recording += 1
                    first_failure_seen = True
                if len(g.files) < 3 and r.name not in g.files:
                    g.files.append(r.name)
            elif e.klass == "pass":
                pk["passing"] += 1
            if e.values_compared:
                values_compared += e.values_compared
                pk["values_compared"] += e.values_compared
            if e.value_diffs:
                entries_with_value_diffs += 1
                values_differ += len(e.value_diffs)
                pk["values_differ"] += len(e.value_diffs)
                for d in e.value_diffs:
                    vk = (e.pattern, e.actual_status, d.kind, _generic_path(d.key))
                    vg = value_groups.get(vk)
                    if vg is None:
                        vg = value_groups[vk] = ValueGroup(e.pattern, e.actual_status, d.kind, _generic_path(d.key),
                                                           example=(d.recorded, d.actual, r.name, e.index))
                    vg.count += 1
                    if len(vg.files) < 3 and r.name not in vg.files:
                        vg.files.append(r.name)
            if e.soft:
                soft_entries += 1
            seen_soft = set()
            for kind, detail in e.soft:
                st = e.actual_status if e.actual_status is not None else "ERR"
                if kind == "header-missing":
                    k = (e.pattern, st, detail)
                    soft_header[k] += 1
                elif kind in ("shape-missing", "shape-extra"):
                    k = (e.pattern, st, kind, detail)
                    soft_shape[k] += 1
                else:
                    k = (e.pattern, st, kind, detail)
                    soft_other[k] += 1
                if (kind, k) not in seen_soft:
                    seen_soft.add((kind, k))
                    soft_examples.setdefault((kind, k), r.name)
    passing = classes["pass"]
    unreplayable = sum(n for k, n in classes.items() if k.startswith("unreplayable:"))
    asyncn = classes[CLASS_ASYNC]
    hard_failing = classes["fail"]
    summary = {
        "recordings_total": rec_total,
        "recordings_passing": rec_pass,
        "recordings_passing_pct": _pct(rec_pass, rec_total),
        "recordings_passing_adjusted": rec_pass_adj,
        "recordings_passing_adjusted_pct": _pct(rec_pass_adj, rec_total),
        "recordings_adjusted_pass_with_nothing_verified": rec_vacuous,
        "recordings_with_failures": rec_fail,
        "recordings_errored": rec_err,
        "entries_total": entries_total,
        "entries_skipped": skipped,
        "entries_evaluated": evaluated,
        "entries_passing": passing,
        "entries_failing": evaluated - passing,
        "entries_hard_failing": hard_failing,
        "entries_unreplayable": unreplayable,
        "entries_ahead_async": asyncn,
        "entries_pass_raw_pct": _pct(passing, evaluated),
        "entries_pass_adjusted": passing + asyncn,
        "entries_adjusted_denominator": evaluated - unreplayable,
        "entries_pass_adjusted_pct": _pct(passing + asyncn, evaluated - unreplayable),
        "entries_status_match": status_ok,
        "entries_status_match_pct": _pct(status_ok, evaluated),
        "entries_status_match_adjusted": status_ok_adj,
        "entries_status_match_adjusted_pct": _pct(status_ok_adj, evaluated - unreplayable),
        "entries_with_soft_differences": soft_entries,
        "values_compared": values_compared,
        "values_differ": values_differ,
        "values_differ_pct": _pct(values_differ, values_compared) if values_compared else 0.0,
        "entries_with_value_differences": entries_with_value_diffs,
        "classes": dict(classes),
    }
    return {
        "summary": summary,
        "groups": sorted(groups.values(), key=lambda g: (-g.count, g.key)),
        "value_groups": sorted(value_groups.values(), key=lambda g: (-g.count, g.label)),
        "soft_header": soft_header, "soft_shape": soft_shape, "soft_other": soft_other,
        "soft_examples": soft_examples,
        "per_package": per_package,
        "class_examples": class_examples,
    }


def format_summary(agg: dict, show: int, show_soft: int) -> str:
    s = agg["summary"]
    lines = [
        "== Summary: RAW (everything that is not a pass counts against) ==",
        f"recordings: {s['recordings_total']} total | {s['recordings_passing']} fully passing "
        f"({s['recordings_passing_pct']}%) | {s['recordings_with_failures']} with failures | "
        f"{s['recordings_errored']} errored",
        f"entries:    {s['entries_total']} total | {s['entries_evaluated']} replayed | "
        f"{s['entries_passing']} passing ({s['entries_pass_raw_pct']}%) | {s['entries_failing']} not passing | "
        f"{s['entries_skipped']} skipped (host not ours)",
        f"status code matches: {s['entries_status_match']}/{s['entries_evaluated']} "
        f"({s['entries_status_match_pct']}%)",
        "",
        "== Summary: ADJUSTED (unreplayable entries removed from the denominator, ahead-async counted as passing) ==",
        f"recordings fully passing (adjusted, no hard failure left): {s['recordings_passing_adjusted']}/"
        f"{s['recordings_total']} ({s['recordings_passing_adjusted_pct']}%)"
        + (f"  [{s['recordings_adjusted_pass_with_nothing_verified']} of them have no verified entry at all]"
           if s["recordings_adjusted_pass_with_nothing_verified"] else ""),
        f"entries passing (adjusted): {s['entries_pass_adjusted']}/{s['entries_adjusted_denominator']} "
        f"({s['entries_pass_adjusted_pct']}%)",
        f"status code matches (adjusted): {s['entries_status_match_adjusted']}/{s['entries_adjusted_denominator']} "
        f"({s['entries_status_match_adjusted_pct']}%)",
        "",
        "== Result classes (replayed entries) ==",
    ]
    for k in sorted(s["classes"], key=lambda k: (k != "pass", k != "fail", k)):
        ex = agg["class_examples"].get(k, [])
        lines.append(f"{s['classes'][k]:7d}  {k}" + (f"   e.g. {ex[0][0]}#{ex[0][1]}" if ex and k != "pass" else ""))
    if not any(k != "pass" and k != "fail" for k in s["classes"]):
        lines.append("        (no entry was classified; --no-classify was given or nothing matched)")
    lines.append(f"entries with soft differences: {s['entries_with_soft_differences']}")
    lines.append(
        f"values compared (headers, body values, error texts; the rules that leave a value out are in README.md): "
        f"{s['values_compared']} | values that differ: {s['values_differ']} ({s['values_differ_pct']}%) "
        f"in {s['entries_with_value_differences']} entries")
    pp = agg["per_package"]
    if len(pp) > 1:
        lines.append("")
        lines.append("== By package ==")
        for name in sorted(pp):
            c = pp[name]
            lines.append(
                f"  {name:36s} recordings {c['recordings_pass']:5d}/{c['recordings']:<5d} pass raw, "
                f"{c['recordings_pass_adjusted']:5d} adjusted"
                f"  entries {c['passing']:6d}/{c['evaluated']:<6d} pass  status ok {c['status_ok']}/{c['evaluated']}"
            )
    groups = agg["groups"]
    lines.append("")
    lines.append(f"== Hard failure groups, after classification ({len(groups)} groups, showing {min(show, len(groups))}) ==")
    if not groups:
        lines.append("  none")
    for g in groups[:show]:
        lines.append(f"{g.count:6d}  {g.key}   [first failure in {g.first_in_recording} recordings]")
        for f in g.files:
            lines.append(f"          {f}")
    vgroups = agg["value_groups"]
    lines.append("")
    lines.append(f"== Values that differ, grouped ({len(vgroups)} groups, showing {min(show, len(vgroups))}) ==")
    if not vgroups:
        lines.append("  none")
    for g in vgroups[:show]:
        lines.append(f"{g.count:6d}  {g.label}")
        if g.example:
            lines.append(f"          Azure: {g.example[0]!r}   emulator: {g.example[1]!r}   ({g.example[2]}#{g.example[3]})")
    soft_h = agg["soft_header"].most_common(show_soft)
    lines.append("")
    lines.append(f"== Soft: response headers missing (top {len(soft_h)} of {len(agg['soft_header'])}) ==")
    for (pat, st, name), n in soft_h:
        lines.append(f"{n:6d}  {pat}  {st}  missing header {name}")
    soft_s = agg["soft_shape"].most_common(show_soft)
    lines.append("")
    lines.append(f"== Soft: body shape differences (top {len(soft_s)} of {len(agg['soft_shape'])}) ==")
    for (pat, st, kind, path), n in soft_s:
        word = "missing from actual" if kind == "shape-missing" else "extra in actual"
        lines.append(f"{n:6d}  {pat}  {st}  {word}: {path}")
    soft_o = agg["soft_other"].most_common(show_soft)
    if soft_o:
        lines.append("")
        lines.append(f"== Soft: other body differences (top {len(soft_o)} of {len(agg['soft_other'])}) ==")
        for (pat, st, kind, detail), n in soft_o:
            lines.append(f"{n:6d}  {pat}  {st}  {detail}")
    return "\n".join(lines)


def build_report(agg: dict, results: list[RecordingResult], args_dict: dict, elapsed: float) -> dict:
    def entry_json(e: EntryResult) -> dict:
        if e.skipped:
            return {"index": e.index, "class": "skipped", "skipped": e.skipped, "method": e.method,
                    "recorded_url": e.recorded_url}
        d = {
            "index": e.index, "method": e.method, "url": e.url, "host": e.host,
            "recorded_url": e.recorded_url,
            "pattern": e.pattern,
            "expected_status": e.expected_status, "actual_status": e.actual_status,
            "expected_error_code": e.expected_code, "actual_error_code": e.actual_code,
            "class": e.klass, "class_reason": e.class_detail,
            "failed": e.failed, "hard": [list(h) for h in e.hard],
            "soft": [list(s) for s in e.soft],
        }
        if e.values_compared:
            d["values_compared"] = e.values_compared
        if e.value_diffs:
            d["value_diffs"] = [{"kind": v.kind, "key": v.key, "recorded": v.recorded, "actual": v.actual} for v in e.value_diffs]
        if e.klass != "pass" and e.actual_body is not None:
            d["actual_body"] = e.actual_body
        if e.error:
            d["error"] = e.error
        return d

    return {
        "summary": agg["summary"],
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "elapsed_seconds": round(elapsed, 2),
        "arguments": args_dict,
        "groups": [
            {"key": g.key, "method": g.method, "pattern": g.pattern, "expected_status": g.expected_status,
             "actual_status": g.actual_status, "expected_error_code": g.expected_code,
             "actual_error_code": g.actual_code, "count": g.count,
             "first_failure_in_recordings": g.first_in_recording, "files": g.files}
            for g in agg["groups"]
        ],
        "value_groups": [
            {"pattern": g.pattern, "status": g.status, "kind": g.kind, "key": g.key, "count": g.count,
             "recorded": g.example[0] if g.example else None, "actual": g.example[1] if g.example else None,
             "example": f"{g.example[2]}#{g.example[3]}" if g.example else None, "files": g.files}
            for g in agg["value_groups"]
        ],
        "soft_missing_headers": [
            {"pattern": p, "status": st, "header": h, "count": n,
             "example": agg["soft_examples"].get(("header-missing", (p, st, h)))}
            for (p, st, h), n in agg["soft_header"].most_common()
        ],
        "soft_shape_differences": [
            {"pattern": p, "status": st, "kind": k, "path": path, "count": n,
             "example": agg["soft_examples"].get((k, (p, st, k, path)))}
            for (p, st, k, path), n in agg["soft_shape"].most_common()
        ],
        "soft_other": [
            {"pattern": p, "status": st, "kind": k, "detail": d, "count": n}
            for (p, st, k, d), n in agg["soft_other"].most_common()
        ],
        "packages": {k: dict(v) for k, v in agg["per_package"].items()},
        "recordings": [
            {"file": r.name, "package": r.package, "result": r.status, "result_adjusted": r.status_adjusted,
             "error": r.error, "entries": [entry_json(e) for e in r.entries]}
            for r in results
        ],
    }


def format_dry_run(results: list[RecordingResult]) -> str:
    per: dict[str, Counter] = defaultdict(Counter)
    problems: dict[str, list] = defaultdict(list)
    skip_hosts: dict[str, Counter] = defaultdict(Counter)
    for r in results:
        c = per[r.package]
        c["files"] += 1
        if r.status == "error":
            c["problems"] += 1
            problems[r.package].append(f"{r.name}: {r.error}")
        c["entries"] += len(r.entries) + len(r.skipped)
        c["skipped"] += len(r.skipped)
        c["built"] += sum(1 for e in r.entries if not e.skipped)
        for sk in r.skipped:
            skip_hosts[r.package][sk.reason] += 1
        for e in r.entries:
            if e.klass in (CLASS_AUTH, CLASS_COPY, CLASS_THROTTLED):
                c[e.klass] += 1
            for kind, _d in e.soft:
                if kind == "recorded-body-unparseable":
                    c["recorded_body_unparseable"] += 1
                elif kind == "copy-source-rewritten":
                    c["copy_source_rewritten"] += 1
    lines = [f"{'package':36s} {'files':>6s} {'entries':>8s} {'built':>7s} {'skipped':>8s} {'parse problems':>15s}"]
    tot: Counter = Counter()
    for name in sorted(per):
        c = per[name]
        tot.update(c)
        lines.append(f"{name:36s} {c['files']:6d} {c['entries']:8d} {c['built']:7d} {c['skipped']:8d} {c['problems']:15d}")
    lines.append(f"{'TOTAL':36s} {tot['files']:6d} {tot['entries']:8d} {tot['built']:7d} {tot['skipped']:8d} {tot['problems']:15d}")
    for name in sorted(per):
        if skip_hosts[name]:
            lines.append(f"\nskipped hosts in {name}:")
            for reason, n in skip_hosts[name].most_common(8):
                lines.append(f"    {n:6d}  {reason}")
        for p in problems[name][:10]:
            lines.append(f"  PROBLEM {p}")
    lines.append("")
    lines.append("would not be sent (classification on): auth-dependent / sanitized-copy-source / "
                 "throttled; copy sources rewritten:")
    for name in sorted(per):
        c = per[name]
        lines.append(f"  {name:36s} {c[CLASS_AUTH]:6d} {c[CLASS_COPY]:6d} {c[CLASS_THROTTLED]:6d} "
                     f"{c['copy_source_rewritten']:6d}")
    ub = tot["recorded_body_unparseable"]
    if ub:
        lines.append(f"\nrecorded bodies that look like JSON/XML but do not parse: {ub}")
    return "\n".join(lines)


def parse_auth_overrides(values: Optional[list[str]]) -> dict[str, str]:
    auth = dict(DEFAULT_AUTH)
    for v in values or []:
        if v.strip().lower() in ("none", ""):
            auth = {}
            continue
        m = re.match(r"^([a-z\-]+)=(.*)$", v, re.S)
        if m and m.group(1) in set(LABEL_ALIASES.values()):
            auth[m.group(1)] = m.group(2)
            if m.group(2).lower() == "none":
                del auth[m.group(1)]
        else:
            auth = {lab: v for lab in set(LABEL_ALIASES.values())}
    return auth


def build_arg_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="Replay Azure SDK recordings against an azemu emulator and compare with real Azure.")
    ap.add_argument("--recordings", required=True, action="append",
                    help="directory (searched recursively) or file; repeatable")
    ap.add_argument("--gateway", default="http://127.0.0.1:4577", help="emulator address")
    ap.add_argument("--label", help="force the host label instead of deriving it from the recorded host")
    ap.add_argument("--filter", action="append", default=[], metavar="SUBSTR",
                    help="only recording files whose name contains this (repeatable)")
    ap.add_argument("--exclude", action="append", default=[], metavar="SUBSTR",
                    help="skip files whose name contains this (repeatable)")
    ap.add_argument("--limit", type=int, help="replay at most N recordings")
    ap.add_argument("--report", help="write a JSON report here")
    ap.add_argument("--show", type=int, default=25, help="failure groups to print (default 25)")
    ap.add_argument("--show-soft", type=int, default=15, help="soft difference groups to print per table")
    ap.add_argument("--verbose", action="store_true", help="print every mismatching entry")
    ap.add_argument("--no-reset", action="store_true", help="do not POST /_azemu/reset before each recording")
    ap.add_argument("--default-auth", action="append", metavar="VALUE",
                    help='Authorization to add when the recording has none; {account} is filled in. '
                         'Use "label=value" to set one label, "none" to disable. '
                         'Defaults: storage SharedKey, azconfig HMAC, vault Bearer')
    ap.add_argument("--timeout", type=float, default=30.0, help="per request seconds (default 30)")
    ap.add_argument("--strict", action="store_true", help="exit non-zero if any hard failure or error")
    ap.add_argument("--dry-run", action="store_true", help="parse and build every request but send nothing")
    ap.add_argument("--no-time-shift", action="store_true",
                    help="do not shift If-(Un)Modified-Since dates by the emulator's clock offset")
    ap.add_argument("--no-classify", action="store_true",
                    help="turn every classifier off (no unreplayable / ahead-async classes; raw behaviour)")
    ap.add_argument("--quiet", action="store_true", help="no progress on stderr")
    return ap


def main(argv: Optional[list[str]] = None) -> int:
    args = build_arg_parser().parse_args(argv)
    gw = urlsplit(args.gateway)
    settings = Settings(
        gateway=args.gateway, gateway_host=gw.hostname or "127.0.0.1",
        gateway_port=gw.port or (443 if gw.scheme == "https" else 80),
        force_label=args.label, auth_defaults=parse_auth_overrides(args.default_auth),
        timeout=args.timeout, reset=not args.no_reset, dry_run=args.dry_run,
        time_shift=not args.no_time_shift,
        classify=not args.no_classify, gateway_scheme=gw.scheme or "http",
    )
    files = find_recordings(args.recordings)
    if args.filter:
        files = [f for f in files if any(s in os.path.basename(f[0]) for s in args.filter)]
    if args.exclude:
        files = [f for f in files if not any(s in os.path.basename(f[0]) for s in args.exclude)]
    if args.limit is not None:
        files = files[: args.limit]
    if not files:
        print("no recordings matched", file=sys.stderr)
        return 2

    client = None if args.dry_run else Client(args.gateway, args.timeout)
    results: list[RecordingResult] = []
    t0 = time.time()
    for i, (name, path) in enumerate(files, 1):
        r = run_recording(name, path, settings, client)
        results.append(r)
        if args.verbose:
            if r.status == "error":
                print(f"ERROR {name}: {r.error}")
            for e in r.entries:
                if e.failed:
                    codes = f" ({e.expected_code or ''}→{e.actual_code or ''})" if (e.expected_code or e.actual_code) else ""
                    print(f"FAIL {name}#{e.index} {e.method} {e.url} "
                          f"{e.expected_status}→{e.actual_status if e.actual_status is not None else 'ERR'}{codes} "
                          f"{[h[0] for h in e.hard]}")
        if not args.quiet and i % 250 == 0:
            print(f"  ... {i}/{len(files)} recordings ({time.time() - t0:.0f}s)", file=sys.stderr)
    if client:
        client.close()
    elapsed = time.time() - t0

    if args.dry_run:
        print(f"== Dry run: {len(results)} files, nothing sent ({elapsed:.1f}s) ==")
        print(format_dry_run(results))
        return 1 if (args.strict and any(r.status == "error" for r in results)) else 0

    agg = aggregate(results)
    print(format_summary(agg, args.show, args.show_soft))
    print(f"\n({len(results)} recordings in {elapsed:.1f}s)")
    if args.report:
        os.makedirs(os.path.dirname(os.path.abspath(args.report)), exist_ok=True)
        report = build_report(agg, results, {k: v for k, v in vars(args).items()}, elapsed)
        with open(args.report, "w", encoding="utf-8") as fh:
            json.dump(report, fh, indent=1, ensure_ascii=False)
        print(f"report written to {args.report}")
    sm = agg["summary"]
    bad = (sm["recordings_total"] - sm["recordings_passing_adjusted"]) if not args.no_classify \
        else (sm["recordings_with_failures"] or sm["recordings_errored"])
    return 1 if (args.strict and bad) else 0


if __name__ == "__main__":
    sys.exit(main())
