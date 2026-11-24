"""The world of the strict authorisation suite: four principals, the accounts they use, and the clients.

Emulator: the accounts are made through Resource Manager and the roles are given with Resource Manager requests (what Terraform does).
Azure (`AZEMU_AZURE=1`): everything exists; the principals come from the environment and nothing is created on the management plane.
"""

import base64
import hashlib
import hmac
import os
import re
import time
import urllib.parse
import uuid

import requests
from azure.identity import ClientSecretCredential

AZURE = os.environ.get("AZEMU_AZURE") == "1"
SUB = "11111111-2222-3333-4444-555555555555"
GROUP = "authrg"
TENANT = "22222222-3333-4444-5555-666666666666"

# the emulator side: the names of what Resource Manager makes
BLOB_ACCOUNT = "authblob"
HNS_ACCOUNT = "authhns"
VAULT_RBAC = "authkvrbac"
VAULT_POLICY = "authkvpolicy"
STORE = "authappcfg"
COSMOS = "authcosmos"
TOPIC = "authtopic"
SB_NAMESPACE = "authsbus"
SB_QUEUE = "authqueue"
SB_OTHER_QUEUE = "authother"
EH_NAMESPACE = "authehub"
EH_HUB = "authhub"
EH_GROUP = "authgroup"
SCOPED_CONTAINER = "scopedcont"
SCOPED_SECRET = "scoped-secret"

COSMOS_KEY = "dGhpcyBpcyBhIHRlc3Qga2V5IGZvciB0aGUgZW11bGF0b3IgdGhhdCBpcyA2NCBieXRlcyBsb25nLCBvaw=="

CLIENT_IDS = {
    "main": "aaaaaaaa-0000-4000-8000-000000000001",
    "reader": "aaaaaaaa-0000-4000-8000-000000000002",
    "norole": "aaaaaaaa-0000-4000-8000-000000000003",
    "scoped": "aaaaaaaa-0000-4000-8000-000000000004",
}
ENV = {
    "main": ("AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"),
    "reader": ("AZ_READER_CLIENT_ID", "AZ_READER_CLIENT_SECRET"),
    "norole": ("AZ_NOROLE_CLIENT_ID", "AZ_NOROLE_CLIENT_SECRET"),
    "scoped": ("AZ_SCOPED_CLIENT_ID", "AZ_SCOPED_CLIENT_SECRET"),
}


def unique(prefix="t"):
    return f"{prefix}{uuid.uuid4().hex[:10]}"


class Principal:
    """A service principal: a credential for the SDKs and raw tokens for the requests that need to be made by hand."""

    def __init__(self, name, client_id, secret, emu):
        self.name = name
        self.client_id = client_id
        self.emu = emu
        kw = {}
        if emu is not None:
            kw = dict(authority=f"https://localhost:{emu.port}/_azemu/identity/login", disable_instance_discovery=True, connection_verify=emu.ca_file)
            tenant = TENANT
        else:
            tenant = os.environ["AZURE_TENANT_ID"]
        self._cred = ClientSecretCredential(tenant, client_id, secret, additionally_allowed_tenants=["*"], **kw)
        self.oid = None

    # azure-core credential protocol
    def get_token(self, *scopes, **kwargs):
        return self._cred.get_token(*scopes, **kwargs)

    def get_token_info(self, *scopes, **kwargs):
        return self._cred.get_token_info(*scopes, **kwargs)

    def token(self, scope):
        return self._cred.get_token(scope).token


class Cred:
    """One shared access policy: its name, its key and the connection string that carries them."""

    def __init__(self, conn):
        self.conn = conn
        parts = dict(p.split("=", 1) for p in conn.split(";") if "=" in p)
        self.name = parts.get("SharedAccessKeyName")
        self.key = parts.get("SharedAccessKey")
        self.entity = parts.get("EntityPath")


class Messaging:
    """What a messaging namespace of the world has: where it is, its entities and its policies (root, send-only, listen-only, and a queue-level
    send rule for Service Bus)."""

    def __init__(self, host, https, entity, other, root, send, listen, qsend=None):
        self.host = host  # `<namespace>.servicebus.windows.net` (the emulator: `<namespace>.servicebus.localhost:<port>`)
        self.https = https
        self.entity = entity  # the queue, or the event hub
        self.other = other  # another queue (None when there is none)
        self.root, self.send, self.listen, self.qsend = root, send, listen, qsend

    @property
    def namespace(self):
        return self.host.split(".")[0]


def sas_token(resource, key_name, key, expires_at):
    """What the SDKs make: `sr` is the URL-encoded resource, the signature is over `sr` and the expiry."""
    sr = urllib.parse.quote_plus(resource)
    sig = base64.b64encode(hmac.new(key.encode(), f"{sr}\n{expires_at}".encode(), hashlib.sha256).digest()).decode()
    return f"SharedAccessSignature sr={sr}&sig={urllib.parse.quote_plus(sig)}&se={expires_at}&skn={key_name}"


class FixedToken:
    """A credential that hands out one token whatever scope is asked for (a token for another audience, or with its signature changed)."""

    def __init__(self, token, expires_in=3600):
        self.token = token
        self.expires = int(time.time()) + expires_in

    def get_token(self, *scopes, **kwargs):
        from azure.core.credentials import AccessToken

        return AccessToken(self.token, self.expires)


class SasCredential:
    """A shared access signature as a credential for the Service Bus and Event Hubs SDKs. The SDK is told the token is good for an hour whatever
    its own `se` says, so that a token that is already past its expiry reaches the broker (the SDK would otherwise refuse it itself)."""

    token_type = b"servicebus.windows.net:sastoken"

    def __init__(self, token):
        self.token = token
        self.expiry = int(time.time()) + 3600

    def get_token(self, *scopes, **kwargs):
        from azure.core.credentials import AccessToken

        return AccessToken(self.token, self.expiry)


class World:
    def __init__(self, emu):
        self.emu = emu
        self.azure = AZURE
        self.principals = {}
        self._cleanup = []
        self.sb = None
        self.eh = None
        self.schema_group = None

    # -- addresses ----------------------------------------------------------
    def blob_url(self, account=None):
        if self.azure:
            return f"https://{os.environ['AZ_STORAGE_ACCOUNT']}.blob.core.windows.net"
        return self.emu.https_url("blob", account or BLOB_ACCOUNT)

    def queue_url(self):
        if self.azure:
            return f"https://{os.environ['AZ_STORAGE_ACCOUNT']}.queue.core.windows.net"
        return self.emu.https_url("queue", BLOB_ACCOUNT)

    def table_url(self):
        if self.azure:
            return f"https://{os.environ['AZ_STORAGE_ACCOUNT']}.table.core.windows.net"
        return self.emu.https_url("table", BLOB_ACCOUNT)

    def dfs_url(self):
        if self.azure:
            return f"https://{os.environ['AZ_HNS_ACCOUNT']}.dfs.core.windows.net"
        return self.emu.https_url("dfs", HNS_ACCOUNT)

    def hns_blob_url(self):
        if self.azure:
            return f"https://{os.environ['AZ_HNS_ACCOUNT']}.blob.core.windows.net"
        return self.emu.https_url("blob", HNS_ACCOUNT)

    def vault_url(self, policy=False):
        if self.azure:
            return os.environ["AZ_KEYVAULT_POLICY_URL" if policy else "AZ_KEYVAULT_URL"].rstrip("/")
        return self.emu.https_url("vault", VAULT_POLICY if policy else VAULT_RBAC)

    def appconfig_url(self):
        if self.azure:
            return os.environ["AZ_APPCONFIG_ENDPOINT"].rstrip("/")
        return self.emu.https_url("azconfig", STORE)

    def cosmos_url(self):
        if self.azure:
            return os.environ["AZ_COSMOS_ENDPOINT"]
        return self.emu.https_url("documents", COSMOS) + "/"

    def cosmos_key(self):
        return os.environ["AZ_COSMOS_KEY"] if self.azure else COSMOS_KEY

    def eventgrid_url(self):
        if self.azure:
            return os.environ["AZ_EVENTGRID_ENDPOINT"]
        return self.emu.https_url("eventgrid", TOPIC) + "/api/events"

    @property
    def scoped_container(self):
        return os.environ["AZ_SCOPED_CONTAINER"] if self.azure else SCOPED_CONTAINER

    @property
    def scoped_secret(self):
        return os.environ["AZ_SCOPED_SECRET"] if self.azure else SCOPED_SECRET

    def sdk_kw(self):
        """Keyword arguments the SDK clients need to trust the emulator."""
        return {} if self.azure else {"connection_verify": self.emu.ca_file}

    def on_close(self, fn):
        self._cleanup.append(fn)

    def close(self):
        for fn in reversed(self._cleanup):
            try:
                fn()
            except Exception:
                pass


def build_world(emu):
    w = World(emu)
    if AZURE:
        for name, (cid, sec) in ENV.items():
            p = Principal(name, os.environ[cid], os.environ[sec], None)
            w.principals[name] = p
        w.principals["main"].oid = os.environ.get("AZ_MAIN_OBJECT_ID")
        w.principals["reader"].oid = os.environ.get("AZ_READER_OBJECT_ID")
        w.principals["norole"].oid = os.environ.get("AZ_NOROLE_OBJECT_ID")
        w.principals["scoped"].oid = os.environ.get("AZ_SCOPED_OBJECT_ID")
        _messaging_from_env(w)
        return w
    for name, cid in CLIENT_IDS.items():
        p = Principal(name, cid, "not-checked", emu)
        r = requests.get(f"{emu.gateway}/_azemu/identity/principal", params={"client_id": cid}, timeout=10)
        p.oid = r.json()["objectId"]
        w.principals[name] = p
    _make_world_on_emulator(w)
    return w


def _fqdn(conn):
    ep = dict(p.split("=", 1) for p in conn.split(";") if "=" in p)["Endpoint"]
    return urllib.parse.urlparse(ep).netloc


def _messaging_from_env(w):
    """Real namespaces: nothing is made or read on the management plane. The names of the queues come from the namespace's own list (a read)."""
    e = os.environ
    if "AZ_SERVICEBUS_CONN_STR" in e:
        root = Cred(e["AZ_SERVICEBUS_CONN_STR"])
        host = _fqdn(root.conn)
        queue = e.get("AZ_SERVICEBUS_AUTH_QUEUE")
        other = None
        try:
            from azure.servicebus.management import ServiceBusAdministrationClient

            with ServiceBusAdministrationClient.from_connection_string(root.conn) as admin:
                names = [q.name for q in admin.list_queues()]
            other = next((n for n in names if n != queue), None)
        except Exception:
            pass
        w.sb = Messaging(host, f"https://{host}", queue, other, root, Cred(e["AZ_SERVICEBUS_SEND_CONN_STR"]), Cred(e["AZ_SERVICEBUS_LISTEN_CONN_STR"]),
                         Cred(e["AZ_SERVICEBUS_QUEUE_SEND_CONN_STR"]) if "AZ_SERVICEBUS_QUEUE_SEND_CONN_STR" in e else None)
    if "AZ_EVENTHUBS_CONN_STR" in e:
        root = Cred(e["AZ_EVENTHUBS_CONN_STR"])
        host = _fqdn(root.conn)
        w.eh = Messaging(host, f"https://{host}", e.get("AZ_EVENTHUBS_AUTH_HUB"), None, root, Cred(e["AZ_EVENTHUBS_SEND_CONN_STR"]),
                         Cred(e["AZ_EVENTHUBS_LISTEN_CONN_STR"]))
        w.schema_group = e.get("AZ_SCHEMA_GROUP")


# -- the emulator side: Resource Manager makes the accounts and the roles are assigned ----------------------------------------------------------------

class Arm:
    def __init__(self, emu):
        self.base = f"http://management.localhost:{emu.port}"

    def call(self, method, path, body=None, api="2022-04-01"):
        sep = "&" if "?" in path else "?"
        r = requests.request(method, f"{self.base}{path}{sep}api-version={api}", json=body, headers={"Authorization": "Bearer arm"}, timeout=30)
        return r

    def put(self, path, body, ok=(200, 201), api="2022-04-01"):
        r = self.call("PUT", path, body, api)
        assert r.status_code in ok, f"PUT {path}: {r.status_code} {r.text}"
        return r.json()


ROLES_MAIN = [
    "Storage Blob Data Owner",
    "Storage Queue Data Contributor",
    "Storage Table Data Contributor",
    "Key Vault Administrator",
    "App Configuration Data Owner",
    "EventGrid Data Sender",
    "Azure Service Bus Data Owner",
    "Azure Event Hubs Data Owner",
    "Schema Registry Contributor",
]
ROLES_READER = [
    "Reader",
    "Storage Blob Data Reader",
    "Storage Queue Data Reader",
    "Storage Table Data Reader",
    "Key Vault Secrets User",
    "App Configuration Data Reader",
    "Azure Service Bus Data Receiver",
    "Azure Event Hubs Data Receiver",
]


def _make_world_on_emulator(w):
    emu = w.emu
    arm = Arm(emu)
    rg = f"/subscriptions/{SUB}/resourceGroups/{GROUP}"
    arm.put(rg, {"location": "eastus"})
    prov = lambda ns, t, n: f"{rg}/providers/{ns}/{t}/{n}"
    for acct in (BLOB_ACCOUNT, HNS_ACCOUNT):
        arm.put(prov("Microsoft.Storage", "storageAccounts", acct), {"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {"isHnsEnabled": acct == HNS_ACCOUNT}})
    kv = lambda rbac, policies: {"location": "eastus", "properties": {"tenantId": TENANT, "sku": {"family": "A", "name": "standard"}, "enableRbacAuthorization": rbac, "accessPolicies": policies}}
    arm.put(prov("Microsoft.KeyVault", "vaults", VAULT_RBAC), kv(True, []))
    perms = lambda secrets: {"secrets": secrets, "keys": [], "certificates": []}
    arm.put(
        prov("Microsoft.KeyVault", "vaults", VAULT_POLICY),
        kv(False, [
            {"tenantId": TENANT, "objectId": w.principals["main"].oid, "permissions": perms(["get", "list", "set", "delete", "purge", "recover"])},
            {"tenantId": TENANT, "objectId": w.principals["reader"].oid, "permissions": perms(["get", "list"])},
        ]),
    )
    arm.put(prov("Microsoft.AppConfiguration", "configurationStores", STORE), {"location": "eastus", "sku": {"name": "standard"}})
    cosmos = prov("Microsoft.DocumentDB", "databaseAccounts", COSMOS)
    arm.put(cosmos, {"location": "eastus", "kind": "GlobalDocumentDB", "properties": {"databaseAccountOfferType": "Standard", "locations": [{"locationName": "East US", "failoverPriority": 0}]}})
    arm.put(prov("Microsoft.EventGrid", "topics", TOPIC), {"location": "eastus", "properties": {}})
    arm.put(f"{prov('Microsoft.Storage', 'storageAccounts', BLOB_ACCOUNT)}/blobServices/default/containers/{SCOPED_CONTAINER}", {"properties": {}})

    defs = {}

    def role_id(name):
        if name not in defs:
            r = arm.call("GET", f"/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions?$filter=roleName%20eq%20%27{name.replace(' ', '%20')}%27")
            assert r.status_code == 200 and r.json()["value"], f"role {name}: {r.text}"
            defs[name] = r.json()["value"][0]["id"]
        return defs[name]

    def assign(scope, role, principal):
        arm.put(f"{scope}/providers/Microsoft.Authorization/roleAssignments/{uuid.uuid4()}", {"properties": {"roleDefinitionId": role_id(role), "principalId": principal.oid, "principalType": "ServicePrincipal"}})

    for r in ROLES_MAIN:
        assign(rg, r, w.principals["main"])
    for r in ROLES_READER:
        assign(rg, r, w.principals["reader"])
    acct = prov("Microsoft.Storage", "storageAccounts", BLOB_ACCOUNT)
    assign(f"{acct}/blobServices/default/containers/{SCOPED_CONTAINER}", "Storage Blob Data Reader", w.principals["scoped"])
    assign(f"{prov('Microsoft.KeyVault', 'vaults', VAULT_RBAC)}/secrets/{SCOPED_SECRET}", "Key Vault Secrets User", w.principals["scoped"])
    _make_messaging_on_emulator(w, arm, rg, prov, assign)
    # Cosmos DB's own roles: the contributor for main, the reader for the reader
    for pr, definition in (("main", "00000000-0000-0000-0000-000000000002"), ("reader", "00000000-0000-0000-0000-000000000001")):
        arm.put(f"{cosmos}/sqlRoleAssignments/{uuid.uuid4()}", {"properties": {"roleDefinitionId": f"{cosmos}/sqlRoleDefinitions/{definition}", "scope": cosmos, "principalId": w.principals[pr].oid}})


def _make_messaging_on_emulator(w, arm, rg, prov, assign):
    """A Service Bus namespace with two queues and a queue-level rule, an Event Hubs namespace with one hub and a schema group; the rules
    of both (the keys come from `listKeys`: the keys the data planes check), and the roles of the principals."""
    emu = w.emu
    api = "2022-10-01-preview"
    sb = prov("Microsoft.ServiceBus", "namespaces", SB_NAMESPACE)
    arm.put(sb, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}, "properties": {}}, api=api)
    for q in (SB_QUEUE, SB_OTHER_QUEUE):
        arm.put(f"{sb}/queues/{q}", {"properties": {}}, api=api)

    def rule(parent, name, rights):
        arm.put(f"{parent}/authorizationRules/{name}", {"properties": {"rights": rights}}, api=api)

    def conn_of(parent, name):
        r = arm.call("POST", f"{parent}/authorizationRules/{name}/listKeys", {}, api=api)
        assert r.status_code == 200, r.text
        return Cred(r.json()["primaryConnectionString"])

    rule(sb, "sendonly", ["Send"])
    rule(sb, "listenonly", ["Listen"])
    rule(f"{sb}/queues/{SB_QUEUE}", "qsend", ["Send"])
    w.sb = Messaging(
        urllib.parse.urlparse(emu.url("servicebus", SB_NAMESPACE)).netloc, emu.https_url("servicebus", SB_NAMESPACE), SB_QUEUE, SB_OTHER_QUEUE,
        conn_of(sb, "RootManageSharedAccessKey"), conn_of(sb, "sendonly"), conn_of(sb, "listenonly"), conn_of(f"{sb}/queues/{SB_QUEUE}", "qsend"),
    )
    eh = prov("Microsoft.EventHub", "namespaces", EH_NAMESPACE)
    arm.put(eh, {"location": "eastus", "sku": {"name": "Standard", "tier": "Standard"}, "properties": {}}, api=api)
    arm.put(f"{eh}/eventhubs/{EH_HUB}", {"properties": {"partitionCount": 2}}, api=api)
    arm.put(f"{eh}/schemagroups/{EH_GROUP}", {"properties": {"schemaType": "Avro", "schemaCompatibility": "Forward"}}, api=api)
    rule(eh, "sendonly", ["Send"])
    rule(eh, "listenonly", ["Listen"])
    w.eh = Messaging(
        urllib.parse.urlparse(emu.url("eventhub", EH_NAMESPACE)).netloc, emu.https_url("eventhub", EH_NAMESPACE), EH_HUB, None,
        conn_of(eh, "RootManageSharedAccessKey"), conn_of(eh, "sendonly"), conn_of(eh, "listenonly"),
    )
    w.schema_group = EH_GROUP
    # the scoped principal: Data Sender on the one queue (nothing on Event Hubs)
    assign(f"{sb}/queues/{SB_QUEUE}", "Azure Service Bus Data Sender", w.principals["scoped"])


# -- small helpers the tests share ----------------------------------------------------------------------------------------

def code_of(e):
    """The error code of an Azure SDK exception."""
    c = getattr(e, "error_code", None)
    return getattr(c, "value", c)


def raw(method, url, token=None, headers=None, **kw):
    h = dict(headers or {})
    if token:
        h["Authorization"] = f"Bearer {token}"
    return requests.request(method, url, headers=h, timeout=60, verify=kw.pop("verify", True), **kw)


def message_without_ids(text):
    """The stable part of an answer: request ids and times replaced."""
    t = re.sub(r"RequestId:[0-9a-fA-F-]+", "RequestId:<guid>", text)
    t = re.sub(r"Time:\d{4}-\d\d-\d\dT[\d:.]+Z", "Time:<time>", t)
    return t


def udk_window():
    """The start and the expiry of a user delegation key request."""
    from datetime import datetime, timedelta, timezone

    now = datetime.now(timezone.utc)
    return now - timedelta(minutes=1), now + timedelta(hours=1)
