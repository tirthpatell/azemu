"""Blob, Data Lake, Queue and Table state across restarts, through the official SDKs.

Each test creates state, stops the emulator (SIGTERM: it saves on the way out; or SIGKILL after an explicit
POST /_azemu/save), starts it again on the same data directory and looks with new clients.
"""

import base64
import datetime as dt
import hashlib
import time

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.data.tables import TableServiceClient, UpdateMode
from azure.storage.blob import (
    BlobServiceClient,
    BlobType,
    CustomerProvidedEncryptionKey,
    StaticWebsite,
    CorsRule,
    RetentionPolicy,
    ContainerSasPermissions,
    AccessPolicy,
)
from azure.storage.filedatalake import DataLakeServiceClient
from azure.storage.queue import QueueServiceClient

ACCOUNT = "devstoreaccount1"
SUB = "11111111-2222-3333-4444-555555555555"
KEY = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="


def versioning_on_through_arm(p, account=ACCOUNT):
    """Blob versioning is off until someone turns it on. Like a team would: make the storage account and set
    `isVersioningEnabled` on `blobServices/default` through ARM. The switch is part of the Blob state, so it must survive
    the restart these tests make."""
    import time

    from azure.core.credentials import AccessToken
    from azure.mgmt.resource.resources import ResourceManagementClient
    from azure.mgmt.storage import StorageManagementClient

    class Cred:
        def get_token(self, *scopes, **kwargs):
            return AccessToken("azemu-test-token", int(time.time()) + 3600)

    kw = dict(base_url=f"https://management.localhost:{p.emu.port}", credential_scopes=["https://management.azure.com/.default"], connection_verify=p.emu.ca_file, retry_total=0)
    with ResourceManagementClient(Cred(), SUB, **kw) as rc:
        rc.resource_groups.create_or_update("rg-versioning", {"location": "eastus"})
    with StorageManagementClient(Cred(), SUB, **kw) as sc:
        sc.storage_accounts.begin_create("rg-versioning", account, {"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}).result()
        on = sc.blob_services.set_service_properties("rg-versioning", account, {"properties": {"isVersioningEnabled": True}})
        assert on.is_versioning_enabled is True


def arm_versioning(p, account=ACCOUNT):
    """What ARM says about the account's versioning (it asks the Blob service)."""
    import time

    from azure.core.credentials import AccessToken
    from azure.mgmt.storage import StorageManagementClient

    class Cred:
        def get_token(self, *scopes, **kwargs):
            return AccessToken("azemu-test-token", int(time.time()) + 3600)

    kw = dict(base_url=f"https://management.localhost:{p.emu.port}", credential_scopes=["https://management.azure.com/.default"], connection_verify=p.emu.ca_file, retry_total=0)
    with StorageManagementClient(Cred(), SUB, **kw) as sc:
        return sc.blob_services.get_service_properties("rg-versioning", account).is_versioning_enabled


def blob_service(p):
    return BlobServiceClient(account_url=p.emu.url("blob", ACCOUNT), credential=KEY)


def blob_service_https(p):
    """Customer-provided keys are only sent over https."""
    return BlobServiceClient(account_url=p.emu.https_url("blob", ACCOUNT), credential=KEY, connection_verify=p.emu.ca_file)


def lake_service(p):
    return DataLakeServiceClient(account_url=p.emu.url("dfs", ACCOUNT), credential=KEY)


def queue_service(p):
    return QueueServiceClient(account_url=p.emu.url("queue", ACCOUNT), credential=KEY)


def table_service(p):
    return TableServiceClient.from_connection_string(p.emu.storage_conn_str())


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_blob_and_datalake_state_survives(persistent, how):
    p = persistent()
    versioning_on_through_arm(p)
    svc = blob_service(p)

    # service properties: soft delete, CORS, static website
    svc.set_service_properties(
        delete_retention_policy=RetentionPolicy(enabled=True, days=5),
        cors=[CorsRule(["https://example.com"], ["GET", "PUT"], allowed_headers=["x-ms-meta-*"], exposed_headers=["x-ms-request-id"], max_age_in_seconds=120)],
        static_website=StaticWebsite(enabled=True, index_document="index.html"),
    )

    # a container with metadata and a stored access policy; a block blob with everything
    c = svc.create_container("things", metadata={"owner": "alice", "team": "blue"})
    c.set_container_access_policy(
        {"reader": AccessPolicy(permission=ContainerSasPermissions(read=True, list=True), expiry=dt.datetime(2099, 1, 1, tzinfo=dt.timezone.utc))}
    )
    b = c.get_blob_client("dir/a.txt")
    b.upload_blob(b"first", metadata={"k": "v1"}, tags={"env": "dev"})
    snap = b.create_snapshot()
    b.upload_blob(b"second version", overwrite=True, metadata={"k": "v2"})
    b.set_blob_tags({"env": "prod", "n": "1"})

    # blocks staged and committed, and one block only staged
    blocks = c.get_blob_client("blocks")
    blocks.stage_block("b1", b"AAAA")
    blocks.stage_block("b2", b"BBBB")
    from azure.storage.blob import BlobBlock

    blocks.commit_block_list([BlobBlock("b1"), BlobBlock("b2")])
    blocks.stage_block("st", b"SSSS")

    # append and page blobs
    app = c.get_blob_client("app")
    app.create_append_blob()
    app.append_block(b"hello ")
    app.append_block(b"world")
    page = c.get_blob_client("page")
    page.create_page_blob(4096)
    page.upload_page(b"p" * 512, offset=512, length=512)

    # a lease, a legal hold, a customer-provided key
    leased = c.get_blob_client("leased")
    leased.upload_blob(b"L")
    lease = leased.acquire_lease(lease_duration=60)
    held = c.get_blob_client("held")
    held.upload_blob(b"H")
    held.set_legal_hold(True)
    cpk = CustomerProvidedEncryptionKey(key_value=base64.b64encode(b"k" * 32).decode(), key_hash=base64.b64encode(hashlib.sha256(b"k" * 32).digest()).decode())
    secret = blob_service_https(p).get_container_client("things").get_blob_client("secret")
    secret.upload_blob(b"cpk data", cpk=cpk)

    # a copy, a soft deleted blob
    copied = c.get_blob_client("copied")
    copied.start_copy_from_url(b.url)
    gone = c.get_blob_client("gone")
    gone.upload_blob(b"bye")
    gone.delete_blob()

    # Data Lake: filesystem, directory with ACL, a file, an unflushed append
    lake = lake_service(p)
    fs = lake.create_file_system("lake")
    d = fs.create_directory("d")
    d.set_access_control(owner="alice", group="staff", acl="user::rwx,group::r-x,other::---,default:user::rwx,default:group::r-x,default:other::---")
    f = d.create_file("f1")
    f.upload_data(b"lake data", overwrite=True)
    f2 = d.create_file("f2")
    f2.append_data(b"not flushed yet", offset=0, length=15)

    if how == "sigterm":
        assert p.stop() == 0
    else:
        saved = p.save()
        assert saved["services"]["blob"]["result"] in ("saved", "unchanged")
        p.stop(kill=True)
    p.start()

    svc = blob_service(p)
    props = svc.get_service_properties()
    assert props["delete_retention_policy"].enabled and props["delete_retention_policy"].days == 5
    assert props["cors"][0].allowed_origins == "https://example.com"
    assert props["static_website"].enabled and props["static_website"].index_document == "index.html"

    c = svc.get_container_client("things")
    cp = c.get_container_properties()
    assert cp.metadata == {"owner": "alice", "team": "blue"}
    assert "reader" in c.get_container_access_policy()["signed_identifiers"][0].id or c.get_container_access_policy()["signed_identifiers"][0].id == "reader"

    b = c.get_blob_client("dir/a.txt")
    assert b.download_blob().readall() == b"second version"
    assert b.get_blob_properties().metadata == {"k": "v2"}
    assert b.get_blob_tags() == {"env": "prod", "n": "1"}
    assert c.get_blob_client("dir/a.txt", snapshot=snap["snapshot"]).download_blob().readall() == b"first"
    versions = [x for x in c.list_blobs(name_starts_with="dir/", include=["versions"])]
    assert len(versions) >= 2
    assert arm_versioning(p) is True, "the account's versioning switch is part of the state"
    assert [x.name for x in svc.find_blobs_by_tags("\"env\"='prod'")] == ["dir/a.txt"]

    blocks = c.get_blob_client("blocks")
    committed, uncommitted = blocks.get_block_list("all")
    assert [x.id for x in committed] == ["b1", "b2"] and [x.id for x in uncommitted] == ["st"]
    assert blocks.download_blob().readall() == b"AAAABBBB"
    blocks.commit_block_list([BlobBlock("b1"), BlobBlock("st")])
    assert blocks.download_blob().readall() == b"AAAASSSS"

    app = c.get_blob_client("app")
    assert app.download_blob().readall() == b"hello world"
    app.append_block(b"!")
    assert app.download_blob().readall() == b"hello world!"
    page = c.get_blob_client("page")
    ranges, _ = page.get_page_ranges()
    assert [(r["start"], r["end"]) for r in ranges] == [(512, 1023)]
    assert page.download_blob(offset=512, length=512).readall() == b"p" * 512

    leased = c.get_blob_client("leased")
    lp = leased.get_blob_properties()
    assert lp.lease.state == "leased" and lp.lease.status == "locked"
    leased.upload_blob(b"L2", overwrite=True, lease=lease.id)
    held = c.get_blob_client("held")
    assert held.get_blob_properties().has_legal_hold
    with pytest.raises(Exception):
        held.delete_blob()
    secret = blob_service_https(p).get_container_client("things").get_blob_client("secret")
    assert secret.download_blob(cpk=cpk).readall() == b"cpk data"
    assert secret.get_blob_properties(cpk=cpk).encryption_key_sha256 == cpk.key_hash
    copied = c.get_blob_client("copied")
    assert copied.get_blob_properties().copy.status == "success"
    assert [x.name for x in c.list_blobs(include=["deleted"]) if x.deleted] == ["gone"]
    c.get_blob_client("gone").undelete_blob()
    assert c.get_blob_client("gone").download_blob().readall() == b"bye"

    lake = lake_service(p)
    fs = lake.get_file_system_client("lake")
    d = fs.get_directory_client("d")
    ac = d.get_access_control()
    assert ac["owner"] == "alice" and ac["group"] == "staff" and "default:user::rwx" in ac["acl"]
    assert fs.get_file_client("d/f1").download_file().readall() == b"lake data"
    f2 = fs.get_file_client("d/f2")
    f2.flush_data(15)
    assert f2.download_file().readall() == b"not flushed yet"


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_queue_state_survives(persistent, how):
    p = persistent()
    q = queue_service(p).create_queue("work", metadata={"owner": "alice"})
    q.send_message("first")
    q.send_message("delayed", visibility_timeout=3000)
    q.send_message("forever", time_to_live=-1)
    got = q.receive_messages(visibility_timeout=3000)
    taken = next(iter(got))
    assert taken.content == "first" and taken.dequeue_count == 1
    updated = q.update_message(taken, visibility_timeout=3000, content="first (updated)")
    before = [m.content for m in q.peek_messages(max_messages=10)]

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    q = queue_service(p).get_queue_client("work")
    assert q.get_queue_properties().metadata == {"owner": "alice"}
    assert [m.content for m in q.peek_messages(max_messages=10)] == before == ["forever"]
    # the receipt from before the restart deletes the hidden message
    q.delete_message(taken.id, updated.pop_receipt)
    assert q.get_queue_properties().approximate_message_count == 2


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_table_state_survives(persistent, how):
    p = persistent()
    svc = table_service(p)
    t = svc.create_table("People")
    e1 = {"PartitionKey": "p1", "RowKey": "r1", "Name": "Zoë", "Age": 42, "Pi": 3.14159, "Ok": True, "When": dt.datetime(2024, 5, 6, 7, 8, 9, tzinfo=dt.timezone.utc), "Bin": b"\x00\x01\xff"}
    t.create_entity(e1)
    t.create_entity({"PartitionKey": "p1", "RowKey": "r2", "v": 1})
    t.update_entity({"PartitionKey": "p1", "RowKey": "r2", "added": "x"}, mode=UpdateMode.MERGE)
    t.create_entity({"PartitionKey": "p2", "RowKey": "gone"})
    t.delete_entity("p2", "gone")
    etag = t.get_entity("p1", "r2").metadata["etag"]

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    t = table_service(p).get_table_client("People")
    got = t.get_entity("p1", "r1")
    assert got["Name"] == "Zoë" and got["Age"] == 42 and got["Ok"] is True and got["Bin"] == b"\x00\x01\xff"
    assert got["When"].replace(tzinfo=dt.timezone.utc) == dt.datetime(2024, 5, 6, 7, 8, 9, tzinfo=dt.timezone.utc)
    r2 = t.get_entity("p1", "r2")
    assert r2["added"] == "x" and r2.metadata["etag"] == etag
    with pytest.raises(ResourceNotFoundError):
        t.get_entity("p2", "gone")
    assert sorted(e["RowKey"] for e in t.list_entities()) == ["r1", "r2"]
    # a conditional write with the ETag from before the restart
    from azure.core import MatchConditions

    t.update_entity({"PartitionKey": "p1", "RowKey": "r2", "v": 2}, mode=UpdateMode.MERGE, etag=etag, match_condition=MatchConditions.IfNotModified)
