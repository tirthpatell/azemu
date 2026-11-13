"""Small helpers shared by the table tests (not a test module)."""

from datetime import datetime, timedelta, timezone

from azure.core import MatchConditions  # noqa: F401  (re-exported for tests)
from azure.core.credentials import AzureNamedKeyCredential
from azure.core.exceptions import (  # noqa: F401
    HttpResponseError,
    ResourceExistsError,
    ResourceNotFoundError,
)
from azure.data.tables import AccountSasPermissions, ResourceTypes, generate_account_sas


def parse_conn_str(conn: str) -> dict:
    parts = {}
    for item in conn.split(";"):
        if not item:
            continue
        k, _, v = item.partition("=")
        parts[k] = v
    return parts


def now() -> datetime:
    return datetime.now(timezone.utc)


def future(minutes: int = 30) -> datetime:
    return now() + timedelta(minutes=minutes)


def past(minutes: int = 30) -> datetime:
    return now() - timedelta(minutes=minutes)


def make_account_sas(account_name, account_key, *, permission="raud", resource_types="sco",
                     expiry=None, start=None) -> str:
    """Account SAS for the table service. `resource_types` is a string of s/c/o."""
    cred = AzureNamedKeyCredential(account_name, account_key)
    return generate_account_sas(
        cred,
        resource_types=ResourceTypes.from_string(resource_types),
        permission=AccountSasPermissions.from_string(permission),
        expiry=expiry or future(),
        start=start,
    )


def seed(table, entities):
    """Insert entities one by one, return them."""
    for e in entities:
        table.create_entity(e)
    return entities


def keys(entities):
    """List of (PartitionKey, RowKey) for an iterable of entities."""
    return [(e["PartitionKey"], e["RowKey"]) for e in entities]


def code(exc) -> str:
    """The service error code of an SDK exception, whichever way the SDK exposes it.

    Some SDK calls decode the error (exc.error_code), some re-raise the raw
    azure-core error. Fall back to the x-ms-error-code header and then to the
    JSON error body (`{"odata.error": {"code": ...}}`).
    """
    import json

    ec = getattr(exc, "error_code", None)
    if ec:
        return getattr(ec, "value", str(ec))
    resp = getattr(exc, "response", None)
    if resp is None:
        return ""
    hdr = resp.headers.get("x-ms-error-code")
    if hdr:
        return hdr
    try:
        return json.loads(resp.text())["odata.error"]["code"]
    except Exception:
        return ""
