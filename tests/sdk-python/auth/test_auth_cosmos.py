"""Cosmos DB with Entra tokens: its own roles, and three kinds of 403 (`docs/strict-auth.md`, as Azure answered in November 2026).
`5300`: not available with a token at all; `5301`: no metadata role; `5302`: no role for this item, query or stored procedure."""

import urllib.parse

import pytest
import requests
from azure.cosmos import CosmosClient, PartitionKey
from azure.cosmos.exceptions import CosmosHttpResponseError, CosmosResourceNotFoundError

from authutil import unique


def key_client(w):
    return client(w, w.cosmos_key())


def client(w, credential):
    kw = dict(retry_total=1, retry_throttle_total=1, retry_backoff_max=1, connection_timeout=10, read_timeout=60)
    if not w.azure:
        from azure.cosmos.documents import SSLConfiguration

        kw.update(connection_verify=w.emu.ca_file, ssl_config=SSLConfiguration())
    return CosmosClient(w.cosmos_url(), credential=credential, **kw)


@pytest.fixture(scope="module")
def data(world):
    """A database with a container and one item, made with the account key (no token may make them)."""
    c = key_client(world)
    name = unique("db")
    db = c.create_database(name)
    cont = db.create_container("c1", PartitionKey("/pk"))
    cont.create_item({"id": "i1", "pk": "a", "v": 1})
    yield name
    try:
        c.delete_database(name)
    except Exception:
        pass


def refused(fn, status, sub):
    with pytest.raises(CosmosHttpResponseError) as e:
        fn()
    assert e.value.status_code == status, e.value
    assert e.value.sub_status == sub, e.value
    return str(e.value)


def test_norole_cannot_even_build_a_client_the_account_read_is_refused(world, norole):
    text = refused(lambda: client(world, norole), 403, 5301)
    assert "Request blocked by Auth" in text
    assert "does not have required RBAC permissions to perform action [Microsoft.DocumentDB/databaseAccounts/readMetadata]" in text


def test_data_reader_reads_queries_and_misses_with_404(world, reader, data):
    cont = client(world, reader).get_database_client(data).get_container_client("c1")
    assert cont.read_item("i1", "a")["v"] == 1
    assert [i["id"] for i in cont.query_items("select * from c", enable_cross_partition_query=True)] == ["i1"]
    assert [i["id"] for i in cont.query_items("select * from c", partition_key="a")] == ["i1"]
    assert [i["id"] for i in cont.read_all_items()] == ["i1"]
    with pytest.raises(CosmosResourceNotFoundError):
        cont.read_item("nothere", "a")
    assert client(world, reader).get_database_client(data).read()["id"] == data
    assert cont.read()["id"] == "c1"


def test_data_reader_writes_are_5302_with_the_action_and_the_operation_numbers(world, reader, data):
    cont = client(world, reader).get_database_client(data).get_container_client("c1")
    for call, action, op in (
        (lambda: cont.create_item({"id": "n1", "pk": "a"}), "items/create", 0),
        (lambda: cont.upsert_item({"id": "n1", "pk": "a"}), "items/upsert", 20),
        (lambda: cont.replace_item("i1", {"id": "i1", "pk": "a", "v": 2}), "items/replace", 5),
        (lambda: cont.delete_item("i1", "a"), "items/delete", 4),
    ):
        text = refused(call, 403, 5302)
        assert f"[Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers/{action}] with OperationType [{op}] and ResourceType [2]" in text
        assert "principal [" in text and "Learn more: https://aka.ms/cosmos-native-rbac" in text


def test_a_token_may_not_make_or_delete_databases_and_containers_whoever_it_is(world, main, reader, data):
    for who in (main, reader):
        c = client(world, who)
        db = c.get_database_client(data)
        for call in (
            lambda: c.create_database(unique("db")),
            lambda: c.delete_database(data),
            lambda: db.create_container(unique("c"), PartitionKey("/pk")),
            lambda: db.delete_container("c1"),
        ):
            text = refused(call, 403, 5300)
            assert "cannot be authorized by AAD token in data plane" in text


def test_data_contributor_writes_items(world, main, data):
    cont = client(world, main).get_database_client(data).get_container_client("c1")
    cont.create_item({"id": "w1", "pk": "a", "v": 1})
    cont.upsert_item({"id": "w1", "pk": "a", "v": 2})
    cont.replace_item("w1", {"id": "w1", "pk": "a", "v": 3})
    assert cont.read_item("w1", "a")["v"] == 3
    cont.delete_item("w1", "a")


@pytest.mark.parametrize("what, sub", [("audience", 5008), ("signature", 5004), ("garbage", 5003)])
def test_a_token_that_is_wrong_is_401_with_its_own_sub_status(world, main, what, sub):
    good = main.token("https://cosmos.azure.com/.default")
    token = {"audience": main.token("https://vault.azure.net/.default"), "signature": good[:-4] + ("AAAA" if not good.endswith("AAAA") else "BBBB"), "garbage": "not-a-token"}[what]
    auth = urllib.parse.quote(f"type=aad&ver=1.0&sig={token}", safe="")
    from email.utils import formatdate

    r = requests.get(
        world.cosmos_url(),
        headers={"Authorization": auth, "x-ms-version": "2018-12-31", "x-ms-date": formatdate(usegmt=True)},
        timeout=60,
        verify=world.emu.ca_file if world.emu else True,
    )
    assert r.status_code == 401
    assert r.headers["x-ms-substatus"] == str(sub)
    assert r.json()["message"].startswith("Request blocked by Auth ")
    if what == "audience":
        assert "Provided AAD token has an invalid audience. This database account accepts tokens intended for [https://" in r.json()["message"]
