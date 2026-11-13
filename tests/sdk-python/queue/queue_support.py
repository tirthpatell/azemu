"""Helpers shared by the Queue Storage SDK tests.

This is a plain helper module, not a test module (it does not match test_*.py).
It is imported by name from the test files in this directory.
"""

import re
import uuid
from datetime import datetime, timezone

DEV_ACCOUNT = "devstoreaccount1"


def unique_queue_name(prefix: str = "q") -> str:
    """A queue name that satisfies the naming rules.

    Rules (Microsoft Learn, Naming Queues and Metadata): 3-63 characters,
    lowercase letters, digits and single hyphens, first and last character
    alphanumeric.
    """
    name = f"{prefix}-{uuid.uuid4().hex[:16]}"
    assert 3 <= len(name) <= 63
    return name


def unique_prefix() -> str:
    """A short prefix for grouping queues created by one test."""
    return f"p{uuid.uuid4().hex[:10]}"


def utc_now() -> datetime:
    """Current UTC time, truncated to whole seconds (the service's precision)."""
    return datetime.now(timezone.utc).replace(microsecond=0)


def as_utc(value) -> datetime:
    """Return a service timestamp as an aware UTC datetime.

    The SDK hands back some timestamps (stored access policy Start and Expiry) as
    strings such as "2022-10-27T22:01:36.0000000Z", with 7 fractional digits.
    """
    if isinstance(value, datetime):
        return value
    text = re.sub(r"(\.\d{6})\d+", r"\1", value.replace("Z", "+00:00"))
    return datetime.fromisoformat(text).astimezone(timezone.utc)


def parse_conn_str(conn_str: str) -> dict:
    """Split an Azure storage connection string into its key/value parts."""
    parts = {}
    for item in conn_str.split(";"):
        if "=" in item:
            key, value = item.split("=", 1)
            parts[key] = value
    return parts


def assert_storage_error(exc, status: int, code: str) -> None:
    """Check an azure-core HttpResponseError subclass for status and error code.

    The SDK copies the x-ms-error-code header onto error.error_code, and
    StorageErrorCode is a str enum, so it compares equal to the plain string.
    """
    assert exc.status_code == status, f"status {exc.status_code}, expected {status}: {exc}"
    assert exc.error_code == code, f"error_code {exc.error_code}, expected {code}: {exc}"
