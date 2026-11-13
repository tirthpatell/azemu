"""The signing key across restarts: a token issued before verifies against the key set after the restart."""

import json
import ssl
import urllib.parse
import urllib.request

import jwt

SCOPE = "https://storage.azure.com/.default"


def token(emu, tenant="contoso"):
    ctx = ssl.create_default_context(cafile=emu.ca_file)
    body = urllib.parse.urlencode({"grant_type": "client_credentials", "client_id": "app-1", "client_secret": "s", "scope": SCOPE}).encode()
    req = urllib.request.Request(f"https://login.localhost:{emu.port}/{tenant}/oauth2/v2.0/token", data=body, method="POST")
    with urllib.request.urlopen(req, context=ctx, timeout=30) as r:
        return json.load(r)["access_token"]


def jwks(emu):
    with urllib.request.urlopen(f"{emu.gateway}/_azemu/identity/login/common/discovery/v2.0/keys", timeout=30) as r:
        return json.load(r)


def verify(emu, tok):
    kid = jwt.get_unverified_header(tok)["kid"]
    key = next(k for k in jwks(emu)["keys"] if k["kid"] == kid)
    return jwt.decode(tok, jwt.PyJWK(key).key, algorithms=["RS256"], audience="https://storage.azure.com", leeway=10)


def test_tokens_issued_before_a_restart_still_verify(persistent):
    p = persistent()
    old = token(p.emu)
    keys_before = jwks(p.emu)
    p.stop()
    p.start()
    assert jwks(p.emu) == keys_before
    assert verify(p.emu, old)["appid"] == "app-1"
    assert jwt.get_unverified_header(token(p.emu))["kid"] == jwt.get_unverified_header(old)["kid"]


def test_the_key_survives_a_kill_after_an_explicit_save(persistent):
    p = persistent()
    old = token(p.emu)
    p.save()
    p.stop(kill=True)
    p.start()
    assert verify(p.emu, old)["appid"] == "app-1"
