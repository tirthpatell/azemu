"""A handful of raw REST calls with a master-key Authorization header."""

import json
import urllib.parse

import pytest
from azure.cosmos import PartitionKey

from cosmosutil import RawCosmos, uid


@pytest.fixture
def raw(endpoint, ca_file):
    return RawCosmos(endpoint, ca_file)


@pytest.fixture
def rdb(raw):
    name = uid("db")
    assert raw.post("dbs", body={"id": name}).status_code == 201
    return name


@pytest.fixture
def rcoll(raw, rdb):
    name = uid("c")
    r = raw.post(f"dbs/{rdb}/colls", body={"id": name, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}})
    assert r.status_code == 201, r.text
    return rdb, name


def test_get_root_is_database_account_json(raw, endpoint):
    r = raw.get("", resource_type="", signed_link="")
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("application/json")
    body = r.json()
    assert body["userConsistencyPolicy"]["defaultConsistencyLevel"]
    assert body["_dbs"].strip("/") == "dbs"
    assert body["writableLocations"] and body["readableLocations"]


def test_account_locations_point_back_at_the_emulator_host(raw, endpoint):
    body = raw.get("", resource_type="", signed_link="").json()
    host = urllib.parse.urlparse(endpoint).netloc
    for loc in body["writableLocations"] + body["readableLocations"]:
        assert loc["name"]
        u = urllib.parse.urlparse(loc["databaseAccountEndpoint"])
        assert u.netloc == host, loc
        assert "documents.azure.com" not in loc["databaseAccountEndpoint"]


def test_create_database_response_shape(raw):
    name = uid("db")
    r = raw.post("dbs", body={"id": name})
    assert r.status_code == 201
    body = r.json()
    assert body["id"] == name
    for k in ("_rid", "_self", "_etag", "_ts"):
        assert k in body
    assert r.headers["etag"] == body["_etag"]


def test_not_found_error_json_shape(raw):
    r = raw.get(f"dbs/{uid('nope')}")
    assert r.status_code == 404
    body = r.json()
    assert body["code"] == "NotFound"
    assert isinstance(body["message"], str) and body["message"]
    assert r.headers.get("x-ms-activity-id")


def test_conflict_error_json_shape(raw, rdb):
    r = raw.post("dbs", body={"id": rdb})
    assert r.status_code == 409
    assert r.json()["code"] == "Conflict"


def test_partition_key_header_not_matching_body_is_400_with_json_error(raw, rcoll):
    db, c = rcoll
    r = raw.post(
        f"dbs/{db}/colls/{c}/docs",
        body={"id": "d", "pk": "x"},  # body says "x", header says "other"
        headers={"x-ms-documentdb-partitionkey": json.dumps(["other"])},
    )
    assert r.status_code == 400
    assert r.json()["code"] == "BadRequest"


def test_document_response_headers(raw, rcoll):
    db, c = rcoll
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["x"])}
    r = raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "doc1", "pk": "x", "v": 1}, headers=pk)
    assert r.status_code == 201
    created = r.json()
    r = raw.get(f"dbs/{db}/colls/{c}/docs/doc1", headers=pk)
    assert r.status_code == 200
    assert r.json()["v"] == 1
    h = r.headers
    assert float(h["x-ms-request-charge"]) > 0
    assert h["x-ms-activity-id"]
    assert h["etag"] == created["_etag"]
    assert h["x-ms-session-token"]
    assert h["content-type"].startswith("application/json")


def test_document_read_with_other_partition_key_is_404(raw, rcoll):
    db, c = rcoll
    raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "doc1", "pk": "x"}, headers={"x-ms-documentdb-partitionkey": json.dumps(["x"])})
    r = raw.get(f"dbs/{db}/colls/{c}/docs/doc1", headers={"x-ms-documentdb-partitionkey": json.dumps(["y"])})
    assert r.status_code == 404


def test_delete_document_returns_204(raw, rcoll):
    db, c = rcoll
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["x"])}
    raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "doc1", "pk": "x"}, headers=pk)
    r = raw.delete(f"dbs/{db}/colls/{c}/docs/doc1", headers=pk)
    assert r.status_code == 204
    assert raw.get(f"dbs/{db}/colls/{c}/docs/doc1", headers=pk).status_code == 404


def test_partition_key_ranges_of_a_new_container(raw, rcoll):
    db, c = rcoll
    r = raw.get(f"dbs/{db}/colls/{c}/pkranges")
    assert r.status_code == 200
    body = r.json()
    ranges = body["PartitionKeyRanges"]
    assert len(ranges) >= 1
    first = ranges[0]
    assert first["id"] == "0" or first["id"]
    assert first["minInclusive"] == ""
    assert ranges[-1]["maxExclusive"] == "FF"
    assert body["_rid"]
    assert r.headers.get("etag")


def test_query_post_with_query_json_content_type(raw, rcoll):
    db, c = rcoll
    pk = {"x-ms-documentdb-partitionkey": json.dumps(["x"])}
    for i in range(3):
        raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": f"d{i}", "pk": "x", "n": i}, headers=pk)
    r = raw.post(
        f"dbs/{db}/colls/{c}/docs",
        body={"query": "SELECT c.id FROM c WHERE c.n >= @n", "parameters": [{"name": "@n", "value": 1}]},
        headers={
            "content-type": "application/query+json",
            "x-ms-documentdb-isquery": "True",
            "x-ms-documentdb-query-enablecrosspartition": "True",
        },
    )
    assert r.status_code == 200, r.text
    body = r.json()
    assert sorted(d["id"] for d in body["Documents"]) == ["d1", "d2"]
    assert body["_count"] == 2
    assert r.headers["x-ms-item-count"] == "2"
    assert float(r.headers["x-ms-request-charge"]) > 0


def test_list_collections_raw(raw, rcoll):
    db, c = rcoll
    body = raw.get(f"dbs/{db}/colls").json()
    assert [x["id"] for x in body["DocumentCollections"]] == [c]
    assert body["_count"] == 1


def test_sdk_and_raw_see_the_same_data(raw, rcoll, client):
    db, c = rcoll
    raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "doc1", "pk": "x", "v": 9}, headers={"x-ms-documentdb-partitionkey": json.dumps(["x"])})
    assert client.get_database_client(db).get_container_client(c).read_item("doc1", "x")["v"] == 9
