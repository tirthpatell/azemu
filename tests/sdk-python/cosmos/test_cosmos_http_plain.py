"""The SDK against a plain http:// endpoint, the way many developers will use the emulator."""

import pytest
from azure.cosmos import PartitionKey

from cosmosutil import new_client, uid


@pytest.fixture
def plain_client(http_endpoint):
    try:
        c = new_client(http_endpoint, None)
    except ValueError as e:  # the SDK refuses http endpoints
        pytest.skip(f"SDK refuses http endpoints: {e}")
    yield c
    c.close()


@pytest.fixture
def plain_container(plain_client):
    db = plain_client.create_database(uid("db"))
    return db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))


def test_http_client_reads_the_account(plain_client):
    assert plain_client.get_database_account().DatabasesLink == "/dbs/"


def test_http_item_round_trip(plain_container):
    plain_container.create_item({"id": "a", "pk": "x", "v": 1})
    assert plain_container.read_item("a", "x")["v"] == 1


def test_http_query_and_delete(plain_container):
    for i in range(3):
        plain_container.create_item({"id": f"i{i}", "pk": "x", "n": i})
    got = list(plain_container.query_items("SELECT VALUE c.n FROM c ORDER BY c.n", enable_cross_partition_query=True))
    assert got == [0, 1, 2]
    plain_container.delete_item("i0", "x")
    assert len(list(plain_container.read_all_items())) == 2


def test_http_endpoint_redirects_follow_up_requests_to_the_same_host(plain_client, http_endpoint):
    # the account's writable location must point back at the endpoint the client used
    acct = plain_client.client_connection._global_endpoint_manager._database_account_cache
    host = http_endpoint.split("//", 1)[1].rstrip("/")
    assert all(host in loc["databaseAccountEndpoint"] for loc in acct._WritableLocations)
