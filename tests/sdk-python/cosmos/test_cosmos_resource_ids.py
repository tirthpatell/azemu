"""Resource ids (`_rid`) in the layout the Java and .NET SDKs parse, and the owner headers.

The layout is `com.azure.cosmos.implementation.ResourceId.tryParse` (the same as the .NET `ResourceId`),
written out again here: base64 with `-` in place of `/`; database 4 bytes; container database id plus 4 bytes
with the high bit of byte 4 set; user the same with that bit clear; document, stored procedure, trigger, UDF
and partition key range 16 bytes whose last byte carries a type in its high four bits; permission 16 bytes
under a user; offer 3 bytes.
"""

import base64

import pytest

from cosmosutil import RawCosmos, uid


@pytest.fixture
def raw(endpoint, ca_file):
    return RawCosmos(endpoint, ca_file)


def parse(rid):
    """Kind of a resource id by the Java SDK's rules, or raise AssertionError."""
    assert len(rid) % 4 == 0, rid
    b = base64.b64decode(rid.replace("-", "/"))
    assert len(b) <= 20 and (len(b) % 4 == 0 or len(b) == 3), rid
    if len(b) == 3:
        return "offer"
    if len(b) == 4:
        return "database"
    collection = bool(b[4] & 0x80)
    if not collection:
        assert len(b) in (8, 16), rid
        return "user" if len(b) == 8 else "permission"
    if len(b) == 8:
        return "collection"
    assert len(b) in (16, 20), rid
    t = (b[15] - 256 if b[15] > 127 else b[15]) >> 4
    kinds = {0: "document", 8: "stored procedure", -8: "stored procedure", 7: "trigger", 6: "udf",
             4: "conflict", 5: "partition key range"}
    assert t in kinds, (rid, t)
    return kinds[t]


@pytest.fixture
def tree(raw):
    """A database with a container, a document, a user with a permission, a sproc, a trigger and a UDF."""
    db = uid("db")
    c = uid("c")
    out = {"db": db, "c": c}
    out["database"] = raw.post("dbs", body={"id": db}).json()
    out["container"] = raw.post(
        f"dbs/{db}/colls", body={"id": c, "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}}
    ).json()
    base = f"dbs/{db}/colls/{c}"
    out["document"] = raw.post(
        f"{base}/docs", body={"id": "d1", "pk": "a"}, headers={"x-ms-documentdb-partitionkey": '["a"]'}
    ).json()
    out["user"] = raw.post(f"dbs/{db}/users", body={"id": "u1"}).json()
    out["permission"] = raw.post(
        f"dbs/{db}/users/u1/permissions", body={"id": "p1", "permissionMode": "Read", "resource": f"{base}"}
    ).json()
    out["sproc"] = raw.post(f"{base}/sprocs", body={"id": "s1", "body": "function(){}"}).json()
    out["trigger"] = raw.post(
        f"{base}/triggers",
        body={"id": "t1", "body": "function(){}", "triggerType": "Pre", "triggerOperation": "All"},
    ).json()
    out["udf"] = raw.post(f"{base}/udfs", body={"id": "f1", "body": "function(){}"}).json()
    out["pkrange"] = raw.get(f"{base}/pkranges").json()["PartitionKeyRanges"][0]
    return out


@pytest.mark.parametrize(
    "key,kind",
    [
        ("database", "database"),
        ("container", "collection"),
        ("document", "document"),
        ("user", "user"),
        ("permission", "permission"),
        ("sproc", "stored procedure"),
        ("trigger", "trigger"),
        ("udf", "udf"),
        ("pkrange", "partition key range"),
    ],
)
def test_rid_has_the_layout_of_its_resource_kind(tree, key, kind):
    assert parse(tree[key]["_rid"]) == kind


def test_rids_of_children_start_with_the_parent_rid(tree):
    db = base64.b64decode(tree["database"]["_rid"].replace("-", "/"))
    coll = base64.b64decode(tree["container"]["_rid"].replace("-", "/"))
    assert coll[:4] == db
    for key in ("document", "sproc", "trigger", "udf", "pkrange"):
        assert base64.b64decode(tree[key]["_rid"].replace("-", "/"))[:8] == coll
    user = base64.b64decode(tree["user"]["_rid"].replace("-", "/"))
    assert user[:4] == db
    assert base64.b64decode(tree["permission"]["_rid"].replace("-", "/"))[:8] == user


def test_self_links_are_built_from_the_rids(tree):
    d, c = tree["database"]["_rid"], tree["container"]["_rid"]
    assert tree["database"]["_self"] == f"dbs/{d}/"
    assert tree["container"]["_self"] == f"dbs/{d}/colls/{c}/"
    assert tree["document"]["_self"] == f"dbs/{d}/colls/{c}/docs/{tree['document']['_rid']}/"
    assert tree["user"]["_self"] == f"dbs/{d}/users/{tree['user']['_rid']}/"
    assert tree["permission"]["_self"] == f"dbs/{d}/users/{tree['user']['_rid']}/permissions/{tree['permission']['_rid']}/"
    assert tree["sproc"]["_self"] == f"dbs/{d}/colls/{c}/sprocs/{tree['sproc']['_rid']}/"
    assert tree["pkrange"]["_self"] == f"dbs/{d}/colls/{c}/pkranges/{tree['pkrange']['_rid']}/"


def test_offer_rid_is_an_offer_id(raw, tree):
    offers = raw.get("offers").json()["Offers"]
    mine = [o for o in offers if o["offerResourceId"] == tree["container"]["_rid"]]
    assert mine and parse(mine[0]["_rid"]) == "offer"
    assert len(mine[0]["_rid"]) == 4


def test_resources_can_be_read_by_rid_in_the_path(raw, tree):
    d, c = tree["database"]["_rid"], tree["container"]["_rid"]
    assert raw.get(f"dbs/{d}").json()["id"] == tree["db"]
    assert raw.get(f"dbs/{d}/colls/{c}").json()["id"] == tree["c"]
    got = raw.get(
        f"dbs/{d}/colls/{c}/docs/{tree['document']['_rid']}", headers={"x-ms-documentdb-partitionkey": '["a"]'}
    )
    assert got.status_code == 200 and got.json()["id"] == "d1"
    assert raw.get(f"dbs/{d}/users/{tree['user']['_rid']}").json()["id"] == "u1"


def test_rid_with_a_slash_in_standard_base64_is_written_with_a_dash(raw):
    # Make containers until one rid would contain "/" in standard base64: it must show as "-".
    db = uid("db")
    raw.post("dbs", body={"id": db})
    seen_dash = False
    for i in range(80):
        rid = raw.post(f"dbs/{db}/colls", body={"id": f"c{i}", "partitionKey": {"paths": ["/pk"], "kind": "Hash", "version": 2}}).json()["_rid"]
        assert "/" not in rid and "+" not in rid
        seen_dash = seen_dash or "-" in rid
        assert parse(rid) == "collection"
    # 80 ids of 4 varying bytes: the chance of no "-" is negligible; this documents the alphabet
    assert seen_dash


def test_content_path_is_the_owner_rid_and_alt_content_path_the_owner_name(raw, tree):
    db, c = tree["db"], tree["c"]
    d_rid, c_rid = tree["database"]["_rid"], tree["container"]["_rid"]
    base = f"dbs/{db}/colls/{c}"
    pk = {"x-ms-documentdb-partitionkey": '["a"]'}

    r = raw.get(base)  # container: owner is the database
    assert r.headers["x-ms-content-path"] == d_rid
    assert r.headers["x-ms-alt-content-path"] == f"dbs/{db}"

    for r in (
        raw.get(f"{base}/docs/d1", headers=pk),
        raw.post(f"{base}/docs", body={"id": "d2", "pk": "a"}, headers=pk),
        raw.get(f"{base}/docs"),
        raw.get(f"{base}/pkranges"),
        raw.get(f"{base}/sprocs/s1"),
        raw.delete(f"{base}/docs/d2", headers=pk),
    ):
        assert r.headers["x-ms-content-path"] == c_rid, r.request.url
        assert r.headers["x-ms-alt-content-path"] == base, r.request.url

    r = raw.get(f"dbs/{db}/users/u1")
    assert r.headers["x-ms-content-path"] == d_rid and r.headers["x-ms-alt-content-path"] == f"dbs/{db}"
    r = raw.get(f"dbs/{db}/users/u1/permissions/p1")
    assert r.headers["x-ms-content-path"] == tree["user"]["_rid"]
    assert r.headers["x-ms-alt-content-path"] == f"dbs/{db}/users/u1"

    r = raw.get(f"dbs/{db}")
    assert r.headers["x-ms-content-path"] == d_rid


def test_deleting_a_container_names_the_container_in_the_content_path(raw, tree):
    r = raw.delete(f"dbs/{tree['db']}/colls/{tree['c']}")
    assert r.status_code == 204
    assert r.headers["x-ms-content-path"] == tree["container"]["_rid"]
    assert r.headers["x-ms-alt-content-path"] == f"dbs/{tree['db']}"
