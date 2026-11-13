"""Shared Key and Shared Key Lite signatures for the development account.

Expectations come from Microsoft Learn, "Authorize with Shared Key", Table service sections, and from
the Python SDK:

  Shared Key       StringToSign = VERB \\n Content-MD5 \\n Content-Type \\n Date \\n CanonicalizedResource
                   (no canonicalized headers; the Date line holds x-ms-date when the request has it)
  Shared Key Lite  StringToSign = Date \\n CanonicalizedResource
  CanonicalizedResource = /account + encoded URL path + "?comp=value" when the query has comp.

Only `devstoreaccount1` is checked; any other account name is accepted whatever the key (azemu invents
those keys). A wrong signature is 403 AuthenticationFailed, which the SDK raises as
ClientAuthenticationError.
"""

import base64
import hashlib
import hmac
import os
import subprocess
import tempfile
import urllib.parse
from email.utils import formatdate

import pytest

import emubin
import requests
from tableutil import code
from azure.core.credentials import AzureNamedKeyCredential
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError
from azure.data.tables import TableClient, TableServiceClient, TransactionOperation, UpdateMode
from azure.data.tables.aio import TableClient as AsyncTableClient
from azure.data.tables.aio import TableServiceClient as AsyncTableServiceClient

import pathlib
import socket
import time

ROOT = pathlib.Path(__file__).resolve().parents[3]
DEV_ACCOUNT = "devstoreaccount1"
DEV_KEY = (
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/"
    "K1SZFPTOtr/KBHBeksoGMGw=="
)


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_healthy(port, proc, timeout=20.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"emulator exited early with code {proc.returncode}")
        try:
            if requests.get(f"http://127.0.0.1:{port}/_azemu/health", timeout=1).status_code == 200:
                return
        except requests.RequestException:
            pass
        time.sleep(0.1)
    raise RuntimeError("emulator did not become healthy")

# 32 bytes of base64 that are not the account key
WRONG_KEY = base64.b64encode(bytes(range(32))).decode()


def conn(emu, key, account=DEV_ACCOUNT):
    return emu.storage_conn_str(account=account, key=key)


def service_with(emu, key, account=DEV_ACCOUNT):
    return TableServiceClient.from_connection_string(conn(emu, key, account))


# -- the SDK ---------------------------------------------------------------------------------


def test_right_key_works(emu, uniq):
    name = uniq()
    with service_with(emu, DEV_KEY) as svc:
        table = svc.create_table(name)
        table.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
        assert table.get_entity("p", "r")["v"] == 1
        assert [e["RowKey"] for e in table.query_entities("PartitionKey eq 'p'")] == ["r"]
        assert name in [t.name for t in svc.list_tables()]
        svc.delete_table(name)


def test_wrong_key_is_client_authentication_error(emu, uniq):
    with service_with(emu, WRONG_KEY) as svc:
        with pytest.raises(ClientAuthenticationError) as e:
            svc.create_table(uniq())
    assert e.value.status_code == 403
    assert e.value.error_code is not None and e.value.error_code.value == "AuthenticationFailed"


@pytest.mark.parametrize("op", ["list", "get_properties", "set_properties", "query"])
def test_wrong_key_fails_for_every_kind_of_request(emu, table_name, op):
    # the table exists (made with the right key), so the failure is the signature and nothing else
    with service_with(emu, DEV_KEY) as good:
        good.create_table(table_name)
    with service_with(emu, WRONG_KEY) as svc:
        with pytest.raises(ClientAuthenticationError):
            if op == "list":
                list(svc.list_tables())
            elif op == "get_properties":
                svc.get_service_properties()  # ?restype=service&comp=properties
            elif op == "set_properties":
                svc.set_service_properties(analytics_logging=None, hour_metrics=None, minute_metrics=None, cors=[])
            else:
                list(svc.get_table_client(table_name).query_entities("PartitionKey eq 'p'"))


def denied(call):
    """The call fails with 403 AuthenticationFailed. The SDK raises ClientAuthenticationError for some calls
    and the plain HttpResponseError for others (it only maps status 401 and a few codes), whatever the
    service says; what is the same for all of them is the status and the error code."""
    with pytest.raises(HttpResponseError) as e:
        call()
    assert e.value.status_code == 403
    assert code(e.value) == "AuthenticationFailed"


def test_wrong_key_fails_for_entity_writes_batch_and_acl(emu, table_name):
    with service_with(emu, DEV_KEY) as good:
        good.create_table(table_name)
    with service_with(emu, WRONG_KEY) as bad:
        t = bad.get_table_client(table_name)
        denied(lambda: t.create_entity({"PartitionKey": "p", "RowKey": "r"}))
        denied(lambda: t.upsert_entity({"PartitionKey": "p", "RowKey": "r"}, mode=UpdateMode.MERGE))
        denied(lambda: t.delete_entity("p", "r"))
        denied(lambda: t.submit_transaction([(TransactionOperation.CREATE, {"PartitionKey": "p", "RowKey": "1"})]))
        denied(lambda: t.get_table_access_policy())  # ?comp=acl
        denied(lambda: bad.delete_table(table_name))


def test_a_table_client_with_the_wrong_key_cannot_read_what_the_right_key_wrote(emu, table_name):
    with service_with(emu, DEV_KEY) as good:
        good.create_table(table_name).create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    with TableClient.from_connection_string(conn(emu, WRONG_KEY), table_name) as t:
        denied(lambda: t.get_entity("p", "r"))


def test_account_key_with_different_case_name_is_checked_too(emu, uniq):
    # the account in the Authorization header is compared without case
    cred = AzureNamedKeyCredential("DEVSTOREACCOUNT1", WRONG_KEY)
    url = emu.url("table", DEV_ACCOUNT)
    with TableServiceClient(endpoint=url, credential=cred) as svc:
        with pytest.raises(ClientAuthenticationError):
            svc.create_table(uniq())


AWKWARD_KEYS = ["with space", "O'Brien", "100%", "a+b", "a&b=c;d,e", "(x) [y] {z} @ : |", "héllo 你好 🙂", "%41%20"]


@pytest.mark.parametrize("key", AWKWARD_KEYS)
def test_right_key_signs_awkward_key_paths(emu, table_name, key):
    # the entity URL is `/{table}(PartitionKey='..',RowKey='..')`: what is signed is the path as sent
    with service_with(emu, DEV_KEY) as svc:
        t = svc.create_table(table_name)
        t.create_entity({"PartitionKey": key, "RowKey": key, "v": 1})
        assert t.get_entity(key, key)["v"] == 1
        t.update_entity({"PartitionKey": key, "RowKey": key, "v": 2})
        t.delete_entity(key, key)


def test_other_accounts_are_not_checked(emu, uniq):
    # azemu makes up the keys of other accounts, so a client cannot know them
    with service_with(emu, WRONG_KEY, account="someotheraccount") as svc:
        name = uniq()
        t = svc.create_table(name)
        t.create_entity({"PartitionKey": "p", "RowKey": "r"})
        svc.delete_table(name)


async def test_async_client_right_and_wrong_key(emu, uniq):
    name = uniq()
    async with AsyncTableServiceClient.from_connection_string(conn(emu, DEV_KEY)) as svc:
        t = await svc.create_table(name)
        await t.create_entity({"PartitionKey": "it's", "RowKey": "a b", "v": 1})
        assert (await t.get_entity("it's", "a b"))["v"] == 1
    async with AsyncTableClient.from_connection_string(conn(emu, WRONG_KEY), name) as t:
        with pytest.raises(HttpResponseError) as e:
            await t.get_entity("it's", "a b")
        assert e.value.status_code == 403 and code(e.value) == "AuthenticationFailed"


# -- raw requests: Shared Key Lite and the Date rules ------------------------------------------


def sign(key, string_to_sign):
    mac = hmac.new(base64.b64decode(key), string_to_sign.encode("utf-8"), hashlib.sha256).digest()
    return base64.b64encode(mac).decode()


def http_date():
    return formatdate(usegmt=True)


def raw(emu, method, path, *, scheme, key, date_headers, string_to_sign, body=None, content_type=None):
    """Send one request to the emulator's table endpoint with an Authorization header we built."""
    host = f"{DEV_ACCOUNT}.table.core.localhost:{emu.port}"
    headers = {
        "Host": host,
        "Authorization": f"{scheme} {DEV_ACCOUNT}:{sign(key, string_to_sign)}",
        "Accept": "application/json;odata=nometadata",
        "DataServiceVersion": "3.0;",
        "MaxDataServiceVersion": "3.0;",
        "x-ms-version": "2019-02-02",
        **date_headers,
    }
    if content_type:
        headers["Content-Type"] = content_type
    return requests.request(method, f"http://127.0.0.1:{emu.port}{path}", headers=headers, data=body, timeout=10)


def test_shared_key_lite_create_and_query_table(emu, uniq):
    # path style is not used here: host style, so the resource is /devstoreaccount1/Tables
    name = uniq()
    d = http_date()
    body = f'{{"TableName":"{name}"}}'
    # Lite: Date \n CanonicalizedResource  (the example in the docs is "Sun, 11 Oct 2009 19:52:39 GMT\n/testaccount1/Tables")
    r = raw(emu, "POST", "/Tables", scheme="SharedKeyLite", key=DEV_KEY, date_headers={"x-ms-date": d},
            string_to_sign=f"{d}\n/{DEV_ACCOUNT}/Tables", body=body, content_type="application/json")
    assert r.status_code == 201, r.text
    d = http_date()
    r = raw(emu, "GET", "/Tables", scheme="SharedKeyLite", key=DEV_KEY, date_headers={"x-ms-date": d},
            string_to_sign=f"{d}\n/{DEV_ACCOUNT}/Tables")
    assert r.status_code == 200, r.text
    assert name in [t["TableName"] for t in r.json()["value"]]


def test_shared_key_lite_wrong_key_is_403_authentication_failed(emu):
    d = http_date()
    r = raw(emu, "GET", "/Tables", scheme="SharedKeyLite", key=WRONG_KEY, date_headers={"x-ms-date": d},
            string_to_sign=f"{d}\n/{DEV_ACCOUNT}/Tables")
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "AuthenticationFailed"
    err = r.json()["odata.error"]
    assert err["code"] == "AuthenticationFailed"
    assert err["message"]["value"].startswith("Server failed to authenticate the request.")


def test_shared_key_lite_includes_comp_but_no_other_query_parameter(emu, table_name, service):
    service.create_table(table_name)
    d = http_date()
    # ?comp=acl is part of the resource ...
    path = f"/{table_name}?comp=acl&timeout=30"
    ok = raw(emu, "GET", path, scheme="SharedKeyLite", key=DEV_KEY, date_headers={"x-ms-date": d},
             string_to_sign=f"{d}\n/{DEV_ACCOUNT}/{table_name}?comp=acl")
    assert ok.status_code == 200, ok.text
    # ... and leaving it out of the signature is a mismatch
    bad = raw(emu, "GET", path, scheme="SharedKeyLite", key=DEV_KEY, date_headers={"x-ms-date": d},
              string_to_sign=f"{d}\n/{DEV_ACCOUNT}/{table_name}")
    assert bad.status_code == 403


def test_shared_key_date_header_alone_and_with_x_ms_date(emu):
    # Date is signed as is when there is no x-ms-date
    d = http_date()
    sts = f"GET\n\napplication/json\n{d}\n/{DEV_ACCOUNT}/Tables"
    r = raw(emu, "GET", "/Tables", scheme="SharedKey", key=DEV_KEY, date_headers={"Date": d},
            string_to_sign=sts, content_type="application/json")
    assert r.status_code == 200, r.text
    # With x-ms-date, the Date line of the string to sign holds that value (never empty), as the docs say
    d2 = http_date()
    sts = f"GET\n\napplication/json\n{d2}\n/{DEV_ACCOUNT}/Tables"
    r = raw(emu, "GET", "/Tables", scheme="SharedKey", key=DEV_KEY, date_headers={"x-ms-date": d2},
            string_to_sign=sts, content_type="application/json")
    assert r.status_code == 200, r.text
    # An empty Date line (the rule for Blob and Queue) does not match for Table
    sts = "GET\n\napplication/json\n\n" + f"/{DEV_ACCOUNT}/Tables"
    r = raw(emu, "GET", "/Tables", scheme="SharedKey", key=DEV_KEY, date_headers={"x-ms-date": d2},
            string_to_sign=sts, content_type="application/json")
    assert r.status_code == 403


def test_signature_for_one_verb_or_path_does_not_work_for_another(emu, table_name, service):
    service.create_table(table_name)
    d = http_date()
    sts_get = f"GET\n\n\n{d}\n/{DEV_ACCOUNT}/Tables"
    r = raw(emu, "GET", "/Tables", scheme="SharedKey", key=DEV_KEY, date_headers={"x-ms-date": d}, string_to_sign=sts_get)
    assert r.status_code == 200
    # same signature, another verb
    r = raw(emu, "POST", "/Tables", scheme="SharedKey", key=DEV_KEY, date_headers={"x-ms-date": d}, string_to_sign=sts_get,
            body=f'{{"TableName":"{table_name}x"}}', content_type="application/json")
    assert r.status_code == 403
    # same signature, another path
    r = raw(emu, "GET", f"/{table_name}()", scheme="SharedKey", key=DEV_KEY, date_headers={"x-ms-date": d}, string_to_sign=sts_get)
    assert r.status_code == 403


def test_placeholder_signature_is_not_checked(emu):
    # recordings and replay tools carry short placeholders such as "Sanitized"; they pass (as in Blob and Queue)
    r = requests.get(
        f"http://127.0.0.1:{emu.port}/Tables",
        headers={"Host": f"{DEV_ACCOUNT}.table.core.localhost:{emu.port}", "Authorization": f"SharedKey {DEV_ACCOUNT}:Sanitized",
                 "Accept": "application/json;odata=nometadata", "x-ms-date": http_date()},
        timeout=10,
    )
    assert r.status_code == 200


def test_no_credentials_is_still_refused(emu):
    r = requests.get(f"http://127.0.0.1:{emu.port}/Tables",
                     headers={"Host": f"{DEV_ACCOUNT}.table.core.localhost:{emu.port}"}, timeout=10)
    assert r.status_code in (401, 403)


# -- AZEMU_LOOSE_AUTH=1 ------------------------------------------------------------------------


def _binary():
    binary = emubin.for_service("table")
    if not binary:
        pytest.fail(emubin.missing_message())
    return binary


def test_loose_auth_switch_turns_the_check_off(emu, uniq):
    if emu.storage_is_external:
        pytest.skip("starts its own emulator")
    port = _free_port()
    with tempfile.TemporaryDirectory(prefix="azemu-loose-") as cert_dir:
        env = dict(os.environ, AZEMU_LOOSE_AUTH="1")
        proc = subprocess.Popen([_binary(), "--port", str(port), "--cert-dir", cert_dir], env=env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            _wait_healthy(port, proc)
            url = f"http://{DEV_ACCOUNT}.table.core.localhost:{port}"
            cred = AzureNamedKeyCredential(DEV_ACCOUNT, WRONG_KEY)
            with TableServiceClient(endpoint=url, credential=cred) as svc:
                name = uniq()
                t = svc.create_table(name)
                t.create_entity({"PartitionKey": "p", "RowKey": "r"})
                assert t.get_entity("p", "r")["RowKey"] == "r"
        finally:
            proc.terminate()
            proc.wait(timeout=5)


def test_azurite_style_path_endpoint_on_a_pinned_port(emu, uniq):
    """`http://127.0.0.1:{port}/devstoreaccount1` (what `UseDevelopmentStorage=true` means): the SDK signs the
    path with the account in it, so the account appears twice in the resource."""
    if emu.storage_is_external:
        pytest.skip("starts its own emulator")
    gateway, pinned = _free_port(), _free_port()
    with tempfile.TemporaryDirectory(prefix="azemu-pin-") as cert_dir:
        proc = subprocess.Popen([_binary(), "--port", str(gateway), "--pin", f"table={pinned}", "--cert-dir", cert_dir],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            _wait_healthy(gateway, proc)
            url = f"http://127.0.0.1:{pinned}/{DEV_ACCOUNT}"
            with TableServiceClient(endpoint=url, credential=AzureNamedKeyCredential(DEV_ACCOUNT, DEV_KEY)) as svc:
                name = uniq()
                t = svc.create_table(name)
                t.create_entity({"PartitionKey": "a b", "RowKey": "it's", "v": 1})
                assert t.get_entity("a b", "it's")["v"] == 1
                assert name in [x.name for x in svc.list_tables()]
            with TableServiceClient(endpoint=url, credential=AzureNamedKeyCredential(DEV_ACCOUNT, WRONG_KEY)) as svc:
                with pytest.raises(ClientAuthenticationError):
                    svc.create_table(uniq())
        finally:
            proc.terminate()
            proc.wait(timeout=5)
