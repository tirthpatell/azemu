"""Users and permissions (definitions only; resource tokens are not exercised)."""

import pytest
from azure.cosmos import documents
from azure.cosmos.exceptions import CosmosResourceExistsError, CosmosResourceNotFoundError

from cosmosutil import uid


def test_create_user(db):
    u = db.create_user(body={"id": "alice"})
    assert u.id == "alice"
    props = u.read()
    assert props["id"] == "alice" and props["_rid"] and props["_etag"]


def test_duplicate_user_is_409(db):
    db.create_user(body={"id": "alice"})
    with pytest.raises(CosmosResourceExistsError):
        db.create_user(body={"id": "alice"})


def test_list_users(db):
    for i in ("a", "b", "c"):
        db.create_user(body={"id": i})
    assert sorted(u["id"] for u in db.list_users()) == ["a", "b", "c"]


@pytest.mark.not_on_vnext(reason="vnext ignores WHERE in user queries")
def test_query_users_by_id(db):
    db.create_user(body={"id": "a"})
    db.create_user(body={"id": "b"})
    got = list(db.query_users(query="SELECT * FROM root r WHERE r.id=@id", parameters=[{"name": "@id", "value": "b"}]))
    assert [u["id"] for u in got] == ["b"]


@pytest.mark.not_on_vnext(reason="vnext keeps the old user id readable after a rename")
def test_replace_user_renames_it(db):
    u = db.create_user(body={"id": "old"})
    props = u.read()
    props["id"] = "new"
    replaced = db.replace_user("old", props)
    assert replaced.id == "new"
    with pytest.raises(CosmosResourceNotFoundError):
        db.get_user_client("old").read()
    assert db.get_user_client("new").read()["id"] == "new"


def test_upsert_user_creates_then_replaces(db):
    u = db.upsert_user(body={"id": "u1"})
    again = db.upsert_user(u.read())
    assert again.id == "u1"
    assert len(list(db.list_users())) == 1


def test_delete_user(db):
    db.create_user(body={"id": "u1"})
    db.delete_user("u1")
    with pytest.raises(CosmosResourceNotFoundError):
        db.get_user_client("u1").read()


def test_delete_missing_user_is_404(db):
    with pytest.raises(CosmosResourceNotFoundError):
        db.delete_user("nope")


def test_users_are_per_database(client, db):
    other = client.create_database(uid("db"))
    other.create_user(body={"id": "theirs"})
    assert list(db.list_users()) == []


def _perm(container, pid="p1", mode=documents.PermissionMode.Read, **kw):
    d = {"id": pid, "permissionMode": mode, "resource": container.container_link}
    d.update(kw)
    return d


@pytest.mark.not_on_vnext(reason="vnext lower-cases permissionMode")
def test_create_permission_returns_a_token(db, container):
    u = db.create_user(body={"id": "u1"})
    p = u.create_permission(_perm(container))
    assert p.id == "p1"
    assert p.properties["permissionMode"] == "Read"
    assert p.properties["_token"]


@pytest.mark.not_on_vnext(reason="vnext lower-cases permissionMode")
def test_permission_get_list_delete(db, container):
    u = db.create_user(body={"id": "u1"})
    u.create_permission(_perm(container, "p1"))
    u.create_permission(_perm(container, "p2", documents.PermissionMode.All))
    assert sorted(p["id"] for p in u.list_permissions()) == ["p1", "p2"]
    assert u.get_permission("p2").properties["permissionMode"] == "All"
    u.delete_permission("p1")
    with pytest.raises(CosmosResourceNotFoundError):
        u.get_permission("p1")


def test_duplicate_permission_is_409(db, container):
    u = db.create_user(body={"id": "u1"})
    u.create_permission(_perm(container))
    with pytest.raises(CosmosResourceExistsError):
        u.create_permission(_perm(container))


@pytest.mark.not_on_vnext(reason="vnext lower-cases permissionMode")
def test_replace_permission_changes_mode(db, container):
    u = db.create_user(body={"id": "u1"})
    u.create_permission(_perm(container))
    got = u.replace_permission("p1", _perm(container, mode=documents.PermissionMode.All))
    assert got.properties["permissionMode"] == "All"


def test_upsert_permission(db, container):
    u = db.create_user(body={"id": "u1"})
    u.upsert_permission(_perm(container))
    u.upsert_permission(_perm(container, mode=documents.PermissionMode.All))
    assert len(list(u.list_permissions())) == 1
