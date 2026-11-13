"""Transactional batch (execute_item_batch). Scenarios follow Microsoft's test_transactional_batch."""

import uuid

import pytest
from azure.cosmos import PartitionKey
from azure.cosmos.exceptions import CosmosBatchOperationError, CosmosHttpResponseError

from cosmosutil import uid


@pytest.fixture
def co(make_container):
    return make_container("/company")


def doc(i=None, **kw):
    d = {"id": i or str(uuid.uuid4()), "company": "Microsoft"}
    d.update(kw)
    return d


def test_batch_create_many(co):
    ops = [("create", (doc(f"item{i}"),)) for i in range(100)]
    res = co.execute_item_batch(batch_operations=ops, partition_key="Microsoft")
    assert len(res) == 100
    assert all(r["statusCode"] == 201 for r in res)
    assert len(list(co.read_all_items())) == 100


def test_batch_results_carry_status_and_resource_body(co):
    [r] = co.execute_item_batch([("create", (doc("a", v=1),))], partition_key="Microsoft")
    assert r["resourceBody"]["v"] == 1
    assert r["statusCode"] == 201


def test_batch_duplicate_create_fails_whole_batch(co):
    i = str(uuid.uuid4())
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch([("create", (doc(i),)), ("create", (doc(i),))], partition_key="Microsoft")
    assert e.value.status_code == 409
    assert e.value.error_index == 1
    res = e.value.operation_responses
    assert [r["statusCode"] for r in res] == [424, 409]


def test_batch_rolls_back_earlier_operations_on_failure(co):
    good = str(uuid.uuid4())
    dup = str(uuid.uuid4())
    co.create_item(doc(dup))
    with pytest.raises(CosmosBatchOperationError):
        co.execute_item_batch([("create", (doc(good),)), ("create", (doc(dup),))], partition_key="Microsoft")
    assert [d["id"] for d in co.read_all_items()] == [dup]


def test_batch_item_with_different_partition_key_is_400(co):
    ops = [("create", (doc(),)), ("create", (doc(company="Not-Microsoft"),))]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 400
    assert e.value.error_index == 1
    assert [r["statusCode"] for r in e.value.operation_responses] == [424, 400]


def test_batch_item_without_partition_key_is_400(co):
    ops = [("create", (doc(),)), ("create", ({"id": str(uuid.uuid4()), "name": "Simon"},))]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 400 and e.value.error_index == 1


def test_batch_read(co):
    ops = []
    for i in range(10):
        co.create_item(doc(f"item{i}"))
        ops.append(("read", (f"item{i}",)))
    res = co.execute_item_batch(ops, partition_key="Microsoft")
    assert [r["statusCode"] for r in res] == [200] * 10
    assert res[3]["resourceBody"]["id"] == "item3"


def test_batch_read_missing_item_is_404_at_index_0(co):
    ops = [("read", (str(uuid.uuid4()),)), ("read", (str(uuid.uuid4()),))]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 404 and e.value.error_index == 0
    assert [r["statusCode"] for r in e.value.operation_responses] == [404, 424]


def test_batch_replace(co):
    ops = [("create", (doc("new-item"),)), ("replace", ("new-item", doc("new-item", message="item was replaced")))]
    res = co.execute_item_batch(ops, partition_key="Microsoft")
    assert res[1]["resourceBody"]["message"] == "item was replaced"


def test_batch_replace_missing_item_is_404(co):
    ops = [("replace", ("no-item", doc("no-item", message="x")))]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 404 and e.value.error_index == 0


def test_batch_replace_with_wrong_etag_is_412(co):
    i = str(uuid.uuid4())
    ops = [
        ("upsert", (doc(i),)),
        ("replace", (i, doc(i, message="x")), {"if_match_etag": "some-tag"}),
        ("replace", (i, doc(i, message="y")), {"if_none_match_etag": "some-tag"}),
    ]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 412 and e.value.error_index == 1
    assert [r["statusCode"] for r in e.value.operation_responses] == [424, 412, 424]


def test_batch_upsert(co):
    i = str(uuid.uuid4())
    ops = [("upsert", (doc(i),)), ("upsert", (doc(i, message="item was upsert"),)), ("upsert", (doc(),))]
    res = co.execute_item_batch(ops, partition_key="Microsoft")
    assert len(res) == 3 and res[1]["resourceBody"]["message"] == "item was upsert"
    assert co.read_item(i, "Microsoft")["message"] == "item was upsert"


def test_batch_patch_all_operation_kinds(co):
    i = str(uuid.uuid4())
    base = doc(i, city="Seattle", port=9000, remove_path=True, move_path="yes", set_path=1)
    ops = [
        ("upsert", (base,)),
        ("patch", (i, [
            {"op": "add", "path": "/favorite_color", "value": "red"},
            {"op": "remove", "path": "/remove_path"},
            {"op": "replace", "path": "/city", "value": "Redmond"},
            {"op": "set", "path": "/set_path", "value": 0},
            {"op": "incr", "path": "/port", "value": 5},
            {"op": "move", "from": "/move_path", "path": "/moved_path"},
        ])),
    ]
    body = co.execute_item_batch(ops, partition_key="Microsoft")[1]["resourceBody"]
    assert body["favorite_color"] == "red" and "remove_path" not in body
    assert body["city"] == "Redmond" and body["set_path"] == 0 and body["port"] == 9005
    assert "move_path" not in body and body["moved_path"] == "yes"


def test_batch_patch_with_false_filter_is_412(co):
    i = str(uuid.uuid4())
    ops = [
        ("upsert", (doc(i, set_path=1),)),
        ("patch", (i, [{"op": "add", "path": "/c", "value": "red"}]), {"filter_predicate": "from c where c.set_path = 0"}),
    ]
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 412 and e.value.error_index == 1


def test_batch_patch_with_true_filter_succeeds(co):
    i = str(uuid.uuid4())
    ops = [
        ("upsert", (doc(i, set_path=1),)),
        ("patch", (i, [{"op": "add", "path": "/c", "value": "red"}]), {"filter_predicate": "from c where c.set_path = 1"}),
    ]
    assert len(co.execute_item_batch(ops, partition_key="Microsoft")) == 2


def test_batch_delete(co):
    ids = [str(uuid.uuid4()) for _ in range(10)]
    co.execute_item_batch([("create", (doc(i),)) for i in ids], partition_key="Microsoft")
    assert len(list(co.read_all_items())) == 10
    res = co.execute_item_batch([("delete", (i,)) for i in ids], partition_key="Microsoft")
    assert len(res) == 10
    assert list(co.read_all_items()) == []


def test_batch_delete_missing_is_404(co):
    with pytest.raises(CosmosBatchOperationError) as e:
        co.execute_item_batch([("delete", ("new-item",)), ("delete", ("new-item",))], partition_key="Microsoft")
    assert e.value.status_code == 404 and e.value.error_index == 0
    assert [r["statusCode"] for r in e.value.operation_responses] == [404, 424]


def test_batch_mixed_operations_in_one_transaction(co):
    co.upsert_item(doc("read_item"))
    co.upsert_item(doc("replace_item", value=0))
    co.upsert_item(doc("patch_item"))
    co.upsert_item(doc("delete_item"))
    ops = [
        ("create", (doc("create_item"),)),
        ("replace", ("replace_item", doc("replace_item", value=True))),
        ("upsert", (doc("upsert_item"),)),
        ("patch", ("patch_item", [{"op": "add", "path": "/favorite_color", "value": "red"}])),
        ("read", ("read_item",)),
        ("delete", ("delete_item",)),
    ]
    res = co.execute_item_batch(ops, partition_key="Microsoft")
    assert len(res) == 6
    assert sorted(d["id"] for d in co.read_all_items()) == sorted(
        ["read_item", "replace_item", "patch_item", "create_item", "upsert_item"]
    )


def test_batch_create_then_read_sees_own_write(co):
    ops = [("create", (doc("x", v=7),)), ("read", ("x",))]
    res = co.execute_item_batch(ops, partition_key="Microsoft")
    assert res[1]["resourceBody"]["v"] == 7


@pytest.mark.not_on_vnext(reason="vnext accepts an empty batch")
def test_batch_empty_is_400(co):
    with pytest.raises(CosmosHttpResponseError) as e:
        co.execute_item_batch(batch_operations=[], partition_key="Microsoft")
    assert e.value.status_code == 400
    assert "Batch request has no operations." in e.value.message


@pytest.mark.not_on_vnext(reason="vnext words this error differently (status is checked by the next test)")
def test_batch_more_than_100_operations_is_400(co):
    ops = [("create", (doc(f"item{i}"),)) for i in range(101)]
    with pytest.raises(CosmosHttpResponseError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 400
    assert "Batch request has more operations than what is supported." in e.value.message


def test_batch_on_hierarchical_partition_key(make_container):
    c = make_container(PartitionKey(path=["/state", "/city", "/zipcode"], kind="MultiHash"))
    mk = lambda i, **kw: {"id": i, "state": "WA", "city": "Redmond", "zipcode": "98052", **kw}
    c.upsert_item(mk("a"))
    c.upsert_item(mk("b"))
    ops = [("create", (mk(uid("n")),)), ("replace", ("a", mk("a", replaced=True))), ("read", ("b",))]
    res = c.execute_item_batch(ops, partition_key=["WA", "Redmond", "98052"])
    assert len(res) == 3
    assert c.read_item("a", ["WA", "Redmond", "98052"])["replaced"] is True


@pytest.mark.not_on_vnext(reason="vnext does not validate the batch partition key")
def test_batch_with_incomplete_hierarchical_key_is_400(make_container):
    c = make_container(PartitionKey(path=["/state", "/city", "/zipcode"], kind="MultiHash"))
    ops = [("upsert", ({"id": "a", "state": "WA", "city": "Redmond", "zipcode": "98052"},))]
    with pytest.raises(CosmosHttpResponseError) as e:
        c.execute_item_batch(ops, partition_key=["WA", "Redmond"])
    assert e.value.status_code == 400


def test_batch_more_than_100_operations_is_400_status_only(co):
    ops = [("create", (doc(f"item{i}"),)) for i in range(101)]
    with pytest.raises(CosmosHttpResponseError) as e:
        co.execute_item_batch(ops, partition_key="Microsoft")
    assert e.value.status_code == 400
