"""Raw REST requests: what an SDK hides. Headers of every answer, the XML of lists and errors with their extra elements, the
routing rules, limits and what a malformed request is told. Each answer was checked against Azure in November 2026.
"""

import base64
import hashlib
import re

import pytest

import fileutil

ERROR_BODY = re.compile(r"^\ufeff<\?xml version=\"1.0\" encoding=\"utf-8\"\?><Error><Code>(\w+)</Code><Message>(.*?)\nRequestId:[0-9a-f-]{36}\nTime:\d{4}-\d\d-\d\dT[\d:.]{16}Z</Message>(.*)</Error>$", re.S)


@pytest.fixture
def raw(emu):
    return fileutil.Raw(emu)


@pytest.fixture
def sh(raw, share):
    return f"/{share.share_name}"


def error(r):
    m = ERROR_BODY.match(fileutil.text(r))
    assert m, fileutil.text(r)
    return m.group(1), m.group(2), m.group(3)


def test_every_answer_has_the_common_headers(raw, sh):
    r = raw.call("GET", sh, [("restype", "share")])
    assert r.headers["Server"] == "Windows-Azure-File/1.0 Microsoft-HTTPAPI/2.0"
    assert re.fullmatch(r"[0-9a-f-]{36}", r.headers["x-ms-request-id"])
    assert r.headers["x-ms-version"] == fileutil.VERSION
    assert r.headers["Date"].endswith("GMT")
    assert r.headers["ETag"].startswith('"0x') and r.headers["Last-Modified"].endswith("GMT")


def test_client_request_id_is_echoed(raw, sh):
    r = raw.call("GET", sh, [("restype", "share")], {"x-ms-client-request-id": "abc-123"})
    assert r.headers["x-ms-client-request-id"] == "abc-123"


def test_error_body_shape(raw, sh):
    r = raw.call("GET", sh + "x", [("restype", "share")])
    assert r.status_code == 404
    assert r.headers["x-ms-error-code"] == "ShareNotFound" and r.headers["Content-Type"] == "application/xml"
    assert error(r)[:2] == ("ShareNotFound", "The specified share does not exist.")


def test_head_errors_have_no_body(raw, sh):
    r = raw.call("HEAD", sh + "/nope.txt", [])
    assert r.status_code == 404 and r.headers["x-ms-error-code"] == "ResourceNotFound" and r.content == b""


def test_unknown_x_ms_version(raw, sh):
    r = raw.call("GET", sh, [("restype", "share")], version="banana")
    assert r.status_code == 400
    assert error(r)[0] == "InvalidHeaderValue" and "<HeaderName>x-ms-version</HeaderName><HeaderValue>banana</HeaderValue>" in error(r)[2]


def test_share_url_needs_restype(raw, sh):
    r = raw.call("GET", sh, [])
    assert r.status_code == 400
    code, message, extra = error(r)
    assert code == "InvalidUri" and extra == f"<UriPath>{sh}</UriPath>"


def test_wrong_verb_and_unknown_comp(raw, sh):
    r = raw.call("POST", sh, [("restype", "share")])
    assert (r.status_code, r.headers["x-ms-error-code"]) == (405, "UnsupportedHttpVerb")
    r = raw.call("GET", sh, [("restype", "share"), ("comp", "bogus")])
    code, _, extra = error(r)
    assert (r.status_code, code) == (400, "InvalidQueryParameterValue")
    assert extra == "<QueryParameterName>comp</QueryParameterName><QueryParameterValue>bogus</QueryParameterValue>"


def test_unknown_comp_on_a_directory_or_file_names_restype(raw, sh):
    # Azure words this one oddly: it names `restype` and gives it the value of `comp`
    r = raw.call("GET", sh + "/d", [("comp", "list")])
    assert error(r)[2] == "<QueryParameterName>restype</QueryParameterName><QueryParameterValue>list</QueryParameterValue>"


def test_missing_and_invalid_headers_name_the_header(raw, sh):
    r = raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "file"})
    assert error(r)[0] == "MissingRequiredHeader" and error(r)[2] == "<HeaderName>x-ms-content-length</HeaderName>"
    r = raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "block", "x-ms-content-length": "1"})
    assert error(r)[2] == "<HeaderName>x-ms-type</HeaderName><HeaderValue>block</HeaderValue>"
    r = raw.call("PUT", sh + "/f.txt", [], {"x-ms-content-length": "1"})
    assert error(r)[2] == "<HeaderName>x-ms-type</HeaderName>"


def test_share_list_xml(raw, share):
    r = raw.call("GET", "/", [("comp", "list"), ("prefix", share.share_name), ("include", "metadata")])
    body = fileutil.text(r)
    assert r.headers["Content-Type"] == "application/xml"
    assert re.match(r'^\ufeff<\?xml version="1.0" encoding="utf-8"\?><EnumerationResults ServiceEndpoint="https?://[^"]+/">', body)
    assert f"<Prefix>{share.share_name}</Prefix><Shares><Share><Name>{share.share_name}</Name><Properties><Last-Modified>" in body
    assert "<Quota>102400</Quota><AccessTier>TransactionOptimized</AccessTier><AccessTierChangeTime>" in body
    assert "<DefaultEncryptionScope>$account-encryption-key</DefaultEncryptionScope><DenyEncryptionScopeOverride>false</DenyEncryptionScopeOverride></Properties><Metadata /></Share></Shares><NextMarker /></EnumerationResults>" in body


def test_share_list_marker_and_bad_marker(raw, service):
    prefix = fileutil.unique("mk")
    clients = [service.create_share(f"{prefix}{i}") for i in range(2)]
    try:
        r = raw.call("GET", "/", [("comp", "list"), ("prefix", prefix), ("maxresults", "1")])
        m = re.search(r"<NextMarker>(.*)</NextMarker>", fileutil.text(r))
        assert m and m.group(1) == f"/{raw.account}/{prefix}1"
        r2 = raw.call("GET", "/", [("comp", "list"), ("prefix", prefix), ("marker", m.group(1))])
        assert f"<Name>{prefix}1</Name>" in fileutil.text(r2) and f"<Name>{prefix}0</Name>" not in fileutil.text(r2)
        bad = raw.call("GET", "/", [("comp", "list"), ("marker", "garbage")])
        assert bad.status_code == 400
        bad = raw.call("GET", "/", [("comp", "list"), ("maxresults", "0")])
        assert error(bad)[0] == "OutOfRangeQueryParameterValue"
        assert error(bad)[2].endswith("<MinimumAllowed>1</MinimumAllowed><MaximumAllowed>2147483647</MaximumAllowed>")
        bad = raw.call("GET", "/", [("comp", "list"), ("include", "bogus")])
        assert error(bad)[0] == "InvalidQueryParameterValue"
    finally:
        for c in clients:
            fileutil.delete_share(c)


def test_directory_list_xml_and_marker(raw, share, sh):
    d = share.create_directory("d")
    for n in ("a.txt", "b.txt", "c.txt"):
        d.upload_file(n, b"12345")
    d.create_subdirectory("sub")
    r = raw.call("GET", sh + "/d", [("restype", "directory"), ("comp", "list"), ("maxresults", "2")])
    body = fileutil.text(r)
    assert re.search(r'<EnumerationResults ServiceEndpoint="[^"]+" ShareName="' + share.share_name + r'" DirectoryPath="d"><MaxResults>2</MaxResults><DirectoryId>\d+</DirectoryId><Entries>', body)
    assert re.search(r"<File><Name>a.txt</Name><FileId>\d+</FileId><Properties><Content-Length>5</Content-Length></Properties></File>", body)
    assert re.search(r"<NextMarker>1!8!Yy50eHQ-</NextMarker>", body)  # the marker is the name of the next entry: c.txt
    r2 = raw.call("GET", sh + "/d", [("restype", "directory"), ("comp", "list"), ("marker", "1!8!Yy50eHQ-")])
    assert "<Name>c.txt</Name>" in fileutil.text(r2) and "<Name>a.txt</Name>" not in fileutil.text(r2)
    assert re.search(r"<Directory><Name>sub</Name><FileId>\d+</FileId><Properties /></Directory>", fileutil.text(r2))
    bad = raw.call("GET", sh + "/d", [("restype", "directory"), ("comp", "list"), ("include", "bogus")])
    assert error(bad)[0] == "InvalidQueryParameterValue"


def test_directory_list_extended_info_elements(raw, share, sh):
    share.get_file_client("f.txt").upload_file(b"x")
    r = raw.call("GET", sh, [("restype", "directory"), ("comp", "list"), ("include", "Timestamps,ETag,Attributes,PermissionKey")])
    assert re.search(r"<File><Name>f.txt</Name><FileId>\d+</FileId><Properties><Content-Length>1</Content-Length><CreationTime>[^<]+Z</CreationTime>"
                     r"<LastAccessTime>[^<]+</LastAccessTime><LastWriteTime>[^<]+</LastWriteTime><ChangeTime>[^<]+</ChangeTime><Last-Modified>[^<]+GMT</Last-Modified>"
                     r"<Etag>\"0x[0-9A-F]+\"</Etag></Properties><Attributes>Archive</Attributes><PermissionKey>\d+\*\d+</PermissionKey></File>", fileutil.text(r))


def test_file_create_response_headers(raw, sh):
    r = raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "file", "x-ms-content-length": "10"})
    assert r.status_code == 201 and r.content == b""
    for h in ("ETag", "Last-Modified", "x-ms-request-server-encrypted", "x-ms-file-permission-key", "x-ms-file-attributes",
              "x-ms-file-creation-time", "x-ms-file-last-write-time", "x-ms-file-change-time", "x-ms-file-id", "x-ms-file-parent-id"):
        assert h in r.headers, h
    assert r.headers["x-ms-file-attributes"] == "Archive"
    assert re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{7}Z", r.headers["x-ms-file-creation-time"])


def test_file_get_and_head_headers(raw, sh):
    raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "file", "x-ms-content-length": "10", "x-ms-content-type": "text/plain", "x-ms-meta-k": "v"})
    raw.call("PUT", sh + "/f.txt", [("comp", "range")], {"x-ms-range": "bytes=0-4", "x-ms-write": "update"}, b"hello")
    head = raw.call("HEAD", sh + "/f.txt", [])
    assert head.status_code == 200 and head.headers["Content-Length"] == "10" and head.headers["x-ms-type"] == "File"
    assert head.headers["x-ms-meta-k"] == "v" and head.headers["x-ms-lease-state"] == "available" and head.headers["x-ms-lease-status"] == "unlocked"
    assert head.headers["x-ms-server-encrypted"] == "true"
    get = raw.call("GET", sh + "/f.txt", [])
    assert get.content == b"hello" + bytes(5) and get.headers["Accept-Ranges"] == "bytes" and get.headers["Content-Type"] == "text/plain"
    part = raw.call("GET", sh + "/f.txt", [], {"x-ms-range": "bytes=1-3", "x-ms-range-get-content-md5": "true"})
    assert part.status_code == 206 and part.content == b"ell"
    assert part.headers["Content-Range"] == "bytes 1-3/10"
    assert part.headers["Content-MD5"] == base64.b64encode(hashlib.md5(b"ell").digest()).decode()
    past = raw.call("GET", sh + "/f.txt", [], {"x-ms-range": "bytes=50-60"})
    assert past.status_code == 416 and past.headers["Content-Range"] == "bytes */10"
    assert error(past)[0] == "InvalidRange"
    # a range that cannot be read is ignored
    assert raw.call("GET", sh + "/f.txt", [], {"x-ms-range": "bogus"}).status_code == 200


def test_put_range_rules(raw, sh):
    raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "file", "x-ms-content-length": "10"})
    q = [("comp", "range")]
    ok = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-4", "x-ms-write": "update"}, b"hello")
    assert ok.status_code == 201 and ok.headers["Content-MD5"] == base64.b64encode(hashlib.md5(b"hello").digest()).decode()
    assert "x-ms-file-last-write-time" in ok.headers and ok.headers["x-ms-request-server-encrypted"] == "true"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-3", "x-ms-write": "update"}, b"hello")
    assert error(r)[0] == "InvalidHeaderValue" and error(r)[2] == "<HeaderName>Content-Length</HeaderName><HeaderValue>5</HeaderValue>"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-4", "x-ms-write": "update", "Content-MD5": base64.b64encode(hashlib.md5(b"hellx").digest()).decode()}, b"hello")
    code, _, extra = error(r)
    assert code == "Md5Mismatch" and "<UserSpecifiedMd5>" in extra and "<ServerCalculatedMd5>XUFAKrxLKna5cZ2REBfFkg==</ServerCalculatedMd5>" in extra
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=5-2", "x-ms-write": "update"}, b"x")
    assert error(r)[0] == "InvalidHeaderValue"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-write": "update"}, b"x")
    assert error(r)[0] == "MissingRequiredHeader" and error(r)[2] == "<HeaderName>Range</HeaderName>"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-0"}, b"x")
    assert error(r)[0] == "MissingRequiredHeader" and error(r)[2] == "<HeaderName>x-ms-write</HeaderName>"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-0", "x-ms-write": "bogus"}, b"x")
    assert error(r)[0] == "InvalidHeaderValue"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=0-0", "x-ms-write": "clear"}, b"x")
    assert error(r)[0] == "InvalidHeaderValue" and error(r)[2] == "<HeaderName>Content-Length</HeaderName><HeaderValue>1</HeaderValue>"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-range": "bytes=5-20", "x-ms-write": "clear"})
    assert (r.status_code, r.headers["x-ms-error-code"]) == (416, "InvalidRange")


def test_put_range_over_4_mib_is_413(raw, sh):
    raw.call("PUT", sh + "/big.bin", [], {"x-ms-type": "file", "x-ms-content-length": str(6 * 1024 * 1024)})
    r = raw.call("PUT", sh + "/big.bin", [("comp", "range")], {"x-ms-range": f"bytes=0-{4 * 1024 * 1024}", "x-ms-write": "update"}, b"x" * (4 * 1024 * 1024 + 1))
    assert r.status_code == 413
    code, message, extra = error(r)
    assert code == "RequestBodyTooLarge" and extra == "<MaxLimit>4194304</MaxLimit>"
    ok = raw.call("PUT", sh + "/big.bin", [("comp", "range")], {"x-ms-range": f"bytes=0-{4 * 1024 * 1024 - 1}", "x-ms-write": "update"}, b"x" * (4 * 1024 * 1024))
    assert ok.status_code == 201
    cleared = raw.call("PUT", sh + "/big.bin", [("comp", "range")], {"x-ms-range": f"bytes=0-{5 * 1024 * 1024 - 1}", "x-ms-write": "clear"})
    assert cleared.status_code == 201
    assert "<Ranges />" in fileutil.text(raw.call("GET", sh + "/big.bin", [("comp", "rangelist")]))


def test_range_list_xml(raw, sh):
    raw.call("PUT", sh + "/f.bin", [], {"x-ms-type": "file", "x-ms-content-length": "2048"})
    raw.call("PUT", sh + "/f.bin", [("comp", "range")], {"x-ms-range": "bytes=0-4", "x-ms-write": "update"}, b"hello")
    raw.call("PUT", sh + "/f.bin", [("comp", "range")], {"x-ms-range": "bytes=1030-1034", "x-ms-write": "update"}, b"world")
    r = raw.call("GET", sh + "/f.bin", [("comp", "rangelist")])
    assert r.headers["x-ms-content-length"] == "2048"
    assert fileutil.text(r).lstrip("\ufeff").endswith("<Ranges><Range><Start>0</Start><End>511</End></Range><Range><Start>1024</Start><End>1535</End></Range></Ranges>")


def test_lease_headers_and_errors_on_a_share(raw, sh):
    L, M = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
    q = [("restype", "share"), ("comp", "lease")]
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "acquire", "x-ms-lease-duration": "-1", "x-ms-proposed-lease-id": L})
    assert r.status_code == 201 and r.headers["x-ms-lease-id"] == L
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "acquire", "x-ms-lease-duration": "-1"})
    assert error(r)[:2] == ("LeaseAlreadyPresent", "There is already a lease present.")
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "release", "x-ms-lease-id": M})
    assert error(r)[:2] == ("LeaseIdMismatchWithLeaseOperation", "The lease ID specified did not match the lease ID for the file share.")
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "break", "x-ms-lease-break-period": "30"})
    assert r.status_code == 202 and r.headers["x-ms-lease-time"] == "30"
    assert raw.call("GET", sh, [("restype", "share")]).headers["x-ms-lease-state"] == "breaking"
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "release", "x-ms-lease-id": L})
    assert r.status_code == 200
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "break"})
    assert error(r)[:2] == ("LeaseNotPresentWithLeaseOperation", "There is currently no lease on the file share.")
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "acquire", "x-ms-lease-duration": "5"})
    assert error(r)[0] == "InvalidHeaderValue"
    r = raw.call("PUT", sh, q, {})
    assert error(r)[0] == "MissingRequiredHeader"
    r = raw.call("PUT", sh, q, {"x-ms-lease-action": "bogus"})
    assert error(r)[2] == "<HeaderName>x-ms-lease-action</HeaderName><HeaderValue>bogus</HeaderValue>"


def test_file_leases_have_no_renew_and_break_at_once(raw, sh):
    L = "11111111-1111-1111-1111-111111111111"
    raw.call("PUT", sh + "/f.txt", [], {"x-ms-type": "file", "x-ms-content-length": "1"})
    q = [("comp", "lease")]
    assert raw.call("PUT", sh + "/f.txt", q, {"x-ms-lease-action": "acquire", "x-ms-lease-duration": "-1", "x-ms-proposed-lease-id": L}).status_code == 201
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-lease-action": "renew", "x-ms-lease-id": L})
    assert error(r)[0] == "InvalidHeaderValue"
    r = raw.call("PUT", sh + "/f.txt", q, {"x-ms-lease-action": "break", "x-ms-lease-break-period": "10"})
    assert r.status_code == 202 and r.headers["x-ms-lease-time"] == "0"
    head = raw.call("HEAD", sh + "/f.txt", [])
    assert (head.headers["x-ms-lease-state"], head.headers["x-ms-lease-status"]) == ("broken", "unlocked")


def test_permissions(raw, sh):
    sddl = "O:S-1-5-21-2127521184-1604012920-1887927527-21560G:S-1-5-21-2127521184-1604012920-1887927527-513D:(A;;FA;;;SY)(A;;FA;;;BA)"
    q = [("restype", "share"), ("comp", "filepermission")]
    r = raw.call("PUT", sh, q, {"Content-Type": "application/json"}, '{"permission": "%s"}' % sddl)
    assert r.status_code == 201
    key = r.headers["x-ms-file-permission-key"]
    assert re.fullmatch(r"\d+\*\d+", key)
    got = raw.call("GET", sh, q, {"x-ms-file-permission-key": key})
    # Azure hands the descriptor back normalised (an empty audit section is added) with its format
    assert got.status_code == 200 and got.json()["format"] == "sddl" and got.json()["permission"].startswith(sddl)
    r = raw.call("GET", sh, q, {"x-ms-file-permission-key": "1*2"})
    assert error(r)[:2] == ("FileInvalidPermissionKey", "The specified file permission key does not exist in the share.")
    r = raw.call("GET", sh, q, {"x-ms-file-permission-key": "bogus"})
    assert error(r)[0] == "InvalidHeaderValue"
    r = raw.call("GET", sh, q, {})
    assert error(r)[0] == "MissingRequiredHeader"
    r = raw.call("PUT", sh, q, {"Content-Type": "application/json"}, '{"permission": "garbage"}')
    assert error(r)[:2] == ("FileInvalidPermission", "The specified file permission is not valid.")
    r = raw.call("PUT", sh, q, {"Content-Type": "application/json"}, "{}")
    assert error(r)[0] == "InvalidJsonDocument"
    # a directory made with the key has it
    d = raw.call("PUT", sh + "/withkey", [("restype", "directory")], {"x-ms-file-permission-key": key})
    # Azure answers with the key of the descriptor as it applies to the directory, which is not the one it was given
    assert d.status_code == 201 and re.fullmatch(r"\d+\*\d+", d.headers["x-ms-file-permission-key"])
    both = raw.call("PUT", sh + "/both", [("restype", "directory")], {"x-ms-file-permission-key": key, "x-ms-file-permission": "inherit"})
    assert error(both)[0] == "BothPermissionAndPermissionKeyHeaderPresent"
    unknown = raw.call("PUT", sh + "/unknown", [("restype", "directory")], {"x-ms-file-permission-key": "1*2"})
    assert error(unknown)[0] == "FileInvalidPermissionKey"


def test_more_than_five_stored_access_policies(raw, sh):
    ids = "".join(f"<SignedIdentifier><Id>p{i}</Id><AccessPolicy><Permission>r</Permission><Expiry>2030-01-01T00:00:00Z</Expiry></AccessPolicy></SignedIdentifier>" for i in range(6))
    r = raw.call("PUT", sh, [("restype", "share"), ("comp", "acl")], {"Content-Type": "application/xml"}, f"<SignedIdentifiers>{ids}</SignedIdentifiers>")
    assert (r.status_code, error(r)[0]) == (400, "InvalidXmlDocument")


def test_acl_times_come_back_with_seven_digits(raw, sh):
    body = ('<?xml version="1.0" encoding="utf-8"?><SignedIdentifiers><SignedIdentifier><Id>pol1</Id><AccessPolicy>'
            "<Start>2026-01-01T00:00:00Z</Start><Expiry>2027-01-01T00:00:00Z</Expiry><Permission>rwdl</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>")
    assert raw.call("PUT", sh, [("restype", "share"), ("comp", "acl")], {"Content-Type": "application/xml"}, body).status_code == 200
    got = raw.call("GET", sh, [("restype", "share"), ("comp", "acl")])
    assert fileutil.text(got).lstrip("\ufeff") == ('<?xml version="1.0" encoding="utf-8"?><SignedIdentifiers><SignedIdentifier><Id>pol1</Id><AccessPolicy>'
        "<Start>2026-01-01T00:00:00.0000000Z</Start><Expiry>2027-01-01T00:00:00.0000000Z</Expiry><Permission>rwdl</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>")


def test_handles_lists_and_force_close(raw, sh, share):
    share.create_directory("d")
    r = raw.call("GET", sh + "/d", [("comp", "listhandles")])
    assert fileutil.text(r).lstrip("\ufeff").endswith("<EnumerationResults><Entries /><NextMarker /></EnumerationResults>")
    r = raw.call("PUT", sh + "/d", [("comp", "forceclosehandles")], {"x-ms-handle-id": "*"})
    assert (r.status_code, r.headers["x-ms-number-of-handles-closed"], r.headers["x-ms-number-of-handles-failed"]) == (200, "0", "0")
    r = raw.call("PUT", sh + "/d", [("comp", "forceclosehandles")], {})
    assert error(r)[2] == "<HeaderName>x-ms-handle-id</HeaderName>"
    r = raw.call("GET", sh + "/nope", [("comp", "listhandles")])
    assert (r.status_code, r.headers["x-ms-error-code"]) == (404, "ResourceNotFound")


def test_links_are_not_for_smb_shares(raw, sh, emu):
    if emu.azure:
        pytest.skip("Azure drops the connection for these on an SMB share")
    r = raw.call("PUT", sh + "/l", [("restype", "symboliclink")], {"x-ms-link-text": "a"})
    assert r.status_code == 501
