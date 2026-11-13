"""The host check and the cross-site check of the emulator: what a program may do, and what a web page may not.

Programs send neither `Origin` nor `Sec-Fetch-Site`; a browser sends both. The tests use plain HTTP with chosen headers,
because the SDK never sends them. They run only against an azemu started by the fixture (an external target such as Azurite
has other rules).
"""

import http.client
import json
from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.storage.blob import AccountSasPermissions, CorsRule, ResourceTypes, generate_account_sas

pytestmark = pytest.mark.not_on_azurite("azemu's own access rules")


@pytest.fixture
def raw(emu):
    """`raw(method, path, host, headers)` -> (status, headers, body). The Host header is whatever is asked for."""

    def call(method, path, host, headers=None, body=None):
        c = http.client.HTTPConnection("127.0.0.1", emu.port, timeout=15)
        h = {"Host": host}
        h.update(headers or {})
        c.request(method, path, body=body, headers=h)
        r = c.getresponse()
        data = r.read()
        out = (r.status, {k.lower(): v for k, v in r.getheaders()}, data)
        c.close()
        return out

    return call


@pytest.fixture
def sas(account_name, account_key):
    return generate_account_sas(
        account_name,
        account_key,
        resource_types=ResourceTypes(service=True, container=True, object=True),
        permission=AccountSasPermissions(read=True, write=True, delete=True, list=True, add=True, create=True),
        expiry=datetime.now(timezone.utc) + timedelta(hours=1),
    )


def test_hosts_the_emulator_serves(emu, raw, account_name):
    port = emu.port
    for host in [
        f"127.0.0.1:{port}",
        f"localhost:{port}",
        f"{account_name}.blob.core.localhost:{port}",
        f"{account_name}.dfs.core.windows.net.localhost:{port}",
        f"azemu:{port}",
        f"host.docker.internal:{port}",
    ]:
        status, _, body = raw("GET", "/_azemu/health", host)
        assert status == 200, (host, body)


def test_other_hosts_are_refused_with_the_host_and_the_option_named(emu, raw, account_name):
    for host in [f"evil.example:{emu.port}", f"{account_name}.blob.attacker.example:{emu.port}", f"{account_name}.blob.core.windows.net"]:
        for path in ["/_azemu/health", "/_azemu/reset", "/cont/blob?restype=container"]:
            status, headers, body = raw("GET" if "reset" not in path else "POST", path, host)
            assert status == 403, (host, path, body)
            err = json.loads(body)["error"]
            assert err["code"] == "HostNotAllowed"
            assert host.split(":")[0] in err["message"] and "--allow-host" in err["message"]
            assert not [h for h in headers if h.startswith("access-control-")]


def test_a_page_on_another_site_cannot_reset_or_read_the_admin_paths(emu, raw, service, container, sas):
    container.upload_blob("keep", b"data")
    port = emu.port
    attacks = [
        ("POST", "/_azemu/reset", {"Origin": "http://evil.example", "Content-Type": "text/plain", "Sec-Fetch-Site": "cross-site"}),
        ("POST", "/_azemu/reset", {"Origin": "null"}),
        ("POST", "/_azemu/reset", {"Referer": "https://evil.example/x.html"}),
        ("POST", "/_azemu/reset", {"Sec-Fetch-Site": "cross-site"}),
        ("GET", "/_azemu/health", {"Origin": "https://evil.example"}),
        ("GET", "/_azemu/ca.pem", {"Sec-Fetch-Site": "cross-site", "Sec-Fetch-Mode": "navigate"}),
    ]
    for method, path, headers in attacks:
        status, answer_headers, body = raw(method, path, f"localhost:{port}", headers, b"" if method == "POST" else None)
        assert status == 403, (method, path, headers, body)
        assert json.loads(body)["error"]["code"] == "CrossSiteRequestRefused"
        assert not [h for h in answer_headers if h.startswith("access-control-")]
    # nothing was reset
    assert container.get_blob_client("keep").download_blob().readall() == b"data"
    # a program, and a page on the developer's own machine, are fine
    for headers in ({}, {"Origin": "http://localhost:3000", "Sec-Fetch-Site": "same-site"}, {"Sec-Fetch-Site": "same-origin"}):
        status, _, _ = raw("GET", "/_azemu/health", f"localhost:{port}", headers)
        assert status == 200, headers


def test_a_cross_site_service_request_needs_a_cors_rule_for_its_origin(emu, raw, service, container, sas):
    container.upload_blob("page.txt", b"hello")
    path = f"/{container.container_name}/page.txt?{sas}"
    host = f"{service.account_name}.blob.core.localhost:{emu.port}"
    page = {"Origin": "https://app.example.com", "Sec-Fetch-Site": "cross-site"}
    try:
        status, headers, body = raw("GET", path, host, page)
        assert status == 403, body
        assert json.loads(body)["error"]["code"] == "CrossSiteRequestRefused"
        service.set_service_properties(cors=[CorsRule(["https://app.example.com"], ["GET"], allowed_headers=["x-ms-*"], max_age_in_seconds=60)])
        status, headers, body = raw("GET", path, host, page)
        assert status == 200, body
        assert body == b"hello"
        assert headers["access-control-allow-origin"] == "https://app.example.com"
        # another origin, or a method the rule does not list, is still refused
        assert raw("GET", path, host, {"Origin": "https://evil.example", "Sec-Fetch-Site": "cross-site"})[0] == 403
        assert raw("DELETE", path, host, page)[0] == 403
        # a script that only sets Origin is answered as on Azure: served, with CORS headers only for an origin a rule names
        status, headers, _ = raw("GET", path, host, {"Origin": "https://evil.example"})
        assert status == 200 and "access-control-allow-origin" not in headers
    finally:
        service.set_service_properties(cors=[])


def test_head_of_a_missing_blob_has_the_error_code_and_nothing_about_a_body(emu, service, container, sas):
    r = requests.head(f"{container.url}/nothing?{sas}")
    assert r.status_code == 404
    assert r.headers["x-ms-error-code"] == "BlobNotFound"
    assert "Content-Type" not in r.headers
    assert "Content-Length" not in r.headers
