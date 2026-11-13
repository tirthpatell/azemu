"""Shared access signatures: account, container and blob SAS, stored policies."""

from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.storage.blob import (
    AccessPolicy,
    AccountSasPermissions,
    BlobClient,
    BlobSasPermissions,
    BlobServiceClient,
    ContainerClient,
    ContainerSasPermissions,
    ResourceTypes,
    generate_account_sas,
    generate_blob_sas,
    generate_container_sas,
)

from blobutil import assert_error


def _in(minutes):
    return datetime.now(timezone.utc) + timedelta(minutes=minutes)


@pytest.fixture
def acct_sas(account_name, account_key):
    def make(permission, resource_types=None, expiry=None, start=None, **kw):
        return generate_account_sas(
            account_name,
            account_key,
            resource_types=resource_types or ResourceTypes(service=True, container=True, object=True),
            permission=permission,
            expiry=expiry or _in(60),
            start=start,
            **kw,
        )

    return make


@pytest.fixture
def cont_sas(account_name, account_key):
    def make(container_name, permission=None, expiry=None, start=None, **kw):
        return generate_container_sas(
            account_name,
            container_name,
            account_key=account_key,
            permission=permission,
            expiry=expiry or _in(60),
            start=start,
            **kw,
        )

    return make


@pytest.fixture
def blob_sas(account_name, account_key):
    def make(container_name, blob_name, permission=None, expiry=None, start=None, **kw):
        return generate_blob_sas(
            account_name,
            container_name,
            blob_name,
            account_key=account_key,
            permission=permission,
            expiry=expiry or _in(60),
            start=start,
            **kw,
        )

    return make


# --- account SAS -----------------------------------------------------------


def test_account_sas_full_access_roundtrip(service, acct_sas, uniq):
    sas = acct_sas(AccountSasPermissions(read=True, write=True, create=True, list=True, delete=True))
    svc = BlobServiceClient(service.url, credential=sas)
    c = svc.create_container(uniq("c"))
    c.upload_blob("b.txt", b"via account sas")
    assert c.get_blob_client("b.txt").download_blob().readall() == b"via account sas"
    assert [b.name for b in c.list_blobs()] == ["b.txt"]
    c.delete_blob("b.txt")
    assert list(c.list_blobs()) == []
    svc.delete_container(c.container_name)


def test_account_sas_lists_containers(service, container, acct_sas):
    svc = BlobServiceClient(service.url, credential=acct_sas(AccountSasPermissions(list=True)))
    assert container.container_name in [c.name for c in svc.list_containers()]


def test_account_sas_read_only_cannot_write(service, container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True))
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        c.upload_blob("nope", b"x")
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


def test_account_sas_read_only_can_read(container, acct_sas):
    container.upload_blob("r.txt", b"readable")
    b = BlobClient.from_blob_url(container.get_blob_client("r.txt").url, credential=acct_sas(AccountSasPermissions(read=True)))
    assert b.download_blob().readall() == b"readable"


def test_account_sas_without_delete_cannot_delete(container, acct_sas):
    container.upload_blob("d.txt", b"x")
    c = ContainerClient.from_container_url(container.url, credential=acct_sas(AccountSasPermissions(read=True, write=True)))
    with pytest.raises(HttpResponseError) as ei:
        c.delete_blob("d.txt")
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


def test_account_sas_object_only_cannot_list_containers(service, acct_sas):
    sas = acct_sas(AccountSasPermissions(list=True), resource_types=ResourceTypes(object=True))
    svc = BlobServiceClient(service.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(svc.list_containers())
    assert_error(ei, "AuthorizationResourceTypeMismatch", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_account_sas_expired_is_authentication_failed(service, container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True), expiry=_in(-10), start=_in(-60))
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_account_sas_not_yet_valid_is_authentication_failed(service, container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True), start=_in(30), expiry=_in(90))
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_account_sas_with_tampered_signature_is_rejected(service, container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True))
    head, _, sig = sas.partition("sig=")
    bad = head + "sig=" + ("A" if not sig.startswith("A") else "B") + sig[1:]
    c = ContainerClient.from_container_url(container.url, credential=bad)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


def test_account_sas_create_only_cannot_overwrite(container, acct_sas):
    container.upload_blob("exists", b"old")
    c = ContainerClient.from_container_url(container.url, credential=acct_sas(AccountSasPermissions(create=True, read=True)))
    c.upload_blob("fresh", b"new")
    with pytest.raises(HttpResponseError) as ei:
        c.upload_blob("exists", b"overwrite", overwrite=True)
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


def test_account_sas_https_only_rejects_http(container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True), protocol="https")
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthorizationProtocolMismatch", 403)


@pytest.mark.not_on_azurite(reason="Azurite does not enforce the SAS signed IP (sip) restriction")
def test_account_sas_ip_range_excluding_client_is_rejected(container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True), ip="203.0.113.7")
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthorizationSourceIPMismatch", 403)


def test_account_sas_ip_range_including_client_is_accepted(container, acct_sas):
    sas = acct_sas(AccountSasPermissions(read=True, list=True), ip="127.0.0.0-127.255.255.255")
    c = ContainerClient.from_container_url(container.url, credential=sas)
    assert list(c.list_blobs()) == []


# --- container SAS ---------------------------------------------------------


def test_container_sas_read_list_write_delete(container, cont_sas):
    sas = cont_sas(
        container.container_name,
        ContainerSasPermissions(read=True, write=True, list=True, delete=True, create=True),
    )
    c = ContainerClient.from_container_url(container.url, credential=sas)
    c.upload_blob("x", b"container sas")
    assert [b.name for b in c.list_blobs()] == ["x"]
    assert c.download_blob("x").readall() == b"container sas"
    c.delete_blob("x")
    assert list(c.list_blobs()) == []


def test_container_sas_without_list_cannot_list(container, cont_sas):
    c = ContainerClient.from_container_url(container.url, credential=cont_sas(container.container_name, ContainerSasPermissions(read=True)))
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


def test_container_sas_read_only_cannot_write(container, cont_sas):
    c = ContainerClient.from_container_url(container.url, credential=cont_sas(container.container_name, ContainerSasPermissions(read=True)))
    with pytest.raises(HttpResponseError) as ei:
        c.upload_blob("w", b"x")
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_container_sas_for_another_container_is_rejected(container, make_container, cont_sas):
    other = make_container()
    sas = cont_sas(other.container_name, ContainerSasPermissions(read=True, list=True))
    c = ContainerClient.from_container_url(container.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(c.list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


def test_container_sas_cannot_list_containers(service, container, cont_sas):
    sas = cont_sas(container.container_name, ContainerSasPermissions(read=True, list=True))
    svc = BlobServiceClient(service.url, credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        list(svc.list_containers())
    assert ei.value.status_code == 403


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_container_sas_expired(container, cont_sas):
    sas = cont_sas(container.container_name, ContainerSasPermissions(read=True, list=True), expiry=_in(-5), start=_in(-30))
    with pytest.raises(HttpResponseError) as ei:
        list(ContainerClient.from_container_url(container.url, credential=sas).list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


def test_stored_access_policy_grants_its_permissions(container, cont_sas):
    container.upload_blob("p.txt", b"policy data")
    container.set_container_access_policy(
        signed_identifiers={
            "readonly": AccessPolicy(permission=ContainerSasPermissions(read=True, list=True), expiry=_in(60), start=_in(-5))
        }
    )
    sas = generate_container_sas(
        container.account_name,
        container.container_name,
        account_key=container.credential.account_key,
        policy_id="readonly",
    )
    c = ContainerClient.from_container_url(container.url, credential=sas)
    assert [b.name for b in c.list_blobs()] == ["p.txt"]
    assert c.download_blob("p.txt").readall() == b"policy data"
    with pytest.raises(HttpResponseError) as ei:
        c.upload_blob("w", b"x")
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_sas_referencing_unknown_policy_is_rejected(container):
    sas = generate_container_sas(
        container.account_name,
        container.container_name,
        account_key=container.credential.account_key,
        policy_id="no-such-policy",
    )
    with pytest.raises(HttpResponseError) as ei:
        list(ContainerClient.from_container_url(container.url, credential=sas).list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_stored_access_policy_expired(container):
    container.set_container_access_policy(
        signed_identifiers={
            "old": AccessPolicy(permission=ContainerSasPermissions(read=True, list=True), expiry=_in(-5), start=_in(-60))
        }
    )
    sas = generate_container_sas(
        container.account_name, container.container_name, account_key=container.credential.account_key, policy_id="old"
    )
    with pytest.raises(HttpResponseError) as ei:
        list(ContainerClient.from_container_url(container.url, credential=sas).list_blobs())
    assert_error(ei, "AuthenticationFailed", 403)


# --- blob SAS --------------------------------------------------------------


def test_blob_sas_read(container, blob_sas):
    container.upload_blob("one", b"blob sas read")
    sas = blob_sas(container.container_name, "one", BlobSasPermissions(read=True))
    b = BlobClient.from_blob_url(f"{container.url}/one", credential=sas)
    assert b.download_blob().readall() == b"blob sas read"
    assert b.get_blob_properties().size == len(b"blob sas read")


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_blob_sas_does_not_cover_other_blobs(container, blob_sas):
    container.upload_blob("one", b"1")
    container.upload_blob("two", b"2")
    sas = blob_sas(container.container_name, "one", BlobSasPermissions(read=True))
    other = BlobClient.from_blob_url(f"{container.url}/two", credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        other.download_blob()
    assert_error(ei, "AuthenticationFailed", 403)


def test_blob_sas_read_only_cannot_write(container, blob_sas):
    container.upload_blob("one", b"1")
    sas = blob_sas(container.container_name, "one", BlobSasPermissions(read=True))
    b = BlobClient.from_blob_url(f"{container.url}/one", credential=sas)
    with pytest.raises(HttpResponseError) as ei:
        b.upload_blob(b"2", overwrite=True)
    assert_error(ei, "AuthorizationPermissionMismatch", 403)
    assert container.download_blob("one").readall() == b"1"


def test_blob_sas_write_creates_blob(container, blob_sas):
    sas = blob_sas(container.container_name, "new", BlobSasPermissions(write=True, create=True))
    BlobClient.from_blob_url(f"{container.url}/new", credential=sas).upload_blob(b"written")
    assert container.download_blob("new").readall() == b"written"


def test_blob_sas_delete(container, blob_sas):
    container.upload_blob("gone", b"x")
    sas = blob_sas(container.container_name, "gone", BlobSasPermissions(delete=True))
    BlobClient.from_blob_url(f"{container.url}/gone", credential=sas).delete_blob()
    assert not container.get_blob_client("gone").exists()


def test_blob_sas_without_delete_permission(container, blob_sas):
    container.upload_blob("keep", b"x")
    sas = blob_sas(container.container_name, "keep", BlobSasPermissions(read=True, write=True))
    with pytest.raises(HttpResponseError) as ei:
        BlobClient.from_blob_url(f"{container.url}/keep", credential=sas).delete_blob()
    assert_error(ei, "AuthorizationPermissionMismatch", 403)


@pytest.mark.not_on_azurite(reason="Azurite answers a failed SAS validation with AuthorizationFailure; Azure answers AuthenticationFailed")
def test_blob_sas_expired(container, blob_sas):
    container.upload_blob("one", b"1")
    sas = blob_sas(container.container_name, "one", BlobSasPermissions(read=True), expiry=_in(-5), start=_in(-30))
    with pytest.raises(HttpResponseError) as ei:
        BlobClient.from_blob_url(f"{container.url}/one", credential=sas).download_blob()
    assert_error(ei, "AuthenticationFailed", 403)


def test_blob_sas_on_missing_blob_is_not_found(container, blob_sas):
    sas = blob_sas(container.container_name, "ghost", BlobSasPermissions(read=True))
    with pytest.raises(ResourceNotFoundError) as ei:
        BlobClient.from_blob_url(f"{container.url}/ghost", credential=sas).download_blob()
    assert_error(ei, "BlobNotFound", 404)


def test_blob_sas_with_special_character_name(container, blob_sas):
    name = "dir with space/é#?+.txt"
    container.upload_blob(name, b"odd")
    sas = blob_sas(container.container_name, name, BlobSasPermissions(read=True))
    b = BlobClient(container.url.rsplit("/", 1)[0], container.container_name, name, credential=sas)
    assert b.download_blob().readall() == b"odd"


def test_blob_sas_for_snapshot(service, container, blob_sas):
    b = container.get_blob_client("snapped")
    b.upload_blob(b"old")
    snap = b.create_snapshot()["snapshot"]
    b.upload_blob(b"new", overwrite=True)
    sas = blob_sas(container.container_name, "snapped", BlobSasPermissions(read=True), snapshot=snap)
    sc = BlobClient.from_blob_url(f"{container.url}/snapped?snapshot={snap}", credential=sas)
    assert sc.download_blob().readall() == b"old"


def test_sas_response_header_overrides(container, blob_sas):
    container.upload_blob("hdr", b"abc")
    sas = blob_sas(
        container.container_name,
        "hdr",
        BlobSasPermissions(read=True),
        content_type="text/x-test",
        content_disposition="attachment; filename=x.txt",
        cache_control="no-store",
        content_language="de",
        content_encoding="identity",
    )
    r = requests.get(f"{container.url}/hdr?{sas}")
    assert r.status_code == 200
    assert r.headers["Content-Type"] == "text/x-test"
    assert r.headers["Content-Disposition"] == "attachment; filename=x.txt"
    assert r.headers["Cache-Control"] == "no-store"
    assert r.headers["Content-Language"] == "de"
    assert r.content == b"abc"


def test_sas_works_for_plain_http_clients(container, blob_sas):
    container.upload_blob("raw", b"raw bytes")
    sas = blob_sas(container.container_name, "raw", BlobSasPermissions(read=True))
    r = requests.get(f"{container.url}/raw?{sas}")
    assert (r.status_code, r.content) == (200, b"raw bytes")


# --- a SAS with a newer `sv` than the emulator knew (the Go SDK signs with 2026-12-06) ---------------------


@pytest.mark.not_on_azurite(reason="Azurite does not check the signature of a version it does not know")
@pytest.mark.parametrize("sv", ["2026-12-06", "2031-01-01"])
def test_blob_sas_with_a_newer_service_version_is_checked(container, account_name, account_key, sv):
    import base64
    import hashlib
    import hmac
    from urllib.parse import quote

    container.upload_blob("b", b"hello")
    se = "2099-01-01T00:00:00Z"
    # string to sign of the 2026-04-06 layout, which every newer version uses
    sts = f"r\n\n{se}\n/blob/{account_name}/{container.container_name}/b\n\n\n\n{sv}\nb\n\n\n\n\n\n\n"
    sig = base64.b64encode(hmac.new(base64.b64decode(account_key), sts.encode(), hashlib.sha256).digest()).decode()
    qs = lambda sp: f"sv={sv}&sr=b&sp={sp}&se={quote(se, safe='')}&sig={quote(sig, safe='')}"
    ok = requests.get(f"{container.url}/b?{qs('r')}")
    assert ok.status_code == 200 and ok.content == b"hello"
    # the permission was changed after signing: refused, not accepted unchecked
    bad = requests.get(f"{container.url}/b?{qs('rwd')}")
    assert bad.status_code == 403
    assert bad.headers["x-ms-error-code"] == "AuthenticationFailed"
