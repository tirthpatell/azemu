"""Container create / delete / exists / properties / metadata / names."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError

from blobutil import assert_error


def test_create_container_returns_etag_and_last_modified(service, uniq):
    c = service.get_container_client(uniq("c"))
    res = c.create_container()
    assert res["etag"].startswith('"') and res["etag"].endswith('"')
    assert res["last_modified"] is not None


def test_create_existing_container_conflicts(service, container):
    with pytest.raises(ResourceExistsError) as ei:
        service.create_container(container.container_name)
    assert_error(ei, "ContainerAlreadyExists", 409)


def test_create_container_on_client_conflicts(container):
    with pytest.raises(ResourceExistsError) as ei:
        container.create_container()
    assert_error(ei, "ContainerAlreadyExists", 409)


def test_delete_missing_container_is_not_found(service, uniq):
    with pytest.raises(ResourceNotFoundError) as ei:
        service.delete_container(uniq("c"))
    assert_error(ei, "ContainerNotFound", 404)


def test_delete_container_removes_it_from_listing(service, uniq):
    name = uniq("c")
    service.create_container(name)
    service.delete_container(name)
    assert name not in [c.name for c in service.list_containers(name_starts_with=name)]


def test_exists_false_for_missing_true_after_create(service, uniq):
    c = service.get_container_client(uniq("c"))
    assert c.exists() is False
    c.create_container()
    assert c.exists() is True


def test_exists_false_after_delete(container):
    container.delete_container()
    assert container.exists() is False


def test_get_properties_of_missing_container_is_not_found(service, uniq):
    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        c.get_container_properties()
    assert_error(ei, "ContainerNotFound", 404)


def test_get_container_properties_fields(container):
    p = container.get_container_properties()
    assert p.name == container.container_name
    assert p.etag.startswith('"')
    assert p.last_modified is not None
    assert p.public_access is None
    assert p.lease.state == "available"
    assert p.lease.status == "unlocked"
    assert p.has_immutability_policy is False
    assert p.has_legal_hold is False
    assert not p.metadata


@pytest.mark.not_on_azurite(reason="Azurite does not send x-ms-default-encryption-scope / x-ms-deny-encryption-scope-override")
def test_container_properties_report_default_encryption_scope(container):
    p = container.get_container_properties()
    assert p.encryption_scope.default_encryption_scope == "$account-encryption-key"
    assert p.encryption_scope.prevent_encryption_scope_override is False


def test_create_container_with_metadata(service, uniq):
    c = service.create_container(uniq("c"), metadata={"project": "azemu", "number": "42"})
    md = {k.lower(): v for k, v in c.get_container_properties().metadata.items()}
    assert md == {"project": "azemu", "number": "42"}


def test_set_container_metadata_replaces_all(container):
    container.set_container_metadata({"a": "1", "b": "2"})
    container.set_container_metadata({"c": "3"})
    md = {k.lower(): v for k, v in container.get_container_properties().metadata.items()}
    assert md == {"c": "3"}


def test_set_empty_container_metadata_clears_it(container):
    container.set_container_metadata({"a": "1"})
    container.set_container_metadata({})
    assert not container.get_container_properties().metadata


def test_set_container_metadata_changes_etag_and_last_modified(container):
    before = container.get_container_properties()
    res = container.set_container_metadata({"a": "1"})
    after = container.get_container_properties()
    assert res["etag"] == after.etag
    assert after.etag != before.etag
    assert after.last_modified >= before.last_modified


def test_set_metadata_on_missing_container_is_not_found(service, uniq):
    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        c.set_container_metadata({"a": "b"})
    assert_error(ei, "ContainerNotFound", 404)


@pytest.mark.parametrize(
    "name",
    ["UPPERCASE-name", "-leading-dash", "double--dash", "has_underscore", "has.dot"],
)
def test_create_container_with_invalid_name_is_rejected(service, name):
    with pytest.raises(HttpResponseError) as ei:
        service.create_container(name)
    assert_error(ei, "InvalidResourceName", 400)


def test_container_name_with_minimum_and_maximum_length_is_accepted(service, uniq):
    short = "a" + uniq("")[:2]
    longest = ("m" + uniq("")).ljust(63, "z")
    assert len(short) == 3 and len(longest) == 63
    service.create_container(short)
    service.create_container(longest)


def test_root_container_blobs_live_at_the_account_root(service):
    root = service.get_container_client("$root")
    if not root.exists():
        root.create_container()
    blob = root.get_blob_client("root-blob-from-test.txt")
    blob.upload_blob(b"in root", overwrite=True)
    assert blob.download_blob().readall() == b"in root"
    assert "root-blob-from-test.txt" in [b.name for b in root.list_blobs()]


def test_web_container_can_be_created(service):
    web = service.get_container_client("$web")
    if not web.exists():
        web.create_container()
    web.upload_blob("index.html", b"<h1>hi</h1>", overwrite=True)
    assert web.get_blob_client("index.html").download_blob().readall() == b"<h1>hi</h1>"
    assert "$web" in [c.name for c in service.list_containers(name_starts_with="$web")]


def test_list_blobs_in_missing_container_is_not_found(service, uniq):
    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        list(c.list_blobs())
    assert_error(ei, "ContainerNotFound", 404)


def test_upload_to_missing_container_is_not_found(service, uniq):
    c = service.get_container_client(uniq("c"))
    with pytest.raises(ResourceNotFoundError) as ei:
        c.upload_blob("x", b"data")
    assert_error(ei, "ContainerNotFound", 404)


def test_delete_container_deletes_its_blobs(service, uniq):
    name = uniq("c")
    c = service.create_container(name)
    c.upload_blob("x", b"data")
    c.delete_container()
    with pytest.raises(ResourceNotFoundError) as ei:
        service.get_blob_client(name, "x").get_blob_properties()
    assert ei.value.status_code == 404


@pytest.mark.parametrize("name", ["ab", "a" * 64])
def test_create_container_with_name_length_out_of_range_is_rejected(service, name):
    with pytest.raises(HttpResponseError) as ei:
        service.create_container(name)
    assert_error(ei, "OutOfRangeInput", 400)
