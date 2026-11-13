"""Cosmos DB state across restarts, through the official SDK: databases, containers with their policies, documents with
their system properties, change feed and query continuation tokens, session tokens, scripts, users and permissions."""

import pathlib
import sys

import pytest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "cosmos"))
from cosmosutil import new_client  # noqa: E402

from azure.cosmos import PartitionKey, exceptions  # noqa: E402

ACCOUNT = "persistacct"


def client(p):
    return new_client(p.emu.url("documents", ACCOUNT), p.emu.ca_file)


def system(d):
    return {k: d[k] for k in ("id", "_rid", "_self", "_etag", "_ts")}


@pytest.mark.parametrize("how", ["sigterm", "sigkill_after_save"])
def test_resources_documents_feeds_and_scripts_survive(persistent, how):
    p = persistent()
    c = client(p)
    db = c.create_database("appdb", offer_throughput=1000)
    cont = db.create_container(
        "items", partition_key=PartitionKey(path="/pk"), default_ttl=-1, offer_throughput=700,
        indexing_policy={"indexingMode": "consistent", "includedPaths": [{"path": "/*"}], "excludedPaths": [{"path": '/"big"/*'}]},
        unique_key_policy={"uniqueKeys": [{"paths": ["/email"]}]},
        conflict_resolution_policy={"mode": "LastWriterWins", "conflictResolutionPath": "/_ts"},
    )
    for i in range(6):
        cont.create_item({"id": f"d{i}", "pk": "p1" if i % 2 else "p0", "email": f"{i}@x", "n": i, "nested": {"list": [1, 2.5, None, "s"]}})
    cont.replace_item("d2", {"id": "d2", "pk": "p0", "email": "2@x", "n": 22})
    cont.delete_item("d3", partition_key="p1")
    cont.create_item({"id": "short", "pk": "p0", "email": "s@x", "ttl": 100000})
    # a stored procedure, a trigger, a user-defined function
    cont.scripts.create_stored_procedure({"id": "sp", "body": "function(){ getContext().getResponse().setBody(42); }"})
    cont.scripts.create_trigger({"id": "tr", "body": "function(){}", "triggerType": "Pre", "triggerOperation": "All"})
    cont.scripts.create_user_defined_function({"id": "fn", "body": "function(x){ return x + 1; }"})
    # a user with a permission
    user = db.create_user({"id": "u1"})
    user.create_permission({"id": "perm1", "permissionMode": "Read", "resource": cont.container_link})
    docs_before = {d["id"]: d for d in cont.read_all_items()}
    system_before = {i: system(d) for i, d in docs_before.items()}
    # a change feed read and the token of its end
    list(cont.query_items_change_feed(is_start_from_beginning=True))
    feed_token = c.client_connection.last_response_headers["etag"]
    cont.create_item({"id": "after-feed-1", "pk": "p0", "email": "af1@x"})
    cont.create_item({"id": "after-feed-2", "pk": "p1", "email": "af2@x"})
    # a query with a continuation token
    pager = cont.query_items("SELECT c.id FROM c", partition_key="p0", max_item_count=2).by_page()
    first_page = list(next(pager))
    token = pager.continuation_token
    assert token and len(first_page) == 2
    rest_before = [i["id"] for page in cont.query_items("SELECT c.id FROM c", partition_key="p0", max_item_count=2).by_page(token) for i in page]
    # the session token of a read
    cont.read_item("d0", partition_key="p0")
    session = c.client_connection.last_response_headers["x-ms-session-token"]
    throughput = cont.get_throughput().offer_throughput
    docs_before = {d["id"]: d for d in cont.read_all_items()}
    system_before = {i: system(d) for i, d in docs_before.items()}

    if how == "sigterm":
        assert p.stop() == 0
    else:
        p.save()
        p.stop(kill=True)
    p.start()

    c = client(p)
    db = c.get_database_client("appdb")
    cont = db.get_container_client("items")
    props = cont.read()
    assert props["partitionKey"]["paths"] == ["/pk"] and props["defaultTtl"] == -1
    assert props["uniqueKeyPolicy"] == {"uniqueKeys": [{"paths": ["/email"]}]}
    assert props["indexingPolicy"]["excludedPaths"][0]["path"] == '/"big"/*'
    assert props["conflictResolutionPolicy"]["conflictResolutionPath"] == "/_ts"
    assert db.get_throughput().offer_throughput == 1000 and cont.get_throughput().offer_throughput == throughput == 700
    docs_after = {d["id"]: d for d in cont.read_all_items()}
    assert docs_after == docs_before
    assert {i: system(d) for i, d in docs_after.items()} == system_before, "_rid, _self, _etag and _ts are unchanged"
    with pytest.raises(exceptions.CosmosResourceNotFoundError):
        cont.read_item("d3", partition_key="p1")
    # an etag from before the restart still works as a condition
    from azure.core import MatchConditions

    cont.replace_item("d0", {"id": "d0", "pk": "p0", "email": "0@x", "n": 100}, etag=docs_before["d0"]["_etag"], match_condition=MatchConditions.IfNotModified)
    # the unique key still holds
    with pytest.raises(exceptions.CosmosResourceExistsError):
        cont.create_item({"id": "new", "pk": "p0", "email": "0@x"})
    # the change feed goes on from the token of before the restart
    new = [d["id"] for d in cont.query_items_change_feed(continuation=feed_token)]
    assert sorted(new) == ["after-feed-1", "after-feed-2", "d0"]
    # the query continuation of before the restart: the saved result is gone, the query runs again and carries on
    rest = [i["id"] for page in cont.query_items("SELECT c.id FROM c", partition_key="p0", max_item_count=2).by_page(token) for i in page]
    assert rest == rest_before or sorted(rest) == sorted(rest_before)
    # a read that carries the session token of before the restart
    got = cont.read_item("d1", partition_key="p1", session_token=session)
    assert got["id"] == "d1"
    # the scripts
    assert cont.scripts.execute_stored_procedure("sp", partition_key="p0") == 42
    assert [t["id"] for t in cont.scripts.list_triggers()] == ["tr"]
    assert [f["id"] for f in cont.scripts.list_user_defined_functions()] == ["fn"]
    # users and permissions
    user = db.get_user_client("u1")
    assert [x["id"] for x in user.list_permissions()] == ["perm1"]
    # new resource ids never repeat what was used
    old_rids = {d["_rid"] for d in docs_before.values()}
    created = cont.create_item({"id": "brand-new", "pk": "p0", "email": "bn@x"})
    assert created["_rid"] not in old_rids
    other = db.create_container("later", partition_key=PartitionKey(path="/id"))
    assert other.read()["_rid"] != props["_rid"]


def test_ttl_is_absolute(persistent):
    import time

    p = persistent()
    c = client(p)
    db = c.create_database("ttldb")
    cont = db.create_container("t", partition_key=PartitionKey(path="/pk"), default_ttl=-1)
    cont.create_item({"id": "soon", "pk": "a", "ttl": 3})
    cont.create_item({"id": "never", "pk": "a"})
    p.save()
    p.stop(kill=True)
    time.sleep(4)
    p.start()
    cont = client(p).get_database_client("ttldb").get_container_client("t")
    assert sorted(d["id"] for d in cont.read_all_items()) == ["never"]
