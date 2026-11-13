"""Container access policy: public access level and stored access policies."""

from datetime import datetime, timedelta, timezone

import pytest
import requests
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import AccessPolicy, ContainerSasPermissions, PublicAccess

from blobutil import assert_error


def _t(minutes):
    return (datetime.now(timezone.utc) + timedelta(minutes=minutes)).replace(microsecond=0)


def test_new_container_is_private_and_has_no_policies(container):
    acl = container.get_container_access_policy()
    assert acl["public_access"] is None
    assert acl["signed_identifiers"] == []


@pytest.mark.parametrize("level", ["blob", "container"])
def test_create_container_with_public_access(service, uniq, level):
    c = service.create_container(uniq("c"), public_access=level)
    assert c.get_container_access_policy()["public_access"] == level
    assert c.get_container_properties().public_access == level


def test_set_public_access_then_remove_it(container):
    container.set_container_access_policy(signed_identifiers={}, public_access=PublicAccess.BLOB)
    assert container.get_container_access_policy()["public_access"] == "blob"
    container.set_container_access_policy(signed_identifiers={})
    assert container.get_container_access_policy()["public_access"] is None


def test_signed_identifiers_roundtrip(container):
    start, expiry = _t(-5), _t(60)
    container.set_container_access_policy(
        signed_identifiers={
            "readonly": AccessPolicy(permission=ContainerSasPermissions(read=True), start=start, expiry=expiry),
            "rw": AccessPolicy(permission=ContainerSasPermissions(read=True, write=True), expiry=expiry),
        }
    )
    got = {si.id: si.access_policy for si in container.get_container_access_policy()["signed_identifiers"]}
    assert set(got) == {"readonly", "rw"}
    assert got["readonly"].permission == "r"
    assert got["rw"].permission == "rw"
    # The SDK returns these as the ISO strings the service sent.
    assert str(got["readonly"].start)[:19] == start.strftime("%Y-%m-%dT%H:%M:%S")
    assert str(got["readonly"].expiry)[:19] == expiry.strftime("%Y-%m-%dT%H:%M:%S")


def test_setting_policies_replaces_previous_ones(container):
    ap = AccessPolicy(permission=ContainerSasPermissions(read=True), expiry=_t(10))
    container.set_container_access_policy(signed_identifiers={"one": ap})
    container.set_container_access_policy(signed_identifiers={"two": ap})
    ids = [si.id for si in container.get_container_access_policy()["signed_identifiers"]]
    assert ids == ["two"]


def test_empty_signed_identifiers_clear_all_policies(container):
    ap = AccessPolicy(permission=ContainerSasPermissions(read=True), expiry=_t(10))
    container.set_container_access_policy(signed_identifiers={"one": ap})
    container.set_container_access_policy(signed_identifiers={})
    assert container.get_container_access_policy()["signed_identifiers"] == []


def test_five_signed_identifiers_are_accepted(container):
    ap = AccessPolicy(permission=ContainerSasPermissions(read=True), expiry=_t(10))
    container.set_container_access_policy(signed_identifiers={f"id{i}": ap for i in range(5)})
    assert len(container.get_container_access_policy()["signed_identifiers"]) == 5


def test_setting_access_policy_changes_etag(container):
    before = container.get_container_properties().etag
    res = container.set_container_access_policy(
        signed_identifiers={"x": AccessPolicy(permission=ContainerSasPermissions(read=True), expiry=_t(10))}
    )
    assert res["etag"] != before
    assert container.get_container_properties().etag == res["etag"]


def test_access_policy_on_missing_container(service, uniq):
    from azure.core.exceptions import ResourceNotFoundError

    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        c.get_container_access_policy()
    assert_error(ei, "ContainerNotFound", 404)


def test_anonymous_read_of_blob_in_blob_level_container(service, uniq):
    c = service.create_container(uniq("c"), public_access="blob")
    c.upload_blob("pub.txt", b"public data")
    r = requests.get(c.get_blob_client("pub.txt").url)
    assert r.status_code == 200
    assert r.content == b"public data"


def test_anonymous_blob_level_access_does_not_allow_listing(service, uniq):
    c = service.create_container(uniq("c"), public_access="blob")
    c.upload_blob("pub.txt", b"x")
    r = requests.get(c.url, params={"restype": "container", "comp": "list"})
    assert r.status_code in (401, 403, 404)
    assert "<Name>pub.txt</Name>" not in r.text


def test_anonymous_listing_allowed_on_container_level_access(service, uniq):
    c = service.create_container(uniq("c"), public_access="container")
    c.upload_blob("pub.txt", b"x")
    r = requests.get(c.url, params={"restype": "container", "comp": "list"})
    assert r.status_code == 200
    assert "<Name>pub.txt</Name>" in r.text


@pytest.mark.not_on_azurite(reason="Azurite answers anonymous requests with 403 AuthorizationFailure; Azure sends 401 NoAuthenticationInformation")
def test_anonymous_read_of_private_container_blob_is_rejected(container):
    container.upload_blob("secret.txt", b"nope")
    r = requests.get(container.get_blob_client("secret.txt").url)
    assert r.status_code == 401
    assert r.headers["x-ms-error-code"] == "NoAuthenticationInformation"
    assert "<Code>NoAuthenticationInformation</Code>" in r.text
    assert b"nope" not in r.content


@pytest.mark.not_on_azurite(reason="Azurite answers anonymous requests with 403 AuthorizationFailure; Azure sends 401 NoAuthenticationInformation")
def test_anonymous_write_to_public_container_is_rejected(service, uniq):
    c = service.create_container(uniq("c"), public_access="container")
    r = requests.put(c.get_blob_client("w.txt").url, data=b"x", headers={"x-ms-blob-type": "BlockBlob"})
    assert r.status_code == 401
    assert r.headers["x-ms-error-code"] == "NoAuthenticationInformation"
    assert not c.get_blob_client("w.txt").exists()
