"""Shared access signatures: account, filesystem, directory and file SAS (shared-key signed).

The emulator verifies the signature for the well-known development account.
Expired / tampered -> 403 AuthenticationFailed; valid SAS without the needed permission -> 403
AuthorizationPermissionMismatch.  Neither code is in the recordings (they were recorded with
shared-key credentials); they come from the Azure SAS documentation.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import ClientAuthenticationError, HttpResponseError, ResourceNotFoundError
from azure.storage.filedatalake import (
    AccessPolicy,
    AccountSasPermissions,
    DataLakeDirectoryClient,
    DataLakeFileClient,
    DataLakeServiceClient,
    DirectorySasPermissions,
    FileSasPermissions,
    FileSystemClient,
    FileSystemSasPermissions,
    PublicAccess,
    ResourceTypes,
    generate_account_sas,
    generate_directory_sas,
    generate_file_sas,
    generate_file_system_sas,
)

from dlutil import (
    DEV_ACCOUNT,
    DEV_KEY,
    assert_error,
    assert_status,
    put_file,
    read_file,
    sas_with_signature,
    tamper_sas,
    tree,
)


def expiry(minutes=60):
    return datetime.now(timezone.utc) + timedelta(minutes=minutes)


def account_sas(read=False, write=False, delete=False, list=False, create=False, types=None, **kw):
    return generate_account_sas(
        DEV_ACCOUNT,
        DEV_KEY,
        types or ResourceTypes(service=True, file_system=True, object=True),
        AccountSasPermissions(read=read, write=write, delete=delete, list=list, create=create),
        kw.pop("expiry", expiry()),
        **kw,
    )


def fs_sas(fsname, minutes=60, **perms):
    return generate_file_system_sas(DEV_ACCOUNT, fsname, DEV_KEY, FileSystemSasPermissions(**perms), expiry(minutes))


def dir_sas(fsname, directory, **perms):
    return generate_directory_sas(DEV_ACCOUNT, fsname, directory, DEV_KEY, DirectorySasPermissions(**perms), expiry())


def file_sas(fsname, path, **perms):
    directory, _, name = path.rpartition("/")
    return generate_file_sas(DEV_ACCOUNT, fsname, directory, name, DEV_KEY, FileSasPermissions(**perms), expiry())


tamper = tamper_sas


# -- account SAS -------------------------------------------------------------
def test_account_sas_service_client_lists_and_creates(emu, uniq):
    svc = DataLakeServiceClient(emu.url("dfs", DEV_ACCOUNT), credential=account_sas(read=True, write=True, delete=True, list=True, create=True))
    name = uniq("fs")
    fsc = svc.create_file_system(name)
    assert name in [f.name for f in svc.list_file_systems(name_starts_with=name)]
    f = fsc.create_file("d/f.txt")
    f.upload_data(b"hello", overwrite=True)
    assert read_file(f) == b"hello"
    f.delete_file()
    fsc.delete_file_system()


def test_account_sas_read_only_can_read_but_not_write(emu, fs):
    put_file(fs, "f", b"abc")
    sas = account_sas(read=True, list=True)
    client = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "f", credential=sas)
    assert client.get_file_properties().size == 3
    assert read_file(client) == b"abc"
    with pytest.raises(HttpResponseError) as e:
        client.append_data(b"abcd", 3, 4)
    assert_error(e, "AuthorizationPermissionMismatch", 403)


def test_account_sas_without_create_cannot_create_file(emu, fs):
    sas = account_sas(read=True, list=True)
    fsc = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        fsc.create_file("new.txt")
    assert_error(e, "AuthorizationPermissionMismatch", 403)
    assert not fs.get_file_client("new.txt").exists()


def test_account_sas_without_delete_cannot_delete(emu, fs):
    put_file(fs, "f", b"abc")
    sas = account_sas(read=True, write=True, create=True, list=True)
    client = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "f", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.delete_file()
    assert_error(e, "AuthorizationPermissionMismatch", 403)
    assert fs.get_file_client("f").exists()


def test_account_sas_without_list_cannot_list_paths(emu, fs):
    fs.create_file("f")
    sas = account_sas(read=True, write=True, create=True, delete=True)
    fsc = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(fsc.get_paths())
    assert_error(e, "AuthorizationPermissionMismatch", 403)


def test_account_sas_listing_paths_with_list_permission(emu, fs):
    fs.create_file("a/f")
    sas = account_sas(list=True, read=True)
    fsc = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    assert tree(fsc) == {"a": True, "a/f": False}


@pytest.mark.docs
def test_account_sas_without_service_resource_type_cannot_list_filesystems(emu):
    sas = account_sas(read=True, list=True, types=ResourceTypes(object=True))
    svc = DataLakeServiceClient(emu.url("dfs", DEV_ACCOUNT), credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(svc.list_file_systems())
    assert_error(e, "AuthorizationResourceTypeMismatch", 403)


def test_account_sas_can_rename(emu, fs):
    put_file(fs, "old", b"abc")
    sas = account_sas(read=True, write=True, create=True, delete=True, list=True)
    client = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "old", credential=sas)
    new = client.rename_file(fs.file_system_name + "/new")
    assert read_file(new) == b"abc"
    assert not fs.get_file_client("old").exists()


# -- expired / tampered / malformed -----------------------------------------
def test_expired_account_sas_is_403_authentication_failed(emu, fs):
    sas = account_sas(
        read=True,
        list=True,
        expiry=datetime.now(timezone.utc) - timedelta(minutes=5),
        start=datetime.now(timezone.utc) - timedelta(hours=1),
    )
    fsc = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(fsc.get_paths())
    assert_error(e, "AuthenticationFailed", 403)


def test_expired_filesystem_sas_on_blob_endpoint_is_403(emu, fs):
    sas = generate_file_system_sas(
        DEV_ACCOUNT,
        fs.file_system_name,
        DEV_KEY,
        FileSystemSasPermissions(read=True, list=True),
        datetime.now(timezone.utc) - timedelta(minutes=5),
        start=datetime.now(timezone.utc) - timedelta(hours=1),
    )
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.get_file_system_properties()
    assert_error(e, "AuthenticationFailed", 403)


def test_sas_not_yet_valid_is_403(emu, fs):
    sas = generate_file_system_sas(
        DEV_ACCOUNT,
        fs.file_system_name,
        DEV_KEY,
        FileSystemSasPermissions(read=True, list=True),
        datetime.now(timezone.utc) + timedelta(hours=2),
        start=datetime.now(timezone.utc) + timedelta(hours=1),
    )
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(client.get_paths())
    assert_error(e, "AuthenticationFailed", 403)


def test_tampered_signature_is_403_on_dfs(emu, fs):
    sas = tamper(fs_sas(fs.file_system_name, read=True, list=True))
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(client.get_paths())
    assert_error(e, "AuthenticationFailed", 403)


def test_tampered_signature_is_403_on_blob(emu, fs):
    sas = tamper(fs_sas(fs.file_system_name, read=True, list=True))
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.get_file_system_properties()
    assert_error(e, "AuthenticationFailed", 403)


def test_tampered_permissions_invalidate_signature(emu, fs):
    sas = fs_sas(fs.file_system_name, read=True, list=True).replace("sp=rl", "sp=rwdl")
    assert "sp=rwdl" in sas
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.create_file("x")
    assert_error(e, "AuthenticationFailed", 403)


def test_sas_for_other_filesystem_does_not_work(emu, fs, fs2):
    sas = fs_sas(fs2.file_system_name, read=True, list=True)
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(client.get_paths())
    assert e.value.status_code == 403


@pytest.mark.parametrize("how", ["appended", "truncated", "prefixed"])
def test_signature_must_match_exactly(emu, fs, how):
    change = {"appended": lambda s: s + "A", "truncated": lambda s: s[:-3], "prefixed": lambda s: "A" + s}[how]
    sas = sas_with_signature(fs_sas(fs.file_system_name, read=True, list=True), change)
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        list(client.get_paths())
    assert_error(e, "AuthenticationFailed", 403)


# -- filesystem SAS ----------------------------------------------------------
def test_filesystem_sas_read_write_delete_list(emu, fs):
    sas = fs_sas(fs.file_system_name, read=True, write=True, delete=True, list=True)
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    f = client.create_file("d/f.txt")
    f.append_data(b"abc", 0, 3, flush=True)
    assert read_file(f) == b"abc"
    assert tree(client) == {"d": True, "d/f.txt": False}
    f.delete_file()
    assert tree(client) == {"d": True}


def test_filesystem_sas_without_write_cannot_create(emu, fs):
    sas = fs_sas(fs.file_system_name, read=True, list=True)
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.create_file("x")
    assert_error(e, "AuthorizationPermissionMismatch", 403)


def test_filesystem_sas_without_delete_cannot_delete(emu, fs):
    put_file(fs, "x", b"1")
    sas = fs_sas(fs.file_system_name, read=True, write=True, list=True)
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    with pytest.raises(HttpResponseError) as e:
        client.delete_file("x")
    assert_error(e, "AuthorizationPermissionMismatch", 403)


def test_filesystem_sas_rename_directory(emu, fs):
    """Recorded: a filesystem SAS with rwd can rename a directory inside the filesystem."""
    fs.create_directory("olddir")
    sas = fs_sas(fs.file_system_name, read=True, write=True, delete=True)
    d = DataLakeDirectoryClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "olddir", credential=sas)
    new = d.rename_directory(fs.file_system_name + "/newdir")
    assert new.get_directory_properties() is not None
    assert tree(fs) == {"newdir": True}


def test_rename_file_to_other_filesystem_with_destination_sas(emu, fs, fs2):
    """Recorded: source client uses the source filesystem's SAS; the destination SAS follows a `?` in new_name."""
    src_sas = fs_sas(fs.file_system_name, read=True, write=True, delete=True)
    f = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "oldfile", credential=src_sas)
    f.create_file()
    f.append_data(b"abc", 0, 3, flush=True)
    dst_sas = fs_sas(fs2.file_system_name, read=True, write=True, delete=True)
    new = f.rename_file(fs2.file_system_name + "/" + "new?file" + "?" + dst_sas)
    assert new.get_file_properties().name == "new?file"
    assert read_file(fs2.get_file_client("new?file")) == b"abc"
    assert not fs.get_file_client("oldfile").exists()


def test_filesystem_sas_with_stored_access_policy(emu, fs):
    put_file(fs, "f", b"abc")
    policy = AccessPolicy(
        permission=FileSystemSasPermissions(read=True, list=True),
        start=datetime.now(timezone.utc) - timedelta(minutes=5),
        expiry=expiry(),
    )
    fs.set_file_system_access_policy({"readonly": policy})
    sas = generate_file_system_sas(DEV_ACCOUNT, fs.file_system_name, DEV_KEY, policy_id="readonly")
    client = FileSystemClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, credential=sas)
    assert tree(client) == {"f": False}
    with pytest.raises(HttpResponseError) as e:
        client.create_file("nope")
    assert e.value.status_code == 403


# -- directory SAS -----------------------------------------------------------
def test_directory_sas_create_sub_directory_but_not_delete(emu, fs):
    """Recorded: a directory SAS with only `create` can create a sub-directory; delete is refused."""
    fs.create_directory("d1")
    sas = dir_sas(fs.file_system_name, "d1", create=True)
    d = DataLakeDirectoryClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "d1", credential=sas)
    d.create_sub_directory("subdir")
    assert fs.get_directory_client("d1/subdir").exists()
    with pytest.raises(HttpResponseError) as e:
        d.delete_directory()
    assert e.value.status_code == 403
    assert fs.get_directory_client("d1").exists()


def test_directory_sas_read_files_inside_sub_directories(emu, fs):
    put_file(fs, "d1/sub/f.txt", b"abc")
    sas = dir_sas(fs.file_system_name, "d1", read=True, list=True)
    f = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "d1/sub/f.txt", credential=sas)
    assert read_file(f) == b"abc"
    assert f.get_file_properties().size == 3
    # (listing with a directory SAS is not tested: the request path is the filesystem, and neither the
    # recordings nor the docs say how Azure validates a directory SAS there.)


def test_directory_sas_does_not_open_sibling_directory(emu, fs):
    put_file(fs, "d1/f", b"1")
    put_file(fs, "d2/f", b"2")
    sas = dir_sas(fs.file_system_name, "d1", read=True, write=True, create=True, delete=True, list=True)
    sibling = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "d2/f", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        sibling.get_file_properties()
    assert e.value.status_code == 403
    with pytest.raises(HttpResponseError) as e:
        sibling.append_data(b"x", 1, 1)
    assert e.value.status_code == 403
    with pytest.raises(HttpResponseError) as e:
        DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "d2/new", credential=sas).create_file()
    assert e.value.status_code == 403
    assert read_file(fs.get_file_client("d2/f")) == b"2"


def test_directory_sas_does_not_open_directory_with_same_prefix(emu, fs):
    put_file(fs, "dir/f", b"1")
    put_file(fs, "dir2/f", b"2")
    sas = dir_sas(fs.file_system_name, "dir", read=True)
    other = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "dir2/f", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        other.get_file_properties()
    assert e.value.status_code == 403


# -- file SAS ----------------------------------------------------------------
def test_file_sas_read(emu, fs):
    put_file(fs, "d/f.txt", b"abc")
    sas = file_sas(fs.file_system_name, "d/f.txt", read=True)
    f = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "d/f.txt", credential=sas)
    assert f.get_file_properties().size == 3
    assert read_file(f) == b"abc"


def test_file_sas_read_only_cannot_write_or_delete(emu, fs):
    put_file(fs, "f.txt", b"abc")
    sas = file_sas(fs.file_system_name, "f.txt", read=True)
    f = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "f.txt", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        f.append_data(b"abcd", 3, 4)
    assert_error(e, "AuthorizationPermissionMismatch", 403)
    with pytest.raises(HttpResponseError) as e:
        f.delete_file()
    assert_error(e, "AuthorizationPermissionMismatch", 403)
    assert read_file(fs.get_file_client("f.txt")) == b"abc"


def test_file_sas_write_and_delete(emu, fs):
    fs.create_file("f.txt")
    sas = file_sas(fs.file_system_name, "f.txt", read=True, write=True, delete=True)
    f = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "f.txt", credential=sas)
    f.append_data(b"abc", 0, 3, flush=True)
    assert read_file(f) == b"abc"
    f.delete_file()
    assert not fs.get_file_client("f.txt").exists()


def test_file_sas_does_not_open_other_file(emu, fs):
    put_file(fs, "a.txt", b"a")
    put_file(fs, "b.txt", b"b")
    sas = file_sas(fs.file_system_name, "a.txt", read=True)
    other = DataLakeFileClient(emu.url("dfs", DEV_ACCOUNT), fs.file_system_name, "b.txt", credential=sas)
    with pytest.raises(HttpResponseError) as e:
        other.get_file_properties()
    assert e.value.status_code == 403


# -- anonymous / public access -----------------------------------------------
@pytest.mark.docs
def test_anonymous_request_to_private_filesystem_is_refused(emu, fs):
    import requests

    put_file(fs, "f", b"abc")
    r = requests.get(f"{emu.url('blob', DEV_ACCOUNT)}/{fs.file_system_name}/f", timeout=30)
    assert r.status_code in (401, 404)
    assert r.headers.get("x-ms-error-code") in ("ResourceNotFound", "NoAuthenticationInformation")


@pytest.mark.docs
def test_anonymous_read_of_file_in_public_filesystem(emu, service, uniq):
    import requests

    fsc = service.create_file_system(uniq("pub"), public_access=PublicAccess.FILE)
    put_file(fsc, "f", b"abc")
    r = requests.get(f"{emu.url('blob', DEV_ACCOUNT)}/{fsc.file_system_name}/f", timeout=30)
    assert r.status_code == 200
    assert r.content == b"abc"
