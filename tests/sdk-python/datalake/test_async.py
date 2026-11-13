"""The same scenarios through `azure.storage.filedatalake.aio` (aiohttp transport).

pytest-asyncio runs in auto mode (tests/sdk-python/pytest.ini).
"""

import asyncio
from datetime import datetime, timedelta, timezone

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import (
    HttpResponseError,
    ResourceExistsError,
    ResourceModifiedError,
    ResourceNotFoundError,
)
from azure.storage.blob.aio import BlobServiceClient as AioBlobServiceClient
from azure.storage.filedatalake import (
    ContentSettings,
    FileSystemSasPermissions,
    RetentionPolicy,
    generate_file_system_sas,
)
from azure.storage.filedatalake.aio import (
    DataLakeFileClient,
    DataLakeServiceClient,
    FileSystemClient,
)

from dlutil import DEV_ACCOUNT, DEV_KEY, assert_error, rand_bytes, tamper_sas, uniq


@pytest.fixture
async def asvc(emu):
    async with DataLakeServiceClient(emu.url("dfs", DEV_ACCOUNT), credential=DEV_KEY) as svc:
        yield svc


@pytest.fixture
async def afs(asvc):
    return await asvc.create_file_system(uniq("fs"))


async def aput(afs, path, data=b""):
    f = afs.get_file_client(path)
    await f.create_file()
    if data:
        await f.append_data(data, 0, len(data))
        await f.flush_data(len(data))
    return f


async def apaths(afs, **kw):
    return {p.name: p.is_directory async for p in afs.get_paths(**kw)}


# -- filesystems -------------------------------------------------------------
async def test_async_create_exists_properties_delete_file_system(asvc):
    name = uniq("fs")
    fsc = asvc.get_file_system_client(name)
    assert not await fsc.exists()
    created = await fsc.create_file_system(metadata={"hello": "world"})
    assert created["etag"]
    assert await fsc.exists()
    props = await fsc.get_file_system_properties()
    assert props.name == name
    assert props.metadata == {"hello": "world"}
    await fsc.delete_file_system()
    assert not await fsc.exists()


async def test_async_create_existing_file_system_raises(afs):
    with pytest.raises(ResourceExistsError) as e:
        await afs.create_file_system()
    assert_error(e, "ContainerAlreadyExists", 409)


async def test_async_delete_missing_file_system_raises(asvc):
    with pytest.raises(ResourceNotFoundError) as e:
        await asvc.get_file_system_client("fakeclient").delete_file_system()
    assert_error(e, "ContainerNotFound", 404)


async def test_async_list_file_systems_prefix_and_pages(asvc):
    p = uniq("apg")
    names = [p + str(i) for i in range(5)]
    for n in names:
        await asvc.create_file_system(n)
    listed = [f.name async for f in asvc.list_file_systems(name_starts_with=p)]
    assert listed == names
    pages = []
    async for page in asvc.list_file_systems(name_starts_with=p, results_per_page=2).by_page():
        pages.append([f.name async for f in page])
    assert [len(x) for x in pages] == [2, 2, 1]


async def test_async_list_file_systems_include_metadata(asvc):
    name = uniq("md")
    await asvc.create_file_system(name, metadata={"a": "1"})
    items = [f async for f in asvc.list_file_systems(name_starts_with=name, include_metadata=True)]
    assert items[0].metadata == {"a": "1"}


async def test_async_file_system_metadata(afs):
    await afs.set_file_system_metadata({"hello": "world", "number": "42"})
    assert (await afs.get_file_system_properties()).metadata == {"hello": "world", "number": "42"}


async def test_async_service_properties_round_trip(asvc):
    await asvc.set_service_properties(delete_retention_policy=RetentionPolicy(enabled=True, days=2))
    props = await asvc.get_service_properties()
    assert props["delete_retention_policy"].enabled is True
    assert props["delete_retention_policy"].days == 2


async def test_async_file_system_lease(afs):
    lease = await afs.acquire_lease(lease_duration=15)
    assert (await afs.get_file_system_properties()).lease.state == "leased"
    await lease.release()
    assert (await afs.get_file_system_properties()).lease.state == "available"


# -- directories -------------------------------------------------------------
async def test_async_create_nested_directory_and_list(afs):
    await afs.create_directory("a/b/c")
    assert await apaths(afs) == {"a": True, "a/b": True, "a/b/c": True}
    assert await apaths(afs, recursive=False) == {"a": True}


async def test_async_directory_properties_and_metadata(afs):
    d = afs.get_directory_client("d")
    await d.create_directory(metadata={"hello": "world"})
    props = await d.get_directory_properties()
    assert props.metadata["hello"] == "world"
    assert props.metadata["hdi_isfolder"] == "true"
    assert await d.exists()


async def test_async_delete_directory_recursive(afs):
    await aput(afs, "d/a/f", b"x")
    await afs.get_directory_client("d").delete_directory()
    assert await apaths(afs) == {}


async def test_async_delete_missing_directory_raises(afs):
    with pytest.raises(ResourceNotFoundError) as e:
        await afs.get_directory_client("nodir").delete_directory()
    assert e.value.status_code == 404


async def test_async_rename_directory_across_filesystems(asvc, afs):
    other = await asvc.create_file_system(uniq("fs"))
    await aput(afs, "src/f", b"abc")
    new = await afs.get_directory_client("src").rename_directory(other.file_system_name + "/dst")
    assert await apaths(other) == {"dst": True, "dst/f": False}
    assert await apaths(afs) == {}
    assert (await (await new.get_file_client("f").download_file()).readall()) == b"abc"


async def test_async_get_paths_paging(afs):
    for i in range(6):
        await afs.create_directory(f"dir1{i}")
    gen = afs.get_paths(max_results=2).by_page()
    page1 = [p async for p in await gen.__anext__()]
    page2 = [p async for p in await gen.__anext__()]
    assert [p.name for p in page1] == ["dir10", "dir11"]
    assert [p.name for p in page2] == ["dir12", "dir13"]


# -- files -------------------------------------------------------------------
async def test_async_append_flush_download(afs):
    f = afs.get_file_client("d/f.txt")
    await f.create_file()
    await f.append_data(b"hello ", 0, 6)
    await f.append_data(b"world", 6, 5)
    assert (await f.get_file_properties()).size == 0
    resp = await f.flush_data(11)
    assert resp["etag"]
    assert await (await f.download_file()).readall() == b"hello world"
    assert (await f.get_file_properties()).size == 11


async def test_async_upload_data_chunked(afs):
    f = afs.get_file_client("f")
    data = rand_bytes(8 * 1024 + 7)
    await f.upload_data(data, overwrite=True, chunk_size=1024, max_concurrency=3)
    assert await (await f.download_file()).readall() == data


async def test_async_upload_existing_without_overwrite_fails(afs):
    f = await aput(afs, "f", b"abc")
    with pytest.raises(HttpResponseError) as e:
        await f.upload_data(b"zzz")
    assert_error(e, "ConditionNotMet", 412)
    assert await (await f.download_file()).readall() == b"abc"


async def test_async_download_offset_length(afs):
    f = await aput(afs, "f", b"0123456789")
    assert await (await f.download_file(offset=2, length=5)).readall() == b"23456"


async def test_async_empty_file_download(afs):
    f = afs.get_file_client("empty")
    await f.create_file()
    assert await (await f.download_file()).readall() == b""


async def test_async_create_file_if_missing_conflict(afs):
    f = afs.get_file_client("f")
    await f.create_file(match_condition=MatchConditions.IfMissing)
    with pytest.raises(ResourceExistsError) as e:
        await f.create_file(match_condition=MatchConditions.IfMissing)
    assert_error(e, "PathAlreadyExists", 409)


async def test_async_flush_with_stale_etag_fails(afs):
    f = afs.get_file_client("f")
    etag = (await f.create_file())["etag"]
    await f.append_data(b"abc", 0, 3)
    await f.flush_data(3, etag=etag, match_condition=MatchConditions.IfNotModified)
    await f.append_data(b"abc", 3, 3)
    with pytest.raises(ResourceModifiedError) as e:
        await f.flush_data(6, etag=etag, match_condition=MatchConditions.IfNotModified)
    assert_error(e, "ConditionNotMet", 412)


async def test_async_file_properties_metadata_headers(afs):
    f = afs.get_file_client("f")
    await f.create_file(metadata={"a": "1"})
    await f.set_http_headers(ContentSettings(content_type="text/plain"))
    await f.set_metadata({"b": "2"})
    props = await f.get_file_properties()
    assert props.metadata == {"b": "2"}
    assert props.content_settings.content_type == "text/plain"


async def test_async_rename_file_over_existing(afs):
    await aput(afs, "existing", b"a")
    f = await aput(afs, "f", b"abc")
    new = await f.rename_file(afs.file_system_name + "/existing")
    assert await (await new.download_file()).readall() == b"abc"
    assert not await f.exists()


async def test_async_delete_and_exists(afs):
    f = await aput(afs, "f", b"abc")
    assert await f.exists()
    await f.delete_file()
    assert not await f.exists()
    with pytest.raises(ResourceNotFoundError):
        await f.delete_file()


async def test_async_special_character_names(afs):
    for name in ("a b.txt", "100%.txt", "q?x#y+z.txt", "café.txt"):
        f = await aput(afs, name, b"x")
        assert await (await f.download_file()).readall() == b"x"
    assert len(await apaths(afs)) == 4


async def test_async_file_lease(afs):
    f = await aput(afs, "f", b"abc")
    lease = await f.acquire_lease(lease_duration=15)
    assert (await f.get_file_properties()).lease.state == "leased"
    with pytest.raises(HttpResponseError) as e:
        await f.delete_file()
    assert e.value.status_code == 412
    await lease.release()
    await f.delete_file()


async def test_async_set_file_expiry(afs):
    f = await aput(afs, "f")
    when = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(hours=1)
    await f.set_file_expiry("Absolute", expires_on=when)
    assert (await f.get_file_properties()).expiry_time == when


# -- access control ----------------------------------------------------------
async def test_async_access_control_defaults_and_set(afs):
    f = await aput(afs, "f")
    ac = await f.get_access_control()
    assert (ac["owner"], ac["permissions"]) == ("$superuser", "rw-r-----")
    await f.set_access_control(permissions="0777")
    assert (await f.get_access_control())["permissions"] == "rwxrwxrwx"


async def test_async_create_directory_permissions_and_umask(afs):
    d = afs.get_directory_client("d")
    await d.create_directory(permissions="0777", umask="0000")
    assert (await d.get_access_control())["permissions"] == "rwxrwxrwx"


async def test_async_set_access_control_recursive(afs):
    top = afs.get_directory_client("top")
    await top.create_directory()
    for i in range(3):
        sub = await top.create_sub_directory(f"s{i}")
        for j in range(2):
            await sub.create_file(f"f{j}")
    acl = "user::rwx,group::r-x,other::rwx"
    summary = await top.set_access_control_recursive(acl=acl, batch_size=2)
    assert summary.counters.directories_successful == 4
    assert summary.counters.files_successful == 6
    assert summary.counters.failure_count == 0
    assert summary.continuation is None
    assert (await top.get_access_control())["acl"] == acl


# -- cross endpoint / SAS / concurrency --------------------------------------
async def test_async_dfs_file_readable_with_async_blob_client(emu, afs):
    await aput(afs, "d/f.txt", b"hello datalake")
    async with AioBlobServiceClient(emu.url("blob", DEV_ACCOUNT), credential=DEV_KEY) as blob:
        bc = blob.get_blob_client(afs.file_system_name, "d/f.txt")
        assert await (await bc.download_blob()).readall() == b"hello datalake"
        folder = await blob.get_blob_client(afs.file_system_name, "d").get_blob_properties()
        assert folder.metadata["hdi_isfolder"] == "true"


async def test_async_blob_upload_visible_in_get_paths(emu, afs):
    async with AioBlobServiceClient(emu.url("blob", DEV_ACCOUNT), credential=DEV_KEY) as blob:
        await blob.get_blob_client(afs.file_system_name, "top.txt").upload_blob(b"hello")
    assert await apaths(afs) == {"top.txt": False}


async def test_async_filesystem_sas(emu, afs):
    f = await aput(afs, "f", b"abc")
    sas = generate_file_system_sas(
        DEV_ACCOUNT,
        afs.file_system_name,
        DEV_KEY,
        FileSystemSasPermissions(read=True, list=True),
        datetime.now(timezone.utc) + timedelta(hours=1),
    )
    async with FileSystemClient(emu.url("dfs", DEV_ACCOUNT), afs.file_system_name, credential=sas) as client:
        assert await apaths(client) == {"f": False}
        with pytest.raises(HttpResponseError) as e:
            await client.create_file("nope")
        assert_error(e, "AuthorizationPermissionMismatch", 403)


async def test_async_tampered_sas_is_403(emu, afs):
    sas = generate_file_system_sas(
        DEV_ACCOUNT, afs.file_system_name, DEV_KEY, FileSystemSasPermissions(read=True, list=True),
        datetime.now(timezone.utc) + timedelta(hours=1),
    )
    bad = tamper_sas(sas)
    async with FileSystemClient(emu.url("dfs", DEV_ACCOUNT), afs.file_system_name, credential=bad) as client:
        with pytest.raises(HttpResponseError) as e:
            await apaths(client)
        assert_error(e, "AuthenticationFailed", 403)


async def test_async_concurrent_creates_and_reads(afs):
    names = [f"f{i:02d}" for i in range(12)]
    await asyncio.gather(*(aput(afs, n, n.encode()) for n in names))
    assert set(await apaths(afs)) == set(names)
    got = await asyncio.gather(*(afs.get_file_client(n).download_file() for n in names))
    bodies = await asyncio.gather(*(d.readall() for d in got))
    assert bodies == [n.encode() for n in names]


async def test_async_concurrent_appends_to_different_offsets(afs):
    f = afs.get_file_client("f")
    await f.create_file()
    chunks = [bytes([65 + i]) * 100 for i in range(8)]
    await asyncio.gather(*(f.append_data(c, i * 100, 100) for i, c in enumerate(chunks)))
    await f.flush_data(800)
    assert await (await f.download_file()).readall() == b"".join(chunks)
