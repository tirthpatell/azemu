"""Comparing the VALUES of an answer with the recorded one.

The hard checks of `replay.py` look at the status and the error code. This module looks at the rest: the value of every
response header and of every scalar in the body (strings, numbers, booleans) that exists on both sides, and the first
line of every error message. A value the server makes up (a date, an id, an ETag, a token) is left out, because the
emulator can never answer with the same one; each such rule is listed here with its reason, and in `README.md` (a test
checks that the two lists agree). The rules are narrow on purpose: a rule that excuses too much hides exactly the
differences this check is for.

Nothing here changes the score of an entry. A value that differs is counted and grouped (`replay.aggregate`).
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any, Optional

# ---------------------------------------------------------------------------
# Headers
# ---------------------------------------------------------------------------

#: Response headers whose value is not compared: name -> (group, reason). Lower case names.
#: The groups are the headings of the table in README.md.
HEADER_EXCUSES: dict[str, tuple[str, str]] = {}


def _headers(group: str, reason: str, *names: str) -> None:
    for n in names:
        HEADER_EXCUSES[n] = (group, reason)


_headers(
    "made up per request", "the server makes it up for each request; no two answers have the same",
    "date", "x-ms-request-id", "x-ms-client-request-id", "x-ms-correlation-request-id", "x-ms-routing-request-id",
    "client-request-id", "x-ms-date", "sync-token", "dataserviceid",
)
_headers(
    "identity of a stored thing", "an ETag, a handle, a hash or an id the server makes up when it stores something",
    "etag", "last-modified", "x-ms-version-id", "x-ms-snapshot", "x-ms-popreceipt", "x-ms-lease-id", "x-ms-copy-id",
    "x-ms-continuation-nextpartitionkey", "x-ms-continuation-nextrowkey", "x-ms-continuation-nexttablename",
    "content-md5", "x-ms-content-crc64", "x-ms-blob-content-md5", "x-ms-encryption-key-sha256", "x-ms-or-policy-id",
    "x-ms-deletion-id", "preference-applied",
)
_headers(
    "clock", "a point in time or a wait that depends on when the request ran",
    "x-ms-creation-time", "x-ms-copy-completion-time", "x-ms-access-tier-change-time", "x-ms-last-access-time",
    "x-ms-expiry-time", "x-ms-immutability-policy-until-date", "x-ms-time-next-visible", "retry-after",
)
_headers(
    "address", "it holds the address of the server, which differs by design",
    "location", "operation-location", "link", "x-ms-copy-source", "www-authenticate",
)
_headers(
    "transport and edge", "set by the HTTP stack or by Microsoft's front end, not by the service logic",
    "server", "vary", "transfer-encoding", "strict-transport-security", "x-content-type-options", "cache-control",
    "pragma", "expires", "content-length", "accept-ranges", "access-control-allow-origin", "x-ms-version",
    "x-xss-protection", "x-frame-options", "x-ms-ratelimit-remaining-subscription-reads",
    "x-ms-ratelimit-remaining-subscription-writes", "x-ms-keyvault-network-info", "x-ms-keyvault-region",
    "x-ms-keyvault-service-version",
)
_headers(
    "settings of Microsoft's test accounts", "the recording comes from an account whose encryption settings the emulator does not model",
    "x-ms-request-server-encrypted", "x-ms-server-encrypted", "x-ms-default-encryption-scope",
    "x-ms-deny-encryption-scope-override", "x-ms-encryption-scope", "x-ms-namespace-enabled", "x-ms-sku-name",
)
_headers(
    "contents of Microsoft's test accounts", "it counts what the recorded account holds, including what other tests left in it",
    "x-ms-item-count",
)
_headers(
    "replay limits", "depends on the size of a blob, and blobs replayed from sanitized block ids are smaller than the recorded ones (README, known limits)",
    "content-range", "x-ms-range", "x-ms-content-length", "x-ms-blob-sequence-number",
)

#: Header prefixes that are left out, with the reason.
HEADER_EXCUSE_PREFIXES: dict[str, tuple[str, str]] = {
    "x-ms-keyvault-": ("transport and edge", "set by Microsoft's front end, not by the service logic"),
    "x-ms-ests-": ("transport and edge", "set by Microsoft's sign-in service"),
    "access-control-": ("transport and edge", "CORS headers answer for the origin of the test run"),
    "x-ms-ratelimit-": ("transport and edge", "counters of Microsoft's throttling"),
}


_BOUNDARY = re.compile(r"boundary=\S+")


def normalize_header(name: str, value: str) -> str:
    """The value of a header as it is compared: a multipart `boundary=` is random text, so it is left out
    (`multipart/mixed; boundary=batchresponse_<guid>`)."""
    if name == "content-type" and "multipart/" in value.lower():
        return _BOUNDARY.sub("boundary=*", value)
    return value


def header_excused(name: str) -> Optional[str]:
    """The group that excuses this response header, or None."""
    lname = name.lower()
    hit = HEADER_EXCUSES.get(lname)
    if hit:
        return hit[0]
    for p, (g, _r) in HEADER_EXCUSE_PREFIXES.items():
        if lname.startswith(p):
            return g
    return None


# ---------------------------------------------------------------------------
# Values
# ---------------------------------------------------------------------------

_ISO = re.compile(r"^\d{4}-\d\d-\d\dT")
_HTTP_DATE = re.compile(r"^[A-Z][a-z]{2}, \d\d [A-Z][a-z]{2} \d{4}")
_GUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-", re.I)
_ETAG = re.compile(r'^(?:"?0x[0-9A-F]{10,}|W/")', re.I)
_BIG_NUMBER = re.compile(r"^\d{9,}$")
_TOKEN = re.compile(r"^[A-Za-z0-9+/=_-]{24,}$")
_VERSION = re.compile(r"^\d+\.\d+\.\d+(?:\.\d+)?$")
_REQUEST_ID_TEXT = re.compile(r"RequestId:")
_ODATA_DATETIME = re.compile(r"^datetime'")
_EPOCH_FRACTION = re.compile(r"^\d{10}\.\d+$")
_PLACEHOLDER = re.compile(r"^(?:0{8}-0{4}-0{4}-0{4}-0{12}|(.)\1+|sanitized.*|redacted.*|fake[_-]?.*|MA==)$", re.I)
_URL = re.compile(r"[A-Za-z][A-Za-z0-9+.\-]*://[^/?#\s\"'<>]*")

#: The rules for a value (body scalar), in the order they are tried: (id, reason). The code of each is in `generated`.
VALUE_EXCUSES: list[tuple[str, str]] = [
    ("sanitized", "the recording's sanitizer replaced the real value (`Sanitized`, `fake...`, a run of one character, the all-zero GUID, `MA==`: the base64 of `0`, which stands for a user delegation key)"),
    ("account name", "the recorded value contains the account name, which the sanitizer replaced and the replay chose again"),
    ("date", "an ISO 8601 date and time (`2024-05-01T...`) or an HTTP date: the server's clock"),
    ("guid", "starts like a GUID: an id the server makes up"),
    ("etag", "an ETag in the forms Azure writes (`0x8D...`, `W/\"...\"`)"),
    ("big number", "a whole number of nine or more digits: a time stamp, a sequence number or an id"),
    ("token", "24 or more characters of letters, digits and `+/=_-` with a digit in them, no spaces: a key, a handle, a hash or base64 (a certificate thumbprint, `x5t`, needs no digit)"),
    ("version", "dotted digits (`1.2.3`, `1.2.3.4`): a build or service version"),
    ("request id text", "text that holds `RequestId:` (an Azure error text with the request id and a time)"),
    ("odata datetime", "an OData date literal (`datetime'...'`)"),
    ("epoch fraction", "seconds since 1970 with a fraction (ten digits, a dot, digits)"),
    ("answers with the host's account", "the emulator's value is the account (namespace, vault, store) of the request's host, which the sanitizer replaced in the recording: the recorded value is the real name"),
    ("connection string", "a connection string (`Endpoint=sb://...`, `DefaultEndpointsProtocol=...`, `...SharedAccessKey=...`): it holds the server's address and a key"),
    ("named like its member", "the value is the name of its own member (`\"primaryKey\": \"primaryKey\"`): the sanitizer wrote the member name over a secret"),
    ("long", "more than 300 characters: a certificate, a key, a backup or a template"),
    ("retired wording", "a recorded value that older recordings carry and newer recordings of the same call do not: the service stopped writing it, so the emulator "
     "(which answers as the service does now) cannot be asked to. Only the exact values of `RETIRED`: the 401 text `... a Bearer or PoP token.` (every "
     "Key Vault recording from before March 2026; from then on every api-version says `... a Bearer, PoP, or MTLS_POP token.`) and the RSA-HSM public "
     "exponent `AAEAAQ` (recordings of 2022 and 2023; since 2024 `AQAB`), the text `Pending certificate not found: ...` (since 2025 the C is "
     "a capital) and `hsmPlatform` `1` of an HSM key (2024; since 2025 `2`)"),
    ("nearly never", "a duration within a second below .NET's `TimeSpan.MaxValue` (`P10675199DT2H48M5.477539S`; the maximum itself, `...4775807S`, is compared, which is what the Python SDK writes back for the maximum "
     "it read as `P10675199DT2H48M5.4775807S`): Azure keeps the exact tick count the client sent; the emulator keeps milliseconds and answers any such value as the "
     "maximum"),
    ("work in progress", "a state that Azure was still working towards when it answered, and that the emulator finishes at once (by design: the SDK pollers must not wait): "
     "`Running`, `inProgress` or `cancelled` in a `status`, `Creating`, `Created`, `RegisteringDns`, `Updating`, `Deleting` or `Activating` in a `provisioningState`, "
     "`Activating` in the `status` of a Service Bus namespace, and the Key Vault text `Deleted Secret not found: ...` (also Key, Certificate): Azure answers it while a delete or purge is still being finished, and the long text `A secret with (name/id) ... was not found in this key vault` afterwards, which is what the emulator answers"),
    ("continuation marker", "the value of a `NextMarker` or `Marker` element: an opaque text the server makes up for the next page (Azure writes `2!68!MDAw...`), "
     "which no client reads"),
    ("vault retention", "the recorded vault keeps deleted things for another number of days than Azure's default of 90: `recoverableDays` other than 90, and "
     "the `recoveryLevel` that goes with it (`CustomizedRecoverable...`). Microsoft's test vault was set to 7 days; a new emulated vault has Azure's default"),
]
#: (end of the path, recorded value) pairs of the rule `retired wording`.
RETIRED = {
    ("/error/message", "AKV10000: Request is missing a Bearer or PoP token."),
    ("/error/message", "Pending certificate not found: "),
    ("/key/e", "AAEAAQ"),
    ("/attributes/hsmPlatform", "1"),
}
VALUE_EXCUSE_IDS = [i for i, _ in VALUE_EXCUSES]


#: Rules about where a value stands rather than what it is: (id, reason).
STRUCTURE_RULES: list[tuple[str, str]] = [
    ("lists of different length", "the values under a list whose two sides have a different number of elements are not compared: the elements are matched by position, "
     "and two lists of another length hold other things (the recorded account had resources the replay never made), so the pairs are not the same thing"),
    ("lists matched by id", "a list whose elements all have an `id` or `kid` of their own, different from each other, is compared element by element by that id "
     "and not by position; an element that only one side has is left out. A Key Vault list holds the vault's other secrets, keys and certificates too, "
     "in the order Azure keeps them, so the n-th element of the two answers is not the same thing"),
    ("settings the recording never set", "the parts of an answer that only report the account's configuration are not compared in a recording that did not set that part itself "
     "with an earlier Set Service Properties: Get Service Properties of Blob, Queue, Table and Data Lake part by part (logging, metrics, CORS, static website, "
     "delete retention; Set Service Properties changes the parts it names and leaves the others), and Get Service Stats. They are the recorded account's, "
     "which the emulator does not have"),
    ("tier the recording never set", "`x-ms-access-tier` and `AccessTier` are not compared in a recording in which no request names an access tier: the tier of a blob "
     "then is the recorded account's default (`Cool`, or `Smart`), which the emulator does not model"),
    ("delete type of an account the recording did not set up", "`x-ms-delete-type-permanent` of a delete is not compared in a recording that did not turn soft delete on itself, "
     "unless the recording shows versioning (an `x-ms-version-id` in some answer) and the delete is a plain delete of the current blob (no `versionid`, "
     "`snapshot` or `x-ms-delete-snapshots`): whether anything stays recoverable is decided by the recorded account's soft delete and versioning, "
     "and the replay runs every blob recording with versioning on"),
]

_INDEX = re.compile(r"\[(\d+)\]")


def list_lengths(leaves: dict) -> dict:
    """For every list that holds a scalar somewhere below it: the path up to the list -> the number of elements seen."""
    out: dict[str, int] = {}
    for path in leaves:
        for m in _INDEX.finditer(path):
            prefix = path[: m.start()]
            out[prefix] = max(out.get(prefix, 0), int(m.group(1)) + 1)
    return out


_ID_LEAF = re.compile(r"^(.*)\[(\d+)\]/(?:id|kid)(?:\[0\])?$")
_ELEMENT = re.compile(r"^(.*?)\[(\d+)\](.*)$")


def _identity(v: Any) -> str:
    return normalize_urls(str(v)).strip().lower()


def align_lists_by_id(rec: dict, act: dict) -> tuple[dict, dict]:
    """Rule `lists matched by id`: where both sides hold a list whose every element has an `id` or `kid` and the ids are all
    different, renumber the elements by id (the elements both sides have, in the order of their ids) and drop the others."""
    done: set[str] = set()
    for _round in range(8):
        prefixes = set()
        for leaves in (rec, act):
            for path in leaves:
                m = _ID_LEAF.match(path)
                if m and m.group(1) not in done:
                    prefixes.add(m.group(1))
        if not prefixes:
            break
        prefix = min(prefixes, key=lambda x: (len(x), x))
        done.add(prefix)
        sides = []
        for leaves in (rec, act):
            ids: dict[str, int] = {}
            ok = True
            elements = {int(m.group(1)) for p in leaves if (m := re.match(re.escape(prefix) + r"\[(\d+)\]", p))}
            for path, v in leaves.items():
                m = _ID_LEAF.match(path)
                if m and m.group(1) == prefix:
                    key = _identity(v)
                    if key in ids:
                        ok = False
                    ids[key] = int(m.group(2))
            if not ok or not elements or len(ids) != len(elements):
                sides = []
                break
            sides.append(ids)
        if len(sides) != 2:
            continue
        common = sorted(set(sides[0]) & set(sides[1]))
        new_index = {k: i for i, k in enumerate(common)}
        out = []
        for leaves, ids in zip((rec, act), sides):
            old_to_new = {idx: new_index[k] for k, idx in ids.items() if k in new_index}
            renumbered = {}
            for path, v in leaves.items():
                if path.startswith(prefix + "["):
                    m = re.match(re.escape(prefix) + r"\[(\d+)\](.*)$", path, re.S)
                    if m and int(m.group(1)) in old_to_new:
                        renumbered[f"{prefix}[{old_to_new[int(m.group(1))]}]{m.group(2)}"] = v
                    continue
                renumbered[path] = v
            out.append(renumbered)
        rec, act = out
    return rec, act


_SANITIZED_ACCOUNTS = re.compile(r"faketableaccount|storagename|vaultname|fake_app_config|fakeendpoint|fakeresource|sanitized", re.I)
_UPPER_HOST = re.compile(r"\b[A-Z0-9-]+(?:\.[A-Z0-9-]+)*\.(?:AZURE\.NET|LOCALHOST)(?::\d+)?")
_HEX32 = re.compile(r"\b[0-9a-fA-F]{32}\b")
_GUID_TEXT = re.compile(r"(?<![0-9a-fA-F])[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}(?![0-9a-fA-F])")
_SB_NAMESPACE = re.compile(r"'[\w.-]+:(?=(?:Topic|Queue|Subscription|Rule):?)")
_TIMESTAMP = re.compile(r"Timestamp:\S+")


def normalize_message(s: str, account: str) -> str:
    """The first line of an error message as it is compared: addresses lose their host (`normalize_urls`), the name of the account
    (or the sanitizer's word for it, also the namespace in front of `:Topic:`), an id of 32 hex digits (a version), a GUID (tracking ids), a `Timestamp:` and an upper-case host name (Event Grid names the topic's address in
    capitals) become placeholders."""
    s = _UPPER_HOST.sub("{host}", normalize_urls(re.split(r"[\r\n]", s.strip())[0]))
    s = _SANITIZED_ACCOUNTS.sub("{account}", s)
    if len(account) >= 4:
        s = re.sub(re.escape(account), "{account}", s, flags=re.I)
    s = _TIMESTAMP.sub("Timestamp:{time}", s)
    s = _SB_NAMESPACE.sub("'{account}:", s)
    return re.sub(r"\{guid\}_G\d+", "{guid}", _GUID_TEXT.sub("{guid}", _HEX32.sub("{id}", s)))


def _under_different_lists(path: str, rec_len: dict, act_len: dict) -> bool:
    for m in _INDEX.finditer(path):
        prefix = path[: m.start()]
        if rec_len.get(prefix) != act_len.get(prefix):
            return True
    return False


def _has_digit(s: str) -> bool:
    return any(c.isdigit() for c in s)


_CONNECTION_STRING = re.compile(r"(?:^Endpoint=\w+://|^DefaultEndpointsProtocol=|SharedAccessKey=|AccountKey=)")


def generated(v: Any, account: str = "", path: str = "") -> Optional[str]:
    """The id of the rule that excuses this value, or None. `v` is the RECORDED value; the emulator's is not looked at (what
    cannot be compared is a recorded value that cannot be reproduced). A rule that looked at both would also excuse a real
    difference, such as a plain word where the emulator answers with an id."""
    if isinstance(v, bool) or v is None:
        return None
    if isinstance(v, str) and ((path.endswith("/status") and v in ("Running", "inProgress", "cancelled", "Activating"))
                               or (path.endswith("/provisioningState") and v in ("Creating", "Created", "RegisteringDns", "Updating", "Deleting", "Activating"))):
        return "work in progress"
    if isinstance(v, str) and re.match(r"Deleted (?:Secret|Key|Certificate) not found: ", v):
        return "work in progress"
    if isinstance(v, str) and re.fullmatch(r"P10675199DT2H48M5\.(?!4775807S)\d+S", v):
        return "nearly never"
    if isinstance(v, str) and re.search(r"/(?:Next)?Marker(?:\[\d+\])?$", path):
        return "continuation marker"
    if isinstance(v, str) and any(path.endswith(p) and v.strip().startswith(t) and (len(t) > 1 or v.strip() == t) for p, t in RETIRED):
        return "retired wording"
    if path.endswith("/attributes/recoverableDays") and isinstance(v, (int, float)) and v != 90:
        return "vault retention"
    if path.endswith("/attributes/recoveryLevel") and isinstance(v, str) and v.startswith("Customized"):
        return "vault retention"
    if isinstance(v, int):
        # an int can be thousands of digits long (one recording has 5,125): it must not go through a float
        return "big number" if abs(v) >= 10**9 else None
    if isinstance(v, float):
        if abs(v) >= 1e9:
            return "big number" if v.is_integer() else "epoch fraction"
        return None
    s = str(v)
    if _PLACEHOLDER.match(s):
        return "sanitized"
    if path and s.lower() == re.split(r"[/\[]", path.rstrip("]"))[-1].lower() and len(s) >= 4:
        return "named like its member"
    if _CONNECTION_STRING.search(s):
        return "connection string"
    if len(account) >= 4 and account.lower() in s.lower():
        return "account name"
    if _ISO.search(s) or _HTTP_DATE.search(s):
        return "date"
    if _GUID.search(s):
        return "guid"
    if _ETAG.search(s):
        return "etag"
    if _BIG_NUMBER.match(s):
        return "big number"
    if _EPOCH_FRACTION.match(s):
        return "epoch fraction"
    if _TOKEN.match(s) and (_has_digit(s) or path.endswith(("/x5t", "/x5t#S256"))):
        return "token"
    if _VERSION.match(s):
        return "version"
    if _REQUEST_ID_TEXT.search(s):
        return "request id text"
    if _ODATA_DATETIME.match(s):
        return "odata datetime"
    if len(s) > 300:
        return "long"
    return None


def normalize_urls(s: str) -> str:
    """Scheme and host of every address in `s` become `URL`, and the query string goes: the host is the emulator's by design
    and the query holds continuation tokens and signatures. A path segment of 32 hex digits or a GUID (a version or an id the
    server makes up) becomes `{id}`. What is left of the address is compared."""
    def cut(m: "re.Match[str]") -> str:
        return "URL"
    out = _URL.sub(cut, s)
    # a version or an id the server made up (32 hex digits, or a GUID) inside the path
    out = re.sub(r"/(?:[0-9a-fA-F]{32}|[0-9a-fA-F]{8}-(?:[0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12})(?=[/?#\s\"']|$)", "/{id}", out)
    # drop `?query` of an address (up to a space or a quote)
    return re.sub(r"(URL[^?\s\"']*)\?[^\s\"']*", r"\1", out)


def first_line(s: str) -> str:
    return re.split(r"[\r\n]", s.strip())[0][:140]


def _is_message_path(path: str) -> bool:
    return path.lower().endswith(("message", "/message/value", "message/value")) or bool(re.search(r"/Error/Detail(?:\[\d+\])?$", path))


@dataclass
class ValueDiff:
    #: `header`, `body` or `text` (the first line of an error message)
    kind: str
    #: the header name or the path of the value in the body
    key: str
    recorded: str
    actual: str


def _same(rv: Any, av: Any) -> bool:
    if isinstance(rv, bool) or isinstance(av, bool):
        return isinstance(rv, bool) and isinstance(av, bool) and rv == av
    if isinstance(rv, (int, float)) and isinstance(av, (int, float)):
        return rv == av
    if isinstance(rv, str) and isinstance(av, str):
        if rv == av:
            return True
        if "://" in rv or "://" in av:
            return normalize_urls(rv) == normalize_urls(av)
        return False
    return False


def _show(v: Any) -> str:
    return v if isinstance(v, str) else repr(v)


def compare_values(account: str, rec_headers: dict, act_headers: dict, rec_leaves: dict, act_leaves: dict,
                   skip_headers: frozenset = frozenset(), skip_paths: tuple = (),
                   skip_endings: tuple = ()) -> tuple[int, list[ValueDiff]]:
    """-> (values compared, the ones that differ).

    `rec_headers` and `act_headers` map lower-case names to text. `rec_leaves` and `act_leaves` map the path of every scalar of
    the body to its value (strings, numbers, booleans; see `replay.value_leaves`). Only what exists on both sides is compared:
    a missing header or key is the soft check's business."""
    compared = 0
    diffs: list[ValueDiff] = []
    for name, rv in rec_headers.items():
        if name not in act_headers or header_excused(name) or name.startswith("x-ms-error-code") or name in skip_headers:
            continue
        av = act_headers[name]
        if generated(rv, account):
            continue
        compared += 1
        rn, an = normalize_header(name, rv), normalize_header(name, av)
        if rn.strip().lower() != an.strip().lower() and not _same(rn, an):
            diffs.append(ValueDiff("header", name, rv[:200], av[:200]))
    rec_leaves, act_leaves = align_lists_by_id(rec_leaves, act_leaves)
    rec_len, act_len = list_lengths(rec_leaves), list_lengths(act_leaves)
    for path, rv in rec_leaves.items():
        if path not in act_leaves or _under_different_lists(path, rec_len, act_len) or path.startswith(skip_paths) or re.sub(r"\[\d+\]$", "", path).endswith(skip_endings):
            continue
        av = act_leaves[path]
        if _is_message_path(path) and isinstance(rv, str) and isinstance(av, str):
            if generated(first_line(rv), account, path) in ("retired wording", "work in progress"):
                continue
            compared += 1
            if normalize_message(rv, account) != normalize_message(av, account):
                diffs.append(ValueDiff("text", path, first_line(rv), first_line(av)))
            continue
        if generated(rv, account, path):
            continue
        if account and isinstance(av, str) and av.lower() == account.lower() and rv != av:
            continue
        compared += 1
        if not _same(rv, av):
            diffs.append(ValueDiff("body", path, _show(rv)[:200], _show(av)[:200]))
    return compared, diffs
