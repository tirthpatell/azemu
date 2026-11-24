"""Blob, Queue, Table and Data Lake with Entra tokens: what each of the four principals may do, as Azure answered in November 2026
(`docs/strict-auth.md`). main has every data role, reader the read roles, norole nothing, scoped the Blob Data Reader role on one container."""

import pytest
import requests
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import TableServiceClient
from azure.storage.blob import BlobServiceClient
from azure.storage.filedatalake import DataLakeServiceClient
from azure.storage.queue import QueueServiceClient

from authutil import code_of, message_without_ids, unique

MISMATCH = "This request is not authorized to perform this operation using this permission."


def blob_service(w, who):
    return BlobServiceClient(w.blob_url(), credential=who, **w.sdk_kw())


def queue_service(w, who):
    return QueueServiceClient(w.queue_url(), credential=who, **w.sdk_kw())


def table_service(w, who):
    return TableServiceClient(w.table_url(), credential=who, credential_scopes=None, **w.sdk_kw()) if False else TableServiceClient(w.table_url(), credential=who, **w.sdk_kw())


@pytest.fixture(scope="module")
def fx(world, main):
    """A container with a blob, a queue with a message and a table with an entity, made by main and removed at the end."""
    name = unique("c")
    bsc = blob_service(world, main)
    bsc.create_container(name)
    bsc.get_blob_client(name, "b1").upload_blob(b"hello")
    # the scoped principal's own container holds a blob as well
    sc = bsc.get_container_client(world.scoped_container)
    sc.upload_blob("inside", b"in", overwrite=True)
    qs = queue_service(world, main)
    q = unique("q")
    qs.create_queue(q)
    qs.get_queue_client(q).send_message("m1")
    ts = table_service(world, main)
    t = unique("t")
    ts.create_table(t)
    ts.get_table_client(t).create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    yield {"container": name, "queue": q, "table": t}
    for fn in (lambda: bsc.delete_container(name), lambda: sc.delete_blob("inside"), lambda: qs.delete_queue(q), lambda: ts.delete_table(t)):
        try:
            fn()
        except Exception:
            pass


def refused(fn, status=403):
    with pytest.raises(HttpResponseError) as e:
        fn()
    assert e.value.status_code == status, e.value
    return e.value


# -- Blob ------------------------------------------------------------------------------------------------------------

def test_norole_is_refused_for_every_blob_call_also_for_what_does_not_exist(world, norole, fx):
    bsc = blob_service(world, norole)
    c = fx["container"]
    for call in (
        lambda: bsc.get_blob_client(c, "b1").download_blob().readall(),
        lambda: bsc.get_blob_client(c, "missing").download_blob().readall(),
        lambda: bsc.get_blob_client("nosuchcontainer", "b").download_blob().readall(),
        lambda: bsc.get_blob_client(c, "b1").get_blob_properties(),
        lambda: list(bsc.get_container_client(c).list_blobs()),
        lambda: list(bsc.list_containers()),
        lambda: bsc.get_container_client(c).get_container_properties(),
        lambda: bsc.get_blob_client(c, "new").upload_blob(b"x"),
        lambda: bsc.get_blob_client(c, "b1").delete_blob(),
        lambda: bsc.get_user_delegation_key(*__import__("authutil").udk_window()),
    ):
        e = refused(call)
        assert code_of(e) == "AuthorizationPermissionMismatch"


def test_create_container_is_authorization_failure_with_a_shorter_text(world, norole):
    e = refused(lambda: blob_service(world, norole).create_container(unique("c")))
    assert code_of(e) == "AuthorizationFailure"
    assert "This request is not authorized to perform this operation." in str(e) and MISMATCH not in str(e)


def test_reader_reads_lists_and_peeks_at_everything_but_tags(world, reader, fx):
    bsc = blob_service(world, reader)
    c = fx["container"]
    assert bsc.get_blob_client(c, "b1").download_blob().readall() == b"hello"
    assert bsc.get_blob_client(c, "b1").get_blob_properties().size == 5
    assert [b.name for b in bsc.get_container_client(c).list_blobs()] == ["b1"]
    assert [b.name for b in bsc.get_container_client(c).list_blobs(include=["metadata", "snapshots", "versions", "deleted", "uncommittedblobs", "copy"])] == ["b1"]
    assert fx["container"] in [x.name for x in bsc.list_containers()]
    assert bsc.get_container_client(c).get_container_properties().name == c
    with pytest.raises(ResourceNotFoundError) as e:
        bsc.get_blob_client(c, "missing").download_blob()
    assert code_of(e.value) == "BlobNotFound"
    with pytest.raises(ResourceNotFoundError) as e:
        bsc.get_blob_client("nosuchcontainer", "b").download_blob()
    assert code_of(e.value) == "ContainerNotFound"


def test_reader_may_get_a_user_delegation_key_and_the_service_properties(world, reader):
    from authutil import udk_window

    bsc = blob_service(world, reader)
    assert bsc.get_user_delegation_key(*udk_window()).value
    assert bsc.get_service_properties() is not None


def test_reader_may_not_use_tags_or_write(world, reader, fx):
    bsc = blob_service(world, reader)
    c = fx["container"]
    cc = bsc.get_container_client(c)
    for call in (
        lambda: list(cc.list_blobs(include=["tags"])),
        lambda: bsc.get_blob_client(c, "b1").get_blob_tags(),
        lambda: bsc.get_blob_client(c, "b1").set_blob_tags({"a": "b"}),
        lambda: list(cc.find_blobs_by_tags("\"a\"='b'")),
        lambda: bsc.get_blob_client(c, "new").upload_blob(b"x"),
        lambda: bsc.get_blob_client(c, "b1").set_blob_metadata({"a": "b"}),
        lambda: bsc.get_blob_client(c, "b1").create_snapshot(),
        lambda: bsc.get_blob_client(c, "b1").delete_blob(),
        lambda: cc.get_container_access_policy(),
        lambda: cc.set_container_metadata({"a": "b"}),
        lambda: cc.delete_container(),
        lambda: bsc.get_account_information(),
    ):
        assert code_of(refused(call)) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: bsc.create_container(unique("c")))) == "AuthorizationFailure"


def test_reader_batch_of_deletes_is_202_and_each_part_is_403(world, reader, fx):
    bsc = blob_service(world, reader)
    cc = bsc.get_container_client(fx["container"])
    # the SDK raises for a part that failed; the exception carries the part's answer
    with pytest.raises(HttpResponseError) as e:
        cc.delete_blobs("b1", "b2", raise_on_any_failure=True)
    assert e.value.status_code == 202 or e.value.response.status_code == 202 or "AuthorizationPermissionMismatch" in str(e.value)
    results = cc.delete_blobs("b1", "b2", raise_on_any_failure=False)
    assert [r.status_code for r in results] == [403, 403]
    assert [r.headers.get("x-ms-error-code") for r in results] == ["AuthorizationPermissionMismatch"] * 2
    assert bsc.get_blob_client(fx["container"], "b1").exists()


def test_scoped_reads_only_inside_its_container(world, scoped, fx):
    bsc = blob_service(world, scoped)
    inside = world.scoped_container
    assert bsc.get_blob_client(inside, "inside").download_blob().readall() == b"in"
    assert [b.name for b in bsc.get_container_client(inside).list_blobs()] == ["inside"]
    assert bsc.get_container_client(inside).get_container_properties().name == inside
    with pytest.raises(ResourceNotFoundError) as e:
        bsc.get_blob_client(inside, "missing").download_blob()
    assert code_of(e.value) == "BlobNotFound"
    # inside, but not what a Data Reader may not do
    assert code_of(refused(lambda: list(bsc.get_container_client(inside).list_blobs(include=["tags"])))) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: bsc.get_blob_client(inside, "new").upload_blob(b"x"))) == "AuthorizationPermissionMismatch"
    # outside: 403 for what exists and for what does not
    other = fx["container"]
    for call in (
        lambda: bsc.get_blob_client(other, "b1").download_blob(),
        lambda: bsc.get_blob_client(other, "missing").download_blob(),
        lambda: list(bsc.get_container_client(other).list_blobs()),
        lambda: bsc.get_container_client(other).get_container_properties(),
        lambda: bsc.get_blob_client("nosuchcontainer", "b").download_blob(),
        lambda: list(bsc.list_containers()),
        lambda: bsc.get_service_properties(),
    ):
        assert code_of(refused(call)) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: bsc.create_container(unique("c")))) == "AuthorizationFailure"


def test_a_blob_error_has_the_xml_shape(world, norole, fx):
    r = requests.get(f"{world.blob_url()}/{fx['container']}/b1", headers={"Authorization": f"Bearer {norole.token('https://storage.azure.com/.default')}", "x-ms-version": "2024-11-04"}, verify=world.emu.ca_file if world.emu else True)
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "AuthorizationPermissionMismatch"
    assert r.headers["content-type"].startswith("application/xml")
    body = message_without_ids(r.text)
    assert f"<Code>AuthorizationPermissionMismatch</Code><Message>{MISMATCH}\nRequestId:<guid>\nTime:<time></Message></Error>" in body


# -- Queue -----------------------------------------------------------------------------------------------------------

def test_queue_reader_lists_and_peeks_but_does_not_receive(world, reader, fx):
    qs = queue_service(world, reader)
    q = qs.get_queue_client(fx["queue"])
    assert fx["queue"] in [x.name for x in qs.list_queues()]
    assert [m.content for m in q.peek_messages()] == ["m1"]
    assert q.get_queue_properties().name == fx["queue"]
    assert code_of(refused(lambda: list(q.receive_messages()))) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: q.send_message("x"))) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: q.clear_messages())) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: q.set_queue_metadata({"a": "b"}))) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: q.delete_queue())) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: qs.create_queue(unique("q")))) == "AuthorizationFailure"


def test_queue_norole_is_refused_for_peek_too(world, norole, fx):
    q = queue_service(world, norole).get_queue_client(fx["queue"])
    assert code_of(refused(lambda: q.peek_messages())) == "AuthorizationPermissionMismatch"
    assert code_of(refused(lambda: list(queue_service(world, norole).list_queues()))) == "AuthorizationPermissionMismatch"


# -- Table -----------------------------------------------------------------------------------------------------------

def test_table_reader_queries_and_gets_but_does_not_write(world, reader, fx):
    ts = table_service(world, reader)
    t = ts.get_table_client(fx["table"])
    assert fx["table"] in [x.name for x in ts.list_tables()]
    assert t.get_entity("p", "r")["v"] == 1
    assert [e["RowKey"] for e in t.query_entities("PartitionKey eq 'p'")] == ["r"]
    for call in (
        lambda: t.create_entity({"PartitionKey": "p", "RowKey": "r2"}),
        lambda: t.upsert_entity({"PartitionKey": "p", "RowKey": "r"}),
        lambda: t.delete_entity("p", "r"),
        lambda: ts.create_table(unique("t")),
        lambda: ts.delete_table(fx["table"]),
    ):
        e = refused(call)
        # a Table error carries no `x-ms-error-code`, so the SDK has no code for it
        assert e.response.headers.get("x-ms-error-code") is None
        assert e.response.json()["odata.error"]["code"] == "AuthorizationPermissionMismatch"


def test_table_delete_of_a_missing_table_is_404_first_even_for_norole(world, norole):
    # the SDK takes a 404 of this call as "gone": no error, where a 403 would raise
    table_service(world, norole).delete_table(unique("t"))
    r = requests.delete(
        f"{world.table_url()}/Tables('{unique('t')}')",
        headers={"Authorization": f"Bearer {norole.token('https://storage.azure.com/.default')}", "x-ms-version": "2019-02-02", "Accept": "application/json;odata=nometadata"},
        verify=world.emu.ca_file if world.emu else True,
    )
    assert r.status_code == 404
    assert r.json()["odata.error"]["code"] == "ResourceNotFound"


def test_table_norole_is_refused_with_odata_json(world, norole, fx):
    r = requests.get(
        f"{world.table_url()}/{fx['table']}()",
        headers={"Authorization": f"Bearer {norole.token('https://storage.azure.com/.default')}", "x-ms-version": "2019-02-02", "Accept": "application/json;odata=nometadata"},
        verify=world.emu.ca_file if world.emu else True,
    )
    assert r.status_code == 403
    assert "x-ms-error-code" not in r.headers
    assert r.headers["content-type"] == "application/json;odata=nometadata;streaming=true;charset=utf-8"
    v = r.json()["odata.error"]
    assert v["code"] == "AuthorizationPermissionMismatch" and v["message"]["lang"] == "en-US"
    assert message_without_ids(v["message"]["value"]) == f"{MISMATCH}\nRequestId:<guid>\nTime:<time>"


# -- Data Lake -------------------------------------------------------------------------------------------------------

@pytest.fixture(scope="module")
def lake(world, main):
    svc = DataLakeServiceClient(world.dfs_url(), credential=main, **world.sdk_kw())
    name = unique("fs")
    fs = svc.create_file_system(name)
    fs.create_file("dir/f1").upload_data(b"abc", overwrite=True)
    yield name
    try:
        svc.delete_file_system(name)
    except Exception:
        pass


def test_data_lake_reader_lists_and_reads_and_nothing_else(world, reader, lake):
    svc = DataLakeServiceClient(world.dfs_url(), credential=reader, **world.sdk_kw())
    fs = svc.get_file_system_client(lake)
    assert lake in [x.name for x in svc.list_file_systems()]
    assert sorted(p.name for p in fs.get_paths(recursive=True)) == ["dir", "dir/f1"]
    assert fs.get_file_client("dir/f1").download_file().readall() == b"abc"
    assert fs.get_file_client("dir/f1").get_access_control()["permissions"]
    for call in (
        lambda: fs.create_file("dir/f2"),
        lambda: fs.get_file_client("dir/f1").set_access_control(permissions="rwxr-x---"),
        lambda: fs.get_file_client("dir/f1").delete_file(),
        lambda: svc.create_file_system(unique("fs")),
        lambda: svc.delete_file_system(lake),
    ):
        e = refused(call)
        # the Data Lake SDK makes and deletes a file system through the Blob endpoint, which says `AuthorizationFailure` for a create
        assert "AuthorizationPermissionMismatch" in str(e) or code_of(e) == "AuthorizationFailure"


def test_data_lake_error_is_json_with_the_code_in_the_header(world, norole, lake):
    r = requests.get(
        f"{world.dfs_url()}/{lake}?resource=filesystem&recursive=false",
        headers={"Authorization": f"Bearer {norole.token('https://storage.azure.com/.default')}", "x-ms-version": "2024-11-04"},
        verify=world.emu.ca_file if world.emu else True,
    )
    assert r.status_code == 403
    assert r.headers["x-ms-error-code"] == "AuthorizationPermissionMismatch"
    assert r.headers["content-type"] == "application/json;charset=utf-8"
    e = r.json()["error"]
    assert e["code"] == "AuthorizationPermissionMismatch"
    assert message_without_ids(e["message"]) == f"{MISMATCH}\nRequestId:<guid>\nTime:<time>"
