"""Tokens that are wrong, at the storage endpoints (`docs/strict-auth.md`, as Azure answered in November 2026): the answer depends on the
`x-ms-version` of the request, and each endpoint writes it in its own shape. A token that has expired could not be made on Azure (an Entra token
cannot be shortened), so that case is only tested against the emulator (`test_auth_emulator.py`)."""

import re

import pytest
import requests

from authutil import unique

SCOPE = "https://storage.azure.com/.default"
NEW = "2024-11-04"


def get(w, url, token, version=NEW, accept=None):
    h = {"Authorization": f"Bearer {token}", "x-ms-version": version}
    if accept:
        h["Accept"] = accept
    return requests.get(url, headers=h, timeout=60, verify=w.emu.ca_file if w.emu else True)


def detail(r):
    m = re.search(r"<AuthenticationErrorDetail>(.*?)</AuthenticationErrorDetail>", r.text, re.S)
    return m.group(1) if m else None


def tampered(token):
    return token[:-4] + ("AAAA" if not token.endswith("AAAA") else "BBBB")


@pytest.fixture
def urls(world):
    return {"blob": f"{world.blob_url()}/?comp=list", "queue": f"{world.queue_url()}/?comp=list"}


@pytest.mark.parametrize("service", ["blob", "queue"])
def test_a_wrong_audience_is_401_with_a_challenge_for_a_version_from_2019_12_12(world, main, urls, service):
    r = get(world, urls[service], main.token("https://vault.azure.net/.default"))
    assert (r.status_code, r.headers["x-ms-error-code"]) == (401, "InvalidAuthenticationInfo")
    assert re.fullmatch(r"Bearer authorization_uri=https://login\.microsoftonline\.com/[^/]+/oauth2/authorize resource_id=https://storage\.azure\.com", r.headers["WWW-Authenticate"])
    assert "Server failed to authenticate the request. Please refer to the information in the www-authenticate header.\nRequestId:" in r.text
    assert detail(r) == "Audience validation failed. Audience did not match."


@pytest.mark.parametrize("service", ["blob", "queue"])
def test_a_changed_signature_and_a_garbage_token(world, main, urls, service):
    good = main.token(SCOPE)
    r = get(world, urls[service], tampered(good))
    assert (r.status_code, r.headers["x-ms-error-code"], detail(r)) == (401, "InvalidAuthenticationInfo", "Signature validation failed. Signature verification failed.")
    r = get(world, urls[service], "not.a.token")
    assert (r.status_code, detail(r)) == (401, "The access token was missing or malformed.")


def test_a_version_before_2019_12_12_is_403_authentication_failed_without_a_challenge(world, main, urls):
    r = get(world, urls["blob"], main.token("https://vault.azure.net/.default"), version="2019-02-02")
    assert (r.status_code, r.headers["x-ms-error-code"]) == (403, "AuthenticationFailed")
    assert "WWW-Authenticate" not in r.headers
    assert detail(r) == "Audience validation failed. Audience did not match."
    assert "Make sure the value of Authorization header is formed correctly including the signature." in r.text


def test_a_version_before_oauth_says_the_scheme_is_not_supported_and_sends_no_code(world, main, urls):
    for version in ("2017-04-17", "2015-04-05"):
        r = get(world, urls["blob"], main.token(SCOPE), version=version)
        assert r.status_code == 403
        assert "x-ms-error-code" not in r.headers
        assert detail(r) == "Authentication scheme Bearer is not supported in this version."
    assert get(world, urls["blob"], main.token(SCOPE), version="2017-11-09").status_code == 200


def test_the_account_url_is_an_audience_as_well_as_the_service(world, main, urls):
    assert get(world, urls["blob"], main.token(f"{world.blob_url()}/.default")).status_code == 200
    assert get(world, urls["blob"], main.token(SCOPE)).status_code == 200


def test_table_has_no_detail_and_the_python_sdk_version_gets_the_403_form(world, main):
    url = f"{world.table_url()}/Tables"
    wrong = main.token("https://vault.azure.net/.default")
    r = get(world, url, wrong, version="2019-02-02", accept="application/json;odata=nometadata")
    assert r.status_code == 403
    assert r.json()["odata.error"]["code"] == "AuthenticationFailed"
    r = get(world, url, wrong, version="2021-08-06", accept="application/json;odata=nometadata")
    assert r.status_code == 401
    assert r.json()["odata.error"]["code"] == "InvalidAuthenticationInfo"
    assert "Audience" not in r.text
    assert "WWW-Authenticate" in r.headers


def test_data_lake_has_the_detail_in_json(world, main):
    url = f"{world.dfs_url()}/?resource=account"
    r = get(world, url, main.token("https://vault.azure.net/.default"))
    assert r.status_code == 401
    e = r.json()["error"]
    assert e["code"] == "InvalidAuthenticationInfo"
    assert e["detail"]["AuthenticationErrorDetail"] == "Audience validation failed. Audience did not match."


def test_the_sdk_shows_a_wrong_token_as_a_client_authentication_error(world, main):
    from azure.core.credentials import AccessToken
    from azure.core.exceptions import ClientAuthenticationError
    from azure.storage.blob import BlobServiceClient

    class Wrong:
        def get_token(self, *a, **k):
            return AccessToken(main.token("https://vault.azure.net/.default"), 4102444800)

    with pytest.raises(ClientAuthenticationError) as e:
        list(BlobServiceClient(world.blob_url(), credential=Wrong(), **world.sdk_kw()).list_containers())
    assert e.value.status_code == 401
    from authutil import code_of

    assert code_of(e.value) == "InvalidAuthenticationInfo"
