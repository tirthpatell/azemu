"""Shared access signatures for Azure Files: share and file service SAS, account SAS, stored access policies, what an expired
or too weak token is told, and what a request with no credential is told.

Written from the REST reference ("Create a service SAS", "Create an account SAS") and azure-storage-file-share 12.27.1.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceNotFoundError
from azure.storage.fileshare import (AccessPolicy, AccountSasPermissions, FileSasPermissions, ResourceTypes, ShareClient,
                                     ShareFileClient, ShareSasPermissions, ShareServiceClient, generate_account_sas,
                                     generate_file_sas, generate_share_sas)

import fileutil
from fileutil import expect


def _expiry(hours=1):
    return datetime.now(timezone.utc) + timedelta(hours=hours)


@pytest.fixture
def key(emu):
    return fileutil.account_key(emu)


@pytest.fixture
def endpoint(emu):
    return fileutil.endpoint(emu)


def test_share_sas_reads_and_lists(emu, share, account, key, endpoint):
    share.get_file_client("a.txt").upload_file(b"hello")
    sas = generate_share_sas(account, share.share_name, key, permission=ShareSasPermissions(read=True, list=True), expiry=_expiry())
    via = ShareClient(endpoint, share.share_name, credential=sas)
    assert [i.name for i in via.list_directories_and_files()] == ["a.txt"]
    assert via.get_file_client("a.txt").download_file().readall() == b"hello"


def test_share_sas_without_write_cannot_write(emu, share, account, key, endpoint):
    sas = generate_share_sas(account, share.share_name, key, permission=ShareSasPermissions(read=True), expiry=_expiry())
    via = ShareClient(endpoint, share.share_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        via.get_file_client("x.txt").create_file(1)
    assert e.value.status_code == 403
    assert e.value.error_code == "AuthorizationPermissionMismatch"


def test_share_sas_with_write_creates_and_deletes(emu, share, account, key, endpoint):
    sas = generate_share_sas(account, share.share_name, key, permission=ShareSasPermissions(read=True, write=True, create=True, delete=True, list=True), expiry=_expiry())
    via = ShareClient(endpoint, share.share_name, credential=sas)
    f = via.get_file_client("w.txt")
    f.create_file(5)
    f.upload_range(b"12345", offset=0, length=5)
    assert share.get_file_client("w.txt").download_file().readall() == b"12345"
    f.delete_file()
    with pytest.raises(ResourceNotFoundError):
        share.get_file_client("w.txt").get_file_properties()


def test_file_sas_covers_that_file_only(emu, share, account, key, endpoint):
    share.get_file_client("one.txt").upload_file(b"1")
    share.get_file_client("two.txt").upload_file(b"2")
    sas = generate_file_sas(account, share.share_name, ["one.txt"], key, permission=FileSasPermissions(read=True), expiry=_expiry())
    one = ShareFileClient(endpoint, share.share_name, "one.txt", credential=sas)
    assert one.download_file().readall() == b"1"
    two = ShareFileClient(endpoint, share.share_name, "two.txt", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        two.download_file().readall()
    assert e.value.status_code == 403
    assert e.value.error_code == "AuthenticationFailed"


def test_file_sas_in_a_directory_with_response_header_overrides(emu, share, account, key, endpoint):
    d = share.create_directory("dir")
    d.upload_file("f.txt", b"content")
    sas = generate_file_sas(account, share.share_name, ["dir", "f.txt"], key, permission=FileSasPermissions(read=True), expiry=_expiry(),
                            content_type="text/override", cache_control="max-age=5")
    f = ShareFileClient(endpoint, share.share_name, "dir/f.txt", credential=sas)
    got = f.download_file()
    assert got.readall() == b"content"
    assert got.properties.content_settings.content_type == "text/override"
    assert got.properties.content_settings.cache_control == "max-age=5"


def test_file_sas_cannot_list_a_directory(emu, share, account, key, endpoint):
    d = share.create_directory("dir")
    d.upload_file("f.txt", b"x")
    sas = generate_file_sas(account, share.share_name, ["dir", "f.txt"], key, permission=FileSasPermissions(read=True), expiry=_expiry())
    via = ShareClient(endpoint, share.share_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(via.list_directories_and_files("dir"))
    assert e.value.status_code == 403


def test_stored_access_policy_gives_the_permissions(emu, share, account, key, endpoint):
    share.get_file_client("p.txt").upload_file(b"policy")
    policy = AccessPolicy(permission=ShareSasPermissions(read=True), start=_expiry(-1), expiry=_expiry(24))
    share.set_share_access_policy({"rd": policy})
    sas = generate_share_sas(account, share.share_name, key, policy_id="rd")
    via = ShareClient(endpoint, share.share_name, credential=sas)
    assert via.get_file_client("p.txt").download_file().readall() == b"policy"
    with pytest.raises(HttpResponseError) as e:
        via.get_file_client("q.txt").create_file(1)
    assert e.value.status_code == 403


def test_a_policy_that_is_not_there(emu, share, account, key, endpoint):
    sas = generate_share_sas(account, share.share_name, key, policy_id="missing")
    via = ShareClient(endpoint, share.share_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(via.list_directories_and_files())
    assert e.value.status_code == 403
    assert e.value.error_code == "AuthenticationFailed"


def test_expired_sas(emu, share, account, key, endpoint):
    sas = generate_share_sas(account, share.share_name, key, permission=ShareSasPermissions(read=True, list=True),
                             expiry=datetime.now(timezone.utc) - timedelta(hours=1), start=datetime.now(timezone.utc) - timedelta(hours=2))
    via = ShareClient(endpoint, share.share_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(via.list_directories_and_files())
    assert e.value.status_code == 403
    assert e.value.error_code == "AuthenticationFailed"
    assert "Signature not valid in the specified time frame" in str(e.value)


def test_account_sas_for_the_file_service(emu, share, account, key, endpoint):
    sas = generate_account_sas(account, key, resource_types=ResourceTypes(service=True, container=True, object=True),
                               permission=AccountSasPermissions(read=True, write=True, list=True, create=True, delete=True), expiry=_expiry())
    svc = ShareServiceClient(endpoint, credential=sas)
    assert share.share_name in [s.name for s in svc.list_shares(name_starts_with=share.share_name)]
    f = svc.get_share_client(share.share_name).get_file_client("acct.txt")
    f.upload_file(b"by account sas")
    assert f.download_file().readall() == b"by account sas"


def test_account_sas_for_another_service_is_refused(emu, share, account, key, endpoint):
    sas = generate_account_sas(account, key, resource_types=ResourceTypes(service=True, container=True, object=True),
                               permission=AccountSasPermissions(read=True, list=True), expiry=_expiry())
    # the SDK builds an account SAS for the services of its own client, so take the blob one by changing `ss`
    assert "ss=f" in sas
    changed = sas.replace("ss=f", "ss=b")
    svc = ShareServiceClient(endpoint, credential=changed)
    with pytest.raises(HttpResponseError) as e:
        list(svc.list_shares())
    assert e.value.status_code == 403


def test_account_sas_without_the_resource_type(emu, share, account, key, endpoint):
    sas = generate_account_sas(account, key, resource_types=ResourceTypes(service=True), permission=AccountSasPermissions(read=True, list=True), expiry=_expiry())
    via = ShareClient(endpoint, share.share_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(via.list_directories_and_files())
    assert e.value.status_code == 403


def test_no_credential_is_401_with_a_challenge(emu, share, endpoint):
    raw = fileutil.Raw(emu)
    r = raw.call("GET", f"/{share.share_name}", [("restype", "share")], sign=False)
    assert r.status_code == 401
    assert r.headers["x-ms-error-code"] == "NoAuthenticationInformation"
    assert r.headers["WWW-Authenticate"].startswith("Bearer authorization_uri=")


def test_old_version_without_credentials_is_404(emu, share):
    # checked against Azure in November 2026: before 2019-12-12 an anonymous caller learns nothing
    raw = fileutil.Raw(emu)
    r = raw.call("GET", f"/{share.share_name}", [("restype", "share")], sign=False, version="2018-11-09")
    assert (r.status_code, r.headers["x-ms-error-code"]) == (404, "ResourceNotFound")


def test_wrong_key_is_403(emu, share, account, endpoint):
    wrong = ShareClient(endpoint, share.share_name, credential={"account_name": account, "account_key": "d3Jvbmc=" * 8})
    if not emu.azure and account != fileutil.DEV_ACCOUNT:
        pytest.skip("only the keys of devstoreaccount1 are compared")
    with pytest.raises(ClientAuthenticationError) as e:
        wrong.get_share_properties()
    assert e.value.status_code == 403
