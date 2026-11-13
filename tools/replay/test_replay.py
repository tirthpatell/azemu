"""Tests for replay.py, mockserver.py and run_all.py.

    .venv/bin/pytest tools/replay -q -p no:cacheprovider

The end-to-end tests start `mockserver.MockServer` on port 0 (a free port) and replay real
recordings from refs/recordings against it; they are skipped when the corpus is absent.
"""
from __future__ import annotations

import base64
import json
import os
import socket
import sys
import threading

import pytest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mockserver  # noqa: E402
import replay  # noqa: E402
import run_all  # noqa: E402

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SDK = os.path.join(REPO, "refs", "recordings", "python", "sdk")
HAVE_CORPUS = os.path.isdir(SDK)
needs_corpus = pytest.mark.skipif(not HAVE_CORPUS, reason="refs/recordings not present")


# --------------------------------------------------------------------------- helpers

def settings(**kw) -> replay.Settings:
    base = dict(gateway="http://127.0.0.1:4999", gateway_host="127.0.0.1", gateway_port=4999, force_label=None,
                auth_defaults=replay.parse_auth_overrides(None), timeout=10.0, reset=False, dry_run=False,
                time_shift=True, classify=True, gateway_scheme="http")
    base.update(kw)
    return replay.Settings(**base)


def entry(method="GET", uri="https://acct.blob.core.windows.net/c/b", status=200, req_headers=None, req_body=None,
          resp_headers=None, resp_body=None) -> dict:
    return {"RequestUri": uri, "RequestMethod": method, "RequestHeaders": req_headers or {},
            "RequestBody": req_body, "StatusCode": status, "ResponseHeaders": resp_headers or {},
            "ResponseBody": resp_body}


def prep(i=0, **kw) -> replay.PreparedEntry:
    p = replay.prepare_entry(entry(**kw), i)
    assert not isinstance(p, replay.Skipped)
    return p


def error_entry(method, uri, status, code, **kw) -> dict:
    return entry(method, uri, status, resp_headers={"x-ms-error-code": code, "Content-Type": "application/xml"},
                 resp_body=f"<?xml version='1.0'?><Error><Code>{code}</Code><Message>m</Message></Error>", **kw)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def corpus_files(rel: str, n: int, exclude: str = "") -> list[str]:
    files = replay.find_recordings([os.path.join(SDK, rel)])
    return [p for name, p in files if not exclude or exclude not in os.path.basename(name)][:n]


def replay_against(mock_url: str, files: list[str], **kw) -> list[replay.RecordingResult]:
    gw = mock_url.rsplit(":", 1)
    kw.setdefault("reset", True)
    st = settings(gateway=mock_url, gateway_port=int(gw[1]), **kw)
    client = replay.Client(mock_url, 30)
    try:
        return [replay.run_recording(os.path.basename(f), f, st, client) for f in files]
    finally:
        client.close()


# --------------------------------------------------------------------------- body decoding

class TestDecodeBody:
    def test_none(self):
        assert replay.decode_body(None) == (b"", "none")

    def test_text_json(self):
        assert replay.decode_body('{"a":1}', "application/json") == (b'{"a":1}', "text")

    def test_empty_string(self):
        assert replay.decode_body("", "application/json") == (b"", "text")

    def test_dict_and_list_and_int(self):
        assert replay.decode_body({"a": "é"}) == ('{"a":"é"}'.encode(), "json")
        assert replay.decode_body([1, 2]) == (b"[1,2]", "json")
        assert replay.decode_body(5) == (b"5", "json")

    def test_plain_list_is_concatenated(self):
        assert replay.decode_body(["a\r\n", "b\r\n"]) == (b"a\r\nb\r\n", "list")

    def test_list_with_base64_pieces_is_decoded(self):
        raw = b"POST https://x/t HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{}"
        piece = "b64:" + base64.b64encode(raw).decode()
        body, kind = replay.decode_body(["--b\r\n", "\r\n", piece, "\r\n--b--\r\n"])
        assert kind == "list"
        assert body == b"--b\r\n\r\n" + raw + b"\r\n--b--\r\n"
        assert b"b64:" not in body

    def test_list_with_undecodable_b64_prefix_stays_text(self):
        assert replay.decode_body(["b64:not base64!"])[0] == b"b64:not base64!"

    def test_base64_string_for_binary_content_type(self):
        enc = base64.b64encode(b"\x00\x01\xff").decode()
        assert replay.decode_body(enc, "application/octet-stream") == (b"\x00\x01\xff", "base64")
        assert replay.decode_body(enc, None)[1] == "base64"

    def test_textual_type_is_not_decoded(self):
        enc = base64.b64encode(b"abcd").decode()  # "YWJjZA==" looks like base64 but is text
        assert replay.decode_body(enc, "text/plain") == (enc.encode(), "text")

    def test_non_base64_binary_type_falls_back_to_text(self):
        assert replay.decode_body("not base64!", "application/octet-stream")[1] == "text-fallback"

    @needs_corpus
    def test_table_batch_request_from_corpus_has_no_literal_b64(self):
        f = os.path.join(SDK, "tables", "azure-data-tables", "tests", "recordings",
                         "test_table_batch.pyTestTableBatchtest_batch_single_insert.json")
        raw = replay.load_recording(f)[1]
        p = replay.prepare_entry(raw, 1)
        assert p.req_body_kind == "list"
        assert b"b64:" not in p.req_body
        assert b"POST https://" in p.req_body
        assert b"b64:" not in p.rec_body and b"HTTP/1.1 201" in p.rec_body
        assert len(p.req_body) == int(replay.header_get(raw["RequestHeaders"], "Content-Length"))

    @needs_corpus
    def test_list_bodies_match_recorded_content_length(self):
        checked = 0
        for path in corpus_files("tables/azure-data-tables", 80):
            for raw in replay.load_recording(path):
                for side, hk in (("RequestBody", "RequestHeaders"), ("ResponseBody", "ResponseHeaders")):
                    if isinstance(raw.get(side), list) and raw["RequestMethod"] != "HEAD":
                        h = replay.lower_headers(raw.get(hk))
                        body, _ = replay.decode_body(raw[side], h.get("content-type"))
                        if h.get("content-length", "").isdigit():
                            assert len(body) == int(h["content-length"])
                            checked += 1
        assert checked > 20


# --------------------------------------------------------------------------- URIs, hosts, labels

class TestUriAndHosts:
    def test_target_is_byte_for_byte(self):
        uri = "https://a.blob.core.windows.net/c/b%2Fx%3D%20y?comp=block&blockid=MQ%3D%3D&x=%7e"
        scheme, host, port, target = replay.split_request_uri(uri)
        assert (scheme, host, port) == ("https", "a.blob.core.windows.net", None)
        assert target == "/c/b%2Fx%3D%20y?comp=block&blockid=MQ%3D%3D&x=%7e"

    def test_port_and_empty_path(self):
        assert replay.split_request_uri("http://h:1234")[2:] == (1234, "/")
        assert replay.split_request_uri("https://h?restype=service")[3] == "/?restype=service"

    def test_sanitize_target_only_touches_illegal_characters(self):
        assert replay.sanitize_target("/a%2Fb?c=%3D") == "/a%2Fb?c=%3D"
        assert replay.sanitize_target("/é b") == "/%C3%A9%20b"

    def test_request_line_on_the_wire_keeps_percent_encoding(self):
        seen = {}
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)

        def serve():
            c, _ = srv.accept()
            data = b""
            while b"\r\n\r\n" not in data:
                data += c.recv(4096)
            seen["line"] = data.split(b"\r\n")[0]
            c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            c.close()

        t = threading.Thread(target=serve, daemon=True)
        t.start()
        port = srv.getsockname()[1]
        cl = replay.Client(f"http://127.0.0.1:{port}", 5)
        st, _h, _b = cl.request("GET", "/c/a%2Fb%3D?blockid=MQ%3D%3D&s=%7e", "x.blob.localhost", [], None)
        cl.close()
        t.join(5)
        srv.close()
        assert st == 200
        assert seen["line"] == b"GET /c/a%2Fb%3D?blockid=MQ%3D%3D&s=%7e HTTP/1.1"

    @pytest.mark.parametrize("host,expected", [
        ("acct.blob.core.windows.net", ("acct", "blob")),
        ("Sanitized.queue.core.windows.net", ("Sanitized", "queue")),
        ("x.table.core.windows.net", ("x", "table")),
        ("x.table.cosmos.azure.com", ("x", "table")),
        ("vaultname.vault.azure.net", ("vaultname", "vault")),
        ("x.managedhsm.azure.net", ("x", "vault")),
        ("fake_app_config.azconfig-test.io", ("fake_app_config", "azconfig")),
        ("x.dfs.core.windows.net", ("x", "dfs")),
        ("MixedCase.blob.core.windows.net", ("MixedCase", "blob")),
    ])
    def test_resolve_service(self, host, expected):
        assert replay.resolve_service(host) == expected

    @pytest.mark.parametrize("host", ["login.microsoftonline.com", "127.0.0.1", "localhost",
                                      "[::1]", ""])
    def test_resolve_service_unknown(self, host):
        assert replay.resolve_service(host) is None

    def test_force_label(self):
        assert replay.resolve_service("x.blob.core.windows.net", "queue") == ("x", "queue")

    def test_host_header_is_account_label_localhost_port(self):
        p = prep(uri="https://acct.queue.core.windows.net/q/messages")
        _t, host, _h, _b = replay.build_request(p, replay.ValueMap(), settings(gateway_port=4321), 0.0)
        assert host == "acct.queue.localhost:4321"

    def test_unknown_host_is_skipped(self):
        r = replay.prepare_entry(entry(uri="https://login.microsoftonline.com/x"), 0)
        assert isinstance(r, replay.Skipped)


# --------------------------------------------------------------------------- value mapping

class TestValueMap:
    def test_longest_first_and_single_pass(self):
        vm = replay.ValueMap()
        vm.remember("abc123xyz", "NEW1")
        vm.remember("abc123xyz789", "NEW2")
        assert vm.apply("/p/abc123xyz789/q/abc123xyz") == "/p/NEW2/q/NEW1"

    def test_substituted_value_is_not_substituted_again(self):
        vm = replay.ValueMap()
        vm.remember("aaa111", "bbb222")
        vm.remember("bbb222", "ccc333")
        assert vm.apply("aaa111 bbb222") == "bbb222 ccc333"

    def test_placeholders_are_never_keys(self):
        vm = replay.ValueMap()
        for v in ("Sanitized", "sanitized-thing", "REDACTED", "00000000-0000-0000-0000-000000000000", "fake_token_value",
                  "aaaaaaaa", "storagename"):
            vm.remember(v, "real1234")
        assert len(vm) == 0

    def test_percent_encoded_variant(self):
        vm = replay.ValueMap()
        vm.remember("a/b+c=1234", "x/y+z=5678")
        assert vm.apply("/u?k=a%2Fb%2Bc%3D1234") == "/u?k=x%2Fy%2Bz%3D5678"

    def test_apply_json_maps_leaves_only(self):
        vm = replay.ValueMap()
        vm.remember("tok12345", "new99999")
        assert vm.apply_json({"tok12345": ["tok12345", 5, {"a": "x tok12345 y"}]}) == \
            {"tok12345": ["new99999", 5, {"a": "x new99999 y"}]}

    def _pair(self, rec, act, name, max_len):
        vm, hist = replay.ValueMap(), replay.RequestHistory()
        replay.consider_pair(vm, hist, rec, act, name, max_len=max_len)
        return vm

    def test_value_generated_by_server_is_mapped(self):
        assert self._pair("0x8DA6F3688F776F0", "0x1234ABCD99", "etag", replay.MAX_MAP_LEN).pairs

    def test_long_body_leaf_is_mapped(self):
        rec, act = "A1" * 3000, "B2" * 3000
        assert len(rec) > replay.MAX_MAP_LEN
        assert self._pair(rec, act, "value", replay.MAX_BODY_MAP_LEN).pairs == {rec: act}

    def test_long_header_value_is_not_mapped(self):
        assert not self._pair("A1" * 3000, "B2" * 3000, "x-ms-foo", replay.MAX_MAP_LEN).pairs

    def test_value_chosen_by_the_client_is_not_mapped(self):
        vm, hist = replay.ValueMap(), replay.RequestHistory()
        hist.add(prep(method="PUT", uri="https://a.blob.core.windows.net/c/blob12345?x=1"))
        replay.consider_pair(vm, hist, "blob12345", "other9999", "name")
        assert not vm.pairs

    def test_short_digitless_dates_numbers_are_not_mapped(self):
        for rec in ("abc", "123456789", "has space 1", "2026-03-30T19:06:48Z", "Mon, 30 Mar 2026 19:06:48 GMT"):
            assert not self._pair(rec, "zzzz1234", "x", replay.MAX_MAP_LEN).pairs

    def test_snapshot_dates_are_mapped(self):
        assert self._pair("2026-03-30T19:06:48.1234567Z", "2026-11-11T01:00:00.0000001Z", "x-ms-snapshot",
                          replay.MAX_MAP_LEN).pairs

    def test_end_to_end_mapping_of_a_long_restore_value(self):
        vm, hist = replay.ValueMap(), replay.RequestHistory()
        rec_resp = {"value": "R1" * 3000}
        act_resp = json.dumps({"value": "E2" * 3000}).encode()
        p = prep(method="POST", uri="https://v.vault.azure.net/secrets/n/backup", resp_body=rec_resp,
                 resp_headers={"Content-Type": "application/json"})
        leaves = replay.body_leaves(p.rec_body, p.rec_ct)
        replay.update_value_map(vm, hist, p, {"content-type": "application/json"}, act_resp, leaves)
        restore = prep(method="POST", uri="https://v.vault.azure.net/secrets/restore", req_body={"value": "R1" * 3000})
        _t, _h, _hd, body = replay.build_request(restore, vm, settings(), 0.0)
        assert json.loads(body) == {"value": "E2" * 3000}


# --------------------------------------------------------------------------- comparison

class TestCompare:
    def run(self, p, status, headers=None, body=b""):
        codes = replay.error_codes(p.rec_headers, p.rec_body, p.rec_ct, p.rec_status)
        return replay.compare_entry(p, status, headers or {}, body, codes)

    def test_status_mismatch_is_hard(self):
        p = prep(status=404)
        hard, _soft, _e, _a = self.run(p, 200)
        assert ("status", "404->200") in hard

    def test_error_code_header_mismatch_is_hard(self):
        p = prep(status=409, resp_headers={"x-ms-error-code": "ContainerAlreadyExists"})
        hard, _s, exp, act = self.run(p, 409, {"x-ms-error-code": "Other"})
        assert hard == [("error-code-header", "ContainerAlreadyExists->Other")]
        assert (exp, act) == ("ContainerAlreadyExists", "Other")

    def test_error_code_in_json_body(self):
        p = prep(status=404, resp_headers={"Content-Type": "application/json"},
                 resp_body={"error": {"code": "SecretNotFound", "message": "x"}})
        assert self.run(p, 404, {"content-type": "application/json"},
                        b'{"error":{"code":"SecretNotFound"}}')[0] == []
        assert self.run(p, 404, {"content-type": "application/json"},
                        b'{"error":{"code":"Other"}}')[0] == [("error-code-body", "SecretNotFound->Other")]

    def test_odata_error_and_xml_error(self):
        assert replay.error_codes({}, b'{"odata.error":{"code":"TableNotFound"}}', "application/json", 404)[1] == \
            "TableNotFound"
        assert replay.error_codes({}, b"<Error><Code>QueueNotFound</Code></Error>", "application/xml", 404)[1] == \
            "QueueNotFound"
        assert replay.error_codes({}, b"<Error><Code>X</Code></Error>", "application/xml", 200)[1] is None

    def test_missing_header_is_soft_and_noise_is_ignored(self):
        p = prep(resp_headers={"ETag": '"1"', "Server": "x", "x-ms-request-id": "r", "Content-Type": "text/plain"},
                 resp_body="hi")
        hard, soft, _e, _a = self.run(p, 200, {"content-type": "text/plain"}, b"hi")
        assert hard == []
        assert sorted(soft) == [("header-missing", "etag"), ("header-missing", "x-ms-request-id")]

    def test_xml_shape(self):
        rec = b"<A><B><C>1</C></B><D/></A>"
        assert replay.shape_paths(rec, "xml") == {"/A", "/A/B", "/A/B/C", "/A/D"}
        p = prep(resp_headers={"Content-Type": "application/xml"}, resp_body=rec.decode())
        _h, soft, _e, _a = self.run(p, 200, {"content-type": "application/xml"}, b"<A><B><X>1</X></B><D/></A>")
        assert ("shape-missing", "/A/B/C") in soft and ("shape-extra", "/A/B/X") in soft

    def test_xml_namespaces_are_ignored(self):
        assert replay.shape_paths(b'<n:A xmlns:n="u"><n:B/></n:A>', "xml") == {"/A", "/A/B"}

    def test_json_shape(self):
        assert replay.shape_paths(b'{"a":{"b":[{"c":1},{"d":2}]},"e":null}', "json") == \
            {"/a", "/a/b", "/a/b[]", "/a/b[]/c", "/a/b[]/d", "/e"}

    def test_json_shape_differences_are_soft(self):
        p = prep(resp_headers={"Content-Type": "application/json"}, resp_body={"a": 1, "b": 2})
        hard, soft, _e, _a = self.run(p, 200, {"content-type": "application/json"}, b'{"a":1,"c":3}')
        assert hard == [] and ("shape-missing", "/b") in soft and ("shape-extra", "/c") in soft

    def test_body_presence(self):
        p = prep(resp_headers={"Content-Type": "application/json"}, resp_body={"a": 1})
        assert ("body-empty", "recorded non-empty, actual empty") in self.run(p, 200)[1]
        p2 = prep()
        assert self.run(p2, 200, body=b"x")[1][0][0] == "body-extra"

    def test_placeholder_recorded_body_has_no_shape(self):
        p = prep(resp_headers={"Content-Type": "application/xml"}, resp_body="Sanitized")
        assert self.run(p, 200, {"content-type": "application/xml"}, b"<A/>")[1] == []

    def test_head_ignores_body(self):
        p = prep(method="HEAD", resp_headers={"Content-Length": "5"})
        assert self.run(p, 200, {"content-length": "5"})[1] == []


# --------------------------------------------------------------------------- patterns and grouping

class TestPatterns:
    @pytest.mark.parametrize("method,label,target,expected", [
        ("PUT", "queue", "/q1/messages/abc?popreceipt=x&visibilitytimeout=5&timeout=3",
         "PUT /{queue}/messages/{id}?popreceipt&visibilitytimeout"),
        ("GET", "queue", "/q1/messages?peekonly=true", "GET /{queue}/messages?peekonly"),
        ("PUT", "blob", "/c/b?comp=block&blockid=MQ%3D%3D", "PUT /{container}/{blob}?comp=block&blockid"),
        ("GET", "blob", "/?comp=properties&restype=service", "GET /?comp=properties&restype=service"),
        ("POST", "table", "/Tables", "POST /Tables"),
        ("DELETE", "table", "/Tables('t1')", "DELETE /Tables('{table}')"),
        ("GET", "table", "/t1(PartitionKey='a',RowKey='b')", "GET /{table}(PartitionKey,RowKey)"),
        ("GET", "vault", "/secrets/n1/v1?api-version=7.0", "GET /secrets/{name}/{version}"),
        ("GET", "vault", "/deletedsecrets/n1?api-version=7.0", "GET /deletedsecrets/{name}"),
        ("POST", "vault", "/keys/n/v/unwrapkey?api-version=7.0", "POST /keys/{name}/{version}/unwrapkey"),
    ])
    def test_operation_pattern(self, method, label, target, expected):
        assert replay.operation_pattern(method, label, target) == expected

    def test_sas_and_version_params_are_ignored(self):
        assert replay.operation_pattern("GET", "blob", "/c/b?sv=1&sig=x&se=2&snapshot=3") == \
            "GET /{container}/{blob}?snapshot"

    def test_group_key(self):
        assert replay.group_key("GET /x", 404, 200, "NotFound", None) == "GET /x  404→200 (NotFound→)"
        assert replay.group_key("GET /x", 200, None, None, None) == "GET /x  200→ERR"
        assert replay.group_key("GET /x", 200, 500, None, None) == "GET /x  200→500"


# --------------------------------------------------------------------------- auth

class TestAuth:
    def test_placeholder_credentials(self):
        for v in ("Sanitized", "sanitized", "Bearer Sanitized", "SharedKey acct:Sanitized", "REDACTED", ""):
            assert replay.is_placeholder_credential(v), v
        for v in ("Bearer eyJhbGciOiJSUzI1NiJ9.abc.def", "SharedKey acct:Zm9vYmFyMTIzNDU2Nzg5MA=="):
            assert not replay.is_placeholder_credential(v), v

    def test_placeholder_on_vault_becomes_bearer(self):
        p = prep(uri="https://v.vault.azure.net/secrets/n", req_headers={"Authorization": "Sanitized"})
        assert p.auth_placeholder and not p.has_auth
        assert replay.choose_auth(p, settings().auth_defaults) == "Bearer sanitized"

    def test_placeholder_on_azconfig_becomes_bearer(self):
        p = prep(uri="https://fake_app_config.azconfig-test.io/kv", req_headers={"Authorization": "Sanitized"})
        assert replay.choose_auth(p, settings().auth_defaults) == "Bearer sanitized"

    def test_placeholder_on_storage_gets_default_storage_auth(self):
        p = prep(uri="https://acct.table.core.windows.net/Tables", req_headers={"Authorization": "Sanitized"})
        assert replay.choose_auth(p, settings().auth_defaults) == "SharedKey acct:replay"

    def test_placeholder_is_removed_from_forwarded_headers(self):
        p = prep(uri="https://acct.table.core.windows.net/Tables", req_headers={"Authorization": "Sanitized"})
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(), 0.0)
        auth = [v for k, v in headers if k.lower() == "authorization"]
        assert auth == ["SharedKey acct:replay"]

    def test_sanitized_customer_key_headers_become_a_real_key_and_its_hash(self):
        # the recordings replace the key and its hash with "Sanitized"; the emulator checks that they agree
        import hashlib
        p = prep(uri="https://acct.blob.core.windows.net/c/b", req_headers={
            "x-ms-encryption-key": "Sanitized", "x-ms-encryption-key-sha256": "Sanitized", "x-ms-encryption-algorithm": "AES256"})
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(), 0.0)
        h = {k.lower(): v for k, v in headers}
        key = base64.b64decode(h["x-ms-encryption-key"])
        assert len(key) == 32
        assert base64.b64decode(h["x-ms-encryption-key-sha256"]) == hashlib.sha256(key).digest()
        assert h["x-ms-encryption-algorithm"] == "AES256"

    def test_real_authorization_is_kept_and_nothing_added(self):
        p = prep(req_headers={"Authorization": "SharedKey acct:Zm9vYmFyMTIzNDU2Nzg5MA=="})
        assert p.has_auth and replay.choose_auth(p, settings().auth_defaults) is None

    def test_default_auth_added_per_label(self):
        d = settings().auth_defaults
        assert replay.choose_auth(prep(uri="https://a.blob.core.windows.net/c"), d) == "SharedKey a:replay"
        assert replay.choose_auth(prep(uri="https://v.vault.azure.net/secrets/n"), d) == "Bearer replay"

    def test_recorded_401_gets_no_credential(self):
        assert replay.choose_auth(prep(uri="https://v.vault.azure.net/secrets/n", status=401),
                                  settings().auth_defaults) is None

    def test_sas_request_gets_no_credential(self):
        assert replay.choose_auth(prep(uri="https://a.blob.core.windows.net/c/b?sv=1&sig=abc"),
                                  settings().auth_defaults) is None

    def test_default_auth_none_disables_placeholder_replacement(self):
        p = prep(uri="https://v.vault.azure.net/secrets/n", req_headers={"Authorization": "Sanitized"})
        assert replay.choose_auth(p, {}) is None

    def test_copy_source_authorization_placeholder(self):
        p = prep(method="PUT", req_headers={"x-ms-copy-source-authorization": "Sanitized"})
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(), 0.0)
        assert ("x-ms-copy-source-authorization", "Bearer sanitized") in headers


# --------------------------------------------------------------------------- date headers

class TestTimeShift:
    @pytest.mark.parametrize("name", [
        "if-modified-since", "if-unmodified-since", "x-ms-source-if-modified-since",
        "x-ms-source-if-unmodified-since", "x-ms-blob-if-modified-since", "x-ms-blob-if-unmodified-since",
        "x-ms-access-tier-if-modified-since", "x-ms-access-tier-if-unmodified-since",
        "x-ms-immutability-policy-until-date", "x-ms-expiry-time"])
    def test_shifted_names(self, name):
        assert replay.TIME_SHIFT_HEADER_RE.match(name)

    @pytest.mark.parametrize("name", ["x-ms-date", "date", "if-match", "x-ms-source-if-match", "last-modified",
                                      "x-ms-version", "x-ms-if-tags"])
    def test_not_shifted_names(self, name):
        assert not replay.TIME_SHIFT_HEADER_RE.match(name)

    def test_build_request_shifts_every_condition_header(self):
        names = ["If-Modified-Since", "x-ms-source-if-unmodified-since", "x-ms-blob-if-modified-since",
                 "x-ms-access-tier-if-unmodified-since"]
        p = prep(method="PUT", req_headers={n: "Mon, 30 Mar 2026 19:00:00 GMT" for n in names} | {
            "x-ms-date": "Mon, 30 Mar 2026 19:00:00 GMT"})
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(), 3600.0)
        got = dict(headers)
        for n in names:
            assert got[n] == "Mon, 30 Mar 2026 20:00:00 GMT"
        assert got["x-ms-date"] == "Mon, 30 Mar 2026 19:00:00 GMT"

    def test_disabled_by_setting(self):
        p = prep(req_headers={"If-Modified-Since": "Mon, 30 Mar 2026 19:00:00 GMT"})
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(time_shift=False), 3600.0)
        assert dict(headers)["If-Modified-Since"] == "Mon, 30 Mar 2026 19:00:00 GMT"


# --------------------------------------------------------------------------- copy sources

BLOB = "https://acct.blob.core.windows.net"


def put_blob(path, status=201, extra=None):
    return prep(method="PUT", uri=f"{BLOB}{path}", status=status,
                req_headers={"x-ms-blob-type": "BlockBlob", **(extra or {})})


def copy_to(path, status=202):
    return prep(method="PUT", uri=f"{BLOB}{path}", status=status, req_headers={"x-ms-copy-source": "Sanitized"})


class TestCopySource:
    def test_write_detection(self):
        assert replay.blob_write_path(put_blob("/c/b")) == "/c/b"
        assert replay.blob_write_path(copy_to("/c/d")) == "/c/d"
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b?comp=blocklist", status=201)) == "/c/b"
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b?comp=page", status=201)) == "/c/b"
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b?comp=appendblock", status=201)) == "/c/b"

    def test_not_writes(self):
        assert replay.blob_write_path(put_blob("/c/b", status=409)) is None
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c?restype=container", status=201)) is None
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b?comp=metadata", status=200)) is None
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b?comp=block&blockid=MQ%3D%3D",
                                           status=201)) is None
        assert replay.blob_write_path(prep(method="GET", uri=f"{BLOB}/c/b")) is None
        assert replay.blob_write_path(prep(method="PUT", uri=f"{BLOB}/c/b", status=201)) is None  # no blob type

    def test_picks_most_recent_other_than_destination(self):
        log = replay.BlobWriteLog()
        for path in ("/c/one", "/c/two", "/c/dest"):
            log.note(put_blob(path))
        assert log.pick("/c/dest") == "/c/two"
        assert log.pick("/c/other") == "/c/dest"

    def test_rewrite_moves_a_blob_to_the_end(self):
        log = replay.BlobWriteLog()
        for path in ("/c/one", "/c/two"):
            log.note(put_blob(path))
        log.note(prep(method="PUT", uri=f"{BLOB}/c/one?comp=appendblock", status=201))
        assert log.pick("/c/dest") == "/c/one"

    def test_no_candidate(self):
        log = replay.BlobWriteLog()
        assert log.pick("/c/dest") is None
        log.note(put_blob("/c/dest"))
        assert log.pick("/c/dest") is None  # the only write is the destination itself

    def test_guess_url_is_on_the_emulator(self):
        log = replay.BlobWriteLog()
        log.note(put_blob("/c/src%20x"))
        url = replay.guess_copy_source(copy_to("/c/dest"), log, settings(gateway_port=4321, gateway_scheme="https"))
        assert url == "https://acct.blob.localhost:4321/c/src%20x"

    def test_header_is_rewritten_only_when_sanitized(self):
        p = copy_to("/c/dest")
        _t, _h, headers, _b = replay.build_request(p, replay.ValueMap(), settings(), 0.0,
                                                   copy_source="http://acct.blob.localhost:1/c/src")
        assert ("x-ms-copy-source", "http://acct.blob.localhost:1/c/src") in headers
        q = prep(method="PUT", uri=f"{BLOB}/c/dest", req_headers={"x-ms-copy-source": "https://other/x"})
        _t, _h, headers, _b = replay.build_request(q, replay.ValueMap(), settings(), 0.0, copy_source="never")
        assert ("x-ms-copy-source", "https://other/x") in headers

    def test_plan_request_unsent_without_candidate(self):
        log = replay.BlobWriteLog()
        src, unsent = replay.plan_request(copy_to("/c/dest"), log, settings())
        assert src is None and unsent[0] == replay.CLASS_COPY
        # with classification off the entry is sent as recorded
        src, unsent = replay.plan_request(copy_to("/c/dest"), log, settings(classify=False))
        assert src is None and unsent is None

    def test_plan_request_rewrites_with_candidate(self):
        log = replay.BlobWriteLog()
        log.note(put_blob("/c/src"))
        src, unsent = replay.plan_request(copy_to("/c/dest"), log, settings())
        assert unsent is None and src.endswith("/c/src")


# --------------------------------------------------------------------------- classifiers

def run_classifiers(raw_entries, actuals):
    """raw_entries: recording dicts; actuals: {index: (status, error code or None)} for the replayed ones.
    Returns the EntryResult list after apply_classifiers."""
    prepared = [replay.prepare_entry(r, i) for i, r in enumerate(raw_entries)]
    results = []
    for p in prepared:
        status, code = actuals.get(p.index, (p.rec_status, None))
        codes = replay.error_codes(p.rec_headers, p.rec_body, p.rec_ct, p.rec_status)
        hard = [("status", "x")] if status != p.rec_status else []
        results.append(replay.EntryResult(p.index, p.method, p.target, "", p.recorded_uri, p.rec_status, status,
                                          codes[0] or codes[1], code, hard, [], "", klass="fail" if hard else "pass"))
    replay.apply_classifiers(prepared, results)
    return results


VAULT = "https://v.vault.azure.net"


def kv(method, path, status, body=None):
    return entry(method, f"{VAULT}{path}?api-version=7.0", status, resp_body=body,
                 resp_headers={"Content-Type": "application/json"} if body else None)


NOT_FOUND = {"error": {"code": "SecretNotFound", "message": "x"}}


class TestAuthClassifier:
    @pytest.mark.parametrize("code,status", [
        ("AuthenticationFailed", 403), ("InvalidAuthenticationInfo", 401), ("NoAuthenticationInformation", 401),
        ("InvalidToken", 401), ("AuthorizationFailure", 403), ("AuthorizationPermissionMismatch", 403),
        ("CannotVerifyCopySource", 401)])
    def test_positive(self, code, status):
        p = replay.prepare_entry(error_entry("GET", f"{BLOB}/c", status, code), 0)
        assert replay.auth_failure_reason(p) == code

    def test_audience_message(self):
        p = prep(status=401, resp_headers={"Content-Type": "application/json"},
                 resp_body={"error": {"code": "Unauthorized", "message": "Token audience is not valid"}})
        assert replay.auth_failure_reason(p) == "audience"

    @pytest.mark.parametrize("status,code", [(404, "BlobNotFound"), (409, "ContainerAlreadyExists"),
                                             (412, "ConditionNotMet"), (403, "SecretDisabled"),
                                             (403, "RequestForbiddenByContainerEncryptionPolicy"),
                                             (412, "CannotVerifyCopySource"), (409, "CannotVerifyCopySource"),
                                             (404, "CannotVerifyCopySource")])
    def test_negative_codes(self, status, code):
        p = replay.prepare_entry(error_entry("GET", f"{BLOB}/c", status, code), 0)
        assert replay.auth_failure_reason(p) is None

    def test_key_vault_challenge_is_replayable(self):
        p = prep(method="GET", uri=f"{VAULT}/secrets/n", status=401, resp_headers={"Content-Type": "application/json"},
                 resp_body={"error": {"code": "Unauthorized", "message": "AKV10000: Request is missing a Bearer or "
                                                                             "PoP token."}})
        assert replay.auth_failure_reason(p) is None

    def test_success_with_such_a_word_is_not_classified(self):
        assert replay.auth_failure_reason(prep(status=200, resp_body="audience", resp_headers={})) is None

    def test_entry_is_not_sent_and_counted(self):
        raws = [entry("PUT", f"{BLOB}/c?restype=container", 201),
                error_entry("GET", f"{BLOB}/c?restype=container", 403, "AuthenticationFailed")]
        with MockOnce(raws) as url:
            r = replay_against(url, [raws_to_file(raws)], classify=True)[0]
        assert [e.klass for e in r.entries] == ["pass", replay.CLASS_AUTH]
        assert r.entries[1].actual_status is None
        assert r.status == "fail" and r.status_adjusted == "pass"


class TestThrottledClassifier:
    def test_429_is_not_sent(self):
        p = prep(method="DELETE", uri="https://fake_app_config.azconfig-test.io/kv/k", status=429)
        src, unsent = replay.plan_request(p, replay.BlobWriteLog(), settings())
        assert unsent[0] == replay.CLASS_THROTTLED

    def test_other_statuses_are_sent(self):
        for st in (200, 404, 412, 501, 409):
            assert replay.plan_request(prep(status=st), replay.BlobWriteLog(), settings())[1] is None

    def test_a_server_error_of_azure_is_not_sent(self):
        for st in (500, 502, 503, 504):
            unsent = replay.plan_request(prep(status=st), replay.BlobWriteLog(), settings())[1]
            assert unsent is not None and unsent[0] == replay.CLASS_AZURE_ERROR
        # 501 is "not implemented" and is a real answer; with classification off everything is sent
        assert replay.plan_request(prep(status=500), replay.BlobWriteLog(), settings(classify=False))[1] is None

    def test_not_applied_with_classification_off(self):
        p = prep(status=429)
        assert replay.plan_request(p, replay.BlobWriteLog(), settings(classify=False))[1] is None

    def test_retry_after_throttle_is_replayed_normally(self):
        raws = [entry("DELETE", "https://x.azconfig-test.io/kv/k", 429, resp_headers={"retry-after-ms": "10"}),
                entry("DELETE", "https://x.azconfig-test.io/kv/k", 200)]
        with MockOnce(raws) as url:
            r = replay_against(url, [raws_to_file(raws)], classify=True)[0]
        assert [e.klass for e in r.entries] == [replay.CLASS_THROTTLED, "pass"]
        assert r.entries[0].actual_status is None and r.status_adjusted == "pass"


class TestAsyncClassifier:
    base = [kv("DELETE", "/secrets/n", 200, {"recoveryId": "x"}),
            kv("GET", "/deletedsecrets/n", 404, NOT_FOUND),
            kv("GET", "/deletedsecrets/n", 404, NOT_FOUND),
            kv("GET", "/deletedsecrets/n", 200, {"recoveryId": "x"})]

    def test_positive(self):
        res = run_classifiers(self.base, {1: (200, None), 2: (200, None)})
        assert [e.klass for e in res] == ["pass", replay.CLASS_ASYNC, replay.CLASS_ASYNC, "pass"]
        assert "later 2xx at entry 3" in res[1].class_detail

    def test_positive_recover(self):
        raws = [kv("DELETE", "/secrets/n", 200, {"x": 1}), kv("GET", "/deletedsecrets/n", 200, {"x": 1}),
                kv("POST", "/deletedsecrets/n/recover", 200, {"x": 1}),
                kv("GET", "/secrets/n", 404, NOT_FOUND), kv("GET", "/secrets/n", 200, {"x": 1})]
        assert run_classifiers(raws, {3: (200, None)})[3].klass == replay.CLASS_ASYNC

    def test_needs_later_2xx_in_same_file(self):
        res = run_classifiers(self.base[:3], {1: (200, None), 2: (200, None)})
        assert [e.klass for e in res] == ["pass", "fail", "fail"]

    def test_later_entry_must_be_the_same_url(self):
        raws = self.base[:3] + [kv("GET", "/deletedsecrets/other", 200, {"x": 1})]
        assert run_classifiers(raws, {1: (200, None), 2: (200, None)})[1].klass == "fail"

    def test_later_entry_must_be_2xx(self):
        raws = self.base[:3] + [kv("GET", "/deletedsecrets/n", 404, NOT_FOUND)]
        assert run_classifiers(raws, {1: (200, None), 2: (200, None)})[1].klass == "fail"

    def test_write_in_between_prevents_classification(self):
        raws = [self.base[0], self.base[1], kv("PUT", "/secrets/n", 200, {"x": 1}), self.base[3]]
        assert run_classifiers(raws, {1: (200, None)})[1].klass == "fail"

    def test_needs_a_delete_or_recover_before(self):
        raws = [self.base[1], self.base[3]]  # a plain "does not exist yet" check
        assert run_classifiers(raws, {0: (200, None)})[0].klass == "fail"

    def test_failed_delete_does_not_start_async_work(self):
        raws = [kv("DELETE", "/secrets/n", 404, NOT_FOUND)] + self.base[1:]
        assert run_classifiers(raws, {1: (200, None)})[1].klass == "fail"

    def test_emulator_must_have_answered_2xx(self):
        assert run_classifiers(self.base, {1: (500, None)})[1].klass == "fail"

    def test_only_get_with_recorded_404(self):
        raws = [kv("DELETE", "/secrets/n", 200, {"x": 1}),
                entry("GET", f"{VAULT}/deletedsecrets/n?api-version=7.0", 403,
                      resp_body={"error": {"code": "Forbidden"}}, resp_headers={"Content-Type": "application/json"}),
                self.base[3]]
        assert run_classifiers(raws, {1: (200, None)})[1].klass == "fail"

    def test_other_resource_is_not_a_starter(self):
        raws = [kv("DELETE", "/secrets/zzz", 200, {"x": 1})] + self.base[1:]
        assert run_classifiers(raws, {1: (200, None), 2: (200, None)})[1].klass == "fail"


class TestLeftoverClassifier:
    def exists(self, method, uri, code="ContainerAlreadyExists", status=409, **kw):
        return error_entry(method, uri, status, code, **kw)

    def test_container_positive(self):
        raws = [self.exists("PUT", f"{BLOB}/c?restype=container")]
        res = run_classifiers(raws, {0: (201, None)})
        assert res[0].klass == replay.CLASS_LEFTOVER

    def test_queue_and_table_and_blob_positive(self):
        res = run_classifiers([self.exists("PUT", "https://a.queue.core.windows.net/q", "QueueAlreadyExists")],
                              {0: (201, None)})
        assert res[0].klass == replay.CLASS_LEFTOVER
        tbl = entry("POST", "https://a.table.core.windows.net/Tables", 409, req_body={"TableName": "t1"},
                    resp_headers={"Content-Type": "application/json"},
                    resp_body={"odata.error": {"code": "TableAlreadyExists", "message": {"value": "x"}}})
        assert run_classifiers([tbl], {0: (201, None)})[0].klass == replay.CLASS_LEFTOVER
        assert run_classifiers([self.exists("PUT", f"{BLOB}/c/b", "BlobAlreadyExists",
                                            req_headers={"x-ms-blob-type": "BlockBlob"})],
                               {0: (201, None)})[0].klass == replay.CLASS_LEFTOVER

    def test_conflict_code_on_vault_create(self):
        raw = entry("PUT", f"{VAULT}/secrets/n?api-version=7.0", 409, resp_headers={"Content-Type": "application/json"},
                    resp_body={"error": {"code": "Conflict", "message": "x"}})
        assert run_classifiers([raw], {0: (200, None)})[0].klass == replay.CLASS_LEFTOVER

    def test_not_first_request_for_the_resource(self):
        raws = [entry("GET", f"{BLOB}/c?restype=container", 200), self.exists("PUT", f"{BLOB}/c?restype=container")]
        assert run_classifiers(raws, {1: (201, None)})[1].klass == "fail"

    def test_request_below_the_resource_counts_as_earlier(self):
        raws = [entry("PUT", f"{BLOB}/c/b", 201, req_headers={"x-ms-blob-type": "BlockBlob"}),
                self.exists("PUT", f"{BLOB}/c?restype=container")]
        assert run_classifiers(raws, {1: (201, None)})[1].klass == "fail"

    def test_other_resource_before_does_not_matter(self):
        raws = [entry("PUT", f"{BLOB}/other?restype=container", 201), self.exists("PUT", f"{BLOB}/c?restype=container")]
        assert run_classifiers(raws, {1: (201, None)})[1].klass == replay.CLASS_LEFTOVER

    def test_earlier_401_challenge_is_ignored(self):
        raws = [entry("PUT", f"{VAULT}/secrets/n?api-version=7.0", 401),
                entry("PUT", f"{VAULT}/secrets/n?api-version=7.0", 409,
                      resp_headers={"Content-Type": "application/json"},
                      resp_body={"error": {"code": "Conflict", "message": "x"}})]
        assert run_classifiers(raws, {1: (200, None)})[1].klass == replay.CLASS_LEFTOVER

    def test_emulator_must_have_created_it(self):
        raws = [self.exists("PUT", f"{BLOB}/c?restype=container")]
        assert run_classifiers(raws, {0: (500, None)})[0].klass == "fail"

    def test_not_a_create_request(self):
        for raw in (self.exists("DELETE", f"{BLOB}/c?restype=container"),
                    self.exists("GET", f"{BLOB}/c?restype=container"),
                    self.exists("PUT", f"{BLOB}/c?restype=container&comp=lease")):
            assert run_classifiers([raw], {0: (201, None)})[0].klass == "fail"

    def test_other_error_code_or_status(self):
        assert run_classifiers([self.exists("PUT", f"{BLOB}/c?restype=container", "InvalidInput", 400)],
                               {0: (201, None)})[0].klass == "fail"
        assert run_classifiers([self.exists("PUT", f"{BLOB}/c?restype=container", "ContainerAlreadyExists", 412)],
                               {0: (201, None)})[0].klass == "fail"

    def test_vault_restore_conflict_is_not_leftover(self):
        raw = entry("POST", f"{VAULT}/secrets/restore?api-version=7.0", 409,
                    resp_headers={"Content-Type": "application/json"},
                    resp_body={"error": {"code": "Conflict", "message": "x"}})
        assert run_classifiers([raw], {0: (200, None)})[0].klass == "fail"


# --------------------------------------------------------------------------- reports

class TestAggregate:
    def make(self, classes):
        r = replay.RecordingResult("a.json", "/x/azure-p/tests/recordings/a.json", "azure-p", "fail")
        for i, k in enumerate(classes):
            r.entries.append(replay.EntryResult(i, "GET", "/", "", "u", 200, 200 if k == "pass" else 500, None, None,
                                                [] if k == "pass" else [("status", "x")], [], "GET /",
                                                status_match=(k == "pass"), klass=k))
        return r

    def test_raw_and_adjusted(self):
        r = self.make(["pass", "pass", "fail", replay.CLASS_ASYNC, replay.CLASS_AUTH, replay.CLASS_LEFTOVER])
        s = replay.aggregate([r])["summary"]
        assert s["entries_evaluated"] == 6 and s["entries_passing"] == 2
        assert s["entries_pass_raw_pct"] == round(100 * 2 / 6, 2)
        assert s["entries_unreplayable"] == 2 and s["entries_ahead_async"] == 1
        assert s["entries_adjusted_denominator"] == 4
        assert s["entries_pass_adjusted"] == 3 and s["entries_pass_adjusted_pct"] == 75.0
        assert s["recordings_passing"] == 0 and s["recordings_passing_adjusted"] == 0  # one hard failure left

    def test_recording_passes_adjusted_without_hard_failures(self):
        r = self.make(["pass", replay.CLASS_ASYNC, replay.CLASS_AUTH])
        s = replay.aggregate([r])["summary"]
        assert s["recordings_passing"] == 0 and s["recordings_passing_adjusted"] == 1

    def test_all_unreplayable_recording_is_flagged_as_unverified(self):
        r = self.make([replay.CLASS_AUTH])
        s = replay.aggregate([r])["summary"]
        assert s["recordings_adjusted_pass_with_nothing_verified"] == 1

    def test_groups_contain_only_hard_failures(self):
        r = self.make(["fail", replay.CLASS_ASYNC, replay.CLASS_AUTH])
        assert sum(g.count for g in replay.aggregate([r])["groups"]) == 1

    def test_report_has_class_and_reason_per_entry(self):
        r = self.make(["pass", replay.CLASS_ASYNC])
        r.entries[1].class_detail = "why"
        agg = replay.aggregate([r])
        rep = replay.build_report(agg, [r], {}, 0.1)
        es = rep["recordings"][0]["entries"]
        assert es[0]["class"] == "pass" and es[1]["class"] == "ahead-async" and es[1]["class_reason"] == "why"
        assert rep["recordings"][0]["result_adjusted"] == "pass"
        text = replay.format_summary(agg, 5, 5)
        assert "RAW" in text and "ADJUSTED" in text and "ahead-async" in text


# --------------------------------------------------------------------------- helpers for the mock

class MockOnce:
    """A mock server answering exactly the recorded entries of an ad-hoc recording."""

    def __init__(self, raws):
        self.path = raws_to_file(raws)

    def __enter__(self):
        self.srv = mockserver.MockServer([self.path], drop_unsent=True).start()
        return self.srv.url

    def __exit__(self, *exc):
        self.srv.stop()


_TMP: list = []


def raws_to_file(raws, under: str = "") -> str:
    import tempfile
    d = tempfile.mkdtemp(prefix="replaytest-")
    _TMP.append(d)
    if under:
        d = os.path.join(d, under)
        os.makedirs(d)
    p = os.path.join(d, "r.json")
    with open(p, "w") as fh:
        json.dump({"Entries": raws}, fh)
    return p


# --------------------------------------------------------------------------- end to end

PACKAGES = [
    ("storage/azure-storage-queue", 25, ""),
    ("storage/azure-storage-blob", 25, ""),
    ("tables/azure-data-tables", 25, "cosmos"),
    ("keyvault/azure-keyvault-secrets", 25, ""),
]


@needs_corpus
@pytest.mark.parametrize("rel,n,exclude", PACKAGES)
class TestEndToEnd:
    def test_perfect_mock_scores_everything(self, rel, n, exclude):
        files = corpus_files(rel, n, exclude)
        with mockserver.MockServer(files, drop_unsent=False).start() as m:
            results = replay_against(m.url, files, classify=False)
            assert m.state.stats["unmatched"] == 0
        for r in results:
            assert r.status == "pass", (r.name, [(e.index, e.hard) for e in r.entries if e.failed])
        assert sum(len(r.entries) for r in results) > n

    def test_mutated_ids_flow_into_later_requests(self, rel, n, exclude):
        files = corpus_files(rel, n, exclude)
        with mockserver.MockServer(files, mutate=True, drop_unsent=False).start() as m:
            results = replay_against(m.url, files, classify=False)
            stats = dict(m.state.stats)
        assert stats["unmutated_hits"] == 0 and stats["unmatched"] == 0
        for r in results:
            assert r.status == "pass", (r.name, [(e.index, e.hard) for e in r.entries if e.failed])

    def test_wrong_status_mode_fails_every_entry(self, rel, n, exclude):
        files = corpus_files(rel, 8, exclude)
        with mockserver.MockServer(files, wrong_rate=1.0, drop_unsent=False).start() as m:
            results = replay_against(m.url, files, classify=False)
        replayed = [e for r in results for e in r.entries if not e.skipped]
        assert replayed and all(e.failed and any(h[0] == "status" for h in e.hard) for e in replayed)
        assert all(r.status == "fail" for r in results if any(not e.skipped for e in r.entries))

    def test_perfect_mock_with_classification_hides_nothing(self, rel, n, exclude):
        files = corpus_files(rel, n, exclude)
        with mockserver.MockServer(files, drop_unsent=True).start() as m:
            results = replay_against(m.url, files, classify=True)
        for r in results:
            assert r.status_adjusted == "pass"
            for e in r.entries:
                # only the entries that are never sent may be classified; nothing was "ahead" or "leftover"
                assert e.klass in ("pass", "skipped", replay.CLASS_AUTH, replay.CLASS_COPY), (r.name, e.index, e.klass)


@needs_corpus
class TestEndToEndSpecifics:
    def test_table_batch_recording_passes_against_perfect_mock(self):
        f = os.path.join(SDK, "tables", "azure-data-tables", "tests", "recordings",
                         "test_table_batch.pyTestTableBatchtest_batch_single_insert.json")
        with mockserver.MockServer([f], drop_unsent=False).start() as m:
            r = replay_against(m.url, [f], classify=False)[0]
        assert r.status == "pass"

    def test_reset_is_called_per_recording(self):
        files = corpus_files("storage/azure-storage-queue", 3)
        with mockserver.MockServer(files, drop_unsent=False).start() as m:
            replay_against(m.url, files, classify=False, reset=True)
            assert m.state.stats["resets"] >= 1 + 3

    def test_dry_run_builds_every_request_and_sends_nothing(self):
        files = corpus_files("tables/azure-data-tables", 40)
        port = free_port()  # nothing listens here; a real send would fail
        st = settings(gateway=f"http://127.0.0.1:{port}", gateway_port=port, dry_run=True)
        for f in files:
            r = replay.run_recording(os.path.basename(f), f, st, None)
            assert r.status != "error", (f, r.error)
            assert not any(e.error for e in r.entries)


# --------------------------------------------------------------------------- run_all

class TestRunAll:
    def test_table_format(self):
        rows = [{"name": "queue", "recordings": 200, "rec_pass_raw": 192, "rec_pass_adj": 196, "entries_raw_pct": 98.89,
                 "entries_adj_pct": 99.44, "unreplayable": 4, "ahead_async": 0, "hard_failing": 4, "classes": {},
                 "values_compared": 1000, "values_differ": 65, "values_differ_pct": 6.5}]
        md = run_all.format_table(rows, markdown=True)
        assert md.splitlines()[0].startswith("| package |") and "| queue | 196/200 | 192/200 | 98.89 | 99.44 | 4 | 0 | 4 | 65/1000 (6.50%) |" in md
        assert "queue" in run_all.format_table(rows, markdown=False)

    def test_services_list_shapes(self):
        assert run_all.mounted_services({"services": ["blob", "queue"]}) == ["blob", "queue"]
        assert run_all.mounted_services({"services": [{"name": "table"}]}) == ["table"]
        assert run_all.mounted_services({}) == []

    def test_every_mounted_service_has_a_package_and_unmounted_ones_are_skipped(self, tmp_path):
        assert {p.service for p in run_all.PACKAGES} == {"blob", "dfs", "queue", "table", "keyvault", "appconfig", "eventgrid", "servicebus", "arm"}
        if not HAVE_CORPUS:
            pytest.skip("corpus missing")
        files = corpus_files("storage/azure-storage-queue", 5)
        with mockserver.MockServer(files, drop_unsent=True).start() as m:
            out = tmp_path / "reports"
            scores = tmp_path / "SCORES.md"
            # the mock's health answers 200 but lists no services: nothing may run
            rc = run_all.main(["--gateway", m.url, "--out-dir", str(out), "--scores", str(scores)])
        assert rc == 0
        assert not out.exists() or not list(out.iterdir())
        text = scores.read_text()
        assert "Fidelity scores" in text and "Not run (service not mounted)" in text and "queue (queue)" in text

    def test_run_package_writes_a_report(self, tmp_path):
        if not HAVE_CORPUS:
            pytest.skip("corpus missing")
        files = corpus_files("storage/azure-storage-queue", 6)
        root = tmp_path / "sdk"
        dest = root / "storage" / "azure-storage-queue" / "tests" / "recordings"
        dest.mkdir(parents=True)
        for f in files:
            (dest / os.path.basename(f)).write_bytes(open(f, "rb").read())
        with mockserver.MockServer(files, drop_unsent=True).start() as m:
            row = run_all.run_package(run_all.PACKAGES[1], m.url, str(root), str(tmp_path / "rep"), 30)
        assert row["recordings"] == 6 and row["rec_pass_adj"] == 6 and row["hard_failing"] == 0
        rep = json.loads((tmp_path / "rep" / "queue.json").read_text())
        assert rep["summary"]["recordings_total"] == 6
        assert all("class" in e for rec in rep["recordings"] for e in rec["entries"])


# ---------------------------------------------------------------------------
# Quick query answers (Avro object container files): data, errors and the closing messages are compared;
# the random sync marker and the way the data is cut into records are not.
# ---------------------------------------------------------------------------

_QQ = os.path.join(os.path.dirname(__file__), "..", "..", "crates", "services", "blob", "src", "query", "testdata")


def _avro_fixture(name: str) -> bytes:
    with open(os.path.join(_QQ, name), "rb") as f:
        return f.read()


def _resync(raw: bytes, new_sync: bytes) -> bytes:
    """The same file with another sync marker (every occurrence after the header)."""
    assert raw[:4] == b"Obj\x01"
    # the sync marker is the 16 bytes after the metadata map; find it as the bytes that end the header
    schema_end = raw.index(b"\n]\n") + 3
    assert raw[schema_end] == 0
    old = raw[schema_end + 1:schema_end + 17]
    return raw.replace(old, new_sync)


def test_avro_messages_decode_a_recorded_answer():
    data, errors, progress, end = replay.avro_messages(_avro_fixture("fatal_json.avro"))
    assert data == b""
    assert errors == [(True, "ParseError", 0)]
    assert progress == (414, 414) and end == 414
    data, errors, progress, end = replay.avro_messages(_avro_fixture("nonfatal.avro"))
    assert data.startswith(b"appconfiguration.eventhubs.")
    assert errors == [(False, "InvalidColumnOrdinal", 118)]
    assert replay.avro_messages(b"not avro") is None
    assert replay.avro_messages(_avro_fixture("readall.avro")[:-3]) is None


def test_compare_avro_ignores_the_sync_marker_and_flags_real_differences():
    raw = _avro_fixture("headers_out.avro")
    assert replay.compare_avro(raw, raw) == []
    assert replay.compare_avro(raw, _resync(raw, bytes(range(16)))) == []
    assert replay.compare_avro(raw, b"nope")[0][0] == "avro-format"
    other = _avro_fixture("json_out.avro")
    kinds = [k for k, _ in replay.compare_avro(raw, other)]
    assert "avro-data" in kinds
    kinds = [k for k, _ in replay.compare_avro(_avro_fixture("readall.avro"), _avro_fixture("nonfatal.avro"))]
    assert "avro-errors" in kinds


# --------------------------------------------------------------------------- values that differ

import threading  # noqa: E402
from http.server import BaseHTTPRequestHandler, HTTPServer  # noqa: E402

import valuecheck  # noqa: E402


class FixedServer:
    """Answers every request with one fixed response (status, headers, body)."""

    def __init__(self, status=200, headers=None, body=b""):
        outer = self
        self.status, self.headers, self.body = status, headers or {}, body
        self.requests: list = []

        class H(BaseHTTPRequestHandler):
            def _do(self):
                n = int(self.headers.get("Content-Length") or 0)
                if n:
                    self.rfile.read(n)
                outer.requests.append((self.command, self.path, self.headers.get("Host")))
                self.send_response(outer.status)
                for k, v in outer.headers.items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(outer.body)))
                self.end_headers()
                self.wfile.write(outer.body)

            do_GET = do_PUT = do_POST = do_DELETE = do_HEAD = do_PATCH = _do

            def log_message(self, *a):
                pass

        self.httpd = HTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_port}"

    def __enter__(self):
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


class TestValueComparison:
    def cmp(self, rec_h=None, act_h=None, rec_l=None, act_l=None, account="acct"):
        return valuecheck.compare_values(account, rec_h or {}, act_h or {}, rec_l or {}, act_l or {})

    def test_equal_values_are_compared_and_do_not_differ(self):
        n, diffs = self.cmp({"x-ms-blob-type": "BlockBlob"}, {"x-ms-blob-type": "BlockBlob"}, {"/a": "Succeeded", "/n": 3, "/b": True},
                            {"/a": "Succeeded", "/n": 3, "/b": True})
        assert (n, diffs) == (4, [])

    def test_different_values_are_reported_with_both_sides(self):
        n, diffs = self.cmp({"x-ms-blob-type": "BlockBlob"}, {"x-ms-blob-type": "PageBlob"}, {"/a": "Succeeded", "/n": 3, "/b": True},
                            {"/a": "Created", "/n": 4, "/b": False})
        assert n == 4 and len(diffs) == 4
        by = {(d.kind, d.key): (d.recorded, d.actual) for d in diffs}
        assert by[("header", "x-ms-blob-type")] == ("BlockBlob", "PageBlob")
        assert by[("body", "/a")] == ("Succeeded", "Created")
        assert by[("body", "/n")] == ("3", "4")

    def test_a_boolean_is_not_a_number_and_a_string_is_not_a_number(self):
        _n, diffs = self.cmp(rec_l={"/a": 1, "/b": True, "/c": "1"}, act_l={"/a": True, "/b": 1, "/d": 1})
        assert {d.key for d in diffs} == {"/a", "/b"}

    def test_only_what_exists_on_both_sides_is_compared(self):
        n, diffs = self.cmp({"x-a": "1"}, {"x-b": "2"}, {"/only": "recorded"}, {"/other": "actual"})
        assert (n, diffs) == (0, [])

    def test_headers_are_compared_without_case_and_surrounding_blanks(self):
        _n, diffs = self.cmp({"x-ms-blob-type": " blockblob"}, {"x-ms-blob-type": "BlockBlob "})
        assert diffs == []

    def test_the_spacing_inside_a_header_value_is_a_difference(self):
        # the Table content-type difference (`;odata` against `; odata`)
        _n, diffs = self.cmp({"content-type": "application/json; odata=nometadata"}, {"content-type": "application/json;odata=nometadata"})
        assert len(diffs) == 1

    @pytest.mark.parametrize("name", ["date", "etag", "x-ms-request-id", "x-ms-version-id", "location", "server", "content-length",
                                      "x-ms-creation-time", "x-ms-keyvault-region", "access-control-allow-origin"])
    def test_excused_headers_are_not_even_counted(self, name):
        assert self.cmp({name: "a"}, {name: "b"}) == (0, [])

    @pytest.mark.parametrize("recorded", [
        "Sanitized", "fake_value", "aaaaaaaa", "00000000-0000-0000-0000-000000000000",
        "2024-05-01T10:00:00Z", "Wed, 01 May 2024 10:00:00 GMT", "3f2a9c1e-0000-4000-8000-000000000001",
        '"0x8DC6A5E1B0F0A55"', 'W/"abc"', "1714557600123", "1714557600.123", "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA==",
        "1.2.3", "1.2.3.4", "see RequestId:abc Time:2024", "datetime'2024-05-01T10:00:00Z'", "x" * 301,
        "Endpoint=sb://x.servicebus.windows.net/;SharedAccessKeyName=a;SharedAccessKey=k", "DefaultEndpointsProtocol=https;AccountName=x",
    ])
    def test_every_excused_value_is_left_out(self, recorded):
        assert valuecheck.generated(recorded, "acct"), recorded
        assert self.cmp(rec_l={"/v": recorded}, act_l={"/v": "something else"}) == (0, [])

    @pytest.mark.parametrize("recorded", [
        "Succeeded", "BlockBlob", "eastus", "East US", "Microsoft.ServiceBus/namespaces/queues", "true", "application/json", "12", "Standard",
        "abc123", "a-long-name-without-digits-at-all-here", "1.2", "2024-05-01", "queue1",
    ])
    def test_ordinary_values_are_not_excused(self, recorded):
        assert valuecheck.generated(recorded, "acct") is None, recorded

    def test_numbers(self):
        assert valuecheck.generated(10 ** 5124) == "big number", "an integer of thousands of digits does not go through a float"
        assert valuecheck.generated(1714557600.5) == "epoch fraction" and valuecheck.generated(2.5) is None
        assert valuecheck.generated(5) is None and valuecheck.generated(0) is None
        assert valuecheck.generated(1714557600123) == "big number"
        assert valuecheck.generated(True) is None

    def test_the_account_name_excuses_only_a_name_of_four_characters_or_more(self):
        assert valuecheck.generated("https://myaccount.blob.core.windows.net/c", "myaccount") == "account name"
        assert valuecheck.generated("the cs value", "cs") is None

    def test_a_member_named_like_its_value_is_a_sanitized_secret(self):
        assert valuecheck.generated("primaryKey", "", "/primaryKey") == "named like its member"
        assert self.cmp(rec_l={"/primaryKey": "primaryKey"}, act_l={"/primaryKey": "abc"}) == (0, [])
        assert valuecheck.generated("Standard", "", "/sku/name") is None

    def test_addresses_differ_in_host_and_query_only(self):
        same = self.cmp(rec_l={"/next": "https://x.blob.core.windows.net/c?marker=1&sig=a"}, act_l={"/next": "http://x.blob.core.localhost:1/c?marker=2"})
        assert same == (1, [])
        other = self.cmp(rec_l={"/u": "https://x.blob.core.windows.net/c/one"}, act_l={"/u": "http://x.blob.core.localhost:1/c/two"})
        assert other[0] == 1 and len(other[1]) == 1

    def test_the_random_boundary_of_a_multipart_content_type_is_left_out(self):
        a = {"content-type": "multipart/mixed; boundary=batchresponse_3f2a"}
        b = {"content-type": "multipart/mixed; boundary=batchresponse_9999"}
        assert self.cmp(a, b) == (1, [])
        c = {"content-type": "multipart/mixed;boundary=batchresponse_9999"}
        assert len(self.cmp(a, c)[1]) == 1, "the spacing still counts"

    def test_an_answer_that_names_the_account_of_the_host_is_not_a_difference(self):
        assert self.cmp(rec_l={"/entry/author/name": "sb-real-namespace-1"}, act_l={"/entry/author/name": "Sanitized"}, account="sanitized") == (0, [])
        n, diffs = self.cmp(rec_l={"/entry/author/name": "one"}, act_l={"/entry/author/name": "two"}, account="sanitized")
        assert n == 1 and len(diffs) == 1

    def test_values_under_lists_of_different_length_are_not_compared(self):
        rec = {"/value[0]/name": "a", "/value[1]/name": "b", "/other[0]/x": "1"}
        act = {"/value[0]/name": "z", "/other[0]/x": "2"}
        n, diffs = self.cmp(rec_l=rec, act_l=act)
        assert [d.key for d in diffs] == ["/other[0]/x"] and n == 1
        # the same length: compared by position
        n, diffs = self.cmp(rec_l={"/value[0]/name": "a", "/value[1]/name": "b"}, act_l={"/value[0]/name": "a", "/value[1]/name": "c"})
        assert n == 2 and [d.key for d in diffs] == ["/value[1]/name"]
        # nested
        n, diffs = self.cmp(rec_l={"/a[0]/b[0]/c": "1", "/a[0]/b[1]/c": "2"}, act_l={"/a[0]/b[0]/c": "9"})
        assert (n, diffs) == (0, [])

    def test_a_version_in_an_address_is_an_id_the_server_made_up(self):
        a = "https://v.vault.azure.net/secrets/name/0123456789abcdef0123456789abcdef"
        b = "http://x.vault.localhost:1/secrets/name/fedcba9876543210fedcba9876543210"
        assert self.cmp(rec_l={"/id": a}, act_l={"/id": b}) == (1, [])
        n, diffs = self.cmp(rec_l={"/id": a}, act_l={"/id": b.replace("/name/", "/other/")})
        assert n == 1 and len(diffs) == 1

    def test_an_error_message_is_compared_by_its_first_line(self):
        n, diffs = self.cmp(rec_l={"/error/message": "The vault was not found.\r\nRequestId: 1\r\nTime: 2"},
                            act_l={"/error/message": "The vault was not found.\r\nother"})
        assert (n, diffs) == (1, [])
        n, diffs = self.cmp(rec_l={"/error/message": "The vault was not found."}, act_l={"/error/message": "The vault does not exist."})
        assert n == 1 and diffs[0].kind == "text"

    def test_lists_are_matched_by_id_when_every_element_has_one(self):
        # Key Vault: the recorded vault held other secrets, and Azure keeps its own order
        rec = {"/value[0]/id": "https://v.vault.azure.net/secrets/a", "/value[0]/attributes/enabled": True,
               "/value[1]/id": "https://v.vault.azure.net/secrets/other", "/value[1]/attributes/enabled": False,
               "/value[2]/id": "https://v.vault.azure.net/secrets/b", "/value[2]/attributes/enabled": True}
        act = {"/value[0]/id": "http://x.vault.localhost:1/secrets/b", "/value[0]/attributes/enabled": False,
               "/value[1]/id": "http://x.vault.localhost:1/secrets/a", "/value[1]/attributes/enabled": True}
        n, diffs = self.cmp(rec_l=rec, act_l=act)
        # a and b are compared with their own: ids equal, `a` agrees, `b` differs; `other` is left out
        assert n == 4 and [(d.key, d.recorded, d.actual) for d in diffs] == [("/value[1]/attributes/enabled", "True", "False")]

    def test_lists_whose_ids_repeat_are_still_matched_by_position(self):
        # the versions of one secret: after the version (32 hex digits) is folded to an id, the addresses are all the same
        a = "https://v.vault.azure.net/secrets/n/" + "0123456789abcdef" * 2
        b = "http://x.vault.localhost:1/secrets/n/" + "fedcba9876543210" * 2
        rec = {"/value[0]/id": a, "/value[0]/x": "1", "/value[1]/id": a, "/value[1]/x": "2"}
        act = {"/value[0]/id": b, "/value[0]/x": "1", "/value[1]/id": b, "/value[1]/x": "3"}
        n, diffs = self.cmp(rec_l=rec, act_l=act)
        assert n == 4 and [d.key for d in diffs] == ["/value[1]/x"]

    def test_the_vault_retention_of_the_recorded_vault_is_not_compared_but_the_default_is(self):
        assert self.cmp(rec_l={"/attributes/recoverableDays": 7, "/attributes/recoveryLevel": "CustomizedRecoverable+Purgeable"},
                        act_l={"/attributes/recoverableDays": 90, "/attributes/recoveryLevel": "Recoverable+Purgeable"}) == (0, [])
        n, diffs = self.cmp(rec_l={"/attributes/recoverableDays": 90, "/attributes/recoveryLevel": "Recoverable+Purgeable"},
                            act_l={"/attributes/recoverableDays": 7, "/attributes/recoveryLevel": "Recoverable"})
        assert n == 2 and len(diffs) == 2

    def test_retired_wording_is_only_the_exact_values_of_the_list(self):
        msg = {"/error/message": "AKV10000: Request is missing a Bearer or PoP token."}
        assert self.cmp(rec_l=msg, act_l={"/error/message": "AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token."}) == (0, [])
        n, diffs = self.cmp(rec_l={"/error/message": "AKV10000: Something else."}, act_l={"/error/message": "AKV10000: Request is missing a Bearer or PoP token."})
        assert n == 1 and len(diffs) == 1
        assert self.cmp(rec_l={"/key/e": "AAEAAQ"}, act_l={"/key/e": "AQAB"}) == (0, [])
        assert len(self.cmp(rec_l={"/key/e": "EQ"}, act_l={"/key/e": "AQAB"})[1]) == 1
        assert self.cmp(rec_l={"/error/message": "Pending certificate not found: x"}, act_l={"/error/message": "Pending Certificate not found: x"}) == (0, [])
        assert self.cmp(rec_l={"/attributes/hsmPlatform": "1"}, act_l={"/attributes/hsmPlatform": "2"}) == (0, [])
        assert len(self.cmp(rec_l={"/attributes/hsmPlatform": "0"}, act_l={"/attributes/hsmPlatform": "2"})[1]) == 1

    def test_a_message_is_compared_without_the_host_the_account_and_the_ids(self):
        rec = "There was a conflict restoring the secret 'https://vaultname.vault.azure.net/secrets/a/82fd1b91bce34271bfa34edf86ba2900' This can happen"
        act = "There was a conflict restoring the secret 'http://Sanitized.vault.localhost:49729/secrets/a/0d3be99816f54875b2ec879a5ba20000' This can happen"
        assert self.cmp(rec_l={"/error/message": rec}, act_l={"/error/message": act}) == (1, [])
        assert len(self.cmp(rec_l={"/error/message": rec}, act_l={"/error/message": act.replace("This can", "That can")})[1]) == 1
        # Service Bus: the Detail of an error, with a tracking id
        d1 = "No service is hosted at the specified address. TrackingId:66952809-c475-4245-883d-0125fbc2b4f1"
        d2 = "No service is hosted at the specified address. TrackingId:b7bd78e5-75f9-4ec0-b88d-90e8311aaaaa"
        assert self.cmp(rec_l={"/Error/Detail[0]": d1}, act_l={"/Error/Detail[0]": d2}) == (1, [])
        # Event Grid names the topic's address in capitals
        e1 = "The request authorization key is not authorized for LLAWTOPIC.EASTUS-1.EVENTGRID.AZURE.NET. This is"
        e2 = "The request authorization key is not authorized for SANITIZED.EVENTGRID.LOCALHOST. This is"
        assert self.cmp(rec_l={"/error/message": e1}, act_l={"/error/message": e2}) == (1, [])

    @pytest.mark.parametrize("path,recorded", [
        ("/StorageServiceStats/GeoReplication/Status", "unavailable"),
    ])
    def test_paths_the_recording_never_set_are_skipped(self, path, recorded):
        assert valuecheck.compare_values("acct", {}, {}, {path: recorded}, {path: "live"}, skip_paths=("/StorageServiceStats",)) == (0, [])
        assert len(valuecheck.compare_values("acct", {}, {}, {path: recorded}, {path: "live"})[1]) == 1

    def test_a_skipped_header_is_not_compared(self):
        n, diffs = valuecheck.compare_values("acct", {"x-ms-delete-type-permanent": "true"}, {"x-ms-delete-type-permanent": "false"}, {}, {},
                                             skip_headers=frozenset({"x-ms-delete-type-permanent"}))
        assert (n, diffs) == (0, [])
        assert len(self.cmp({"x-ms-delete-type-permanent": "true"}, {"x-ms-delete-type-permanent": "false"})[1]) == 1

    def test_small_rules_of_values(self):
        assert len(self.cmp({"x-ms-access-tier": "Cool"}, {"x-ms-access-tier": "Hot"})[1]) == 1
        assert self.cmp({"x-ms-sku-name": "Standard_LRS"}, {"x-ms-sku-name": "Standard_RAGRS"}) == (0, [])
        assert self.cmp({"x-ms-item-count": "43"}, {"x-ms-item-count": "2"}) == (0, [])
        assert self.cmp(rec_l={"/UserDelegationKey/Value[0]": "MA=="}, act_l={"/UserDelegationKey/Value[0]": "abc"}) == (0, [])
        assert self.cmp(rec_l={"/EnumerationResults/NextMarker[0]": "2!68!MDAw"}, act_l={"/EnumerationResults/NextMarker[0]": "QgAw"}) == (0, [])
        assert self.cmp(rec_l={"/x5t": "vxaucuiwtqJbHvaWDjOMvHrlLts"}, act_l={"/x5t": "scSY-wat-YtOgAr4gDW4eNoPnkk"}) == (0, [])
        assert len(self.cmp(rec_l={"/name": "vxaucuiwtqJbHvaWDjOMvHrlLts"}, act_l={"/name": "scSY-wat-YtOgAr4gDW4eNoPnkk"})[1]) == 1
        # a duration just below the maximum is the client's own value; the maximum is compared
        assert self.cmp(rec_l={"/a/DefaultMessageTimeToLive": "P10675199DT2H48M5.477539S"}, act_l={"/a/DefaultMessageTimeToLive": "P10675199DT2H48M5.4775807S"}) == (0, [])
        assert len(self.cmp(rec_l={"/a/DefaultMessageTimeToLive": "P10675199DT2H48M5.4775807S"}, act_l={"/a/DefaultMessageTimeToLive": "P1D"})[1]) == 1
        # work in progress
        assert self.cmp(rec_l={"/status": "Running", "/properties/provisioningState": "Creating"}, act_l={"/status": "Succeeded", "/properties/provisioningState": "Succeeded"}) == (0, [])
        assert len(self.cmp(rec_l={"/status": "Failed"}, act_l={"/status": "Succeeded"})[1]) == 1

    def test_the_short_deleted_text_is_work_in_progress_and_the_long_text_is_compared(self):
        short = "Deleted Secret not found: livekvtestsecret1"
        long_ = "A secret with (name/id) livekvtestsecret1 was not found in this key vault. If you recently deleted this secret you may be able to recover it"
        assert self.cmp(rec_l={"/error/message": short}, act_l={"/error/message": long_}) == (0, [])
        assert self.cmp(rec_l={"/error/message": long_}, act_l={"/error/message": long_}) == (1, [])
        assert len(self.cmp(rec_l={"/error/message": long_}, act_l={"/error/message": short})[1]) == 1

    def test_a_service_bus_error_detail_is_compared_without_ids_times_and_the_namespace(self):
        rec = "Entity 'sb-te9c6b9643d59d62d:Topic:dfjfj|iewdm' was not found. TrackingId:5044ae00-1111-2222-3333-444455556666_G4, SystemTracker:fakeresource.servicebus.windows.net:Topic, Timestamp:2024-09-04T18:14:21"
        act = "Entity 'sanitized:Topic:dfjfj|iewdm' was not found. TrackingId:e89165b6-cf14-4911-8b00-111122223333_G7, SystemTracker:sanitized.servicebus.windows.net:Topic, Timestamp:2026-11-13T07:00:00"
        assert self.cmp(rec_l={"/Error/Detail[0]": rec}, act_l={"/Error/Detail[0]": act}, account="sanitized") == (1, [])
        assert len(self.cmp(rec_l={"/Error/Detail[0]": rec}, act_l={"/Error/Detail[0]": act.replace("was not found", "exists")}, account="sanitized")[1]) == 1

    def test_the_account_settings_a_recording_set_itself_are_compared(self):
        put = entry("PUT", "https://acct.blob.core.windows.net/?restype=service&comp=properties", 202,
                    req_body="<StorageServiceProperties><Cors><CorsRule/></Cors><DeleteRetentionPolicy><Enabled>true</Enabled><Days>1</Days></DeleteRetentionPolicy></StorageServiceProperties>")
        get = entry("GET", "https://acct.blob.core.windows.net/?restype=service&comp=properties", 200)
        stats = entry("GET", "https://acct.blob.core.windows.net/?restype=service&comp=stats", 200)
        delete = entry("DELETE", "https://acct.blob.core.windows.net/c/b", 202, resp_headers={"x-ms-delete-type-permanent": "true"})
        prepared = [replay.prepare_entry(e, i) for i, e in enumerate([put, get, stats, delete])]
        s = replay.AccountSettings(prepared)
        assert s.skip_paths(prepared[1]) != () and "x-ms-delete-type-permanent" in s.skip_headers(prepared[3])
        s.note(prepared[0])
        assert s.sections_set == {"Cors", "DeleteRetentionPolicy"} and s.soft_delete_set
        skipped = s.skip_paths(prepared[1])
        assert "/StorageServiceProperties/Logging" in skipped and "/StorageServiceProperties/Cors" not in skipped
        assert s.skip_paths(prepared[2]) == ("/StorageServiceStats",)
        assert "x-ms-delete-type-permanent" not in s.skip_headers(prepared[3]), "the recording turned soft delete on itself"

    def test_the_default_tier_of_the_recorded_account_is_left_out_unless_the_recording_sets_a_tier(self):
        head = entry("HEAD", "https://acct.blob.core.windows.net/c/b", 200, resp_headers={"x-ms-access-tier": "Cool"})
        put = entry("PUT", "https://acct.blob.core.windows.net/c/b", 201)
        s = replay.AccountSettings([replay.prepare_entry(put, 0), replay.prepare_entry(head, 1)])
        assert not s.tier_set
        h = replay.prepare_entry(head, 1)
        assert "x-ms-access-tier" in s.skip_headers(h) and s.skip_endings() == ("/AccessTier",)
        assert valuecheck.compare_values("acct", {}, {}, {"/EnumerationResults/Blobs[0]/Blob[0]/Properties[0]/AccessTier[0]": "Cool"},
                                         {"/EnumerationResults/Blobs[0]/Blob[0]/Properties[0]/AccessTier[0]": "Hot"}, skip_endings=s.skip_endings()) == (0, [])
        tiered = entry("PUT", "https://acct.blob.core.windows.net/c/b?comp=tier", 200, req_headers={"x-ms-access-tier": "Cool"})
        s = replay.AccountSettings([replay.prepare_entry(put, 0), replay.prepare_entry(tiered, 1), replay.prepare_entry(head, 2)])
        assert s.tier_set and "x-ms-access-tier" not in s.skip_headers(replay.prepare_entry(head, 2)) and s.skip_endings() == ()

    def test_versioning_alone_decides_a_plain_delete(self):
        v = entry("PUT", "https://acct.blob.core.windows.net/c/b", 201, resp_headers={"x-ms-version-id": "2024-01-01T00:00:00.0000000Z"})
        plain = entry("DELETE", "https://acct.blob.core.windows.net/c/b", 202)
        snap = entry("DELETE", "https://acct.blob.core.windows.net/c/b?snapshot=2024-01-01T00%3A00%3A00.0000000Z", 202)
        by_version = entry("DELETE", "https://acct.blob.core.windows.net/c/b?versionid=2024-01-01T00%3A00%3A00.0000000Z", 202)
        prepared = [replay.prepare_entry(e, i) for i, e in enumerate([v, plain, snap, by_version])]
        s = replay.AccountSettings(prepared)
        assert s.versioned
        assert "x-ms-delete-type-permanent" not in s.skip_headers(prepared[1])
        assert "x-ms-delete-type-permanent" in s.skip_headers(prepared[2]) and "x-ms-delete-type-permanent" in s.skip_headers(prepared[3])

    def test_the_readme_lists_every_rule_and_header_group(self):
        readme = open(os.path.join(os.path.dirname(__file__), "README.md"), encoding="utf-8").read()
        for rule_id, _reason in valuecheck.VALUE_EXCUSES:
            assert f"`{rule_id}`" in readme, f"README.md does not list the value rule {rule_id!r}"
        groups = {g for g, _r in valuecheck.HEADER_EXCUSES.values()} | {g for g, _r in valuecheck.HEADER_EXCUSE_PREFIXES.values()}
        for rule_id, _reason in valuecheck.STRUCTURE_RULES:
            assert f"`{rule_id}`" in readme, f"README.md does not list the rule {rule_id!r}"
        for g in groups:
            assert g in readme, f"README.md does not list the header group {g!r}"
        for name in valuecheck.HEADER_EXCUSES:
            assert f"`{name}`" in readme, f"README.md does not list the header {name!r}"

    def test_a_run_counts_and_groups_the_differences(self):
        rec = [entry("GET", "https://acct.blob.core.windows.net/c/b", 200, resp_headers={"x-ms-blob-type": "BlockBlob", "Content-Type": "application/json"},
                     resp_body={"state": "Succeeded", "n": 3, "id": "3f2a9c1e-0000-4000-8000-000000000001"})]
        path = raws_to_file(rec)
        with FixedServer(200, {"x-ms-blob-type": "PageBlob", "Content-Type": "application/json"}, b'{"state":"Created","n":3,"id":"zzz"}') as srv:
            r = replay_against(srv.url, [path], reset=False)[0]
        e = r.entries[0]
        assert e.klass == "pass", "a difference in a value never fails an entry"
        assert e.values_compared == 4, "x-ms-blob-type, content-type, /state and /n are compared; the GUID is excused"
        assert {(d.kind, d.key) for d in e.value_diffs} == {("header", "x-ms-blob-type"), ("body", "/state")}
        agg = replay.aggregate([r])
        assert agg["summary"]["values_differ"] == 2 and agg["summary"]["entries_with_value_differences"] == 1
        text = replay.format_summary(agg, 10, 10)
        assert "values that differ: 2" in text and "Values that differ, grouped" in text and "Azure: 'Succeeded'" in text
        report = replay.build_report(agg, [r], {}, 0.1)
        assert report["summary"]["values_differ"] == 2 and len(report["value_groups"]) == 2
        assert report["recordings"][0]["entries"][0]["value_diffs"][0]["kind"] in ("header", "body")

    def test_array_positions_do_not_make_new_groups(self):
        assert replay._generic_path("/value[3]/properties/x") == "/value[]/properties/x"

    def test_no_values_are_compared_when_the_status_differs(self):
        rec = [entry("GET", "https://acct.blob.core.windows.net/c/b", 200, resp_headers={"x-ms-blob-type": "BlockBlob"})]
        with FixedServer(404, {"x-ms-blob-type": "PageBlob", "x-ms-error-code": "BlobNotFound"}) as srv:
            e = replay_against(srv.url, [raws_to_file(rec)], reset=False)[0].entries[0]
        assert e.klass == "fail" and e.values_compared == 0 and e.value_diffs == []


# --------------------------------------------------------------------------- Resource Manager recordings

class TestResourceManagerRecordings:
    ARM = "https://management.azure.com/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rgname/providers/Microsoft.ServiceBus/namespaces/n1?api-version=2021-11-01"

    def test_the_host_is_ours_and_has_no_account(self):
        assert replay.resolve_service("management.azure.com") == ("", "management")
        # not in the recordings of the data-plane packages: their score does not change
        assert replay.resolve_service("management.azure.com", arm=False) is None
        p = replay.prepare_entry(entry("PUT", self.ARM), 0, arm=False)
        assert isinstance(p, replay.Skipped)
        p = replay.prepare_entry(entry("GET", "https://Sanitized.azure.com/subscriptions?api-version=1"), 0, arm=False)
        assert isinstance(p, replay.Skipped)
        p = prep(method="PUT", uri=self.ARM)
        assert (p.account, p.label) == ("", "management")

    def test_the_sanitized_host_is_ours_only_for_a_resource_manager_path(self):
        p = replay.prepare_entry(entry("GET", "https://Sanitized.azure.com/subscriptions?api-version=2022-12-01"), 0)
        assert not isinstance(p, replay.Skipped) and p.label == "management"
        p = replay.prepare_entry(entry("GET", "https://Sanitized.azure.com/providers/Microsoft.Storage/operations?api-version=2023-05-01"), 0)
        assert not isinstance(p, replay.Skipped)
        p = replay.prepare_entry(entry("GET", "https://Sanitized.azure.com/container/blob"), 0)
        assert isinstance(p, replay.Skipped)
        p = replay.prepare_entry(entry("GET", "https://login.microsoftonline.com/common/discovery/instance"), 0)
        assert isinstance(p, replay.Skipped)

    def test_the_request_goes_to_the_management_host_with_a_bearer_token(self):
        rec = [entry("PUT", self.ARM, 200, req_body={"location": "eastus"}, req_headers={"Content-Type": "application/json"})]
        with FixedServer(200, {"Content-Type": "application/json"}, b"{}") as srv:
            replay_against(srv.url, [raws_to_file(rec, "azure-mgmt-x/tests/recordings")], reset=False)
        assert srv.requests[-1][2] == f"management.localhost:{srv.httpd.server_port}"

    def test_operation_patterns_keep_types_and_drop_names(self):
        pat = replay.operation_pattern("PUT", "management", "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rgname/providers/Microsoft.ServiceBus/namespaces/n1/queues/q1/authorizationRules/r1?api-version=2021-11-01")
        assert pat == "PUT /subscriptions/{sub}/resourceGroups/{rg}/providers/Microsoft.ServiceBus/namespaces/{name}/queues/{name}/authorizationRules/{name}"
        pat = replay.operation_pattern("POST", "management", "/subscriptions/s/resourceGroups/g/providers/Microsoft.ServiceBus/namespaces/n1/listKeys?api-version=1")
        assert pat.endswith("/namespaces/{name}/listKeys")
        assert replay.operation_pattern("GET", "management", "/providers/Microsoft.Storage/operations?api-version=1") == "GET /providers/Microsoft.Storage/operations"

    def test_a_group_the_recording_never_creates_is_a_prerequisite(self):
        g = "https://management.azure.com/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/"
        raws = [entry("PUT", self.ARM, 200), entry("PUT", g + "made?api-version=1", 201), entry("GET", g + "made/providers/Microsoft.X/y/z?api-version=1", 200),
                entry("GET", g + "other/providers/Microsoft.X/y/z?api-version=1", 200), entry("GET", "https://acct.blob.core.windows.net/c/b", 200)]
        prepared = [replay.prepare_entry(r, i) for i, r in enumerate(raws)]
        assert replay.prerequisite_groups(prepared) == [("00000000-0000-0000-0000-000000000000", "rgname"), ("00000000-0000-0000-0000-000000000000", "other")]

    def test_the_prerequisite_group_is_created_before_the_first_request_and_is_not_scored(self):
        rec = [entry("GET", self.ARM, 200, resp_headers={"Content-Type": "application/json"}, resp_body={})]
        with FixedServer(200, {"Content-Type": "application/json"}, b"{}") as srv:
            r = replay_against(srv.url, [raws_to_file(rec, "azure-mgmt-x/tests/recordings")], reset=False)[0]
        assert [req[0] for req in srv.requests] == ["PUT", "GET"]
        assert "/resourcegroups/rgname" in srv.requests[0][1]
        assert len(r.entries) == 1

    def test_ahead_async_for_a_202_poll_that_the_emulator_finished(self):
        op = "https://management.azure.com/subscriptions/00000000-0000-0000-0000-000000000000/providers/Microsoft.X/locations/l/operationResults/abc?api-version=1"
        raws = [entry("DELETE", self.ARM, 202), entry("GET", op, 202), entry("GET", op, 200)]
        prepared = [replay.prepare_entry(r, i) for i, r in enumerate(raws)]
        results = [replay.EntryResult(i, p.method, "", "", p.recorded_uri, p.rec_status, 200, None, None, [("status", "x")], [], "", klass="fail")
                   for i, p in enumerate(prepared)]
        results[0].klass = "pass"
        results[2].klass = "pass"
        replay.apply_classifiers(prepared, results)
        assert results[1].klass == replay.CLASS_ASYNC
        # not for another label, and not without a later 2xx
        raws[1]["RequestUri"] = "https://acct.blob.core.windows.net/c/b"
        raws[2]["RequestUri"] = "https://acct.blob.core.windows.net/c/b"
        prepared = [replay.prepare_entry(r, i) for i, r in enumerate(raws)]
        results = [replay.EntryResult(i, p.method, "", "", p.recorded_uri, p.rec_status, 200, None, None, [("status", "x")], [], "", klass="fail")
                   for i, p in enumerate(prepared)]
        replay.apply_classifiers(prepared, results)
        assert results[1].klass == "fail"


# --------------------------------------------------------------------------- the emulator each package is started with

FAKE_EMULATOR = r'''
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
port = int(sys.argv[sys.argv.index("--port") + 1])
with open(os.environ["FAKE_OUT"], "w") as f:
    json.dump({k: v for k, v in os.environ.items() if k.startswith("AZEMU_")}, f)
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        b = json.dumps({"status": "ok", "services": ["blob", "eventgrid"]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", port), H).serve_forever()
'''


class TestPackageSettings:
    def pkg(self, name):
        return next(p for p in run_all.PACKAGES if p.name == name)

    def test_each_package_names_the_settings_it_needs_and_why(self):
        assert self.pkg("blob").env == {"AZEMU_BLOB_VERSIONING": "1"} and self.pkg("blob").why
        assert self.pkg("eventgrid").env == {"AZEMU_EVENTGRID_AUTOCREATE": "1"} and self.pkg("eventgrid").why
        for name in ("queue", "table", "keyvault-secrets", "keyvault-keys", "keyvault-certificates", "appconfig", "servicebus", "datalake", "arm"):
            assert self.pkg(name).env == {}, name
        assert all("AZEMU_LENIENT" not in p.env for p in run_all.PACKAGES), "one switch for everything would hide which convenience a score needs"

    def test_the_emulator_gets_only_the_settings_of_its_package(self, tmp_path, monkeypatch):
        script = tmp_path / "fake.py"
        script.write_text(FAKE_EMULATOR)
        sh = tmp_path / "fake-emulator"
        sh.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
        sh.chmod(0o755)
        out = tmp_path / "env.json"
        monkeypatch.setenv("FAKE_OUT", str(out))
        monkeypatch.setenv("AZEMU_LENIENT", "1")        # the caller's shell: must not reach the emulator
        monkeypatch.setenv("AZEMU_BLOB_VERSIONING", "0")
        proc, tmp, gateway = run_all.start_emulator(str(sh), {"AZEMU_EVENTGRID_AUTOCREATE": "1"})
        try:
            assert run_all.health(gateway) is not None
            assert json.loads(out.read_text()) == {"AZEMU_EVENTGRID_AUTOCREATE": "1"}
        finally:
            run_all.stop_emulator(proc, tmp)
        assert proc.poll() is not None


# --------------------------------------------------------------------------- the order of seconds (SecondGate)

class FakeClock:
    """A clock that only moves when something sleeps or the test moves it."""

    def __init__(self, start: float) -> None:
        self.t, self.slept = start, []

    def now(self) -> float:
        return self.t

    def sleep(self, s: float) -> None:
        self.slept.append(s)
        self.t += s


def dated(date: str, **kw) -> replay.PreparedEntry:
    h = {"Date": f"Wed, 27 May 2026 15:{date} GMT"}
    return prep(resp_headers=h, **kw)


CONDITIONAL = {"x-ms-blob-if-unmodified-since": "Wed, 27 May 2026 15:23:50 GMT"}


class TestSecondGate:
    """Two things seconds apart in a recording must also be in different seconds in the replay (it fixes the Data Lake score that moved
    between runs: the blob was written again 11 seconds later in the recording, and the replay did both in the same second or not,
    depending on the clock)."""

    def prepared(self):
        return [dated("23:50"), dated("23:50"), dated("24:01", req_headers=CONDITIONAL), dated("24:01")]

    def test_it_waits_for_the_next_second_when_the_recording_moved_on_and_the_clock_did_not(self):
        clock = FakeClock(1000.30)
        gate = replay.SecondGate(self.prepared(), clock.now, clock.sleep)
        ps = self.prepared()
        gate.before(0, ps[0]); clock.t += 0.01; gate.after(ps[0])
        gate.before(1, ps[1]); clock.t += 0.01; gate.after(ps[1])        # same recorded second: no wait
        assert clock.slept == []
        gate.before(2, ps[2])                                            # 11 recorded seconds later, same real second: wait
        assert len(clock.slept) == 1 and int(clock.t) == 1001, (clock.slept, clock.t)
        assert gate.waits == 1
        gate.after(ps[2])
        gate.before(3, ps[3])                                            # after the last conditional entry: never waits
        assert len(clock.slept) == 1

    def test_it_does_not_wait_when_the_real_second_already_moved_on(self):
        clock = FakeClock(1000.99)
        ps = self.prepared()
        gate = replay.SecondGate(ps, clock.now, clock.sleep)
        gate.before(0, ps[0]); gate.after(ps[0])
        clock.t = 1001.5
        gate.before(2, ps[2])
        assert clock.slept == []

    def test_a_recording_without_a_date_conditional_header_never_waits(self):
        clock = FakeClock(1000.10)
        ps = [dated("23:50"), dated("24:01"), dated("24:20")]
        gate = replay.SecondGate(ps, clock.now, clock.sleep)
        for i, p in enumerate(ps):
            gate.before(i, p); gate.after(p)
        assert clock.slept == [] and gate.waits == 0

    def test_the_wait_is_at_most_a_little_over_a_second(self):
        clock = FakeClock(1000.0)
        ps = self.prepared()
        gate = replay.SecondGate(ps, clock.now, clock.sleep)
        gate.before(0, ps[0]); gate.after(ps[0])
        gate.before(2, ps[2])
        assert 0 < clock.slept[0] <= 1.05, clock.slept


# --------------------------------------------------------------------------- keys the emulator generated (unreplayable:generated-key)

KV = "https://vaultname.vault.azure.net"


def vault_entry(method, path, status=200, i=0, **kw):
    return prep(i=i, method=method, uri=f"{KV}{path}?api-version=7.5", status=status, **kw)


class TestGeneratedKeyClassifier:
    """A key the vault generated (create, rotate) is a different key on the emulator, so Azure's recorded ciphertext cannot be
    decrypted with it. Whether the emulator then answers 400 (almost always) or 200 is chance; the entry is classified without being
    sent, so the score does not move between runs. When the value mapping has put the emulator's own ciphertext into the request
    (an `encrypt` answer just before), the entry is sent."""

    @staticmethod
    def decrypt(path, value="QVpVUkU", status=200, uri=None, **kw):
        body = json.dumps({"alg": "RSA-OAEP", "value": value})
        return prep(method="POST", uri=uri or f"{KV}{path}?api-version=7.5", status=status, req_body=body,
                    req_headers={"Content-Type": "application/json"}, **kw)

    @staticmethod
    def sent(p, value):
        """The body as the replay would send it."""
        return json.dumps({"alg": "RSA-OAEP", "value": value}).encode()

    def keys_with(self, *created):
        keys = replay.GeneratedKeys()
        for c in created:
            keys.note(c)
        return keys

    def test_decrypt_and_unwrap_of_azures_own_ciphertext_with_a_created_key_are_not_sent(self):
        keys = self.keys_with(vault_entry("POST", "/keys/k1/create"))
        for op in ("decrypt", "unwrapkey"):
            p = self.decrypt(f"/keys/k1/abc123/{op}")
            reason = keys.reason(p, self.sent(p, "QVpVUkU"))
            assert reason and "generated" in reason and op in reason, op

    def test_a_ciphertext_the_value_mapping_replaced_is_sent(self):
        keys = self.keys_with(vault_entry("POST", "/keys/k1/create"))
        p = self.decrypt("/keys/k1/abc123/decrypt")
        assert keys.reason(p, self.sent(p, "THE-EMULATORS-OWN")) is None

    def test_rotate_makes_a_generated_key_too(self):
        keys = self.keys_with(vault_entry("POST", "/keys/k1/rotate"))
        p = self.decrypt("/keys/k1/v2/decrypt")
        assert keys.reason(p, self.sent(p, "QVpVUkU"))

    def test_other_keys_other_operations_and_other_outcomes_are_sent(self):
        keys = self.keys_with(vault_entry("POST", "/keys/k1/create"))
        def reason(path, **kw):
            p = self.decrypt(path, **kw)
            return keys.reason(p, self.sent(p, "QVpVUkU"))
        assert reason("/keys/other/v/decrypt") is None, "a key that was not created in this recording"
        assert reason("/keys/k1/v/encrypt") is None, "encrypt works with any key"
        assert reason("/keys/k1/v/sign") is None
        assert reason("/keys/k1/v/decrypt", status=400) is None, "an error Azure gave is still compared"
        # an imported key (PUT) carries its own material
        imported = self.keys_with(vault_entry("PUT", "/keys/k2"))
        p = self.decrypt("/keys/k2/v/decrypt")
        assert imported.reason(p, self.sent(p, "QVpVUkU")) is None
        # a create that failed made no key
        failed = self.keys_with(vault_entry("POST", "/keys/k3/create", status=403))
        p = self.decrypt("/keys/k3/v/decrypt")
        assert failed.reason(p, self.sent(p, "QVpVUkU")) is None
        # no body to look at
        assert keys.reason(self.decrypt("/keys/k1/v/decrypt"), None) is None

    def test_the_same_name_in_another_vault_is_another_key(self):
        keys = self.keys_with(prep(method="POST", uri="https://one.vault.azure.net/keys/k/create?api-version=7.5"))
        p = self.decrypt("", uri="https://two.vault.azure.net/keys/k/v/decrypt?api-version=7.5")
        assert keys.reason(p, self.sent(p, "QVpVUkU")) is None

    def test_a_whole_recording_replayed_with_classification_leaves_the_entry_out(self):
        recording = [
            entry("POST", f"{KV}/keys/k1/create?api-version=7.5", 200, req_body='{"kty":"RSA"}', resp_body='{"key":{"kid":"x"}}'),
            entry("POST", f"{KV}/keys/k1/v1/decrypt?api-version=7.5", 200, req_body='{"alg":"RSA-OAEP","value":"QVpVUkU"}',
                  resp_body='{"kid":"x","value":"aGk"}'),
        ]
        prepared = [replay.prepare_entry(r, i) for i, r in enumerate(recording)]
        result = replay.RecordingResult(name="r", path="/x/azure-keyvault-keys/r.json", package="azure-keyvault-keys", status="pass")
        out = replay._dry_run(result, prepared, settings())
        assert [e.klass for e in out.entries] == ["pass", replay.CLASS_GENERATED_KEY]
        result = replay.RecordingResult(name="r", path="/x/azure-keyvault-keys/r.json", package="azure-keyvault-keys", status="pass")
        out = replay._dry_run(result, prepared, settings(classify=False))
        assert [e.klass for e in out.entries] == ["pass", "pass"]
