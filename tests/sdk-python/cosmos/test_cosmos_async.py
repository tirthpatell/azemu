"""The same scenarios through azure.cosmos.aio (pytest-asyncio, auto mode)."""

import pytest
from azure.cosmos import PartitionKey
from azure.cosmos.aio import CosmosClient
from azure.cosmos.exceptions import (
    CosmosBatchOperationError,
    CosmosResourceExistsError,
    CosmosResourceNotFoundError,
)

from cosmosutil import KEY, async_client_kwargs, uid


@pytest.fixture
async def aclient(endpoint, ca_file):
    async with CosmosClient(endpoint, credential=KEY, **async_client_kwargs(ca_file)) as c:
        yield c


@pytest.fixture
async def acontainer(aclient):
    db = await aclient.create_database(uid("db"))
    return await db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))


async def collect(it):
    return [i async for i in it]


async def test_async_client_reads_account(aclient):
    acct = await aclient.client_connection.GetDatabaseAccount()
    assert acct.DatabasesLink == "/dbs/"


async def test_async_database_crud(aclient):
    name = uid("db")
    db = await aclient.create_database(name)
    assert (await db.read())["id"] == name
    with pytest.raises(CosmosResourceExistsError):
        await aclient.create_database(name)
    assert name in [d["id"] async for d in aclient.list_databases()]
    await aclient.delete_database(name)
    with pytest.raises(CosmosResourceNotFoundError):
        await aclient.get_database_client(name).read()


async def test_async_container_create_read_delete(aclient):
    db = await aclient.create_database(uid("db"))
    c = await db.create_container(id="c1", partition_key=PartitionKey(path="/pk"))
    props = await c.read()
    assert props["partitionKey"]["paths"] == ["/pk"] and props["_rid"]
    await db.delete_container("c1")
    with pytest.raises(CosmosResourceNotFoundError):
        await db.get_container_client("c1").read()


async def test_async_item_crud(acontainer):
    created = await acontainer.create_item({"id": "a", "pk": "x", "v": 1})
    assert created["_etag"] and created["_ts"]
    assert (await acontainer.read_item("a", "x"))["v"] == 1
    await acontainer.upsert_item({"id": "a", "pk": "x", "v": 2})
    assert (await acontainer.replace_item("a", {"id": "a", "pk": "x", "v": 3}))["v"] == 3
    await acontainer.delete_item("a", "x")
    with pytest.raises(CosmosResourceNotFoundError):
        await acontainer.read_item("a", "x")


async def test_async_duplicate_item_is_409(acontainer):
    await acontainer.create_item({"id": "a", "pk": "x"})
    with pytest.raises(CosmosResourceExistsError):
        await acontainer.create_item({"id": "a", "pk": "x"})


async def test_async_read_all_items(acontainer):
    for i in range(4):
        await acontainer.create_item({"id": f"i{i}", "pk": f"p{i % 2}"})
    assert sorted(d["id"] for d in await collect(acontainer.read_all_items())) == ["i0", "i1", "i2", "i3"]


async def test_async_cross_partition_query_with_order_by(acontainer):
    for i, p in enumerate(["a", "b", "c", "a"]):
        await acontainer.create_item({"id": f"i{i}", "pk": p, "n": 10 - i})
    got = await collect(acontainer.query_items("SELECT VALUE c.n FROM c ORDER BY c.n", ))
    assert got == [7, 8, 9, 10]


async def test_async_single_partition_query_with_parameters(acontainer):
    for i in range(5):
        await acontainer.create_item({"id": f"i{i}", "pk": "x", "n": i})
    got = await collect(acontainer.query_items(
        "SELECT VALUE c.id FROM c WHERE c.n > @n", parameters=[{"name": "@n", "value": 2}], partition_key="x"))
    assert sorted(got) == ["i3", "i4"]


async def test_async_aggregate_query(acontainer):
    for i in range(5):
        await acontainer.create_item({"id": f"i{i}", "pk": "x", "n": i})
    assert await collect(acontainer.query_items("SELECT VALUE SUM(c.n) FROM c", partition_key="x")) == [10]


async def test_async_paging_with_continuation_token(acontainer):
    for i in range(5):
        await acontainer.create_item({"id": f"i{i}", "pk": "x"})
    pager = acontainer.query_items("SELECT c.id FROM c", partition_key="x", max_item_count=2).by_page()
    first = [i async for i in await pager.__anext__()]
    token = pager.continuation_token
    assert len(first) == 2 and token
    pager2 = acontainer.query_items("SELECT c.id FROM c", partition_key="x", max_item_count=2).by_page(token)
    rest = [i async for page in pager2 async for i in page]
    assert len(rest) == 3 and not ({i["id"] for i in first} & {i["id"] for i in rest})


async def test_async_patch(acontainer):
    await acontainer.create_item({"id": "a", "pk": "x", "n": 1})
    got = await acontainer.patch_item("a", "x", [{"op": "incr", "path": "/n", "value": 4}, {"op": "add", "path": "/c", "value": "red"}])
    assert got["n"] == 5 and got["c"] == "red"


async def test_async_batch_success_and_rollback(acontainer):
    res = await acontainer.execute_item_batch(
        [("create", ({"id": "a", "pk": "x"},)), ("create", ({"id": "b", "pk": "x"},))], partition_key="x")
    assert [r["statusCode"] for r in res] == [201, 201]
    with pytest.raises(CosmosBatchOperationError) as e:
        await acontainer.execute_item_batch(
            [("create", ({"id": "c", "pk": "x"},)), ("create", ({"id": "a", "pk": "x"},))], partition_key="x")
    assert e.value.status_code == 409 and e.value.error_index == 1
    assert sorted(d["id"] for d in await collect(acontainer.read_all_items())) == ["a", "b"]


async def test_async_change_feed(acontainer):
    for i in ("d1", "d2"):
        await acontainer.create_item({"id": i, "pk": "x"})
    got = await collect(acontainer.query_items_change_feed(is_start_from_beginning=True))
    assert [d["id"] for d in got] == ["d1", "d2"]


async def test_async_scripts_definitions(acontainer):
    s = await acontainer.scripts.create_stored_procedure({"id": "sp", "serverScript": "function() {}"})
    assert s["body"] == "function() {}"
    assert [x["id"] async for x in acontainer.scripts.list_stored_procedures()] == ["sp"]
