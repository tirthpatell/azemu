"""Item ids and partition key lengths over raw REST, and resetting one account.

The Python SDK refuses an item id with `/`, `\\`, `?` or `#` before it sends anything, so these tests send the requests
themselves. Azure's service takes every character except `/` and `\\` in an item id, up to 1,023 bytes, and a partition key
value of at most 2,048 bytes ("Service quotas and default limits", per-item limits). Database and container names keep
255 characters and refuse `?` and `#`.
"""

import json
import os
import urllib.parse
import urllib.request

import pytest

from cosmosutil import RawCosmos, uid

EXTERNAL = bool(os.environ.get("AZEMU_COSMOS_ENDPOINT"))
needs_azemu = pytest.mark.skipif(EXTERNAL, reason="uses the emulator's own admin API")


@pytest.fixture
def raw(endpoint, ca_file):
    return RawCosmos(endpoint, ca_file)


@pytest.fixture
def coll(raw):
    db = uid("db")
    assert raw.post("dbs", body={"id": db}).status_code == 201
    c = uid("c")
    r = raw.post(f"dbs/{db}/colls", body={"id": c, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}})
    assert r.status_code == 201, r.text
    return db, c


def pk(v):
    return {"x-ms-documentdb-partitionkey": json.dumps([v])}


def enc(s):
    return urllib.parse.quote(s, safe="")


def create(raw, coll, body, key="p"):
    db, c = coll
    return raw.post(f"dbs/{db}/colls/{c}/docs", body=body, headers=pk(key))


AWKWARD = ["USER#123", "a?b=c", "#", "?", "100% sure", "a#b?c#d", "naïve ☃ id"]


@pytest.mark.parametrize("item_id", AWKWARD)
def test_an_item_id_with_hash_or_question_mark_works_in_every_operation(raw, coll, item_id):
    db, c = coll
    path = f"dbs/{db}/colls/{c}/docs/{enc(item_id)}"
    r = create(raw, coll, {"id": item_id, "pk": "p", "v": 1})
    assert r.status_code == 201, r.text
    assert r.json()["id"] == item_id
    assert "#" not in r.json()["_self"] and "?" not in r.json()["_self"]
    r = raw.get(path, headers=pk("p"), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/{item_id}")
    assert r.status_code == 200, r.text
    assert r.json()["id"] == item_id
    r = raw.put(path, body={"id": item_id, "pk": "p", "v": 2}, headers=pk("p"), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/{item_id}")
    assert r.status_code == 200, r.text
    r = raw.request("PATCH", path, body={"operations": [{"op": "incr", "path": "/v", "value": 5}]}, headers=pk("p"),
                    resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/{item_id}")
    assert r.status_code == 200, r.text
    assert r.json()["v"] == 7
    assert create(raw, coll, {"id": item_id, "pk": "p"}).status_code == 409
    q = raw.post(f"dbs/{db}/colls/{c}/docs", body={"query": "SELECT c.id FROM c WHERE c.id = @i", "parameters": [{"name": "@i", "value": item_id}]},
                 headers={**pk("p"), "x-ms-documentdb-isquery": "True", "content-type": "application/query+json"})
    assert q.json()["Documents"] == [{"id": item_id}]
    r = raw.delete(path, headers=pk("p"), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/{item_id}")
    assert r.status_code == 204
    r = raw.get(path, headers=pk("p"), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/{item_id}")
    assert r.status_code == 404


def test_the_change_feed_and_a_batch_take_such_ids(raw, coll):
    db, c = coll
    assert create(raw, coll, {"id": "USER#1?x", "pk": "p"}).status_code == 201
    r = raw.get(f"dbs/{db}/colls/{c}/docs", headers={"A-IM": "Incremental Feed"}, resource_type="docs", signed_link=f"dbs/{db}/colls/{c}")
    assert r.status_code == 200, r.text
    assert [d["id"] for d in r.json()["Documents"]] == ["USER#1?x"]
    ops = [
        {"operationType": "Create", "resourceBody": {"id": "B#1", "pk": "p"}},
        {"operationType": "Read", "id": "B#1"},
        {"operationType": "Delete", "id": "B#1"},
    ]
    r = raw.post(f"dbs/{db}/colls/{c}/docs", body=ops,
                 headers={**pk("p"), "x-ms-cosmos-is-batch-request": "True", "x-ms-cosmos-batch-atomic": "True"},
                 resource_type="docs", signed_link=f"dbs/{db}/colls/{c}")
    assert r.status_code == 200, r.text
    assert [o["statusCode"] for o in r.json()] == [201, 200, 204]


def test_an_item_id_is_at_most_1023_bytes_and_has_no_slash_or_backslash(raw, coll):
    assert create(raw, coll, {"id": "a" * 256, "pk": "p"}).status_code == 201, "more than the 255 characters of a container name"
    assert create(raw, coll, {"id": "a" * 1023, "pk": "p"}).status_code == 201
    assert create(raw, coll, {"id": "€" * 341, "pk": "p"}).status_code == 201, "1,023 bytes"
    r = create(raw, coll, {"id": "a" * 1024, "pk": "p"})
    assert r.status_code == 400, r.text
    assert create(raw, coll, {"id": "€" * 342, "pk": "p"}).status_code == 400, "1,026 bytes"
    for bad in ["a/b", "a\\b", "x ", ""]:
        assert create(raw, coll, {"id": bad, "pk": "p"}).status_code == 400, repr(bad)


def test_database_and_container_names_keep_their_own_rules(raw):
    assert raw.post("dbs", body={"id": "d" * 255}).status_code == 201
    assert raw.post("dbs", body={"id": "d" * 256}).status_code == 400
    for bad in ["a#b", "a?b", "a/b", "a\\b"]:
        assert raw.post("dbs", body={"id": bad}).status_code == 400, bad


def test_a_partition_key_value_is_at_most_2048_bytes(raw, coll):
    db, c = coll
    ok, long = "k" * 2048, "k" * 2049
    assert create(raw, coll, {"id": "1", "pk": ok}, key=ok).status_code == 201
    r = create(raw, coll, {"id": "2", "pk": long}, key="p")
    assert r.status_code == 400, r.text
    assert "2048" in r.text
    assert raw.get(f"dbs/{db}/colls/{c}/docs/1", headers=pk(ok), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/1").status_code == 200
    assert raw.get(f"dbs/{db}/colls/{c}/docs/1", headers=pk(long), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/1").status_code == 400
    assert create(raw, coll, {"id": "3", "pk": "€" * 682}, key="€" * 682).status_code == 201, "2,046 bytes"
    assert create(raw, coll, {"id": "4", "pk": "€" * 683}, key="€" * 683).status_code == 400, "2,049 bytes"


@needs_azemu
def test_reset_of_one_account_leaves_the_other(emu, make_client, acct, ca_file):
    other = "azemu" + uid("")
    mine, theirs = make_client(), None
    from cosmosutil import new_client

    theirs = new_client(emu.https_url("documents", other) + "/", ca_file)
    for cl in (mine, theirs):
        db = cl.create_database(uid("db"))
        db.create_container(id="c", partition_key={"paths": ["/pk"], "kind": "Hash"}).create_item({"id": "x", "pk": "p"})
    url = f"{emu.gateway}/_azemu/reset?service=cosmos&account={acct}"
    assert json.load(urllib.request.urlopen(urllib.request.Request(url, method="POST", data=b""), timeout=10)) == {"reset": ["cosmos"], "notSupported": []}
    assert list(mine.list_databases()) == []
    assert len(list(theirs.list_databases())) == 1
    # an unknown service is a 400, and nothing is reset
    with pytest.raises(urllib.error.HTTPError) as e:
        urllib.request.urlopen(urllib.request.Request(f"{emu.gateway}/_azemu/reset?service=nope&account={other}", method="POST", data=b""), timeout=10)
    assert e.value.code == 400
    assert len(list(theirs.list_databases())) == 1
