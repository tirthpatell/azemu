"""The Schema Registry of an Event Hubs namespace with credentials of every kind (`docs/strict-auth.md`). Each test states one row of what Azure answered in
November 2026 (the REST API, `api-version=2023-07-01`). Nothing is registered on Azure: the writes that succeed are checked on the emulator only; every refusal
creates nothing."""

import json
import re

import pytest
import requests

from authutil import sas_token

SCOPE = "https://eventhubs.azure.net/.default"
API = "api-version=2023-07-01"
SCHEMA = json.dumps({"type": "record", "name": "AuthProbe", "namespace": "azemu.auth", "fields": [{"name": "a", "type": "string"}]})
AVRO = {"Content-Type": "application/json; serialization=Avro"}


@pytest.fixture(autouse=True)
def _namespace(world):
    if world.eh is None or world.schema_group is None:
        pytest.skip("no schema group in this run")


def call(world, method, path, token=None, auth=None, **kw):
    h = dict(kw.pop("headers", {}))
    if token:
        h["Authorization"] = f"Bearer {token}"
    if auth:
        h["Authorization"] = auth
    return requests.request(method, f"{world.eh.https}/$schemaGroups{path}{'&' if '?' in path else '?'}{API}", headers=h, timeout=60,
                            verify=True if world.azure else world.emu.ca_file, **kw)


def message(r):
    return re.sub(r" TrackingId:.*", "", r.json()["error"]["message"], flags=re.S)


def refused(r, words):
    assert r.status_code == 401, r.text
    assert r.json()["error"]["code"] == "ErrorInRequest"
    assert message(r) == f"Authorization failed for specified action: {words}.", message(r)


def test_a_principal_with_every_role_reads(world, main):
    t = main.token(SCOPE)
    assert call(world, "GET", "", t).status_code == 200
    assert world.schema_group in call(world, "GET", "", t).json()["Value"]
    assert call(world, "GET", f"/{world.schema_group}/schemas", t).status_code == 200


def test_a_reader_reads_and_a_missing_schema_is_404_not_401(world, reader):
    t = reader.token(SCOPE)
    assert call(world, "GET", "", t).status_code == 200
    assert call(world, "GET", f"/{world.schema_group}", t).status_code == 200
    assert call(world, "GET", f"/{world.schema_group}/schemas", t).status_code == 200
    assert call(world, "GET", f"/{world.schema_group}/schemas/azemu-auth-probe/versions", t).status_code == 200
    assert call(world, "GET", f"/{world.schema_group}/schemas/azemu-auth-probe/versions/1", t).status_code == 404


def test_a_reader_cannot_write_or_delete(world, reader):
    t = reader.token(SCOPE)
    g = world.schema_group
    refused(call(world, "PUT", f"/{g}/schemas/azemu-auth-probe", t, headers=AVRO, data=SCHEMA), "SchemaGroupWrite,SchemaWrite")
    refused(call(world, "DELETE", f"/{g}/schemas/azemu-auth-probe", t), "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete")
    refused(call(world, "DELETE", f"/{g}/schemas/azemu-auth-probe/versions/1", t), "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete")


def test_a_principal_with_no_role_is_refused_everywhere(world, norole):
    t = norole.token(SCOPE)
    g = world.schema_group
    refused(call(world, "PUT", f"/{g}/schemas/azemu-auth-probe", t, headers=AVRO, data=SCHEMA), "SchemaGroupWrite,SchemaWrite")
    refused(call(world, "POST", f"/{g}/schemas/azemu-auth-probe:get-id", t, headers=AVRO, data=SCHEMA), "SchemaGroupRead,SchemaRead")
    refused(call(world, "DELETE", f"/{g}/schemas/azemu-auth-probe", t), "SchemaGroupWrite,SchemaGroupDelete,SchemaDelete")
    refused(call(world, "GET", "", t), "SchemaGroupRead")
    refused(call(world, "GET", f"/{g}", t), "SchemaGroupRead")
    refused(call(world, "GET", f"/{g}/schemas", t), "SchemaGroupRead")
    refused(call(world, "GET", f"/{g}/schemas/azemu-auth-probe/versions", t), "SchemaGroupRead,SchemaRead")
    refused(call(world, "GET", f"/{g}/schemas/azemu-auth-probe/versions/1", t), "SchemaGroupRead,SchemaRead")
    refused(call(world, "GET", "/$schemas/00000000000000000000000000000001", t), "SchemaGroupRead,SchemaRead")


def test_the_refusal_names_the_request_in_its_tracker(world, norole):
    r = call(world, "GET", f"/{world.schema_group}/schemas", norole.token(SCOPE))
    assert re.search(r"SystemTracker:[^,]+\.servicebus\.windows\.net:\$schemaGroups/[^,]+/schemas, Timestamp:", r.json()["error"]["message"])


def test_a_token_that_is_wrong_says_which_way(world, main):
    good = main.token(SCOPE)
    changed = good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB")
    for token, text in (
        (main.token("https://vault.azure.net/.default"), "InvalidAudience: Invalid authorization token audience."),
        (changed, "InvalidSignature: Invalid authorization token signature."),
        ("not-a-token", "MalformedToken: ."),
    ):
        r = call(world, "GET", f"/{world.schema_group}/schemas", token)
        assert (r.status_code, r.json()["error"]["code"], message(r)) == (401, "ErrorInRequest", text)


def test_a_shared_access_signature_is_not_taken_whatever_its_key(world):
    import time

    for cred in (world.eh.root, world.eh.send, world.eh.listen):
        s = sas_token(f"https://{world.eh.host}/", cred.name, cred.key, int(time.time()) + 600)
        r = call(world, "GET", "", auth=s)
        assert r.status_code == 403
        assert r.json()["error"]["code"] == "NotAvaliable"
        assert message(r) == "SubCode=40301, Nullable object must have a value."


@pytest.mark.emulator_only
def test_a_contributor_writes_and_deletes(world, main):
    t = main.token(SCOPE)
    g = world.schema_group
    r = call(world, "PUT", f"/{g}/schemas/azemu-auth-write", t, headers=AVRO, data=SCHEMA)
    assert r.status_code == 204, r.text
    assert call(world, "DELETE", f"/{g}/schemas/azemu-auth-write", t).status_code == 204
