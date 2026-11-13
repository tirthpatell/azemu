"""One account, two endpoints: what is created through dfs is visible through blob and vice versa.

A filesystem is a container; a file is a block blob; a directory is a zero-length blob with metadata
`hdi_isfolder=true` (recorded in HEAD responses of directories).
"""

import pytest
from azure.core.exceptions import ResourceNotFoundError
from azure.storage.blob import ContentSettings as BlobContentSettings

from dlutil import assert_error, put_file, read_file, tree


# -- filesystem <-> container ------------------------------------------------
def test_filesystem_shows_in_list_containers(fs, blob_service):
    assert fs.file_system_name in [c.name for c in blob_service.list_containers(name_starts_with=fs.file_system_name)]


def test_container_created_through_blob_is_a_filesystem(service, blob_service, uniq):
    name = uniq("c")
    blob_service.create_container(name)
    fsc = service.get_file_system_client(name)
    assert fsc.exists()
    assert name in [f.name for f in service.list_file_systems(name_starts_with=name)]
    assert list(fsc.get_paths()) == []


def test_container_created_through_blob_accepts_dfs_paths(service, blob_service, uniq):
    name = uniq("c")
    blob_service.create_container(name)
    f = service.get_file_system_client(name).create_file("d/f.txt")
    assert f.exists()


def test_filesystem_metadata_visible_on_container(service, blob_service, uniq):
    name = uniq("fs")
    service.create_file_system(name, metadata={"hello": "world"})
    assert blob_service.get_container_client(name).get_container_properties().metadata == {"hello": "world"}


def test_container_metadata_visible_on_filesystem(fs, blob_container):
    blob_container.set_container_metadata({"a": "1"})
    assert fs.get_file_system_properties().metadata == {"a": "1"}


def test_filesystem_delete_removes_container(service, blob_service, uniq):
    name = uniq("fs")
    service.create_file_system(name).delete_file_system()
    assert name not in [c.name for c in blob_service.list_containers(name_starts_with=name)]


def test_container_delete_removes_filesystem(service, blob_service, uniq):
    name = uniq("c")
    blob_service.create_container(name)
    blob_service.delete_container(name)
    assert not service.get_file_system_client(name).exists()


# -- dfs file -> blob --------------------------------------------------------
def test_dfs_file_content_readable_as_blob(fs, blob_container):
    put_file(fs, "dir/f.txt", b"hello datalake")
    assert blob_container.get_blob_client("dir/f.txt").download_blob().readall() == b"hello datalake"


def test_dfs_file_properties_match_blob_properties(fs, blob_container):
    f = put_file(fs, "f.txt", b"hello", metadata={"k": "v"})
    dprops = f.get_file_properties()
    bprops = blob_container.get_blob_client("f.txt").get_blob_properties()
    assert bprops.size == dprops.size == 5
    assert bprops.etag.strip('"') == dprops.etag.strip('"')
    assert bprops.last_modified == dprops.last_modified
    assert bprops.metadata == {"k": "v"}
    assert bprops.blob_type == "BlockBlob"


def test_dfs_flush_etag_matches_blob_etag(fs, blob_container):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    resp = f.flush_data(3)
    bprops = blob_container.get_blob_client("f").get_blob_properties()
    assert bprops.etag.strip('"') == resp["etag"].strip('"')


def test_dfs_upload_data_content_type_default_on_blob(fs, blob_container):
    fs.get_file_client("f").upload_data(b"abc", overwrite=True)
    assert blob_container.get_blob_client("f").get_blob_properties().content_settings.content_type == "application/octet-stream"


def test_dfs_set_http_headers_visible_on_blob(fs, blob_container):
    from azure.storage.filedatalake import ContentSettings

    f = fs.create_file("f")
    f.set_http_headers(ContentSettings(content_type="text/plain", content_language="fr"))
    cs = blob_container.get_blob_client("f").get_blob_properties().content_settings
    assert cs.content_type == "text/plain"
    assert cs.content_language == "fr"


def test_dfs_file_blob_range_read(fs, blob_container):
    put_file(fs, "f", b"0123456789")
    assert blob_container.get_blob_client("f").download_blob(offset=3, length=4).readall() == b"3456"


def test_dfs_file_unflushed_data_is_not_visible_to_blob(fs, blob_container):
    f = fs.create_file("f")
    f.append_data(b"abc", 0, 3)
    assert blob_container.get_blob_client("f").get_blob_properties().size == 0


# -- directory as a blob -----------------------------------------------------
def test_directory_is_zero_length_blob_with_hdi_isfolder(fs, blob_container):
    fs.create_directory("d1")
    bprops = blob_container.get_blob_client("d1").get_blob_properties()
    assert bprops.size == 0
    assert bprops.metadata["hdi_isfolder"] == "true"


def test_directory_metadata_visible_next_to_hdi_isfolder(fs, blob_container):
    fs.create_directory("d1", metadata={"hello": "world"})
    md = blob_container.get_blob_client("d1").get_blob_properties().metadata
    assert md["hdi_isfolder"] == "true"
    assert md["hello"] == "world"


def test_implicit_parent_directories_are_folder_blobs(fs, blob_container):
    fs.create_file("a/b/f.txt")
    for p in ("a", "a/b"):
        assert blob_container.get_blob_client(p).get_blob_properties().metadata["hdi_isfolder"] == "true"
    assert "hdi_isfolder" not in (blob_container.get_blob_client("a/b/f.txt").get_blob_properties().metadata or {})


@pytest.mark.docs
def test_blob_flat_listing_shows_directories_and_files(fs, blob_container):
    put_file(fs, "a/b/f.txt", b"x")
    put_file(fs, "top.txt", b"y")
    got = {b.name: b for b in blob_container.list_blobs(include=["metadata"])}
    assert set(got) == {"a", "a/b", "a/b/f.txt", "top.txt"}
    assert got["a"].metadata["hdi_isfolder"] == "true"
    assert got["a/b"].metadata["hdi_isfolder"] == "true"
    assert got["a/b/f.txt"].size == 1


@pytest.mark.docs
def test_blob_listing_with_prefix_over_directory_tree(fs, blob_container):
    put_file(fs, "a/f1", b"1")
    put_file(fs, "a/sub/f2", b"2")
    put_file(fs, "b/f3", b"3")
    got = {b.name for b in blob_container.list_blobs(name_starts_with="a/")}
    assert got == {"a/f1", "a/sub", "a/sub/f2"}


@pytest.mark.docs
def test_blob_listing_with_delimiter_hides_nested_files(fs, blob_container):
    put_file(fs, "root.txt", b"x")
    put_file(fs, "a/b/nested.txt", b"y")
    names = {getattr(i, "name", None) for i in blob_container.walk_blobs(name_starts_with="", delimiter="/")}
    assert "root.txt" in names
    assert "a/b/nested.txt" not in names
    assert names & {"a", "a/"}


@pytest.mark.docs
def test_blob_listing_with_delimiter_inside_directory(fs, blob_container):
    put_file(fs, "a/f1", b"1")
    put_file(fs, "a/sub/f2", b"2")
    names = {i.name for i in blob_container.walk_blobs(name_starts_with="a/", delimiter="/")}
    assert "a/f1" in names
    assert "a/sub/f2" not in names
    assert names & {"a/sub", "a/sub/"}


# -- blob -> dfs -------------------------------------------------------------
def test_blob_upload_visible_in_get_paths(fs, blob_container):
    blob_container.get_blob_client("top.txt").upload_blob(b"hello")
    [p] = list(fs.get_paths())
    assert p.name == "top.txt"
    assert p.is_directory is False
    assert p.content_length == 5


@pytest.mark.docs
def test_blob_upload_in_virtual_folder_creates_directories(fs, blob_container):
    blob_container.get_blob_client("x/y/z.txt").upload_blob(b"data")
    assert tree(fs) == {"x": True, "x/y": True, "x/y/z.txt": False}
    assert fs.get_directory_client("x/y").exists()


def test_blob_upload_readable_through_dfs(fs, blob_container):
    blob_container.get_blob_client("d/f.bin").upload_blob(b"\x00\x01\x02payload")
    f = fs.get_file_client("d/f.bin")
    assert read_file(f) == b"\x00\x01\x02payload"
    assert f.get_file_properties().size == 10


def test_blob_upload_with_metadata_and_content_type_visible_through_dfs(fs, blob_container):
    blob_container.get_blob_client("f").upload_blob(
        b"abc", metadata={"k": "v"}, content_settings=BlobContentSettings(content_type="text/plain")
    )
    props = fs.get_file_client("f").get_file_properties()
    assert props.metadata == {"k": "v"}
    assert props.content_settings.content_type == "text/plain"


@pytest.mark.docs
def test_blob_created_file_has_default_access_control(fs, blob_container):
    blob_container.get_blob_client("f").upload_blob(b"abc")
    ac = fs.get_file_client("f").get_access_control()
    assert ac["owner"] == "$superuser"
    assert ac["permissions"] == "rw-r-----"


def test_blob_created_file_can_be_renamed_through_dfs(fs, blob_container):
    blob_container.get_blob_client("old").upload_blob(b"abc")
    new = fs.get_file_client("old").rename_file(fs.file_system_name + "/new")
    assert read_file(new) == b"abc"
    assert blob_container.get_blob_client("new").download_blob().readall() == b"abc"
    assert not blob_container.get_blob_client("old").exists()


def test_blob_overwrite_changes_dfs_size(fs, blob_container):
    bc = blob_container.get_blob_client("f")
    bc.upload_blob(b"123456789")
    bc.upload_blob(b"ab", overwrite=True)
    [p] = list(fs.get_paths())
    assert p.content_length == 2


# -- deletes and renames across endpoints ------------------------------------
def test_delete_through_dfs_is_visible_through_blob(fs, blob_container):
    f = put_file(fs, "f", b"abc")
    f.delete_file()
    bc = blob_container.get_blob_client("f")
    assert not bc.exists()
    with pytest.raises(ResourceNotFoundError) as e:
        bc.get_blob_properties()
    assert_error(e, "BlobNotFound", 404)


def test_delete_through_blob_is_visible_through_dfs(fs, blob_container):
    f = put_file(fs, "f", b"abc")
    blob_container.get_blob_client("f").delete_blob()
    assert not f.exists()
    assert list(fs.get_paths()) == []


def test_directory_delete_through_dfs_removes_folder_blobs(fs, blob_container):
    put_file(fs, "d/a/f", b"x")
    fs.get_directory_client("d").delete_directory()
    assert list(blob_container.list_blobs()) == []


def test_directory_rename_through_dfs_is_visible_through_blob(fs, blob_container):
    put_file(fs, "src/a/f.txt", b"x")
    fs.get_directory_client("src").rename_directory(fs.file_system_name + "/dst")
    names = {b.name for b in blob_container.list_blobs()}
    assert names == {"dst", "dst/a", "dst/a/f.txt"}
    assert not blob_container.get_blob_client("src").exists()


@pytest.mark.docs
def test_delete_empty_directory_through_blob(fs, blob_container):
    fs.create_directory("d")
    blob_container.get_blob_client("d").delete_blob()
    assert not fs.get_directory_client("d").exists()


def test_blob_lease_on_dfs_file_blocks_dfs_delete(fs, blob_container):
    f = put_file(fs, "f", b"abc")
    lease = blob_container.get_blob_client("f").acquire_lease()
    assert f.get_file_properties().lease.state == "leased"
    with pytest.raises(Exception) as e:
        f.delete_file()
    assert getattr(e.value, "status_code", None) == 412
    f.delete_file(lease=lease.id)
    assert not f.exists()
