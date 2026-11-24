"""A source that is read by a copy, and a path that is moved away, are judged with the role of the caller (`docs/strict-auth.md`, "Copy and
rename"). Microsoft's table of permissions names the read on the source and the move on the source; what Azure answers for a caller without them
was not probed, so these tests run on the emulator only."""

import uuid

import pytest
import requests
from azure.core.exceptions import HttpResponseError
from azure.storage.blob import BlobServiceClient
from azure.storage.filedatalake import DataLakeServiceClient

from authutil import BLOB_ACCOUNT, HNS_ACCOUNT, Principal, unique

pytestmark = pytest.mark.emulator_only("not probed on Azure")


def give(world, client_id, role, scope):
    r = requests.put(f"{world.emu.gateway}/_azemu/auth/assignments", json={"clientId": client_id, "role": role, "scope": scope}, timeout=30)
    assert r.status_code in (200, 201), r.text


def new_principal(world, name):
    cid = str(uuid.uuid4())
    return Principal(name, cid, "x", world.emu), cid


def refused_copy(fn, status=403):
    with pytest.raises(HttpResponseError) as e:
        fn()
    assert e.value.status_code == status, e.value
    assert e.value.error_code == "CannotVerifyCopySource", e.value
    return e.value


@pytest.fixture(scope="module")
def blobs(world, main):
    """`secret` (a blob only main reads) and `out` (a container the writer may write and read)."""
    bsc = BlobServiceClient(world.blob_url(), credential=main, **world.sdk_kw())
    secret, out = unique("sec"), unique("out")
    bsc.create_container(secret)
    bsc.create_container(out)
    bsc.get_blob_client(secret, "b.txt").upload_blob(b"top secret")
    writer, cid = new_principal(world, "writer")
    give(world, cid, "Storage Blob Data Contributor", f"storage/{BLOB_ACCOUNT}/containers/{out}")
    yield {"bsc": bsc, "secret": secret, "out": out, "writer": writer, "writer_id": cid}
    for c in (secret, out):
        try:
            bsc.delete_container(c)
        except Exception:
            pass


def test_a_writer_cannot_copy_what_it_may_not_read(world, blobs):
    w = BlobServiceClient(world.blob_url(), credential=blobs["writer"], **world.sdk_kw())
    out, secret = blobs["out"], blobs["secret"]
    # the destination is its own: it can write there
    w.get_blob_client(out, "own").upload_blob(b"mine")
    src = f"{world.blob_url()}/{secret}/b.txt"
    # the account written another way is the same source for the copy, so it is the same refusal
    host = world.blob_url().replace(f"//{BLOB_ACCOUNT}.", f"//{BLOB_ACCOUNT}-secondary.")
    for url in (src, f"{host}/{secret}/b.txt"):
        refused_copy(lambda: w.get_blob_client(out, "copy").start_copy_from_url(url))
        refused_copy(lambda: w.get_blob_client(out, "fromurl").upload_blob_from_url(url))
        refused_copy(lambda: w.get_blob_client(out, "blockblob").stage_block_from_url("AAAA", url))
        refused_copy(lambda: w.get_blob_client(out, "appended").append_block_from_url(url))
    names = [b.name for b in blobs["bsc"].get_container_client(out).list_blobs()]
    assert names == ["own"], names
    # the same writer, once it may read the source
    give(world, blobs["writer_id"], "Storage Blob Data Reader", f"storage/{BLOB_ACCOUNT}/containers/{secret}")
    w.get_blob_client(out, "copy").start_copy_from_url(src)
    assert w.get_blob_client(out, "copy").download_blob().readall() == b"top secret"


def test_a_source_authorization_that_is_not_a_good_token_is_refused_and_a_good_one_is_judged_for_the_source(world, main, norole, blobs):
    bsc = BlobServiceClient(world.hns_blob_url(), credential=main, **world.sdk_kw())
    c = unique("src")
    bsc.create_container(c)
    try:
        bsc.get_blob_client(c, "s").upload_blob(b"other account")
        url = f"{world.hns_blob_url()}/{c}/s"
        dest = blobs["bsc"].get_blob_client(blobs["out"], "from-other")
        refused_copy(lambda: dest.upload_blob_from_url(url, source_authorization="Bearer not-a-token"), status=401)
        no_role = norole.token("https://storage.azure.com/.default")
        refused_copy(lambda: dest.upload_blob_from_url(url, source_authorization=f"Bearer {no_role}"))
        wrong_resource = main.token("https://vault.azure.net/.default")
        refused_copy(lambda: dest.upload_blob_from_url(url, source_authorization=f"Bearer {wrong_resource}"), status=401)
        good = main.token("https://storage.azure.com/.default")
        dest.upload_blob_from_url(url, source_authorization=f"Bearer {good}", overwrite=True)
        assert dest.download_blob().readall() == b"other account"
    finally:
        bsc.delete_container(c)


def test_a_data_lake_rename_out_of_a_file_system_needs_the_move_and_the_read_there(world, main):
    svc = DataLakeServiceClient(world.dfs_url(), credential=main, **world.sdk_kw())
    secret, out = unique("sec"), unique("out")
    svc.create_file_system(secret)
    svc.create_file_system(out)
    try:
        svc.get_file_system_client(secret).create_file("m.txt").upload_data(b"data", overwrite=True)
        mover, cid = new_principal(world, "mover")
        give(world, cid, "Storage Blob Data Owner", f"storage/{HNS_ACCOUNT}/containers/{out}")
        m = DataLakeServiceClient(world.dfs_url(), credential=mover, **world.sdk_kw())
        with pytest.raises(HttpResponseError) as e:
            m.get_file_system_client(secret).get_file_client("m.txt").rename_file(f"{out}/moved.txt")
        assert e.value.status_code == 403, e.value
        # the file is where it was, and nothing was made
        assert svc.get_file_system_client(secret).get_file_client("m.txt").download_file().readall() == b"data"
        assert [p.name for p in svc.get_file_system_client(out).get_paths()] == []
        # the owner of both may move it
        svc.get_file_system_client(secret).get_file_client("m.txt").rename_file(f"{out}/moved.txt")
        assert [p.name for p in svc.get_file_system_client(out).get_paths()] == ["moved.txt"]
    finally:
        for fs in (secret, out):
            try:
                svc.delete_file_system(fs)
            except Exception:
                pass
