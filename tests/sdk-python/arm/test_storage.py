"""Microsoft.Storage through azure-mgmt-storage, then used through the data plane SDKs."""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.data.tables import TableServiceClient
from azure.mgmt.storage import StorageManagementClient
from azure.storage.blob import BlobServiceClient
from azure.storage.queue import QueueServiceClient

from conftest import SUB


@pytest.fixture
def storage(cred, mgmt):
    with StorageManagementClient(cred, SUB, **mgmt) as c:
        yield c


def make_account(storage, rg, name):
    return storage.storage_accounts.begin_create(
        rg, name, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}
    ).result()


def first_key(storage, rg, name):
    return storage.storage_accounts.list_keys(rg, name)["keys"][0].value


def test_account_properties_and_endpoints(storage, rg, uniq, emu):
    name = uniq("acct")
    acct = make_account(storage, rg, name)
    assert acct.name == name
    assert acct.kind == "StorageV2"
    assert acct.sku.name == "Standard_LRS"
    assert acct.sku.tier == "Standard"
    assert acct.provisioning_state == "Succeeded"
    ep = acct.primary_endpoints
    assert ep.blob == f"http://{name}.blob.core.localhost:{emu.port}/"
    assert ep.queue == f"http://{name}.queue.core.localhost:{emu.port}/"
    assert ep.table == f"http://{name}.table.core.localhost:{emu.port}/"
    assert ep.dfs == f"http://{name}.dfs.core.localhost:{emu.port}/"
    assert storage.storage_accounts.get_properties(rg, name).id == acct.id
    assert name in [a.name for a in storage.storage_accounts.list_by_resource_group(rg)]
    assert name in [a.name for a in storage.storage_accounts.list()]


def test_check_name_availability(storage, rg, uniq):
    name = uniq("acct")
    ok = storage.storage_accounts.check_name_availability({"name": name, "type": "Microsoft.Storage/storageAccounts"})
    assert ok.name_available is True
    make_account(storage, rg, name)
    taken = storage.storage_accounts.check_name_availability({"name": name, "type": "Microsoft.Storage/storageAccounts"})
    assert taken.name_available is False
    assert taken.reason == "AlreadyExists"
    bad = storage.storage_accounts.check_name_availability({"name": "NO", "type": "Microsoft.Storage/storageAccounts"})
    assert bad.reason == "AccountNameInvalid"


def test_keys_and_regenerate(storage, rg, uniq):
    name = uniq("acct")
    make_account(storage, rg, name)
    keys = storage.storage_accounts.list_keys(rg, name)["keys"]
    assert [k.key_name for k in keys] == ["key1", "key2"]
    assert all(k.permissions == "FULL" for k in keys)
    new = storage.storage_accounts.regenerate_key(rg, name, {"keyName": "key1"})["keys"]
    assert new[0].value != keys[0].value
    assert new[1].value == keys[1].value


def test_container_visible_to_blob_sdk(storage, rg, uniq):
    name = uniq("acct")
    acct = make_account(storage, rg, name)
    c = storage.blob_containers.create(rg, name, "photos", {"properties": {"metadata": {"team": "a"}}})
    assert c.name == "photos"
    assert c.type == "Microsoft.Storage/storageAccounts/blobServices/containers"
    assert c.public_access == "None"
    blob_service = BlobServiceClient(acct.primary_endpoints.blob, credential=first_key(storage, rg, name))
    container = blob_service.get_container_client("photos")
    assert container.get_container_properties().metadata == {"team": "a"}
    container.upload_blob("hello.txt", b"hello from arm")
    assert container.download_blob("hello.txt").readall() == b"hello from arm"
    assert [x.name for x in storage.blob_containers.list(rg, name)] == ["photos"]
    # a container made through the Blob SDK is not listed by ARM (ARM lists what ARM created)
    blob_service.create_container("fromsdk")
    assert [x.name for x in storage.blob_containers.list(rg, name)] == ["photos"]
    storage.blob_containers.delete(rg, name, "photos")
    with pytest.raises(ResourceNotFoundError):
        container.get_container_properties()
    with pytest.raises(ResourceNotFoundError):
        storage.blob_containers.get(rg, name, "photos")


def test_queue_visible_to_queue_sdk(storage, rg, uniq):
    name = uniq("acct")
    acct = make_account(storage, rg, name)
    q = storage.queue.create(rg, name, "jobs", {"properties": {"metadata": {"k": "v"}}})
    assert q.name == "jobs"
    svc = QueueServiceClient(acct.primary_endpoints.queue, credential=first_key(storage, rg, name))
    queue = svc.get_queue_client("jobs")
    assert queue.get_queue_properties().metadata == {"k": "v"}
    queue.send_message("one")
    assert [m.content for m in queue.receive_messages()] == ["one"]
    assert [x.name for x in storage.queue.list(rg, name)] == ["jobs"]
    storage.queue.delete(rg, name, "jobs")
    with pytest.raises(ResourceNotFoundError):
        queue.get_queue_properties()


def test_table_visible_to_table_sdk(storage, rg, uniq):
    name = uniq("acct")
    acct = make_account(storage, rg, name)
    t = storage.table.create(rg, name, "people")
    assert t.table_name == "people"
    from azure.core.credentials import AzureNamedKeyCredential

    cred = AzureNamedKeyCredential(name, first_key(storage, rg, name))
    svc = TableServiceClient(endpoint=acct.primary_endpoints.table, credential=cred)
    table = svc.get_table_client("people")
    table.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    assert table.get_entity("p", "r")["v"] == 1
    assert [x.name for x in storage.table.list(rg, name)] == ["people"]
    storage.table.delete(rg, name, "people")
    assert "people" not in [t["name"] for t in svc.list_tables()]


def test_delete_account_removes_data(storage, rg, uniq):
    name = uniq("acct")
    acct = make_account(storage, rg, name)
    storage.blob_containers.create(rg, name, "data1", {})
    key = first_key(storage, rg, name)
    BlobServiceClient(acct.primary_endpoints.blob, credential=key).get_container_client("data1").upload_blob("b", b"x")
    storage.storage_accounts.delete(rg, name)
    with pytest.raises(ResourceNotFoundError):
        storage.storage_accounts.get_properties(rg, name)
    with pytest.raises(ResourceNotFoundError):
        BlobServiceClient(acct.primary_endpoints.blob, credential=key).get_container_client("data1").get_container_properties()


def test_delete_group_removes_data(storage, resource_client, make_group, uniq):
    group = make_group()
    name = uniq("acct")
    acct = make_account(storage, group, name)
    storage.queue.create(group, name, "q-one", {})
    key = first_key(storage, group, name)
    resource_client.resource_groups.begin_delete(group).result()
    svc = QueueServiceClient(acct.primary_endpoints.queue, credential=key)
    with pytest.raises(ResourceNotFoundError):
        svc.get_queue_client("q-one").get_queue_properties()


def test_blob_service_default_and_file_service(storage, rg, uniq):
    name = uniq("acct")
    make_account(storage, rg, name)
    props = storage.blob_services.get_service_properties(rg, name)
    assert props.name == "default"
    assert [p.name for p in storage.blob_services.list(rg, name)] == ["default"]
