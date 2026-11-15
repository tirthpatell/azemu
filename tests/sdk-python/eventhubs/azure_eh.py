"""Running the Event Hubs suite against a real namespace (AZEMU_AZURE=1).

The namespace is Standard tier. Reads from the environment (never printed):

    AZ_EVENTHUBS_CONN_STR   connection string of the namespace (a key that may send, listen and manage)
    AZ_EVENTHUBS_NAMESPACE  the namespace name
    AZ_SUBSCRIPTION_ID, AZ_RESOURCE_GROUP, AZURE_TENANT_ID, AZURE_CLIENT_ID, AZURE_CLIENT_SECRET
                            a service principal that may create and delete event hubs and consumer groups inside that namespace

The Python SDK has no administration client, so hubs and consumer groups are made through the Resource Manager REST API
(the only management call the suite makes). A namespace holds at most 10 event hubs and a hub at most 20 consumer groups, so the
suite does not make one hub per test: it makes one hub per partition count it needs, once, and every test works in a shared hub
(it records where each partition ends when the test starts and reads from there, see `ehutil.record_baseline`). The hubs are deleted
when the session ends, unless AZEMU_AZURE_KEEP_HUBS=1.
"""

import os
import threading
import time

import requests

API = "2024-01-01"
POOL_PREFIX = "azemu-pool"

_lock = threading.Lock()
_token = {"value": None, "expires": 0.0}
_made_hubs = set()
_made_groups = set()


def enabled() -> bool:
    return os.environ.get("AZEMU_AZURE") == "1"


def namespace() -> str:
    return os.environ["AZ_EVENTHUBS_NAMESPACE"]


def fqdn() -> str:
    return f"{namespace()}.servicebus.windows.net"


def conn_str_base() -> str:
    """The namespace connection string without an `EntityPath`."""
    parts = [p for p in os.environ["AZ_EVENTHUBS_CONN_STR"].split(";") if p and not p.lower().startswith("entitypath=")]
    return ";".join(parts)


def _arm_token() -> str:
    with _lock:
        if _token["value"] and _token["expires"] > time.time() + 60:
            return _token["value"]
        r = requests.post(
            f"https://login.microsoftonline.com/{os.environ['AZURE_TENANT_ID']}/oauth2/v2.0/token",
            data={
                "grant_type": "client_credentials",
                "client_id": os.environ["AZURE_CLIENT_ID"],
                "client_secret": os.environ["AZURE_CLIENT_SECRET"],
                "scope": "https://management.azure.com/.default",
            },
            timeout=30,
        )
        r.raise_for_status()
        body = r.json()
        _token["value"] = body["access_token"]
        _token["expires"] = time.time() + int(body.get("expires_in", 3000))
        return _token["value"]


def _ns_url() -> str:
    return (
        f"https://management.azure.com/subscriptions/{os.environ['AZ_SUBSCRIPTION_ID']}"
        f"/resourceGroups/{os.environ['AZ_RESOURCE_GROUP']}/providers/Microsoft.EventHub/namespaces/{namespace()}"
    )


def _arm(method: str, path: str, body=None, ok=(200, 201, 202, 204)):
    for attempt in range(6):
        r = requests.request(
            method,
            f"{_ns_url()}/{path}?api-version={API}",
            json=body,
            headers={"Authorization": f"Bearer {_arm_token()}"},
            timeout=60,
        )
        if r.status_code == 429 or r.status_code >= 500:
            time.sleep(2 + attempt * 3)
            continue
        if r.status_code not in ok:
            # the body names the problem; it holds no secret
            raise AssertionError(f"ARM {method} {path.split('/')[0]}/<name> -> {r.status_code}: {r.text[:300]}")
        return r
    raise AssertionError(f"ARM {method} kept failing: {r.status_code}")


def hub_name_for(partitions: int) -> str:
    return f"{POOL_PREFIX}-{partitions}p"


def ensure_hub(partitions: int, name: str | None = None, retention_days: int = 1) -> str:
    """Create (or find) the shared hub for a partition count and return its name."""
    name = name or hub_name_for(partitions)
    with _lock:
        if name in _made_hubs:
            return name
    _arm("PUT", f"eventhubs/{name}", {"properties": {"partitionCount": partitions, "messageRetentionInDays": retention_days}})
    with _lock:
        _made_hubs.add(name)
    return name


def ensure_group(hub: str, group: str) -> None:
    if group.lower() == "$default":
        return
    key = (hub, group)
    with _lock:
        if key in _made_groups:
            return
    _arm("PUT", f"eventhubs/{hub}/consumergroups/{group}", {"properties": {}})
    with _lock:
        _made_groups.add(key)
    time.sleep(2)  # let the new group reach the data plane


def delete_hub(name: str) -> None:
    _arm("DELETE", f"eventhubs/{name}", ok=(200, 202, 204))
    with _lock:
        _made_hubs.discard(name)
        for k in [k for k in _made_groups if k[0] == name]:
            _made_groups.discard(k)


def list_hubs() -> list:
    r = _arm("GET", "eventhubs")
    return [h["name"] for h in r.json().get("value", [])]


def delete_all_hubs() -> list:
    """Remove every event hub of the namespace (the namespace is dedicated to this suite)."""
    gone = []
    for h in list_hubs():
        delete_hub(h)
        gone.append(h)
    return gone
