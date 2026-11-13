"""Helpers shared by the Data Lake tests (not a test module)."""

import base64
import json
import os
import urllib.parse
import uuid
from datetime import datetime, timedelta, timezone

import requests
from azure.core import MatchConditions  # noqa: F401  (re-exported for tests)
from azure.core.exceptions import (  # noqa: F401
    HttpResponseError,
    ResourceExistsError,
    ResourceModifiedError,
    ResourceNotFoundError,
    ResourceNotModifiedError,
)
from azure.storage.filedatalake import (
    AccountSasPermissions,
    ResourceTypes,
    generate_account_sas,
)

DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = (
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/"
    "K1SZFPTOtr/KBHBeksoGMGw=="
)

# Default permissions of a new path on a hierarchical-namespace account
# (recorded: umask 0027 applied to 0777 for directories, 0666 for files).
DEFAULT_DIR_PERMISSIONS = "rwxr-x---"
DEFAULT_FILE_PERMISSIONS = "rw-r-----"
DEFAULT_DIR_ACL = "user::rwx,group::r-x,other::---"
DEFAULT_FILE_ACL = "user::rw-,group::r--,other::---"
SUPERUSER = "$superuser"


# -- names and data ----------------------------------------------------------
def uniq(prefix="t"):
    """Unique, container-name-safe (lowercase letters and digits) name."""
    return f"{prefix}{uuid.uuid4().hex[:12]}"


def rand_bytes(n):
    return os.urandom(n)


def now():
    return datetime.now(timezone.utc)


def future(minutes=30):
    return now() + timedelta(minutes=minutes)


def past(minutes=30):
    return now() - timedelta(minutes=minutes)


# -- error assertions --------------------------------------------------------
def code_of(err):
    code = getattr(err, "error_code", None)
    return code.value if hasattr(code, "value") else (None if code is None else str(code))


def assert_error(excinfo, code, status):
    """Check both the Azure error code and the HTTP status of an SDK exception."""
    err = excinfo.value
    assert code_of(err) == code, f"error_code {code_of(err)!r} != {code!r} ({err})"
    assert err.status_code == status, f"status {err.status_code} != {status} ({err})"


def assert_error_in(excinfo, codes, status):
    """Like assert_error, for the few cases where Azure docs allow more than one code."""
    err = excinfo.value
    assert code_of(err) in codes, f"error_code {code_of(err)!r} not in {codes} ({err})"
    assert err.status_code == status, f"status {err.status_code} != {status} ({err})"


def assert_status(excinfo, status):
    assert excinfo.value.status_code == status, f"status {excinfo.value.status_code} != {status}"


# -- building trees ----------------------------------------------------------
def put_file(fs, path, data=b"", **create_kwargs):
    """Create `path` in filesystem client `fs` with `data` (append + flush). Returns the file client."""
    f = fs.get_file_client(path)
    f.create_file(**create_kwargs)
    if data:
        f.append_data(data, 0, len(data))
        f.flush_data(len(data))
    return f


def read_file(client):
    return client.download_file().readall()


def tree(fs, path=None, recursive=True):
    """{name: is_directory} for get_paths."""
    return {p.name: p.is_directory for p in fs.get_paths(path=path, recursive=recursive)}


def names(items):
    return {p.name for p in items}


def acl_set(acl):
    """ACL string -> set of entries (order of entries is not significant)."""
    return set(acl.split(",")) if acl else set()


def sas_with_signature(token, change):
    """Return the SAS query string with `change(sig)` as signature (sig is the decoded base64 text)."""
    pairs = urllib.parse.parse_qsl(token, keep_blank_values=True)
    return urllib.parse.urlencode([(k, change(v) if k == "sig" else v) for k, v in pairs])


def tamper_sas(token):
    """Change the first character of the signature (always yields a different, still well-formed value)."""
    return sas_with_signature(token, lambda sig: ("B" if sig.startswith("A") else "A") + sig[1:])


def month_header(dt):
    return dt.strftime("%a, %d %b %Y %H:%M:%S GMT")


def b64(s: str) -> str:
    return base64.b64encode(s.encode()).decode()


# -- raw HTTP ----------------------------------------------------------------
class Raw:
    """Plain `requests` calls against the emulator, authenticated with an account SAS.

    The SDK hides headers, JSON bodies and some status codes; these calls let
    a test see exactly what the service sent.
    """

    def __init__(self, emu, account=DEV_ACCOUNT, key=DEV_KEY):
        self.emu = emu
        self.account = account
        self.key = key
        self.sas = generate_account_sas(
            account,
            key,
            ResourceTypes(service=True, file_system=True, object=True),
            AccountSasPermissions(read=True, write=True, delete=True, list=True, create=True),
            expiry=datetime.now(timezone.utc) + timedelta(hours=1),
        )

    def url(self, label, path="", query=None):
        base = self.emu.url(label, self.account)
        q = dict(query or {})
        qs = urllib.parse.urlencode(q, quote_via=urllib.parse.quote)
        qs = (qs + "&" if qs else "") + self.sas
        return f"{base}/{path.lstrip('/')}?{qs}" if path else f"{base}/?{qs}"

    def dfs(self, method, path="", query=None, headers=None, data=None, label="dfs"):
        return requests.request(
            method, self.url(label, path, query), headers=headers or {}, data=data, timeout=30
        )

    def blob(self, method, path="", query=None, headers=None, data=None):
        return self.dfs(method, path, query, headers, data, label="blob")


def props_header(**kv):
    """Build the x-ms-properties header value (base64 values)."""
    return ",".join(f"{k}={b64(v)}" for k, v in kv.items())


def parse_props_header(value):
    out = {}
    for part in value.split(","):
        k, _, v = part.partition("=")
        out[k] = base64.b64decode(v).decode()
    return out


def json_of(resp):
    return json.loads(resp.content.decode("utf-8-sig"))
