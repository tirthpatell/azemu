"""The REST API without an SDK (requests): the answers an SDK does not show, the headers and the refusals. Texts and codes are those of a Standard
namespace (November 2026); a test that only holds for the emulator says so."""

import re

import pytest

import srutil
from srutil import API, AVRO, error_text, http


def put(emu, group, name, body, ctype=AVRO, api=API):
    return http(emu, "PUT", f"/$schemaGroups/{group}/schemas/{name}", body=body, ctype=ctype, api=api)


def test_the_headers_of_a_registration(emu, group, names):
    name = names()
    s = srutil.order_schema(name.replace("-", "_"))
    r = put(emu, group, name, s)
    assert r.status_code == 204 and r.content == b""
    sid = r.headers["Schema-Id"]
    assert re.fullmatch(r"[0-9a-f]{32}", sid)
    assert (r.headers["Schema-Version"], r.headers["Schema-Group-Name"], r.headers["Schema-Name"]) == ("1", group, name)
    tail = f"?api-version={API}"
    assert re.search(rf"/\$schemagroups/{group}/schemas/{name}/versions/1{re.escape(tail)}$", r.headers["Location"], re.I)
    assert re.search(rf"/\$schemagroups/\$schemas/{sid}{re.escape(tail)}$", r.headers["Schema-Id-Location"], re.I)
    assert re.search(rf"/\$schemagroups/{group}/schemas/{name}/versions{re.escape(tail)}$", r.headers["Schema-Versions-Location"], re.I)
    # the content comes back as the compact JSON, with the content type Azure gives it
    g = http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions/1")
    assert (g.status_code, g.headers["Content-Type"], g.text) == (200, "application/json;serialization=Avro", s)
    assert g.headers["Schema-Id"] == sid
    # `0` is the latest version
    assert http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions/0").headers["Schema-Version"] == "1"


def test_get_id_with_both_spellings_of_the_path(emu, group, names):
    name = names()
    s = srutil.order_schema(name.replace("-", "_"))
    sid = put(emu, group, name, s).headers["Schema-Id"]
    for path in (f"/$schemaGroups/{group}/schemas/{name}:get-id", f"/$schemaGroups/{group}/schemas/{name}/:get-id"):
        r = http(emu, "POST", path, body=s, ctype=AVRO)
        assert (r.status_code, r.headers["Schema-Id"]) == (204, sid)
    other = http(emu, "POST", f"/$schemaGroups/{group}/schemas/{name}:get-id", body=srutil.order_schema("Other_x"), ctype=AVRO)
    assert (other.status_code, error_text(other)) == (404, f"Schema {group}/{name} does not exist.")
    assert other.headers["x-ms-error-code"] == "ItemNotFound"


def test_versions_and_names_of_a_group(emu, group, names):
    name = names()
    n = name.replace("-", "_")
    put(emu, group, name, srutil.order_schema(n))
    put(emu, group, name, srutil.order_schema(n, [("qty", "int")]))
    v = http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions").json()
    assert v["Value"] == [1, 2]
    assert v["NextLink"].endswith(f"/$schemaGroups/{group}/schemas/{name}/versions?api-version={API}&%24skip=1")
    assert name in http(emu, "GET", f"/$schemaGroups/{group}/schemas").json()["Value"]
    assert group in http(emu, "GET", "/$schemaGroups").json()["Value"]
    props = http(emu, "GET", f"/$schemaGroups/{group}").json()
    assert (props["SchemaCompatibility"], props["SchemaType"]) == ("Forward", "Avro")
    assert http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions?$skip=1").json() == {"NextLink": None, "Value": [2]}
    assert http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions?$top=1&$skip=0").json()["Value"] == [1]


def test_delete_a_version_then_the_schema(emu, group, names):
    """Azure's DELETE is in neither the REST specification nor the SDKs; what it does was found by trying (November 2026)."""
    name = names()
    n = name.replace("-", "_")
    v1 = put(emu, group, name, srutil.order_schema(n)).headers["Schema-Id"]
    put(emu, group, name, srutil.order_schema(n, [("qty", "int")]))
    assert http(emu, "DELETE", f"/$schemaGroups/{group}/schemas/{name}/versions/1").status_code == 204
    assert http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions").json()["Value"] == [2]
    gone = http(emu, "GET", f"/$schemaGroups/$schemas/{v1}")
    assert (gone.status_code, error_text(gone)) == (404, f"Schema id {v1} does not exist.")
    # deleting what is not there is no error
    assert http(emu, "DELETE", f"/$schemaGroups/{group}/schemas/{name}/versions/9").status_code == 204
    # DELETE on the schema takes away its latest version only: the one version left goes, then there is nothing, and that is no error either
    assert http(emu, "DELETE", f"/$schemaGroups/{group}/schemas/{name}").status_code == 204
    assert http(emu, "GET", f"/$schemaGroups/{group}/schemas/{name}/versions").json()["Value"] == []
    assert http(emu, "DELETE", f"/$schemaGroups/{group}/schemas/{name}").status_code == 204


def test_the_refusals(emu, group, names):
    name = names()
    s = srutil.order_schema(name.replace("-", "_"))
    path = f"/$schemaGroups/{group}/schemas/{name}"

    def refused(r, status, code, text):
        assert (r.status_code, r.headers.get("x-ms-error-code"), error_text(r)) == (status, code, text)

    refused(http(emu, "PUT", path, body=s, ctype=None), 400, "InvalidRequest", "Content-Type is required.")
    refused(
        http(emu, "PUT", path, body=s, ctype="application/json"), 400, "InvalidRequest",
        "Missing required header for PUT request. Schema/serialization type must be provided.",
    )
    refused(
        http(emu, "PUT", path, body=s, ctype="application/json; serialization=Thrift"), 415, "InvalidSchemaType",
        "Invalid schema type for PUT request. 'thrift' is not supported.",
    )
    refused(
        http(emu, "PUT", path, body="", ctype=AVRO), 400, "InvalidRequest",
        "Error when consuming request body: The body of the message cannot be read because it is empty.",
    )
    refused(http(emu, "PUT", path, body="{}", ctype=AVRO), 400, "InvalidRequest", "Avro schema validation failed: Property type is required at ''.")
    refused(
        http(emu, "PUT", path, body='{"type":"enum","name":"E"}', ctype=AVRO), 400, "InvalidRequest",
        "Avro schema validation failed: Enum has no symbols: E at ''.",
    )
    refused(http(emu, "GET", f"{path}/versions/abc"), 400, "InvalidRequest", "Schema version abc is not a valid version number.")
    refused(http(emu, "GET", f"{path}/versions/9"), 404, "ItemNotFound", f"Schema {group}/{name}/ver.9 does not exist.")
    refused(http(emu, "GET", "/$schemaGroups/$schemas/notanid"), 400, "InvalidRequest", "The request is invalid. Invalid schema Id format.")
    refused(
        http(emu, "GET", "/$schemaGroups/$schemas/00000000000000000000000000000000"), 400, "InvalidRequest",
        "SubCode=40000, Missing schema id as input.",
    )
    refused(http(emu, "GET", f"/$schemaGroups/{group}/schemas/-bad/versions/1"), 400, "InvalidRequest", "Name must start and end with letter or digit.")
    refused(http(emu, "GET", f"/$schemaGroups/{group}/schemas/a$b/versions/1"), 400, "InvalidRequest", "Invalid character '$' detected in name 'a$b'.")
    refused(http(emu, "GET", f"/$schemaGroups/{group}/schemas/{'a' * 101}/versions/1"), 400, "InvalidRequest", "Name cannot exceed 100 characters.")
    refused(http(emu, "POST", "/$schemaGroups", body="x", ctype=AVRO), 400, "InvalidRequest",
            "The given path $schemaGroups is not a valid path for SchemaRegistry protocol handler's Put or Post request.")


def test_the_size_limit_is_one_mebibyte(emu, group, names):
    name = names()

    def sized(n):
        base = '{"type":"record","name":"Big","doc":"","fields":[]}'
        return '{"type":"record","name":"Big","doc":"%s","fields":[]}' % ("x" * (n - len(base)))

    over = put(emu, group, name, sized(1_048_577))
    assert (over.status_code, over.headers.get("x-ms-error-code")) == (403, "NotAvaliable")
    assert error_text(over) == (
        f"SubCode=40300, Cannot create schema {name} because size of schema data cannot exceed 1048576 bytes. Right now {name} data is 1048577 bytes."
    )


def test_api_version_and_credentials(emu, group):
    path = "/$schemaGroups"
    r = http(emu, "GET", path, api=None)
    assert (r.status_code, error_text(r)) == (400, "No query parameter found. At least api-version should be provided.")
    r = http(emu, "GET", path, api="")
    assert (r.status_code, error_text(r)) == (400, "api-version should be provided.")
    r = http(emu, "GET", path, api="2099-01-01")
    assert r.status_code == 400
    assert error_text(r).startswith("SubCode=40000, The api-version in the query string is not supported. Either remove it from the Uri or use one of '2012-03,")
    for v in ("2021-10", "2022-10", "2023-07-01"):
        assert http(emu, "GET", path, api=v).status_code == 200, v
    r = http(emu, "GET", path, token=False)
    assert (r.status_code, r.headers["x-ms-error-code"], error_text(r)) == (401, "ErrorInRequest", "Missing authorization token.")
    r = http(emu, "GET", path, token=False, headers={"Authorization": "Basic YTpi"})
    assert r.status_code == 401
    assert error_text(r) == "SubCode=40105, MalformedToken: Invalid authorization header: The request is missing WRAP authorization credentials."


@pytest.mark.emulator_only
def test_any_token_is_accepted_by_the_emulator(emu, group):
    r = http(emu, "GET", "/$schemaGroups", token=False, headers={"Authorization": "Bearer not.a.jwt"})
    assert r.status_code == 200


@pytest.mark.emulator_only
def test_a_reset_drops_the_groups_and_their_schemas(emu, group, names):
    name = names()
    put(emu, group, name, srutil.order_schema(name.replace("-", "_")))
    emu.reset()
    assert http(emu, "GET", "/$schemaGroups").json()["Value"] == []
