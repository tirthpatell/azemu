"""What the service checks on a request, and the shape of its errors, over raw REST.

Every expectation here was seen on a real Cosmos DB account (API for NoSQL, serverless, consistency Session) in November 2026
and the tests run against it too (`AZEMU_AZURE=1`): the API version, the consistency level, the session token, the
partition key header, a replace with another id, and the wording of the errors of items and patch.
"""

import json

import pytest

from cosmosutil import RawCosmos, uid


@pytest.fixture
def raw(endpoint, ca_file):
    return RawCosmos(endpoint, ca_file)


@pytest.fixture
def rc(raw):
    db, c = uid("db"), uid("c")
    assert raw.post("dbs", body={"id": db}).status_code == 201
    r = raw.post(f"dbs/{db}/colls", body={"id": c, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}})
    assert r.status_code == 201, r.text
    return db, c


def pk(*v):
    return {"x-ms-documentdb-partitionkey": json.dumps(list(v))}


def message(r):
    return r.json()["message"]


QUERY = {"x-ms-documentdb-isquery": "True", "content-type": "application/query+json"}


# ---- replace with another id ------------------------------------------------------------------------------------------
def test_replace_with_another_id_renames_the_document(raw, rc):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    created = raw.post(docs, body={"id": "p1", "pk": "a", "v": 1}, headers=pk("a")).json()
    raw.post(docs, body={"id": "q1", "pk": "a"}, headers=pk("a"))
    # the new id is taken: 409, and nothing changes
    r = raw.put(f"{docs}/p1", body={"id": "q1", "pk": "a", "v": 2}, headers=pk("a"))
    assert r.status_code == 409, r.text
    assert raw.get(f"{docs}/p1", headers=pk("a")).json()["v"] == 1
    # a new id renames it: the answer is the new document, which keeps the resource id; the old id is gone
    r = raw.put(f"{docs}/p1", body={"id": "other", "pk": "a", "v": 2}, headers=pk("a"))
    assert r.status_code == 200, r.text
    assert r.json()["id"] == "other" and r.json()["v"] == 2 and r.json()["_rid"] == created["_rid"]
    assert raw.get(f"{docs}/p1", headers=pk("a")).status_code == 404
    assert raw.get(f"{docs}/other", headers=pk("a")).json()["v"] == 2
    # a document that is not there cannot be renamed; the body's key must match the header
    assert raw.put(f"{docs}/nope", body={"id": "nope2", "pk": "a"}, headers=pk("a")).status_code == 404
    r = raw.put(f"{docs}/other", body={"id": "other", "pk": "b"}, headers=pk("a"))
    assert r.status_code == 400 and "doesn't match the one specified in the header" in message(r), r.text
    # the precondition is checked against the document the path names
    r = raw.put(f"{docs}/other", body={"id": "third", "pk": "a"}, headers={**pk("a"), "If-Match": '"stale"'})
    assert r.status_code == 412
    assert raw.get(f"{docs}/other", headers=pk("a")).status_code == 200


# ---- headers on every request -----------------------------------------------------------------------------------------
@pytest.mark.parametrize("version", ["2000-01-01", "abc", "", "1.0", "2020-7-15"])
def test_an_api_version_that_is_not_a_version_is_a_400(raw, rc, version):
    db, c = rc
    for verb, path in (("GET", f"dbs/{db}"), ("GET", "dbs"), ("GET", f"dbs/{db}/colls/{c}/docs/x")):
        r = raw.request(verb, path, headers={**pk("a"), "x-ms-version": version})
        assert r.status_code == 400, (path, r.text)
        assert message(r).startswith("Invalid API version. Ensure a valid x-ms-version header value is passed."), r.text


def test_the_versions_the_sdks_send_are_accepted(raw, rc):
    db, _ = rc
    for version in ("2018-12-31", "2020-07-15", "2020-11-05"):
        assert raw.get(f"dbs/{db}", headers={"x-ms-version": version}).status_code == 200, version


def test_a_consistency_level_stronger_than_the_account_s_is_a_400(raw, rc):
    db, c = rc
    for level in ("Strong", "BoundedStaleness"):
        for verb, path in (("GET", f"dbs/{db}/colls/{c}"), ("GET", "dbs"), ("GET", f"dbs/{db}/colls/{c}/docs/x")):
            r = raw.request(verb, path, headers={**pk("a"), "x-ms-consistency-level": level})
            assert r.status_code == 400, (level, path, r.text)
            assert message(r).startswith(
                f"ConsistencyLevel {level} specified in the request is invalid when service is configured with consistency level Session. "
                "Ensure the request consistency level is not stronger than the service consistency level."), r.text
    for level in ("bogus", "session", "STRONG"):
        r = raw.get(f"dbs/{db}/colls/{c}", headers={"x-ms-consistency-level": level})
        assert r.status_code == 400 and message(r).startswith(f"Value '{level}' specified for the header 'x-ms-consistency-level' is invalid."), r.text
    for level in ("Session", "ConsistentPrefix", "Eventual", ""):
        assert raw.get(f"dbs/{db}/colls/{c}", headers={"x-ms-consistency-level": level}).status_code == 200, level


@pytest.mark.parametrize("token", ["garbage", "0", "-1", "0:-1#x", "0:x#5"])
def test_a_session_token_that_is_malformed_is_a_400_on_documents(raw, rc, token):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    raw.post(docs, body={"id": "a", "pk": "p"}, headers=pk("p"))
    shown = token if ":" not in token else token.split(":", 1)[1]
    text = f"The session token provided '{shown}' is invalid."
    h = {**pk("p"), "x-ms-session-token": token}
    for r in (raw.get(f"{docs}/a", headers=h), raw.post(docs, body={"query": "SELECT * FROM c"}, headers={**h, **QUERY}),
              raw.get(docs, headers=h), raw.post(docs, body={"id": "b", "pk": "p"}, headers=h),
              raw.put(f"{docs}/a", body={"id": "a", "pk": "p"}, headers=h)):
        assert r.status_code == 400 and message(r).startswith(text), (r.request.method, r.text)
    # the other resources do not look at it
    assert raw.get(f"dbs/{db}/colls/{c}", headers={"x-ms-session-token": token}).status_code == 200


def test_a_session_token_that_is_ahead_is_a_404_with_sub_status_1002_on_reads(raw, rc):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    created = raw.post(docs, body={"id": "a", "pk": "p"}, headers=pk("p"))
    own = created.headers["x-ms-session-token"]
    assert own.startswith("0:") and "#" in own
    ahead = {**pk("p"), "x-ms-session-token": "0:-1#9999999"}
    text = "The read session is not available for the input session token."
    for r in (raw.get(f"{docs}/a", headers=ahead), raw.post(docs, body={"query": "SELECT * FROM c"}, headers={**ahead, **QUERY}),
              raw.get(docs, headers=ahead), raw.get(docs, headers={**ahead, "A-IM": "Incremental Feed"})):
        assert r.status_code == 404 and r.headers["x-ms-substatus"] == "1002", (r.request.method, r.text)
        assert message(r).startswith(text)
    # the token the service gave, another range, an old one, and writes are fine
    assert raw.get(f"{docs}/a", headers={**pk("p"), "x-ms-session-token": own}).status_code == 200
    assert raw.get(f"{docs}/a", headers={**pk("p"), "x-ms-session-token": "1:-1#9999999"}).status_code == 200
    assert raw.get(f"{docs}/a", headers={**pk("p"), "x-ms-session-token": "0:-1#1"}).status_code == 200
    assert raw.post(docs, body={"id": "b", "pk": "p"}, headers=ahead).status_code == 201
    assert raw.get(f"dbs/{db}/colls/{c}", headers={"x-ms-session-token": "0:-1#9999999"}).status_code == 200


# ---- the partition key header -----------------------------------------------------------------------------------------
def test_the_partition_key_header_errors(raw, rc):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    fewer = "The partition key supplied in x-ms-partitionkey header has fewer components than defined in the the collection."
    wrong = "PartitionKey extracted from document doesn't match the one specified in the header. Learn more: https://aka.ms/CosmosDB/sql/errors/wrong-pk-value"
    more = "Partition key provided either doesn't correspond to definition in the collection or doesn't match partition key field values specified in the document."
    r = raw.post(docs, body={"id": "1", "pk": "a"}, headers=pk("b"))
    assert r.status_code == 400 and wrong in message(r) and r.headers["x-ms-substatus"] == "1001", r.text
    r = raw.post(docs, body={"id": "1"}, headers=pk("b"))
    assert r.status_code == 400 and wrong in message(r), r.text
    for r in (raw.get(f"{docs}/1"), raw.delete(f"{docs}/1"), raw.request("PATCH", f"{docs}/1", body={"operations": [{"op": "set", "path": "/x", "value": 1}]})):
        assert r.status_code == 400 and fewer in message(r) and r.headers["x-ms-substatus"] == "1001", (r.request.method, r.text)
    r = raw.get(f"{docs}/1", headers=pk("a", "b"))
    assert r.status_code == 400 and message(r).startswith(more), r.text
    r = raw.get(f"{docs}/1", headers={"x-ms-documentdb-partitionkey": "abc"})
    assert r.status_code == 400 and message(r).startswith("Partition key abc is invalid."), r.text
    # the undefined partition is `[{}]`
    r = raw.post(docs, body={"id": "u"}, headers={"x-ms-documentdb-partitionkey": "[{}]"})
    assert r.status_code == 201, r.text


# ---- errors of items ---------------------------------------------------------------------------------------------------
def test_item_errors_are_not_wrapped_in_a_message_object(raw, rc):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    raw.post(docs, body={"id": "a", "pk": "p"}, headers=pk("p"))
    cases = [
        (raw.post(docs, body={"id": "a", "pk": "p"}, headers=pk("p")), 409, "Entity with the specified id already exists in the system."),
        (raw.get(f"{docs}/nope", headers=pk("p")), 404, "Entity with the specified id does not exist in the system. More info: https://aka.ms/cosmosdb-tsg-not-found"),
        (raw.delete(f"{docs}/nope", headers=pk("p")), 404, "Entity with the specified id does not exist in the system."),
        (raw.put(f"{docs}/a", body={"id": "a", "pk": "p"}, headers={**pk("p"), "If-Match": '"stale"'}),
         412, "Operation cannot be performed because one of the specified precondition is not met."),
    ]
    for r, status, text in cases:
        assert r.status_code == status, r.text
        msg = message(r)
        assert msg.startswith(text) and ", \r\nRequestStartTime: " in msg, msg
        assert "Number of regions attempted:" in msg and "Message:" not in msg
        assert r.json()["code"] == {409: "Conflict", 404: "NotFound", 412: "PreconditionFailed"}[status]


def test_errors_of_databases_containers_and_users_keep_the_message_object(raw, rc):
    db, c = rc
    exists = "Resource with specified id, name, or unique index already exists."
    for r in (raw.post("dbs", body={"id": db}),
              raw.post(f"dbs/{db}/colls", body={"id": c, "partitionKey": {"paths": ["/pk"], "kind": "Hash"}}),
              raw.post(f"dbs/{db}/users", body={"id": "u"}), raw.post(f"dbs/{db}/users", body={"id": "u"})):
        if r.status_code == 201:
            continue
        assert r.status_code == 409 and message(r).startswith(f'Message: {{"Errors":["{exists}"]}}'), r.text
    for r in (raw.get("dbs/nope"), raw.get(f"dbs/{db}/colls/nope"), raw.get(f"dbs/{db}/users/nope")):
        assert r.status_code == 404
        assert message(r).startswith('Message: {"Errors":["Resource Not Found. Learn more: https://aka.ms/cosmosdb-tsg-not-found"]}'), r.text


def test_a_body_that_is_not_a_document(raw, rc):
    db, c = rc
    docs = f"dbs/{db}/colls/{c}/docs"
    r = raw.post(docs, body="not json", headers={**pk("p"), "content-type": "application/json"})
    assert r.status_code == 400 and "The request payload is invalid. Ensure to provide a valid request payload." in message(r), r.text
    for body in ("[1]", ""):
        r = raw.post(docs, body=body, headers={**pk("p"), "content-type": "application/json"})
        assert r.status_code == 400 and "One of the specified inputs is invalid" in message(r), r.text


def test_a_missing_pre_trigger_is_named(raw, rc):
    db, c = rc
    r = raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "a", "pk": "p"},
                 headers={**pk("p"), "x-ms-documentdb-pre-trigger-include": "nope"})
    assert r.status_code == 400
    assert "The input trigger 'nope' for the document is not present in the collection. Ensure to pass a valid trigger." in message(r), r.text


# ---- patch ------------------------------------------------------------------------------------------------------------
def patch(raw, rc, ops, cond=None, **headers):
    db, c = rc
    body = {"operations": ops}
    if cond:
        body["condition"] = cond
    return raw.request("PATCH", f"dbs/{db}/colls/{c}/docs/d1", body=body, headers={**pk("a"), **headers})


@pytest.fixture
def pdoc(raw, rc):
    db, c = rc
    r = raw.post(f"dbs/{db}/colls/{c}/docs", body={"id": "d1", "pk": "a", "v": 1, "s": "x", "arr": [1, 2], "o": {"k": 1}}, headers=pk("a"))
    assert r.status_code == 201
    return rc


LEARN = "Learn more: https://aka.ms/cosmosdbpatchdocs"


@pytest.mark.parametrize("ops,text", [
    ([{"op": "set", "path": "/id", "value": "x"}], "Invalid patch request: Cannot patch system property 'id'."),
    ([{"op": "remove", "path": "/id"}], "Invalid patch request: Cannot patch system property 'id'."),
    ([{"op": "set", "path": "/_etag", "value": "x"}], "Invalid patch request: Cannot patch system property '_etag'."),
    ([{"op": "set", "path": "/pk", "value": "b"}],
     "PartitionKey extracted from document doesn't match the one specified in the header. Learn more: https://aka.ms/CosmosDB/sql/errors/wrong-pk-value"),
    ([{"op": "bogus", "path": "/s", "value": 1}], "Invalid value of the patch operation 'op' property in patch request: 'bogus'"),
    ([{"op": "set", "value": 1}], "The required property 'path' could not be found in patch request."),
    ([{"path": "/s", "value": 1}], "The required property 'op' could not be found in patch request."),
    ([{"op": "set", "path": "/s"}], "The required property 'value' could not be found in patch request."),
    ([{"op": "replace", "path": "/nothere", "value": 1}], f"For Operation(1): Node('nothere') to be replaced is absent. {LEARN}"),
    ([{"op": "remove", "path": "/nothere"}], f"For Operation(1): Path(from 'nothere') on which patch operation has to be applied is absent. {LEARN}"),
    ([{"op": "move", "from": "/nothere", "path": "/x"}], f"For Operation(1): Path(from 'nothere') on which patch operation has to be applied is absent. {LEARN}"),
    ([{"op": "incr", "path": "/s", "value": 1}], f"For Operation(1): Node('s') is not a number. {LEARN}"),
    ([{"op": "incr", "path": "/v", "value": "x"}], f"For Operation(1): The Increment patch operation requested has an unsupported data type. {LEARN}"),
    ([{"op": "add", "path": "/a/b", "value": 1}],
     f"For Operation(1): Add Operation can only create a child object of an existing node(array or object) and cannot create path recursively, no path found beyond: 'a'. {LEARN}"),
    ([{"op": "set", "path": "/a/b", "value": 1}],
     f"For Operation(1): Given Operation can only create a child object of an existing node(array or object) and cannot create path recursively, no path found beyond: 'a'. {LEARN}"),
    ([{"op": "add", "path": "/arr/5", "value": 1}], "For Operation(1): Index('5') to operate on is out of array bounds. Learn More: https://aka.ms/cosmosdbpatchdocs"),
    ([{"op": "set", "path": "/v", "value": 2}, {"op": "remove", "path": "/nothere"}],
     f"For Operation(2): Path(from 'nothere') on which patch operation has to be applied is absent. {LEARN}"),
    ([{"op": "set", "path": "s", "value": 1}], "One of the specified inputs is invalid"),
])
def test_patch_errors_use_azure_s_words(raw, pdoc, ops, text):
    r = patch(raw, pdoc, ops)
    assert r.status_code == 400, r.text
    assert text in message(r), message(r)


def test_patch_with_eleven_operations_names_the_limit(raw, pdoc):
    r = patch(raw, pdoc, [{"op": "set", "path": f"/z{i}", "value": i} for i in range(11)])
    assert r.status_code == 400 and "The number of patch operations cannot exceed '10'." in message(r), r.text


def test_patch_with_no_operations_is_a_304_with_no_body(raw, pdoc):
    db, c = pdoc
    r = patch(raw, pdoc, [])
    assert r.status_code == 304 and r.text == ""
    r = raw.request("PATCH", f"dbs/{db}/colls/{c}/docs/d1", body=[], headers=pk("a"))
    assert r.status_code == 304
    r = raw.request("PATCH", f"dbs/{db}/colls/{c}/docs/d1", body={}, headers=pk("a"))
    assert r.status_code == 400 and "The required property 'operations' could not be found in patch request." in message(r), r.text
    r = raw.request("PATCH", f"dbs/{db}/colls/{c}/docs/d1", body="nope", headers={**pk("a"), "content-type": "application/json"})
    assert r.status_code == 400 and "Invalid patch request: check syntax of patch spec." in message(r), r.text


def test_patch_condition_errors(raw, pdoc):
    r = patch(raw, pdoc, [{"op": "set", "path": "/s", "value": "y"}], cond="nonsense")
    assert r.status_code == 400
    inner = json.loads(json.loads(message(r).split("\r\nActivityId")[0][len("Message: "):])["Errors"][0])
    assert inner["errors"][0]["code"] == 1001 and inner["errors"][0]["severity"] == "Error"
    assert inner["errors"][0]["message"].startswith("Syntax error, incorrect syntax near 'nonsense'"), inner
    r = patch(raw, pdoc, [{"op": "set", "path": "/s", "value": "y"}], cond="from c where c.v = 99")
    assert r.status_code == 412 and r.headers["x-ms-substatus"] == "1110", r.text
    assert message(r).startswith("Operation cannot be performed because one of the specified precondition is not met."), r.text
    r = patch(raw, pdoc, [{"op": "set", "path": "/s", "value": "y"}], cond="from c where c.v = 1")
    assert r.status_code == 200 and r.json()["s"] == "y"


# ---- the shape of the account and of a container ----------------------------------------------------------------------
def test_the_account_document_has_the_keys_azure_sends(raw):
    body = raw.get("", resource_type="", signed_link="").json()
    for key in ("_dbs", "_rid", "_self", "addresses", "continuousBackupEnabled", "disableCrossRegionalHedging", "enableMultipleWriteLocations",
                "enableNRegionSynchronousCommit", "enablePerPartitionFailoverBehavior", "id", "media", "queryEngineConfiguration", "readPolicy",
                "readableLocations", "systemReplicationPolicy", "userConsistencyPolicy", "userReplicationPolicy", "writableLocations"):
        assert key in body, key
    # the consistency policy is only under `userConsistencyPolicy`
    assert "consistencyPolicy" not in body
    assert body["userConsistencyPolicy"]["defaultConsistencyLevel"] == "Session"


def test_a_container_with_the_default_policy_has_no_unique_key_policy(raw, rc):
    db, c = rc
    made = raw.get(f"dbs/{db}/colls/{c}").json()
    assert "uniqueKeyPolicy" not in made
    for key in ("indexingPolicy", "conflictResolutionPolicy", "geospatialConfig", "partitionKey"):
        assert key in made, key
    with_keys = raw.post(f"dbs/{db}/colls", body={"id": uid("c"), "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2},
                                                    "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]}}).json()
    assert with_keys["uniqueKeyPolicy"] == {"uniqueKeys": [{"paths": ["/email"]}]}

