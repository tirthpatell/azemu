"""What ARM stored without emulating, resetting one subscription, ports in answers."""

import json
import ssl
import urllib.error
import urllib.parse
import urllib.request

from conftest import SUB


def admin(emu, method, path):
    req = urllib.request.Request(f"{emu.gateway}{path}", method=method, data=b"" if method == "POST" else None)
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def raw_with_headers(emu, arm_url, method, path, body=None):
    ctx = ssl.create_default_context(cafile=emu.ca_file)
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"{arm_url}{path}?api-version=2024-01-01", data=data, method=method,
                                 headers={"Authorization": "Bearer t", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, context=ctx, timeout=30) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def test_a_resource_type_nobody_emulates_is_stored_and_says_so(emu, arm_url, rg):
    path = f"/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.Web/sites/myfunc"
    status, headers, body = raw_with_headers(emu, arm_url, "PUT", path, {"location": "eastus", "kind": "functionapp"})
    assert status == 201
    assert json.loads(body)["properties"]["provisioningState"] == "Succeeded"
    assert headers.get("x-azemu-stored-only") == "true"
    status, headers, _ = raw_with_headers(emu, arm_url, "GET", path)
    assert status == 200 and headers.get("x-azemu-stored-only") == "true"
    # a resource group is ARM's own; no header
    _s, headers, _b = raw_with_headers(emu, arm_url, "GET", f"/subscriptions/{SUB}/resourceGroups/{rg}")
    assert "x-azemu-stored-only" not in headers
    # the list a test run can check at its end
    status, listing = admin(emu, "GET", "/_azemu/unsupported")
    assert status == 200
    assert {"resourceType": "Microsoft.Web/sites", "count": 1} in listing["storedOnly"]
    admin(emu, "DELETE", "/_azemu/unsupported")
    assert admin(emu, "GET", "/_azemu/unsupported")[1]["storedOnly"] == []


def test_resetting_one_subscription_leaves_the_other(emu, resource_client, uniq):
    from azure.mgmt.resource.resources import ResourceManagementClient

    other_sub = "99999999-8888-7777-6666-555555555555"
    mine = uniq("rg-")
    resource_client.resource_groups.create_or_update(mine, {"location": "eastus"})
    from conftest import FakeCredential

    mgmt = dict(base_url=f"https://management.localhost:{emu.port}", credential_scopes=["https://management.azure.com/.default"],
                connection_verify=emu.ca_file, retry_total=0)
    with ResourceManagementClient(FakeCredential(), other_sub, **mgmt) as c2:
        c2.resource_groups.create_or_update("theirs", {"location": "eastus"})
        status, out = admin(emu, "POST", f"/_azemu/reset?service=arm&account={SUB}")
        assert status == 200 and out["reset"] == ["arm"]
        assert [g.name for g in resource_client.resource_groups.list()] == []
        assert [g.name for g in c2.resource_groups.list()] == ["theirs"]


def test_an_endpoint_in_a_deployment_output_follows_the_port(emu, arm_url, rg, raw):
    template = {
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [],
        "outputs": {"where": {"type": "string", "value": f"[concat('https://acct.blob.core.localhost:', '{emu.port}', '/c')]"}},
    }
    dep = f"/subscriptions/{SUB}/resourcegroups/{rg}/providers/Microsoft.Resources/deployments/d1"
    status, out = raw("PUT", dep, {"properties": {"mode": "Incremental", "template": template}})
    assert status in (200, 201), out
    assert out["properties"]["outputs"]["where"]["value"] == f"https://acct.blob.core.localhost:{emu.port}/c"
    status, got = raw("GET", dep)
    assert got["properties"]["outputs"]["where"]["value"] == f"https://acct.blob.core.localhost:{emu.port}/c"
    assert "azemu-port" not in json.dumps(got)
