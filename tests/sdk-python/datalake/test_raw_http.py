"""Raw HTTP (requests + account SAS) for what the SDK hides: headers, JSON/XML bodies, status codes.

Recorded shapes (from real Azure): dfs errors are JSON `{"error":{"code","message"}}` with
`x-ms-error-code`; blob errors are XML; list paths is JSON with string-valued fields.
"""

import base64
import re

import pytest

from dlutil import json_of, parse_props_header, props_header, put_file

pytestmark = pytest.mark.usefixtures("emu")


def _rfc1123(s):
    return re.fullmatch(r"[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT", s) is not None


# -- success responses -------------------------------------------------------
def test_create_file_response_headers(raw, fs):
    r = raw.dfs("PUT", f"{fs.file_system_name}/f.txt", {"resource": "file"})
    assert r.status_code == 201
    assert r.headers["ETag"].startswith('"0x') or r.headers["ETag"].startswith("0x")
    assert _rfc1123(r.headers["Last-Modified"])
    assert r.headers["x-ms-request-id"]
    assert r.headers["x-ms-version"]
    assert r.headers.get("Content-Length") == "0"
    assert r.content == b""


def test_create_directory_response(raw, fs):
    r = raw.dfs("PUT", f"{fs.file_system_name}/d", {"resource": "directory"})
    assert r.status_code == 201
    assert r.headers["ETag"]
    assert _rfc1123(r.headers["Last-Modified"])


def test_append_returns_202_and_flush_returns_200_with_etag(raw, fs):
    base = f"{fs.file_system_name}/f"
    assert raw.dfs("PUT", base, {"resource": "file"}).status_code == 201
    r = raw.dfs("PATCH", base, {"action": "append", "position": "0"}, data=b"hello")
    assert r.status_code == 202
    assert r.headers["x-ms-request-id"]
    r = raw.dfs("PATCH", base, {"action": "flush", "position": "5"})
    assert r.status_code == 200
    assert r.headers["ETag"]
    assert _rfc1123(r.headers["Last-Modified"])


def test_x_ms_version_is_echoed_and_request_id_present(raw, fs):
    r = raw.dfs("PUT", f"{fs.file_system_name}/f", {"resource": "file"}, headers={"x-ms-version": "2021-08-06"})
    assert r.status_code == 201
    assert r.headers["x-ms-version"] == "2021-08-06"
    assert r.headers["x-ms-request-id"]


@pytest.mark.docs
def test_client_request_id_is_echoed(raw, fs):
    r = raw.dfs(
        "PUT", f"{fs.file_system_name}/f", {"resource": "file"}, headers={"x-ms-client-request-id": "my-req-1"}
    )
    assert r.headers.get("x-ms-client-request-id") == "my-req-1"


def test_request_ids_differ_between_requests(raw, fs):
    a = raw.dfs("PUT", f"{fs.file_system_name}/a", {"resource": "file"})
    b = raw.dfs("PUT", f"{fs.file_system_name}/b", {"resource": "file"})
    assert a.headers["x-ms-request-id"] != b.headers["x-ms-request-id"]


# -- list paths JSON ---------------------------------------------------------
def test_list_paths_json_shape(raw, fs):
    put_file(fs, "d/f.txt", b"hello")
    r = raw.dfs("GET", fs.file_system_name, {"resource": "filesystem", "recursive": "true"})
    assert r.status_code == 200
    assert r.headers["Content-Type"].startswith("application/json")
    body = json_of(r)
    assert set(body) == {"paths"}
    by_name = {p["name"]: p for p in body["paths"]}
    assert list(by_name) == ["d", "d/f.txt"]
    d, f = by_name["d"], by_name["d/f.txt"]
    for entry in (d, f):
        assert entry["owner"] == "$superuser"
        assert entry["group"] == "$superuser"
        assert isinstance(entry["etag"], str) and entry["etag"]
        assert _rfc1123(entry["lastModified"])
        assert entry["creationTime"].isdigit()
        assert isinstance(entry["contentLength"], str)
        assert entry["expiryTime"] == "0"
    assert d["isDirectory"] == "true"
    assert "isDirectory" not in f
    assert d["permissions"] == "rwxr-x---"
    assert f["permissions"] == "rw-r-----"
    assert f["contentLength"] == "5"
    assert d["contentLength"] == "0"


def test_list_paths_empty_filesystem_json(raw, fs):
    r = raw.dfs("GET", fs.file_system_name, {"resource": "filesystem", "recursive": "true"})
    assert r.status_code == 200
    assert json_of(r) == {"paths": []}


def test_list_paths_continuation_header(raw, fs):
    for i in range(5):
        fs.create_file(f"f{i}")
    r = raw.dfs("GET", fs.file_system_name, {"resource": "filesystem", "recursive": "true", "maxResults": "2"})
    assert r.status_code == 200
    assert [p["name"] for p in json_of(r)["paths"]] == ["f0", "f1"]
    token = r.headers["x-ms-continuation"]
    assert token
    r2 = raw.dfs(
        "GET",
        fs.file_system_name,
        {"resource": "filesystem", "recursive": "true", "maxResults": "10", "continuation": token},
    )
    assert [p["name"] for p in json_of(r2)["paths"]] == ["f2", "f3", "f4"]
    assert "x-ms-continuation" not in r2.headers


def test_list_paths_directory_parameter_and_non_recursive(raw, fs):
    put_file(fs, "a/f1", b"1")
    put_file(fs, "a/sub/f2", b"2")
    put_file(fs, "b/f3", b"3")
    r = raw.dfs("GET", fs.file_system_name, {"resource": "filesystem", "recursive": "false", "directory": "a"})
    assert [p["name"] for p in json_of(r)["paths"]] == ["a/f1", "a/sub"]


# -- access control headers --------------------------------------------------
def test_get_access_control_headers(raw, fs):
    fs.create_directory("d")
    r = raw.dfs("HEAD", f"{fs.file_system_name}/d", {"action": "getAccessControl", "upn": "false"})
    assert r.status_code == 200
    assert r.headers["x-ms-owner"] == "$superuser"
    assert r.headers["x-ms-group"] == "$superuser"
    assert r.headers["x-ms-permissions"] == "rwxr-x---"
    assert r.headers["x-ms-acl"] == "user::rwx,group::r-x,other::---"
    assert r.headers["ETag"]
    assert r.content == b""


def test_set_access_control_response(raw, fs):
    fs.create_file("f")
    r = raw.dfs("PATCH", f"{fs.file_system_name}/f", {"action": "setAccessControl"}, headers={"x-ms-permissions": "0777"})
    assert r.status_code == 200
    assert r.headers["x-ms-namespace-enabled"] == "true"
    assert r.headers["ETag"]
    g = raw.dfs("HEAD", f"{fs.file_system_name}/f", {"action": "getAccessControl"})
    assert g.headers["x-ms-permissions"] == "rwxrwxrwx"
    assert g.headers["x-ms-acl"] == "user::rwx,group::rwx,other::rwx"


def test_set_access_control_recursive_json_body(raw, fs):
    put_file(fs, "d/a/f1", b"1")
    put_file(fs, "d/f2", b"2")
    r = raw.dfs(
        "PATCH",
        f"{fs.file_system_name}/d",
        {"action": "setAccessControlRecursive", "mode": "set"},
        headers={"x-ms-acl": "user::rwx,group::r-x,other::rwx"},
    )
    assert r.status_code == 200
    assert r.headers["x-ms-namespace-enabled"] == "true"
    assert json_of(r) == {"directoriesSuccessful": 2, "failedEntries": [], "failureCount": 0, "filesSuccessful": 2}


def test_set_access_control_recursive_max_records_continuation_header(raw, fs):
    for i in range(4):
        fs.create_file(f"d/f{i}")
    r = raw.dfs(
        "PATCH",
        f"{fs.file_system_name}/d",
        {"action": "setAccessControlRecursive", "mode": "set", "maxRecords": "2"},
        headers={"x-ms-acl": "user::rwx,group::r-x,other::rwx"},
    )
    assert r.status_code == 200
    body = json_of(r)
    assert body["directoriesSuccessful"] + body["filesSuccessful"] == 2
    assert r.headers["x-ms-continuation"]


# -- blob endpoint view ------------------------------------------------------
def test_blob_head_of_file_has_resource_type_and_acl_headers(raw, fs):
    put_file(fs, "f", b"abc")
    r = raw.blob("HEAD", f"{fs.file_system_name}/f")
    assert r.status_code == 200
    assert r.headers["x-ms-resource-type"] == "file"
    assert r.headers["x-ms-owner"] == "$superuser"
    assert r.headers["x-ms-group"] == "$superuser"
    assert r.headers["x-ms-permissions"] == "rw-r-----"
    assert r.headers["x-ms-acl"] == "user::rw-,group::r--,other::---"
    assert r.headers["Content-Length"] == "3"
    assert r.headers["x-ms-blob-type"] == "BlockBlob"
    assert r.headers["x-ms-lease-state"] == "available"
    assert "x-ms-meta-hdi_isfolder" not in r.headers


def test_blob_head_of_directory_has_folder_marker(raw, fs):
    fs.create_directory("d")
    r = raw.blob("HEAD", f"{fs.file_system_name}/d")
    assert r.status_code == 200
    assert r.headers["x-ms-resource-type"] == "directory"
    assert r.headers["x-ms-meta-hdi_isfolder"] == "true"
    assert r.headers["x-ms-permissions"] == "rwxr-x---"
    assert r.headers["Content-Length"] == "0"


def test_blob_get_range_of_file_returns_206(raw, fs):
    put_file(fs, "f", b"0123456789")
    r = raw.blob("GET", f"{fs.file_system_name}/f", headers={"x-ms-range": "bytes=0-3"})
    assert r.status_code == 206
    assert r.content == b"0123"
    assert r.headers["Content-Range"] == "bytes 0-3/10"


# -- properties header encoding ---------------------------------------------
def test_properties_header_is_base64_and_becomes_blob_metadata(raw, fs):
    hdr = props_header(hello="world", number="42")
    assert hdr == "hello=d29ybGQ=,number=NDI="
    r = raw.dfs("PUT", f"{fs.file_system_name}/f", {"resource": "file"}, headers={"x-ms-properties": hdr})
    assert r.status_code == 201
    h = raw.blob("HEAD", f"{fs.file_system_name}/f")
    assert h.headers["x-ms-meta-hello"] == "world"
    assert h.headers["x-ms-meta-number"] == "42"


@pytest.mark.docs
def test_dfs_set_properties_action_replaces_metadata(raw, fs):
    """Path - Update action=setProperties sets x-ms-properties (base64 values)."""
    fs.create_file("f", metadata={"old": "1"})
    r = raw.dfs(
        "PATCH",
        f"{fs.file_system_name}/f",
        {"action": "setProperties"},
        headers={"x-ms-properties": props_header(new="two")},
    )
    assert r.status_code == 200
    assert r.headers["ETag"]
    h = raw.blob("HEAD", f"{fs.file_system_name}/f")
    assert h.headers["x-ms-meta-new"] == "two"
    assert "x-ms-meta-old" not in h.headers


@pytest.mark.docs
def test_dfs_head_returns_properties_header_base64(raw, fs):
    fs.create_file("f", metadata={"hello": "world", "number": "42"})
    r = raw.dfs("HEAD", f"{fs.file_system_name}/f")
    assert r.status_code == 200
    assert parse_props_header(r.headers["x-ms-properties"]) == {"hello": "world", "number": "42"}
    assert r.headers["x-ms-resource-type"] == "file"


@pytest.mark.docs
def test_dfs_head_of_directory_reports_resource_type(raw, fs):
    fs.create_directory("d")
    r = raw.dfs("HEAD", f"{fs.file_system_name}/d")
    assert r.status_code == 200
    assert r.headers["x-ms-resource-type"] == "directory"


# -- errors: dfs JSON vs blob XML -------------------------------------------
def test_dfs_error_is_json_path_already_exists(raw, fs):
    """Recorded: If-None-Match: * on an existing path -> 409 PathAlreadyExists with a JSON body."""
    fs.create_file("f")
    r = raw.dfs("PUT", f"{fs.file_system_name}/f", {"resource": "file"}, headers={"If-None-Match": "*"})
    assert r.status_code == 409
    assert r.headers["Content-Type"].startswith("application/json")
    assert r.headers["x-ms-error-code"] == "PathAlreadyExists"
    assert r.headers["x-ms-request-id"]
    body = json_of(r)
    assert set(body) == {"error"}
    assert body["error"]["code"] == "PathAlreadyExists"
    assert isinstance(body["error"]["message"], str) and body["error"]["message"]
    assert int(r.headers["Content-Length"]) == len(r.content)


def test_dfs_error_condition_not_met_json(raw, fs):
    """Recorded: delete with If-Modified-Since == Last-Modified -> 412 ConditionNotMet (JSON)."""
    d = fs.create_directory("d")
    lm = d.get_directory_properties().last_modified
    r = raw.dfs(
        "DELETE",
        f"{fs.file_system_name}/d",
        {"recursive": "true"},
        headers={"If-Modified-Since": lm.strftime("%a, %d %b %Y %H:%M:%S GMT")},
    )
    assert r.status_code == 412
    assert r.headers["x-ms-error-code"] == "ConditionNotMet"
    assert json_of(r)["error"]["code"] == "ConditionNotMet"


def test_blob_error_is_xml_blob_not_found(raw, fs):
    """Recorded: blob endpoint errors are XML."""
    r = raw.blob("GET", f"{fs.file_system_name}/nofile")
    assert r.status_code == 404
    assert r.headers["Content-Type"].startswith("application/xml")
    assert r.headers["x-ms-error-code"] == "BlobNotFound"
    text = r.content.decode("utf-8-sig")
    assert "<Error><Code>BlobNotFound</Code><Message>" in text.replace("\n", "")
    assert "RequestId:" in text


def test_blob_error_container_not_found_xml(raw, uniq):
    r = raw.blob("GET", uniq("nofs"), {"restype": "container"})
    assert r.status_code == 404
    assert r.headers["x-ms-error-code"] == "ContainerNotFound"
    assert "<Code>ContainerNotFound</Code>" in r.content.decode("utf-8-sig")


@pytest.mark.docs
def test_dfs_error_path_not_found_json(raw, fs):
    r = raw.dfs("DELETE", f"{fs.file_system_name}/nofile")
    assert r.status_code == 404
    assert r.headers["x-ms-error-code"] == "PathNotFound"
    assert json_of(r)["error"]["code"] == "PathNotFound"


@pytest.mark.docs
def test_dfs_error_filesystem_not_found_json(raw, uniq):
    r = raw.dfs("GET", uniq("nofs"), {"resource": "filesystem", "recursive": "true"})
    assert r.status_code == 404
    assert r.headers["x-ms-error-code"] == "FilesystemNotFound"
    assert json_of(r)["error"]["code"] == "FilesystemNotFound"


@pytest.mark.docs
def test_dfs_error_invalid_flush_position_json(raw, fs):
    base = f"{fs.file_system_name}/f"
    raw.dfs("PUT", base, {"resource": "file"})
    raw.dfs("PATCH", base, {"action": "append", "position": "0"}, data=b"abc")
    r = raw.dfs("PATCH", base, {"action": "flush", "position": "10"})
    assert r.status_code == 400
    assert r.headers["x-ms-error-code"] == "InvalidFlushPosition"
    assert json_of(r)["error"]["code"] == "InvalidFlushPosition"


# -- delete semantics --------------------------------------------------------
def test_delete_file_returns_200(raw, fs):
    fs.create_file("f")
    r = raw.dfs("DELETE", f"{fs.file_system_name}/f")
    assert r.status_code == 200
    assert not fs.get_file_client("f").exists()


@pytest.mark.docs
def test_delete_empty_directory_non_recursive_is_ok(raw, fs):
    fs.create_directory("d")
    r = raw.dfs("DELETE", f"{fs.file_system_name}/d", {"recursive": "false"})
    assert r.status_code == 200
    assert not fs.get_directory_client("d").exists()


@pytest.mark.docs
def test_delete_non_empty_directory_non_recursive_is_409_directory_not_empty(raw, fs):
    put_file(fs, "d/f", b"x")
    r = raw.dfs("DELETE", f"{fs.file_system_name}/d", {"recursive": "false"})
    assert r.status_code == 409
    assert r.headers["x-ms-error-code"] == "DirectoryNotEmpty"
    assert json_of(r)["error"]["code"] == "DirectoryNotEmpty"
    assert fs.get_file_client("d/f").exists()


def test_delete_non_empty_directory_recursive_removes_everything(raw, fs):
    put_file(fs, "d/a/f", b"x")
    r = raw.dfs("DELETE", f"{fs.file_system_name}/d", {"recursive": "true"})
    assert r.status_code == 200
    assert list(fs.get_paths()) == []


# -- rename ------------------------------------------------------------------
def test_rename_legacy_mode_via_raw_request(raw, fs):
    put_file(fs, "old", b"abc")
    r = raw.dfs(
        "PUT",
        f"{fs.file_system_name}/new",
        {"mode": "legacy"},
        headers={"x-ms-rename-source": f"/{fs.file_system_name}/old"},
    )
    assert r.status_code == 201
    assert r.headers["ETag"]
    assert fs.get_file_client("new").exists()
    assert not fs.get_file_client("old").exists()


@pytest.mark.docs
def test_rename_posix_mode_via_raw_request(raw, fs):
    put_file(fs, "old", b"abc")
    r = raw.dfs(
        "PUT",
        f"{fs.file_system_name}/new",
        {"mode": "posix"},
        headers={"x-ms-rename-source": f"/{fs.file_system_name}/old"},
    )
    assert r.status_code == 201
    assert fs.get_file_client("new").exists()
    assert not fs.get_file_client("old").exists()


# -- Path - Read on the dfs endpoint ----------------------------------------
@pytest.mark.docs
def test_dfs_read_path_returns_content_and_range(raw, fs):
    put_file(fs, "f", b"0123456789")
    r = raw.dfs("GET", f"{fs.file_system_name}/f")
    assert r.status_code == 200
    assert r.content == b"0123456789"
    assert r.headers["Content-Length"] == "10"
    assert r.headers["x-ms-resource-type"] == "file"
    r2 = raw.dfs("GET", f"{fs.file_system_name}/f", headers={"Range": "bytes=2-4"})
    assert r2.status_code == 206
    assert r2.content == b"234"


# -- Filesystem operations on the dfs endpoint (not used by the SDK) ---------
@pytest.mark.docs
def test_dfs_filesystem_create_list_delete(raw, uniq):
    name = uniq("rawfs")
    r = raw.dfs("PUT", name, {"resource": "filesystem"})
    assert r.status_code == 201
    assert r.headers["ETag"]
    lst = raw.dfs("GET", "", {"resource": "account", "prefix": name})
    assert lst.status_code == 200
    names = [f["name"] for f in json_of(lst)["filesystems"]]
    assert names == [name]
    dup = raw.dfs("PUT", name, {"resource": "filesystem"})
    assert dup.status_code == 409
    d = raw.dfs("DELETE", name, {"resource": "filesystem"})
    assert d.status_code == 202


@pytest.mark.docs
def test_dfs_filesystem_properties_header(raw, service, uniq):
    name = uniq("rawfs")
    service.create_file_system(name, metadata={"hello": "world"})
    r = raw.dfs("HEAD", name, {"resource": "filesystem"})
    assert r.status_code == 200
    assert parse_props_header(r.headers["x-ms-properties"]) == {"hello": "world"}
