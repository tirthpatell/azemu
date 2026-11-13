"""Containers: create / read / replace / list / query / delete, policies, throughput."""

import pytest
from azure.cosmos import PartitionKey, ThroughputProperties
from azure.cosmos.exceptions import (
    CosmosHttpResponseError,
    CosmosResourceExistsError,
    CosmosResourceNotFoundError,
)

from cosmosutil import uid


def test_create_container_returns_proxy(db):
    cid = uid("c")
    c = db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    assert c.id == cid


def test_read_container_properties(db):
    cid = uid("c")
    props = db.create_container(id=cid, partition_key=PartitionKey(path="/pk")).read()
    assert props["id"] == cid
    for key in ("_rid", "_self", "_etag", "_ts", "partitionKey", "indexingPolicy"):
        assert key in props
    assert isinstance(props["_ts"], int)


def test_partition_key_defaults_to_hash_v2(container):
    pk = container.read()["partitionKey"]
    assert pk["paths"] == ["/pk"]
    assert pk["kind"] == "Hash"
    assert pk["version"] == 2


def test_partition_key_version_1(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/id", kind="Hash", version=1))
    assert c.read()["partitionKey"]["version"] == 1


def test_partition_key_on_nested_path(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/address/state"))
    assert c.read()["partitionKey"]["paths"] == ["/address/state"]
    c.create_item({"id": "a", "address": {"state": "WA"}})
    assert c.read_item("a", "WA")["address"]["state"] == "WA"


def test_partition_key_on_id(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/id"))
    c.create_item({"id": "a", "v": 1})
    assert c.read_item("a", "a")["v"] == 1


def test_hierarchical_partition_key_two_levels(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path=["/tenant", "/user"], kind="MultiHash"))
    pk = c.read()["partitionKey"]
    assert pk["kind"] == "MultiHash" and pk["paths"] == ["/tenant", "/user"] and pk["version"] == 2


def test_hierarchical_partition_key_three_levels(db):
    c = db.create_container(
        id=uid("c"), partition_key=PartitionKey(path=["/state", "/city", "/zipcode"], kind="MultiHash")
    )
    assert c.read()["partitionKey"]["paths"] == ["/state", "/city", "/zipcode"]


def test_hierarchical_partition_key_with_four_levels_is_400(db):
    with pytest.raises(CosmosHttpResponseError) as e:
        db.create_container(id=uid("c"), partition_key=PartitionKey(path=["/a", "/b", "/c", "/d"], kind="MultiHash"))
    assert e.value.status_code == 400


@pytest.mark.not_on_vnext(reason="vnext adds a second included path (/_ts)")
def test_default_indexing_policy(container):
    ip = container.read()["indexingPolicy"]
    assert ip["indexingMode"] == "consistent"
    assert ip["automatic"] is True
    # Microsoft test_create_default_indexing_policy: one included path '/*', one excluded path ('/_etag')
    assert len(ip["includedPaths"]) == 1 and ip["includedPaths"][0]["path"] == "/*"
    assert len(ip["excludedPaths"]) == 1


def test_custom_indexing_policy_is_stored(make_container):
    policy = {
        "indexingMode": "consistent",
        "automatic": True,
        "includedPaths": [{"path": "/*"}],
        "excludedPaths": [{"path": '/"notes"/*'}],
    }
    c = make_container("/pk", indexing_policy=policy)
    ip = c.read()["indexingPolicy"]
    assert {p["path"] for p in ip["excludedPaths"]} >= {'/"notes"/*'}


def test_composite_indexes_round_trip(make_container):
    comp = [[{"path": "/a", "order": "ascending"}, {"path": "/b", "order": "descending"}]]
    c = make_container("/pk", indexing_policy={"compositeIndexes": comp})
    assert c.read()["indexingPolicy"]["compositeIndexes"] == comp


def test_unique_key_policy_round_trip(make_container):
    c = make_container("/pk", unique_key_policy={"uniqueKeys": [{"paths": ["/email"]}]})
    assert c.read()["uniqueKeyPolicy"]["uniqueKeys"] == [{"paths": ["/email"]}]


def test_unique_key_violation_in_same_partition_is_409(make_container):
    c = make_container("/pk", unique_key_policy={"uniqueKeys": [{"paths": ["/email"]}]})
    c.create_item({"id": "1", "pk": "p", "email": "a@x"})
    with pytest.raises(CosmosResourceExistsError) as e:
        c.create_item({"id": "2", "pk": "p", "email": "a@x"})
    assert e.value.status_code == 409


def test_unique_key_allows_same_value_in_other_partition(make_container):
    c = make_container("/pk", unique_key_policy={"uniqueKeys": [{"paths": ["/email"]}]})
    c.create_item({"id": "1", "pk": "p", "email": "a@x"})
    c.create_item({"id": "2", "pk": "q", "email": "a@x"})
    assert c.read_item("2", "q")["email"] == "a@x"


def test_unique_key_violation_on_replace_is_409(make_container):
    c = make_container("/pk", unique_key_policy={"uniqueKeys": [{"paths": ["/email"]}]})
    c.create_item({"id": "1", "pk": "p", "email": "a@x"})
    c.create_item({"id": "2", "pk": "p", "email": "b@x"})
    with pytest.raises(CosmosResourceExistsError):
        c.replace_item("2", {"id": "2", "pk": "p", "email": "a@x"})


def test_default_ttl_round_trip(make_container):
    assert make_container("/pk", default_ttl=10).read()["defaultTtl"] == 10
    assert make_container("/pk", default_ttl=-1).read()["defaultTtl"] == -1


def test_no_default_ttl_by_default(container):
    assert "defaultTtl" not in container.read()


@pytest.mark.not_on_vnext(reason="vnext does not validate defaultTtl")
def test_invalid_default_ttl_is_400(db):
    with pytest.raises(CosmosHttpResponseError) as e:
        db.create_container(id=uid("c"), partition_key=PartitionKey(path="/id"), default_ttl=-10)
    assert e.value.status_code == 400


def test_conflict_resolution_policy_default_is_last_writer_wins(container):
    crp = container.read()["conflictResolutionPolicy"]
    assert crp["mode"] == "LastWriterWins"
    assert crp["conflictResolutionPath"] == "/_ts"


@pytest.mark.not_on_vnext(reason="vnext ignores a custom conflict resolution policy")
def test_custom_conflict_resolution_policy_round_trip(make_container):
    c = make_container(
        "/pk", conflict_resolution_policy={"mode": "LastWriterWins", "conflictResolutionPath": "/version"}
    )
    assert c.read()["conflictResolutionPolicy"]["conflictResolutionPath"] == "/version"


def test_duplicate_container_is_409(db):
    cid = uid("c")
    db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    with pytest.raises(CosmosResourceExistsError) as e:
        db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    assert e.value.status_code == 409


def test_same_container_id_in_two_databases_is_allowed(client, db):
    other = client.create_database(uid("db"))
    cid = uid("c")
    db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    other.create_container(id=cid, partition_key=PartitionKey(path="/pk"))


def test_read_missing_container_is_404(db):
    with pytest.raises(CosmosResourceNotFoundError) as e:
        db.get_container_client(uid("nope")).read()
    assert e.value.status_code == 404


def test_create_container_in_missing_database_is_404(client):
    with pytest.raises(CosmosResourceNotFoundError):
        client.get_database_client(uid("nodb")).create_container(id="c", partition_key=PartitionKey(path="/pk"))


def test_create_container_if_not_exists_is_idempotent(db):
    cid = uid("c")
    a = db.create_container_if_not_exists(id=cid, partition_key=PartitionKey(path="/pk"))
    a.create_item({"id": "keep", "pk": "x"})
    b = db.create_container_if_not_exists(id=cid, partition_key=PartitionKey(path="/pk"))
    assert b.read_item("keep", "x")["id"] == "keep"


def test_delete_container_then_read_is_404(db):
    cid = uid("c")
    db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    db.delete_container(cid)
    with pytest.raises(CosmosResourceNotFoundError):
        db.get_container_client(cid).read()


def test_delete_missing_container_is_404(db):
    with pytest.raises(CosmosResourceNotFoundError):
        db.delete_container(uid("nope"))


def test_recreated_container_is_empty(db):
    cid = uid("c")
    c = db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    c.create_item({"id": "a", "pk": "x"})
    db.delete_container(cid)
    c2 = db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    assert list(c2.read_all_items()) == []


def test_list_containers(db):
    ids = [uid("c") for _ in range(3)]
    for i in ids:
        db.create_container(id=i, partition_key=PartitionKey(path="/pk"))
    assert sorted(c["id"] for c in db.list_containers()) == sorted(ids)


def test_list_containers_of_empty_database(db):
    assert list(db.list_containers()) == []


def test_containers_of_other_database_are_not_listed(client, db):
    other = client.create_database(uid("db"))
    other.create_container(id="theirs", partition_key=PartitionKey(path="/pk"))
    assert list(db.list_containers()) == []


@pytest.mark.not_on_vnext(reason="vnext ignores WHERE in container queries")
def test_query_containers_by_id(db):
    a, b = uid("c"), uid("c")
    for i in (a, b):
        db.create_container(id=i, partition_key=PartitionKey(path="/pk"))
    got = list(db.query_containers({"query": "SELECT * FROM root r WHERE r.id=@id", "parameters": [{"name": "@id", "value": a}]}))
    assert [c["id"] for c in got] == [a]


def test_container_id_with_spaces_is_allowed(db):
    cid = "container with spaces " + uid("")
    c = db.create_container(id=cid, partition_key=PartitionKey(path="/pk"))
    c.create_item({"id": "a", "pk": "x"})
    assert c.read_item("a", "x")["id"] == "a"


@pytest.mark.parametrize("bad", ["a/b", "a\\b", "a?b", "a#b"])
def test_container_id_with_illegal_character_is_rejected_by_sdk(db, bad):
    with pytest.raises(ValueError):
        db.create_container(id=bad, partition_key=PartitionKey(path="/pk"))


def test_replace_container_changes_indexing_policy(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    db.replace_container(
        c,
        partition_key=PartitionKey(path="/pk"),
        indexing_policy={"indexingMode": "consistent", "automatic": True, "includedPaths": [{"path": "/*"}], "excludedPaths": [{"path": '/"big"/*'}]},
    )
    assert any(p["path"] == '/"big"/*' for p in c.read()["indexingPolicy"]["excludedPaths"])


def test_replace_container_sets_default_ttl(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    db.replace_container(c, partition_key=PartitionKey(path="/pk"), default_ttl=30)
    assert c.read()["defaultTtl"] == 30


def test_replace_container_keeps_documents(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    c.create_item({"id": "a", "pk": "x"})
    db.replace_container(c, partition_key=PartitionKey(path="/pk"), default_ttl=-1)
    assert c.read_item("a", "x")["id"] == "a"


def test_replace_missing_container_is_404(db):
    with pytest.raises(CosmosResourceNotFoundError):
        db.replace_container(uid("nope"), partition_key=PartitionKey(path="/pk"))


@pytest.mark.not_on_vnext(reason="vnext lets the partition key change on replace")
def test_replace_container_with_different_partition_key_is_400(db):
    c = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    with pytest.raises(CosmosHttpResponseError) as e:
        db.replace_container(c, partition_key=PartitionKey(path="/other"))
    assert e.value.status_code == 400


@pytest.mark.not_on_vnext(reason="vnext offers outlive deleted resources and are matched to reused ids, so throughput reads are unreliable")
def test_container_manual_throughput(make_container):
    c = make_container("/pk", offer_throughput=1000)
    assert c.get_throughput().offer_throughput == 1000


@pytest.mark.not_on_vnext(reason="vnext offers outlive deleted resources and are matched to reused ids, so throughput reads are unreliable")
def test_container_replace_throughput_with_int(make_container):
    c = make_container("/pk", offer_throughput=1000)
    c.replace_throughput(2500)
    assert c.get_throughput().offer_throughput == 2500


@pytest.mark.not_on_vnext(reason="vnext offers outlive deleted resources and are matched to reused ids, so throughput reads are unreliable")
def test_container_replace_throughput_with_object(make_container):
    c = make_container("/pk", offer_throughput=1000)
    c.replace_throughput(ThroughputProperties(offer_throughput=3000))
    assert c.get_throughput().offer_throughput == 3000


@pytest.mark.not_on_vnext(reason="vnext offers point at the wrong resource link")
def test_container_throughput_offer_resource_properties(make_container):
    # Microsoft test_offer_read_and_query: the offer references its container
    c = make_container("/pk", offer_throughput=1000)
    offer = c.get_throughput()
    coll = c.read()
    assert offer.properties["id"] and offer.properties["_rid"] and offer.properties["_self"]
    assert offer.properties["resource"].strip("/") == coll["_self"].strip("/")


@pytest.mark.not_on_vnext(reason="vnext has no autoscale offers")
def test_container_autoscale_throughput(make_container):
    c = make_container("/pk", offer_throughput=ThroughputProperties(auto_scale_max_throughput=7000, auto_scale_increment_percent=0))
    t = c.get_throughput()
    assert t.auto_scale_max_throughput == 7000 and t.auto_scale_increment_percent == 0
    assert t.offer_throughput is None
    c.replace_throughput(ThroughputProperties(auto_scale_max_throughput=9000, auto_scale_increment_percent=5))
    assert c.get_throughput().auto_scale_max_throughput == 9000


@pytest.mark.not_on_vnext(reason="vnext does not validate autoscale throughput")
def test_container_negative_autoscale_is_400(db):
    with pytest.raises(CosmosHttpResponseError) as e:
        db.create_container(
            id=uid("c"),
            partition_key=PartitionKey(path="/id"),
            offer_throughput=ThroughputProperties(auto_scale_max_throughput=-200, auto_scale_increment_percent=0),
        )
    assert e.value.status_code == 400
    assert "Requested throughput -200 is less than required minimum throughput 1000" in str(e.value)


@pytest.mark.not_on_vnext(reason="vnext reports an offer for every container")
def test_container_in_shared_throughput_database_has_no_offer(client):
    d = client.create_database(uid("db"), offer_throughput=1000)
    c = d.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    # The service answers the offer query with an empty list and the SDK turns that
    # into CosmosResourceNotFoundError. azure-cosmos 4.17.1 then trips over its own
    # 404 handling (_retry_utility calls _refresh_container_properties_cache(None))
    # and surfaces AttributeError instead; accept both.
    with pytest.raises((CosmosResourceNotFoundError, AttributeError)):
        c.get_throughput()


@pytest.mark.not_on_vnext(reason="vnext offers outlive deleted resources and are matched to reused ids, so throughput reads are unreliable")
def test_database_with_shared_throughput_has_offer(client):
    d = client.create_database(uid("db"), offer_throughput=1000)
    assert d.get_throughput().offer_throughput == 1000
