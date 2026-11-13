"""The token endpoint with the grant and scope forms msal-go and msal-node use.

`grant_type=device_code` (the short form), and a `scope` in every answer that signs a user in
that holds each scope that was asked for (msal-go refuses an answer that leaves one out).
"""

import requests

import idutil

ASKED = "https://storage.azure.com/.default openid offline_access profile"


def _url(emu, path):
    return f"{idutil.host_authority(emu)}/{idutil.TENANT}/oauth2/v2.0/{path}"


def _check(body):
    got = body["scope"].lower().split()
    for want in ASKED.split():
        assert want in got, (want, body["scope"])


def test_short_device_code_grant_gets_the_token_at_once(emu):
    r = requests.post(_url(emu, "devicecode"), data={"client_id": "pub", "scope": ASKED}, verify=emu.ca_file)
    assert r.status_code == 200
    d = r.json()
    assert d["user_code"] in d["message"]
    for grant in ("device_code", "urn:ietf:params:oauth:grant-type:device_code"):
        t = requests.post(
            _url(emu, "token"),
            data={"grant_type": grant, "client_id": "pub", "device_code": d["device_code"]},
            verify=emu.ca_file,
        )
        assert t.status_code == 200, t.text
        _check(t.json())


def test_every_user_grant_lists_the_requested_scopes(emu):
    post = lambda **form: requests.post(_url(emu, "token"), data=form, verify=emu.ca_file)  # noqa: E731
    r = post(grant_type="password", client_id="pub", username="a@b.c", password="p", scope=ASKED)
    assert r.status_code == 200
    _check(r.json())
    refreshed = post(grant_type="refresh_token", client_id="pub", refresh_token=r.json()["refresh_token"], scope=ASKED)
    _check(refreshed.json())
    obo = post(
        grant_type="urn:ietf:params:oauth:grant-type:jwt-bearer",
        client_id="api",
        client_secret="s",
        assertion=r.json()["access_token"],
        requested_token_use="on_behalf_of",
        scope=ASKED,
    )
    assert obo.status_code == 200, obo.text
    _check(obo.json())
    # client credentials: no scope in the answer, as Entra
    cc = post(grant_type="client_credentials", client_id="a", client_secret="s", scope="https://storage.azure.com/.default")
    assert "scope" not in cc.json()
