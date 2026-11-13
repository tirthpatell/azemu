"""Databases: create / read / list / query / delete, throughput."""

import pytest
from azure.cosmos import ThroughputProperties
from azure.cosmos.exceptions import (
    CosmosHttpResponseError,
    CosmosResourceExistsError,
    CosmosResourceNotFoundError,
)

from cosmosutil import uid


def test_client_construction_reads_database_account(client):
    account = client.get_database_account()
    assert account.DatabasesLink == "/dbs/"
    assert account.MediaLink == "/media/"
    assert account.ConsistencyPolicy["defaultConsistencyLevel"]


def test_create_database_returns_proxy_with_id(client, db_id):
    db = client.create_database(db_id)
    assert db.id == db_id


def test_read_database_has_system_properties(client, db_id):
    props = client.create_database(db_id).read()
    assert props["id"] == db_id
    for key in ("_rid", "_self", "_etag", "_ts"):
        assert key in props
    assert isinstance(props["_ts"], int)


def test_create_duplicate_database_is_409(client, db_id):
    client.create_database(db_id)
    with pytest.raises(CosmosResourceExistsError) as e:
        client.create_database(db_id)
    assert e.value.status_code == 409


def test_read_missing_database_is_404(client):
    with pytest.raises(CosmosResourceNotFoundError) as e:
        client.get_database_client(uid("missing")).read()
    assert e.value.status_code == 404


def test_delete_database_then_read_is_404(client, db_id):
    client.create_database(db_id)
    client.delete_database(db_id)
    with pytest.raises(CosmosResourceNotFoundError):
        client.get_database_client(db_id).read()


def test_delete_missing_database_is_404(client):
    with pytest.raises(CosmosResourceNotFoundError):
        client.delete_database(uid("missing"))


def test_create_database_if_not_exists_is_idempotent(client, db_id):
    first = client.create_database_if_not_exists(db_id)
    second = client.create_database_if_not_exists(db_id)
    assert first.id == second.id == db_id


def test_list_databases_contains_created(client):
    ids = [uid("db") for _ in range(2)]
    for i in ids:
        client.create_database(i)
    listed = {d["id"] for d in client.list_databases()}
    assert set(ids) <= listed


@pytest.mark.not_on_vnext(reason="vnext ignores WHERE in database queries")
def test_query_databases_by_id_with_parameters(client, db_id):
    client.create_database(db_id)
    client.create_database(uid("other"))
    found = list(client.query_databases({
        "query": "SELECT * FROM root r WHERE r.id=@id",
        "parameters": [{"name": "@id", "value": db_id}],
    }))
    assert [d["id"] for d in found] == [db_id]


@pytest.mark.not_on_vnext(reason="vnext ignores WHERE in database queries")
def test_query_databases_no_match_is_empty(client):
    assert list(client.query_databases('SELECT * FROM root r WHERE r.id="nope-nope"')) == []


@pytest.mark.parametrize("bad", ["id/slash", "id\\back", "id?q", "id#hash"])
def test_database_id_with_illegal_character_is_rejected_by_sdk(client, bad):
    # Client-side validation in the SDK (ValueError), as in Microsoft's test_crud_database.
    with pytest.raises(ValueError, match="Id contains illegal chars"):
        client.create_database(bad)


def test_database_id_ending_with_space_is_rejected_by_sdk(client):
    with pytest.raises(ValueError, match="ends with a space"):
        client.create_database("trailing ")


@pytest.mark.not_on_vnext(reason="vnext offers outlive deleted resources and are matched to reused ids, so throughput reads are unreliable")
def test_database_manual_throughput_read_and_replace(client, db_id):
    db = client.create_database(db_id, offer_throughput=1000)
    assert db.get_throughput().offer_throughput == 1000
    db.replace_throughput(2000)
    assert db.get_throughput().offer_throughput == 2000


@pytest.mark.not_on_vnext(reason="vnext has no autoscale offers")
def test_database_autoscale_throughput(client, db_id):
    db = client.create_database(
        db_id, offer_throughput=ThroughputProperties(auto_scale_max_throughput=5000, auto_scale_increment_percent=2)
    )
    t = db.get_throughput()
    assert t.auto_scale_max_throughput == 5000
    assert t.auto_scale_increment_percent == 2
    assert t.offer_throughput is None
    db.replace_throughput(ThroughputProperties(auto_scale_max_throughput=7000, auto_scale_increment_percent=20))
    assert db.get_throughput().auto_scale_max_throughput == 7000


@pytest.mark.not_on_vnext(reason="vnext does not validate autoscale throughput")
def test_create_database_with_negative_autoscale_is_400(client, db_id):
    with pytest.raises(CosmosHttpResponseError) as e:
        client.create_database(
            db_id, offer_throughput=ThroughputProperties(auto_scale_max_throughput=-200, auto_scale_increment_percent=0)
        )
    assert e.value.status_code == 400
