"""What Table Storage answers about the credentials of a request, as real Azure answered it in November 2026.

The emulator does not check bearer tokens and has no role assignments, so the cases below are those that need no key.
Table errors are JSON (`odata.error`) with the Content-Type `application/json`; an unknown `x-ms-version` is the exception.
"""

from datetime import datetime, timedelta, timezone

import pytest
import requests

DEV = "devstoreaccount1"
GOOD_SIG = "A" * 43 + "="
JSON = {"Accept": "application/json;odata=nometadata"}


@pytest.fixture
def get(emu):
    if emu.storage_is_external:
        pytest.skip("raw requests to the emulator's own ports")

    def send(headers=None, query="", account=DEV):
        h = {"Host": f"{account}.table.core.localhost:{emu.port}", **JSON, **(headers or {})}
        return requests.get(f"http://127.0.0.1:{emu.port}/Tables{query}", headers=h, timeout=10)

    return send


def code(r):
    return r.json()["odata.error"]["code"]


def test_no_credentials_is_401_with_the_challenge_in_plain_json(get):
    r = get()
    assert (r.status_code, code(r)) == (401, "NoAuthenticationInformation")
    assert r.headers["Content-Type"] == "application/json"
    assert r.headers["WWW-Authenticate"].startswith("Bearer authorization_uri=https://login.microsoftonline.com/")


def test_no_credentials_with_a_version_from_before_entra_id_is_404(get):
    r = get({"x-ms-version": "2020-10-02"})
    assert (r.status_code, code(r)) == (404, "ResourceNotFound")
    assert get({"x-ms-version": "2020-12-06"}).status_code == 401


@pytest.mark.parametrize("version", ["1999-01-01", "2099-01-01", "banana"])
def test_an_unknown_x_ms_version_is_400_in_the_odata_xml_document(get, version):
    r = get({"x-ms-version": version, "Authorization": "Bearer token"})
    assert r.status_code == 400
    assert r.headers["Content-Type"].startswith("application/xml")
    assert "x-ms-error-code" not in r.headers
    assert '<error xmlns="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">' in r.text
    assert "<code>InvalidHeaderValue</code>" in r.text


@pytest.mark.parametrize("auth", ["Foo", "Bearer", "SharedKey acct:", "SharedKey acct:replay", "SharedKey acct:Sanitized"])
def test_an_authorization_header_that_cannot_be_read_is_400(get, auth):
    r = get({"Authorization": auth})
    assert (r.status_code, code(r)) == (400, "InvalidAuthenticationInfo")


def test_an_unknown_scheme_is_403(get):
    r = get({"Authorization": "Foo bar"})
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")


def test_a_shared_key_request_that_is_too_old_is_refused(get):
    old = (datetime.now(timezone.utc) - timedelta(minutes=16)).strftime("%a, %d %b %Y %H:%M:%S GMT")
    r = get({"Authorization": f"SharedKey someoneelse:{GOOD_SIG}", "x-ms-date": old})
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")
    assert "AuthenticationErrorDetail" not in r.text


@pytest.mark.parametrize("sig, sv", [("", "2021-08-06"), ("Sanitized", "2021-08-06"), ("Zm9vYmFy", "2021-08-06"), (GOOD_SIG, "2099-01-01"), (GOOD_SIG, "banana")])
def test_a_sas_that_is_malformed_is_403(get, sig, sv):
    r = get(query=f"?sv={sv}&ss=t&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig.replace('=', '%3D')}")
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")


def test_sip_that_is_not_the_caller_is_403_authorization_failure(get):
    q = "?sv=2021-08-06&ss=t&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig=" + GOOD_SIG.replace("=", "%3D")
    # (another account than the development account: the signature of that one is compared before anything else)
    r = get(query=q + "&sip=203.0.113.9", account="someoneelse")
    assert (r.status_code, code(r)) == (403, "AuthorizationFailure")
