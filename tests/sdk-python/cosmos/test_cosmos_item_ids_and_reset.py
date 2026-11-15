"""Item ids and partition key lengths over raw REST, and resetting one account.

The Python SDK refuses an item id with `/`, `\\`, `?` or `#` before it sends anything, so these tests send the requests
themselves. What Azure does (checked against a real account in November 2026): an item id may hold any character, up to
1,023 characters (a longer one is a 400; one that is shorter but has more than 1,023 bytes in UTF-8 is a 500); a request by id
cannot reach an id with `#`, `/`, `\\`, a trailing space or a percent escape in it (401, the gateway reads another path than
the one that was signed). A partition key value is at most 2,048 bytes. A database or container name is at most 254
characters, may hold `#`, `?` and spaces, and must not hold `/` or `\\`.
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


AWKWARD = ["a?b=c", "?", "100% sure", "naïve ☃ id", " leading space", "a b"]
# the gateway cuts the path at a decoded `#`, and reads `/`, `\\`, a trailing space and a percent escape differently from the
# client: such an item is created, but a request by id is a 401 that shows the text Azure signed
UNREACHABLE = ["USER#123", "#", "a#b?c#d", "a/b", "a\\b", "x ", "p%20q"]


@pytest.mark.parametrize("item_id", AWKWARD)
def test_an_item_id_with_odd_characters_works_in_every_operation(raw, coll, item_id):
    db, c = coll
    path = f"dbs/{db}/colls/{c}/docs/{enc(item_id)}"
    link = f"dbs/{db}/colls/{c}/docs/{item_id}"
    r = create(raw, coll, {"id": item_id, "pk": "p", "v": 1})
    assert r.status_code == 201, r.text
    assert r.json()["id"] == item_id
    assert "#" not in r.json()["_self"] and "?" not in r.json()["_self"]
    r = raw.get(path, headers=pk("p"), resource_type="docs", signed_link=link)
    assert r.status_code == 200, r.text
    assert r.json()["id"] == item_id
    r = raw.put(path, body={"id": item_id, "pk": "p", "v": 2}, headers=pk("p"), resource_type="docs", signed_link=link)
    assert r.status_code == 200, r.text
    r = raw.request("PATCH", path, body={"operations": [{"op": "incr", "path": "/v", "value": 5}]}, headers=pk("p"),
                    resource_type="docs", signed_link=link)
    assert r.status_code == 200, r.text
    assert r.json()["v"] == 7
    assert create(raw, coll, {"id": item_id, "pk": "p"}).status_code == 409
    q = raw.post(f"dbs/{db}/colls/{c}/docs", body={"query": "SELECT c.id FROM c WHERE c.id = @i", "parameters": [{"name": "@i", "value": item_id}]},
                 headers={**pk("p"), "x-ms-documentdb-isquery": "True", "content-type": "application/query+json"})
    assert q.json()["Documents"] == [{"id": item_id}]
    r = raw.delete(path, headers=pk("p"), resource_type="docs", signed_link=link)
    assert r.status_code == 204
    r = raw.get(path, headers=pk("p"), resource_type="docs", signed_link=link)
    assert r.status_code == 404


@pytest.mark.parametrize("item_id", UNREACHABLE)
def test_an_item_id_the_gateway_cannot_address_is_created_but_every_request_by_id_is_401(raw, coll, item_id):
    db, c = coll
    r = create(raw, coll, {"id": item_id, "pk": "p", "v": 1})
    assert r.status_code == 201, r.text
    assert r.json()["id"] == item_id
    path = f"dbs/{db}/colls/{c}/docs/{enc(item_id)}"
    link = f"dbs/{db}/colls/{c}/docs/{item_id}"
    for verb, body in (("GET", None), ("PUT", {"id": item_id, "pk": "p"}), ("DELETE", None),
                       ("PATCH", {"operations": [{"op": "set", "path": "/v", "value": 2}]})):
        r = raw.request(verb, path, body=body, headers=pk("p"), resource_type="docs", signed_link=link)
        assert r.status_code == 401, (verb, r.text)
        assert "Server used the following payload to sign:" in r.json()["message"], r.text
    # the other ways to reach it work: a query, the change feed, the resource id
    q = raw.post(f"dbs/{db}/colls/{c}/docs", body={"query": "SELECT c.id, c._rid FROM c WHERE c.id = @i", "parameters": [{"name": "@i", "value": item_id}]},
                 headers={**pk("p"), "x-ms-documentdb-isquery": "True", "content-type": "application/query+json"})
    assert [d["id"] for d in q.json()["Documents"]] == [item_id]
    # the item is still there (and not changed by the refused requests)
    assert create(raw, coll, {"id": item_id, "pk": "p"}).status_code == 409


def test_the_text_azure_signed_when_it_refuses_an_id_with_a_hash(raw, coll):
    db, c = coll
    assert create(raw, coll, {"id": "USER#123", "pk": "p"}).status_code == 201
    r = raw.get(f"dbs/{db}/colls/{c}/docs/USER%23123", headers=pk("p"), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/USER#123")
    assert r.status_code == 401
    msg = r.json()["message"]
    assert msg.startswith("The input authorization token can't serve the request."), msg
    # the path is cut at the `#`
    assert f"payload to sign: 'get\ndocs\ndbs/{db}/colls/{c}/docs/USER\n" in msg, msg


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


def test_an_item_id_is_at_most_1023_bytes(raw, coll):
    assert create(raw, coll, {"id": "a" * 256, "pk": "p"}).status_code == 201, "more than the 254 characters of a container name"
    assert create(raw, coll, {"id": "a" * 1023, "pk": "p"}).status_code == 201
    assert create(raw, coll, {"id": "€" * 341, "pk": "p"}).status_code == 201, "1,023 bytes"
    r = create(raw, coll, {"id": "a" * 1024, "pk": "p"})
    assert r.status_code == 400, r.text
    assert "The input name is invalid. Ensure to provide a unique non-empty string less than '1024' characters." in r.text


def test_an_item_id_that_is_empty_or_not_a_string_is_a_400_that_names_it(raw, coll):
    r = create(raw, coll, {"id": "", "pk": "p"})
    assert r.status_code == 400 and "The input name '' is invalid. Ensure to provide a unique non-empty string less than '1024' characters." in r.text
    for value, shown in ((5, "5"), (True, "true"), (None, "null"), (["x"], "[")):
        r = create(raw, coll, {"id": value, "pk": "p"})
        assert r.status_code == 400, (value, r.text)
        assert f"The input name '{shown}' is invalid." in r.text, r.text
    r = create(raw, coll, {"pk": "p"})
    assert r.status_code == 400
    assert "the required properties - 'id; ' - are missing. Learn more: https://aka.ms/CosmosDB/sql/errors/missing-id" in r.text, r.text


def test_an_item_id_of_over_1023_bytes_in_multibyte_characters_is_a_500_not_a_400(raw, coll):
    # what Azure answers: 1,026 bytes in 342 characters
    r = create(raw, coll, {"id": "€" * 342, "pk": "p"})
    assert r.status_code == 500, r.text
    assert "An unknown error occurred while processing this request." in r.text
    assert create(raw, coll, {"id": "ä" * 512, "pk": "p"}).status_code == 500, "1,024 bytes"
    assert create(raw, coll, {"id": "ä" * 511, "pk": "p"}).status_code == 201, "1,022 bytes"
    # more than 1,023 characters is the 400 again, whatever the bytes
    assert create(raw, coll, {"id": "ä" * 1024, "pk": "p"}).status_code == 400


def test_database_names_keep_their_own_rules(raw):
    for bad in ["a/b", "a\\b"]:
        r = raw.post("dbs", body={"id": bad})
        assert r.status_code == 400, bad
        assert "The input name presented contains invalid character." in r.text, r.text


def test_database_name_length_and_hash_and_question_mark(raw):
    assert raw.post("dbs", body={"id": "d" * 254}).status_code == 201
    for bad in ("d" * 255, "d" * 256):
        r = raw.post("dbs", body={"id": bad})
        assert r.status_code == 400, len(bad)
        assert "The input name is invalid. Ensure to provide a unique non-empty string less than '255' characters." in r.text, r.text
        assert "The request payload is invalid. Ensure to provide a valid request payload." in r.text, r.text
    for odd in ["a#b", "a?b", "a b", "a%b", "x "]:
        assert raw.post("dbs", body={"id": odd}).status_code == 201, odd
    r = raw.post("dbs", body={"id": ""})
    assert r.status_code == 400 and "The input name '' is invalid." in r.text, r.text


def test_names_of_containers_and_users_follow_the_same_rules(raw, coll):
    db, _ = coll
    body = {"partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}}
    assert raw.post(f"dbs/{db}/colls", body={"id": "c" * 254, **body}).status_code == 201
    r = raw.post(f"dbs/{db}/colls", body={"id": "c" * 255, **body})
    assert r.status_code == 400 and "less than '255' characters" in r.text, r.text
    for odd in ["a#b", "a?b", " x"]:
        assert raw.post(f"dbs/{db}/colls", body={"id": odd, **body}).status_code == 201, odd
    for bad in ["a/b", "a\\b"]:
        assert raw.post(f"dbs/{db}/colls", body={"id": bad, **body}).status_code == 400, bad
    # users, stored procedures, triggers and functions refuse neither `/` nor `\\` (they cannot be addressed by name then)
    for odd in ["u/1", "u\\1", "u#1"]:
        assert raw.post(f"dbs/{db}/users", body={"id": odd}).status_code == 201, odd
    r = raw.post(f"dbs/{db}/users", body={"id": "u" * 255})
    assert r.status_code == 400 and "less than '255' characters" in r.text, r.text


def test_a_partition_key_value_is_at_most_2048_bytes(raw, coll):
    db, c = coll
    ok, long = "k" * 2048, "k" * 2049
    assert create(raw, coll, {"id": "1", "pk": ok}, key=ok).status_code == 201
    r = create(raw, coll, {"id": "2", "pk": long}, key="p")
    assert r.status_code == 400, r.text
    assert "Supplied partition key length 2049 is bigger than maximum allowed 2048." in r.text, r.text
    assert raw.get(f"dbs/{db}/colls/{c}/docs/1", headers=pk(ok), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/1").status_code == 200
    assert raw.get(f"dbs/{db}/colls/{c}/docs/1", headers=pk(long), resource_type="docs", signed_link=f"dbs/{db}/colls/{c}/docs/1").status_code == 400
    assert create(raw, coll, {"id": "3", "pk": "€" * 682}, key="€" * 682).status_code == 201, "2,046 bytes"
    assert create(raw, coll, {"id": "4", "pk": "€" * 683}, key="€" * 683).status_code == 400, "2,049 bytes"


@needs_azemu
@pytest.mark.emulator_only
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
