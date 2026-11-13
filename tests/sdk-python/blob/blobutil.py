"""Small helpers shared by the blob tests (not a test module)."""

import os
from datetime import datetime, timedelta, timezone

from azure.core import MatchConditions  # noqa: F401  (re-exported for tests)
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError  # noqa: F401


def parse_conn_str(conn: str) -> dict:
    parts = {}
    for item in conn.split(";"):
        if not item:
            continue
        k, _, v = item.partition("=")
        parts[k] = v
    return parts


def rand_bytes(n: int) -> bytes:
    return os.urandom(n)


def now() -> datetime:
    return datetime.now(timezone.utc)


def future(minutes: int = 30) -> datetime:
    return now() + timedelta(minutes=minutes)


def past(minutes: int = 30) -> datetime:
    return now() - timedelta(minutes=minutes)


def assert_error(excinfo, code: str, status: int):
    """Check both the Azure error code and the HTTP status of an SDK exception."""
    err = excinfo.value
    assert getattr(err, "error_code", None) is not None or hasattr(err, "status_code")
    got_code = getattr(err, "error_code", None)
    code_str = got_code.value if hasattr(got_code, "value") else str(got_code)
    assert code_str == code, f"error_code {code_str!r} != {code!r} ({err})"
    assert err.status_code == status, f"status {err.status_code} != {status}"


def assert_error_in(excinfo, codes, status: int):
    """Like assert_error, for the few cases where the docs allow more than one code."""
    err = excinfo.value
    got_code = getattr(err, "error_code", None)
    code_str = got_code.value if hasattr(got_code, "value") else str(got_code)
    assert code_str in codes, f"error_code {code_str!r} not in {codes} ({err})"
    assert err.status_code == status, f"status {err.status_code} != {status}"
