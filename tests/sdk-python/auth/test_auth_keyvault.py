"""Key Vault with Entra tokens, as Azure answered in November 2026 (`docs/strict-auth.md`): a vault with role-based access and a vault with
access policies. Every refusal is 403 `Forbidden`; the inner code and the text tell the kind of vault."""

import re

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.keyvault.keys import KeyClient
from azure.keyvault.secrets import SecretClient

from authutil import unique

API = "api-version=7.5"
RBAC_TEXT = "Caller is not authorized to perform action on resource.\r\nIf role assignments, deny assignments or role definitions were changed recently, please observe propagation time.\r\nCaller: appid="


def secret_client(w, who, policy=False):
    return SecretClient(w.vault_url(policy), who, **w.sdk_kw())


def raw_kv(w, who, method, path, policy=False, body=None):
    return requests.request(
        method,
        f"{w.vault_url(policy)}{path}{'&' if '?' in path else '?'}{API}",
        headers={"Authorization": f"Bearer {who.token('https://vault.azure.net/.default')}"},
        json=body,
        timeout=60,
        verify=w.emu.ca_file if w.emu else True,
    )


@pytest.fixture(scope="module")
def vault(world, main):
    """Secrets made by main in both vaults, and removed (purged) at the end."""
    mine = secret_client(world, main)
    pol = secret_client(world, main, policy=True)
    names = {"a": unique("sec"), "gone": unique("sec"), "pa": unique("sec")}
    mine.set_secret(names["a"], "v1")
    mine.set_secret(names["gone"], "v1")
    mine.begin_delete_secret(names["gone"]).wait()
    pol.set_secret(names["pa"], "p1")
    yield names
    for client, n in ((mine, names["a"]), (mine, names["gone"]), (pol, names["pa"])):
        try:
            client.begin_delete_secret(n).wait()
        except Exception:
            pass
        try:
            client.purge_deleted_secret(n)
        except Exception:
            pass


def forbidden(fn):
    with pytest.raises(HttpResponseError) as e:
        fn()
    assert e.value.status_code == 403
    assert e.value.error.code == "Forbidden"
    return e.value


def test_norole_is_refused_with_the_action_and_the_resource_named(world, norole, vault):
    n = vault["a"]
    r = raw_kv(world, norole, "GET", f"/secrets/{n}")
    assert r.status_code == 403
    j = r.json()["error"]
    assert (j["code"], j["innererror"]["code"]) == ("Forbidden", "ForbiddenByRbac")
    m = j["message"]
    assert m.startswith(RBAC_TEXT + norole.client_id + ";oid=")
    assert "Action: 'Microsoft.KeyVault/vaults/secrets/getSecret/action'\r\n" in m
    assert re.search(rf"Resource: '/subscriptions/[^/]+/resourcegroups/[^/]+/providers/microsoft\.keyvault/vaults/[^/]+/secrets/{n}'\r\nAssignment: \(not found\)\r\nDenyAssignmentId: null\r\nDecisionReason: null \r\nVault: [^;]+;location=\w+\r\n$", m), m
    # a missing secret is the same answer, not a 404
    assert raw_kv(world, norole, "GET", f"/secrets/{unique('nothere')}").status_code == 403


@pytest.mark.parametrize(
    "method, path, action, names_object",
    [
        ("GET", "/secrets", "secrets/readMetadata/action", False),
        ("GET", "/deletedsecrets", "secrets/readMetadata/action", False),
        ("GET", "/secrets/{n}/versions", "secrets/readMetadata/action", True),
        ("PUT", "/secrets/{n}", "secrets/setSecret/action", True),
        ("PATCH", "/secrets/{n}", "secrets/update/action", True),
        ("DELETE", "/secrets/{n}", "secrets/delete", True),
        ("POST", "/secrets/{n}/backup", "secrets/backup/action", True),
        ("GET", "/keys", "keys/read", False),
        ("GET", "/certificates", "certificates/read", False),
    ],
)
def test_the_action_names_of_the_role_based_vault(world, norole, vault, method, path, action, names_object):
    n = vault["a"]
    r = raw_kv(world, norole, method, path.format(n=n), body={"value": "x"} if method in ("PUT", "PATCH") else None)
    assert r.status_code == 403, r.text
    m = r.json()["error"]["message"]
    assert f"Action: 'Microsoft.KeyVault/vaults/{action}'\r\n" in m
    resource = re.search(r"Resource: '([^']*)'", m).group(1)
    assert resource.endswith(f"/secrets/{n}") == names_object


def test_secrets_user_reads_lists_and_reads_deleted_ones(world, reader, vault):
    c = secret_client(world, reader)
    assert c.get_secret(vault["a"]).value == "v1"
    assert vault["a"] in [s.name for s in c.list_properties_of_secrets()]
    assert len(list(c.list_properties_of_secret_versions(vault["a"]))) == 1
    # Azure lets a Secrets User read a deleted secret, with its value, and list the deleted ones
    assert c.get_deleted_secret(vault["gone"]).name == vault["gone"]
    assert vault["gone"] in [s.name for s in c.list_deleted_secrets()]
    with pytest.raises(ResourceNotFoundError) as e:
        c.get_secret(unique("nothere"))
    assert e.value.error.code == "SecretNotFound"


def test_secrets_user_may_not_write_delete_or_touch_keys_and_certificates(world, reader, vault):
    c = secret_client(world, reader)
    n = vault["a"]
    for call in (
        lambda: c.set_secret(unique("sec"), "x"),
        lambda: c.update_secret_properties(n, content_type="x"),
        lambda: c.begin_delete_secret(n),
        lambda: c.begin_delete_secret(unique("nothere")),
        lambda: c.begin_recover_deleted_secret(vault["gone"]),
        lambda: c.purge_deleted_secret(vault["gone"]),
        lambda: c.backup_secret(n),
        lambda: list(KeyClient(world.vault_url(), reader, **world.sdk_kw()).list_properties_of_keys()),
    ):
        e = forbidden(call)
        assert "(Forbidden) Caller is not authorized to perform action on resource." in str(e)


def test_a_role_on_one_secret_reaches_that_secret_and_its_versions_only(world, scoped, main, vault):
    c = secret_client(world, scoped)
    own = world.scoped_secret
    # the secret the scoped principal may read exists on Azure already; on the emulator main makes it
    if not world.azure:
        secret_client(world, main).set_secret(own, "mine")
    assert c.get_secret(own).name == own
    assert len(list(c.list_properties_of_secret_versions(own))) >= 1
    for call in (
        lambda: c.get_secret(vault["a"]),
        lambda: c.get_secret(unique("nothere")),
        lambda: list(c.list_properties_of_secrets()),
        lambda: list(c.list_properties_of_secret_versions(vault["a"])),
        lambda: c.set_secret(own, "x"),
        lambda: c.backup_secret(own),
        lambda: c.get_deleted_secret(vault["gone"]),
    ):
        forbidden(call)
    m = raw_kv(world, scoped, "GET", "/secrets").json()["error"]["message"]
    assert "Action: 'Microsoft.KeyVault/vaults/secrets/readMetadata/action'" in m
    assert not re.search(r"Resource: '[^']*/secrets/", m)


# -- the vault with access policies -----------------------------------------------------------------------------------

def test_policy_vault_tells_a_missing_entry_from_a_missing_permission(world, reader, norole, vault):
    n = vault["pa"]
    assert secret_client(world, reader, policy=True).get_secret(n).value == "p1"
    # an entry that lacks the permission: ForbiddenByPolicy
    r = raw_kv(world, reader, "PUT", f"/secrets/{unique('s')}", policy=True, body={"value": "x"})
    assert r.status_code == 403
    j = r.json()["error"]
    assert (j["code"], j["innererror"]["code"]) == ("Forbidden", "ForbiddenByPolicy")
    assert re.fullmatch(
        r"The user, group or application 'appid=[^;]+;oid=[^;]+;iss=https://sts\.windows\.net/[^/]+/' does not have secrets set permission on key vault '[^;]+;location=\w+'\. For help resolving this issue, please see https://go\.microsoft\.com/fwlink/\?linkid=2125287",
        j["message"],
    ), j["message"]
    # no entry at all: AccessDenied
    r = raw_kv(world, norole, "GET", f"/secrets/{n}", policy=True)
    assert r.status_code == 403
    assert r.json()["error"]["innererror"]["code"] == "AccessDenied"
    assert "does not have secrets get permission" in r.json()["error"]["message"]


def test_policy_vault_update_needs_the_set_permission_and_keys_need_their_own(world, reader, vault):
    r = raw_kv(world, reader, "PATCH", f"/secrets/{vault['pa']}", policy=True, body={"attributes": {"enabled": True}})
    assert "does not have secrets set permission" in r.json()["error"]["message"]
    r = raw_kv(world, reader, "GET", "/keys", policy=True)
    assert r.status_code == 403 and "does not have keys list permission" in r.json()["error"]["message"]
    c = secret_client(world, reader, policy=True)
    assert vault["pa"] in [s.name for s in c.list_properties_of_secrets()]
