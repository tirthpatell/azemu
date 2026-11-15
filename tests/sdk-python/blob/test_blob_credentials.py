"""What Blob Storage answers about the credentials of a request, as real Azure answered it in November 2026.

The emulator does not check bearer tokens and has no role assignments, so the cases below are those that need no key.
"""

import re
from datetime import datetime, timedelta, timezone

import pytest
import requests

GOOD_SIG = "A" * 43 + "="


@pytest.fixture
def raw(emu, service):
    if emu.storage_is_external:
        pytest.skip("raw requests to the emulator's own ports")
    base = service.url.rstrip("/")

    def send(method="GET", path="/", headers=None, query="comp=list", account=None, **kw):
        # another account than the development account: the signature of that one is compared before anything else
        url = base if account is None else base.replace("devstoreaccount1", account)
        base_ = url
        sep = "?" if query else ""
        return requests.request(method, f"{base_}{path}{sep}{query}", headers=headers or {}, timeout=10, **kw)

    return send


def code(r):
    return r.headers.get("x-ms-error-code")


def detail(r):
    m = re.search(r"<AuthenticationErrorDetail>(.*?)</AuthenticationErrorDetail>", r.text, re.S)
    return m.group(1).replace("&apos;", "'") if m else ""


def test_an_anonymous_read_of_a_container_that_does_not_exist_is_401_like_a_private_one(raw):
    for path, query in [("/nosuchcontainer/blob", ""), ("/nosuchcontainer", "restype=container"), ("/nosuchcontainer", "restype=container&comp=list")]:
        r = raw(path=path, query=query)
        assert (r.status_code, code(r)) == (401, "NoAuthenticationInformation"), path
        assert r.headers["WWW-Authenticate"].startswith("Bearer authorization_uri=")


def test_an_anonymous_read_with_a_version_from_before_entra_id_is_404(raw):
    r = raw(path="/nosuchcontainer/blob", query="", headers={"x-ms-version": "2019-07-07"})
    assert (r.status_code, code(r)) == (404, "ResourceNotFound")


def test_an_anonymous_put_blob_without_the_blob_type_is_400_before_the_credential(raw):
    r = raw("PUT", "/c/b", query="", data=b"x")
    assert (r.status_code, code(r)) == (400, "MissingRequiredHeader")
    assert "<HeaderName>x-ms-blob-type</HeaderName>" in r.text
    assert raw("PUT", "/c/b", query="", data=b"x", headers={"x-ms-blob-type": "BlockBlob"}).status_code == 401


@pytest.mark.parametrize("version", ["1999-01-01", "2099-01-01", "banana"])
def test_an_unknown_x_ms_version_is_400_invalid_header_value(raw, version):
    r = raw(headers={"x-ms-version": version, "Authorization": "Bearer token"})
    assert (r.status_code, code(r)) == (400, "InvalidHeaderValue")


@pytest.mark.parametrize("auth, status, expected", [
    ("Foo", 400, "InvalidAuthenticationInfo"),
    ("Bearer", 400, "InvalidAuthenticationInfo"),
    ("SharedKey acct:replay", 400, "InvalidAuthenticationInfo"),
    ("SharedKey acct:", 400, "InvalidAuthenticationInfo"),
    ("Foo bar", 403, "AuthenticationFailed"),
])
def test_an_authorization_header_that_cannot_be_used(raw, auth, status, expected):
    r = raw(headers={"Authorization": auth})
    assert (r.status_code, code(r)) == (status, expected)


def test_a_shared_key_request_that_is_too_old_is_refused(raw):
    old = (datetime.now(timezone.utc) - timedelta(minutes=16)).strftime("%a, %d %b %Y %H:%M:%S GMT")
    r = raw(headers={"Authorization": f"SharedKey someoneelse:{GOOD_SIG}", "x-ms-date": old})
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")
    assert detail(r).startswith("Request date header too old: '")


@pytest.mark.parametrize(
    "sig, sv, expected",
    [
        ("", "2021-08-06", "sig is mandatory. Cannot be empty"),
        ("Sanitized", "2021-08-06", "Signature fields not well formed."),
        ("Zm9vYmFy", "2021-08-06", "Signature size is invalid"),
        (GOOD_SIG, "2099-01-01", "SAS field 'sv' is not well formed."),
        (GOOD_SIG, "banana", "SAS field 'sv' is not well formed."),
    ],
)
def test_a_sas_that_is_malformed_is_403_with_azures_detail(raw, sig, sv, expected):
    r = raw(query=f"comp=list&sv={sv}&ss=b&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig.replace('=', '%3D')}")
    assert (r.status_code, code(r), detail(r)) == (403, "AuthenticationFailed", expected)


def test_a_sas_that_expires_before_it_starts_names_the_two_times(raw):
    q = f"comp=list&sv=2021-08-06&ss=b&srt=sco&sp=rl&se=2000-01-01T00%3A00%3A00Z&sig={GOOD_SIG.replace('=', '%3D')}"
    r = raw(query=q, account="someoneelse")
    assert (r.status_code, code(r)) == (403, "AuthenticationFailed")
    assert re.match(r"Signed expiry time \[.*\] has to be after signed start time \[.*\]$", detail(r))
