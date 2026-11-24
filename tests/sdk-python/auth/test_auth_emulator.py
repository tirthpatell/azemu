"""What only the emulator has (`docs/strict-auth.md`): the admin API, the answer to "why was I refused", the principal of a client id, the mistakes
Resource Manager answers, and the tokens that Azure could not be given (expired)."""

import time
import uuid

import pytest
import requests

from authutil import BLOB_ACCOUNT, GROUP, SUB, unique
from azure.identity import ClientSecretCredential

pytestmark = pytest.mark.emulator_only


def api(world, method, path, **kw):
    return requests.request(method, f"{world.emu.gateway}{path}", timeout=30, **kw)


def test_the_mode_shows_in_the_config_and_the_services_that_check_roles_are_listed(world):
    assert api(world, "GET", "/_azemu/config").json()["strictAuth"] is True
    info = api(world, "GET", "/_azemu/auth").json()
    assert info["strict"] is True
    assert {"blob", "dfs", "queue", "table", "keyvault", "appconfig", "cosmos", "eventgrid"} <= set(info["checksRoles"])
    assert any(e["path"].startswith("/_azemu/auth") for e in api(world, "GET", "/_azemu/").json()["endpoints"])


def test_the_principal_of_a_client_id_is_the_oid_of_its_tokens(world, main):
    p = api(world, "GET", "/_azemu/identity/principal", params={"client_id": main.client_id}).json()
    assert p["objectId"] == main.oid and p["clientId"] == main.client_id
    mi = api(world, "GET", "/_azemu/identity/principal").json()
    assert mi["named"] == "defaultManagedIdentity"
    import base64, json

    claims = json.loads(base64.urlsafe_b64decode(main.token("https://storage.azure.com/.default").split(".")[1] + "=="))
    assert claims["oid"] == main.oid


def test_a_role_given_with_a_short_scope_and_the_check_that_explains_a_refusal(world):
    cid = str(uuid.uuid4())
    oid = api(world, "GET", "/_azemu/identity/principal", params={"client_id": cid}).json()["objectId"]
    r = api(world, "PUT", "/_azemu/auth/assignments", json={"clientId": cid, "role": "Storage Blob Data Reader", "scope": "storage/someaccount/containers/data"})
    assert r.status_code == 201, r.text
    action = "Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read"
    ok = api(world, "GET", "/_azemu/auth/check", params={"principal": oid, "action": action, "scope": "storage/someaccount/containers/data"}).json()
    assert ok["allowed"] is True
    no = api(world, "GET", "/_azemu/auth/check", params={"principal": oid, "action": action, "scope": "storage/someaccount/containers/other"}).json()
    assert no["allowed"] is False
    assert no["lookedAt"][0]["role"] == "Storage Blob Data Reader" and no["lookedAt"][0]["applies"] is False
    assert "none applies" in no["reason"]
    # an action nobody knows is not an error
    assert api(world, "GET", "/_azemu/auth/check", params={"principal": oid, "action": "Contoso/frob/action", "scope": "storage/someaccount"}).json()["allowed"] is False
    assert api(world, "DELETE", "/_azemu/auth/assignments", params={"clientId": cid}).json()["removed"]
    assert api(world, "GET", "/_azemu/auth/assignments", params={"clientId": cid}).json()["value"] == []


def test_a_scope_of_a_resource_group_does_not_reach_an_account_resource_manager_did_not_make(world):
    cid = str(uuid.uuid4())
    r = api(world, "PUT", "/_azemu/auth/assignments", json={"clientId": cid, "role": "Storage Blob Data Reader", "scope": f"/subscriptions/{SUB}/resourceGroups/{GROUP}"})
    assert r.status_code == 201
    oid = api(world, "GET", "/_azemu/identity/principal", params={"client_id": cid}).json()["objectId"]
    action = "Microsoft.Storage/storageAccounts/blobServices/containers/blobs/read"
    made = api(world, "GET", "/_azemu/auth/check", params={"principal": oid, "action": action, "scope": f"storage/{BLOB_ACCOUNT}/containers/x"}).json()
    unknown = api(world, "GET", "/_azemu/auth/check", params={"principal": oid, "action": action, "scope": "storage/accountnobodymade/containers/x"}).json()
    assert made["allowed"] is True and unknown["allowed"] is False
    api(world, "DELETE", "/_azemu/auth/assignments", params={"clientId": cid})


def test_resource_manager_answers_the_common_mistakes(world):
    arm = f"http://management.localhost:{world.emu.port}"
    h = {"Authorization": "Bearer x"}
    base = f"{arm}/subscriptions/{SUB}/resourceGroups/{GROUP}/providers/Microsoft.Authorization/roleAssignments"
    q = {"api-version": "2022-04-01"}
    reader_role = "acdd72a7-3385-48ef-bd42-f606fba81ae7"
    body = lambda role, principal: {"properties": {"roleDefinitionId": f"/subscriptions/{SUB}/providers/Microsoft.Authorization/roleDefinitions/{role}", "principalId": principal}}
    p = str(uuid.uuid4())
    n1 = str(uuid.uuid4())
    assert requests.put(f"{base}/{n1}", params=q, headers=h, json=body(reader_role, p)).status_code == 201
    r = requests.put(f"{base}/{uuid.uuid4()}", params=q, headers=h, json=body(reader_role, p))
    assert (r.status_code, r.json()["error"]["code"]) == (409, "RoleAssignmentExists")
    r = requests.put(f"{base}/{uuid.uuid4()}", params=q, headers=h, json=body(str(uuid.uuid4()), p))
    assert (r.status_code, r.json()["error"]["code"]) == (400, "RoleDefinitionDoesNotExist")
    r = requests.put(f"{base}/{uuid.uuid4()}", params=q, headers=h, json=body(reader_role, "not-a-guid"))
    assert (r.status_code, r.json()["error"]["code"]) == (400, "InvalidRequestContent")
    r = requests.put(f"{arm}/subscriptions/{SUB}/resourceGroups/nosuchgroup/providers/Microsoft.Authorization/roleAssignments/{uuid.uuid4()}", params=q, headers=h, json=body(reader_role, p))
    assert (r.status_code, r.json()["error"]["code"]) == (404, "ResourceGroupNotFound")
    assert requests.delete(f"{base}/{n1}", params=q, headers=h).status_code == 200


def test_a_token_that_has_expired_is_refused(world):
    cred = ClientSecretCredential("tenant", "aaaaaaaa-0000-4000-8000-0000000000ff", "x", authority=f"https://localhost:{world.emu.port}/_azemu/identity/login", disable_instance_discovery=True, connection_verify=world.emu.ca_file)
    t = requests.get(f"{world.emu.gateway}/_azemu/identity/token", params={"resource": "https://storage.azure.com", "expires_in": 1, "client_id": "aaaaaaaa-0000-4000-8000-0000000000ff"}).json()["access_token"]
    time.sleep(2.2)
    r = requests.get(f"{world.blob_url()}/?comp=list", headers={"Authorization": f"Bearer {t}", "x-ms-version": "2024-11-04"}, verify=world.emu.ca_file)
    assert r.status_code == 401 and "Lifetime validation failed" in r.text
    assert cred is not None


def test_requests_without_a_token_and_with_a_key_keep_their_answers(world):
    r = requests.get(f"{world.blob_url()}/?comp=list", headers={"x-ms-version": "2024-11-04"}, verify=world.emu.ca_file)
    assert (r.status_code, r.headers["x-ms-error-code"]) == (401, "NoAuthenticationInformation")
    # a key still signs requests as it did: Shared Key is not touched by strict mode
    from azure.storage.blob import BlobServiceClient

    svc = BlobServiceClient.from_connection_string(world.emu.storage_conn_str())
    svc.create_container(unique("keyc"))
    assert len(list(svc.list_containers())) >= 1
