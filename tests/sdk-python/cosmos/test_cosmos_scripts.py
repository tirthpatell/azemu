"""Stored procedures, triggers and user defined functions (definitions only)."""

import pytest
from azure.cosmos import documents
from azure.cosmos.exceptions import CosmosResourceExistsError, CosmosResourceNotFoundError

from cosmosutil import uid


# ---- stored procedures -------------------------------------------------------
def test_sproc_create_returns_body(container):
    s = container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {var x = 10;}"})
    assert s["id"] == "sp1" and s["body"] == "function() {var x = 10;}"
    assert s["_rid"] and s["_etag"]


def test_sproc_get(container):
    container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {}"})
    assert container.scripts.get_stored_procedure("sp1")["id"] == "sp1"


def test_sproc_list_grows_with_creates(container):
    before = len(list(container.scripts.list_stored_procedures()))
    for i in range(2):
        container.scripts.create_stored_procedure({"id": f"sp{i}", "serverScript": "function() {}"})
    assert len(list(container.scripts.list_stored_procedures())) == before + 2


@pytest.mark.not_on_vnext(reason="vnext has no stored procedures")
def test_sproc_query_by_id(container):
    container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {}"})
    container.scripts.create_stored_procedure({"id": "sp2", "serverScript": "function() {}"})
    got = list(container.scripts.query_stored_procedures(query="SELECT * FROM root r WHERE r.id=@id", parameters=[{"name": "@id", "value": "sp1"}]))
    assert [s["id"] for s in got] == ["sp1"]


def test_sproc_replace(container):
    container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {var x = 10;}"})
    got = container.scripts.replace_stored_procedure("sp1", {"id": "sp1", "serverScript": "function() {var x = 20;}"})
    assert got["body"] == "function() {var x = 20;}"
    assert container.scripts.get_stored_procedure("sp1")["body"] == "function() {var x = 20;}"


def test_sproc_delete_then_get_is_404(container):
    container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {}"})
    container.scripts.delete_stored_procedure("sp1")
    with pytest.raises(CosmosResourceNotFoundError):
        container.scripts.get_stored_procedure("sp1")


def test_sproc_duplicate_is_409(container):
    container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {}"})
    with pytest.raises(CosmosResourceExistsError):
        container.scripts.create_stored_procedure({"id": "sp1", "serverScript": "function() {}"})


def test_sproc_delete_missing_is_404(container):
    with pytest.raises(CosmosResourceNotFoundError):
        container.scripts.delete_stored_procedure("nope")


def test_sproc_execute(container):
    container.scripts.create_stored_procedure(
        {"id": "hello", "serverScript": "function(n) { getContext().getResponse().setBody('hello ' + n); }"}
    )
    assert container.scripts.execute_stored_procedure("hello", partition_key="x", params=["world"]) == "hello world"


# ---- triggers ---------------------------------------------------------------
def _trigger(i="t1", body="function() {var x = 10;}", **kw):
    d = {"id": i, "serverScript": body, "triggerType": documents.TriggerType.Pre, "triggerOperation": documents.TriggerOperation.All}
    d.update(kw)
    return d


@pytest.mark.not_on_vnext(reason="vnext lower-cases triggerType/triggerOperation")
def test_trigger_create_returns_definition(container):
    t = container.scripts.create_trigger(_trigger())
    assert t["id"] == "t1" and t["body"] == "function() {var x = 10;}"
    assert t["triggerType"] == "Pre" and t["triggerOperation"] == "All"


@pytest.mark.not_on_vnext(reason="vnext lower-cases triggerType/triggerOperation")
def test_trigger_get_and_list(container):
    container.scripts.create_trigger(_trigger("t1"))
    container.scripts.create_trigger(_trigger("t2", triggerType=documents.TriggerType.Post, triggerOperation=documents.TriggerOperation.Delete))
    assert container.scripts.get_trigger("t2")["triggerType"] == "Post"
    assert sorted(t["id"] for t in container.scripts.list_triggers()) == ["t1", "t2"]


@pytest.mark.not_on_vnext(reason="vnext has no triggers")
def test_trigger_query_by_id(container):
    container.scripts.create_trigger(_trigger("t1"))
    container.scripts.create_trigger(_trigger("t2"))
    got = list(container.scripts.query_triggers(query="SELECT * FROM root r WHERE r.id=@id", parameters=[{"name": "@id", "value": "t2"}]))
    assert [t["id"] for t in got] == ["t2"]


def test_trigger_replace(container):
    container.scripts.create_trigger(_trigger())
    got = container.scripts.replace_trigger("t1", _trigger(body="function() {var x = 20;}"))
    assert got["body"] == "function() {var x = 20;}"


def test_trigger_delete_then_delete_again_is_404(container):
    container.scripts.create_trigger(_trigger())
    container.scripts.delete_trigger("t1")
    with pytest.raises(CosmosResourceNotFoundError):
        container.scripts.delete_trigger("t1")


def test_trigger_duplicate_is_409(container):
    container.scripts.create_trigger(_trigger())
    with pytest.raises(CosmosResourceExistsError):
        container.scripts.create_trigger(_trigger())


def test_triggers_are_per_container(make_container):
    a, b = make_container("/pk"), make_container("/pk")
    a.scripts.create_trigger(_trigger())
    assert list(b.scripts.list_triggers()) == []


# ---- user defined functions -------------------------------------------------
def test_udf_create_get_list(container):
    u = container.scripts.create_user_defined_function({"id": "sample udf", "body": "function() {var x = 10;}"})
    assert u["id"] == "sample udf" and u["body"] == "function() {var x = 10;}"
    assert container.scripts.get_user_defined_function("sample udf")["id"] == "sample udf"
    assert [x["id"] for x in container.scripts.list_user_defined_functions()] == ["sample udf"]


@pytest.mark.not_on_vnext(reason="vnext has no user defined functions")
def test_udf_query_by_id(container):
    container.scripts.create_user_defined_function({"id": "u1", "body": "function() {}"})
    container.scripts.create_user_defined_function({"id": "u2", "body": "function() {}"})
    got = list(container.scripts.query_user_defined_functions(query="SELECT * FROM root r WHERE r.id=@id", parameters=[{"name": "@id", "value": "u1"}]))
    assert [x["id"] for x in got] == ["u1"]


def test_udf_replace(container):
    container.scripts.create_user_defined_function({"id": "u1", "body": "function() {var x = 10;}"})
    got = container.scripts.replace_user_defined_function("u1", {"id": "u1", "body": "function() {var x = 20;}"})
    assert got["body"] == "function() {var x = 20;}"


def test_udf_delete_then_get_is_404(container):
    container.scripts.create_user_defined_function({"id": "u1", "body": "function() {}"})
    container.scripts.delete_user_defined_function("u1")
    with pytest.raises(CosmosResourceNotFoundError):
        container.scripts.get_user_defined_function("u1")


def test_udf_duplicate_is_409(container):
    container.scripts.create_user_defined_function({"id": "u1", "body": "function() {}"})
    with pytest.raises(CosmosResourceExistsError):
        container.scripts.create_user_defined_function({"id": "u1", "body": "function() {}"})


def test_udf_used_in_query(container):
    container.scripts.create_user_defined_function({"id": "double", "body": "function(x) { return x * 2; }"})
    container.create_item({"id": "a", "pk": "x", "n": 4})
    got = list(container.query_items("SELECT VALUE udf.double(c.n) FROM c", partition_key="x"))
    assert got == [8]
