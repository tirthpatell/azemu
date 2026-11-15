"""What Queue Storage answers about the credentials of a request, as real Azure answered it in November 2026.

The emulator does not check bearer tokens and has no role assignments, so the cases below are those that need no key.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
import requests
from queue_support import DEV_ACCOUNT

GOOD_SIG = "A" * 43 + "="


@pytest.fixture
def get(emu):
    if emu.storage_is_external:
        pytest.skip("raw requests to the emulator's own ports")

    def send(headers=None, query="comp=list", account=DEV_ACCOUNT):
        h = {"Host": f"{account}.queue.core.localhost:{emu.port}", **(headers or {})}
        return requests.get(f"http://127.0.0.1:{emu.port}/?{query}", headers=h, timeout=10)

    return send


def code(r):
    return r.headers.get("x-ms-error-code")


def detail(r):
    m = re.search(r"<AuthenticationErrorDetail>(.*?)</AuthenticationErrorDetail>", r.text, re.S)
    return m.group(1).replace("&apos;", "'") if m else ""


def test_no_credentials_is_401_with_the_challenge(get):
    r = get()
    assert (r.status_code, code(r)) == (401, "NoAuthenticationInformation")
    assert r.headers["WWW-Authenticate"].startswith("Bearer authorization_uri=https://login.microsoftonline.com/")
    assert "Please refer to the information in the www-authenticate header." in r.text


def test_no_credentials_with_a_version_from_before_entra_id_is_404(get):
    r = get({"x-ms-version": "2019-07-07"})
    assert (r.status_code, code(r)) == (404, "ResourceNotFound")
    assert get({"x-ms-version": "2019-12-12"}).status_code == 401


@pytest.mark.parametrize("version", ["1999-01-01", "2099-01-01", "banana"])
def test_an_unknown_x_ms_version_is_400_invalid_header_value(get, version):
    r = get({"x-ms-version": version, "Authorization": "Bearer token"})
    assert (r.status_code, code(r)) == (400, "InvalidHeaderValue")
    assert f"<HeaderName>x-ms-version</HeaderName><HeaderValue>{version}</HeaderValue>" in r.text


@pytest.mark.parametrize("auth", ["Foo", "Bearer", "SharedKey", "SharedKey acct:", "SharedKey acct:replay", "SharedKey acct:Sanitized"])
def test_an_authorization_header_that_cannot_be_read_is_400(get, auth):
    r = get({"Authorization": auth})
    assert (r.status_code, code(r)) == (400, "InvalidAuthenticationInfo")


def test_an_unknown_scheme_is_403(get):
    r = get({"Authorization": "Foo bar"})
    assert (r.status_code, code(r), detail(r)) == (403, "AuthenticationFailed", "Authentication scheme Foo is not supported")


def test_a_shared_key_request_that_is_too_old_is_refused_and_one_from_the_future_is_not(get):
    now = datetime.now(timezone.utc)
    fmt = "%a, %d %b %Y %H:%M:%S GMT"
    auth = {"Authorization": f"SharedKey someoneelse:{GOOD_SIG}"}
    r = get({**auth, "x-ms-date": (now - timedelta(minutes=16)).strftime(fmt)})
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")
    assert detail(r).startswith("Request date header too old: '")
    # (the signature is not the right one for the development account, so these two go on to the 403 of the signature)
    for when in (now - timedelta(minutes=14), now + timedelta(days=30)):
        r = get({**auth, "x-ms-date": when.strftime(fmt)})
        assert not detail(r).startswith("Request date header"), detail(r)


@pytest.mark.parametrize(
    "sig, sv, expected",
    [
        ("", "2021-08-06", "sig is mandatory. Cannot be empty"),
        ("Sanitized", "2021-08-06", "Signature fields not well formed."),
        ("Zm9vYmFy", "2021-08-06", "Signature size is invalid"),
        (GOOD_SIG, "2099-01-01", "sv is either not in the correct format or is not equal or later than version 2012-02-12"),
        (GOOD_SIG, "banana", "sv is either not in the correct format or is not equal or later than version 2012-02-12"),
    ],
)
def test_a_sas_that_is_malformed_is_403_with_azures_detail(get, sig, sv, expected):
    r = get(query=f"comp=list&sv={sv}&ss=q&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig.replace('=', '%3D')}")
    assert (r.status_code, code(r), detail(r)) == (403, "AuthenticationFailed", expected)


def test_sip_that_is_not_the_caller_is_403_authorization_failure(get):
    q = "comp=list&sv=2021-08-06&ss=q&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig=" + GOOD_SIG.replace("=", "%3D")
    # (another account than the development account: the signature of that one is compared before anything else)
    r = get(query=q + "&sip=203.0.113.9", account="someoneelse")
    assert (r.status_code, code(r)) == (403, "AuthorizationFailure")
    assert get(query=q + "&sip=127.0.0.0-127.255.255.255", account="someoneelse").status_code == 200
