"""Session tokens, activity ids, etags and response hooks as the SDK sees them."""

from azure.cosmos import PartitionKey, documents

from cosmosutil import uid


def headers(c):
    return c.client_connection.last_response_headers


def test_last_response_headers_after_create_have_session_activity_and_etag(container):
    created = container.create_item({"id": "a", "pk": "x"})
    h = headers(container)
    assert h["x-ms-session-token"]
    assert h["x-ms-activity-id"]
    assert h["etag"] == created["_etag"]


def test_session_token_has_partition_range_prefix(container):
    container.create_item({"id": "a", "pk": "x"})
    token = headers(container)["x-ms-session-token"]
    pkrange, _, vector = token.partition(":")
    assert pkrange.isdigit() and vector


def test_session_token_advances_after_a_write(container):
    container.create_item({"id": "a", "pk": "x"})
    first = headers(container)["x-ms-session-token"]
    container.create_item({"id": "b", "pk": "x"})
    second = headers(container)["x-ms-session-token"]
    assert first != second


def test_request_with_explicit_session_token_round_trips(container):
    container.create_item({"id": "a", "pk": "x", "v": 1})
    token = headers(container)["x-ms-session-token"]
    assert container.read_item("a", "x", session_token=token)["v"] == 1
    assert list(container.query_items("SELECT VALUE c.v FROM c", partition_key="x", session_token=token)) == [1]


def test_write_with_session_token_is_accepted(container):
    container.create_item({"id": "a", "pk": "x"})
    token = headers(container)["x-ms-session-token"]
    container.upsert_item({"id": "a", "pk": "x", "v": 2}, session_token=token)
    assert container.read_item("a", "x")["v"] == 2


def test_client_with_session_consistency_reads_its_own_writes(make_client):
    c = make_client(consistency_level=documents.ConsistencyLevel.Session)
    db = c.create_database(uid("db"))
    ct = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    for i in range(5):
        ct.create_item({"id": f"i{i}", "pk": "x"})
        assert ct.read_item(f"i{i}", "x")["id"] == f"i{i}"


def test_client_with_eventual_consistency_works(make_client):
    c = make_client(consistency_level=documents.ConsistencyLevel.Eventual)
    db = c.create_database(uid("db"))
    ct = db.create_container(id=uid("c"), partition_key=PartitionKey(path="/pk"))
    ct.create_item({"id": "a", "pk": "x"})
    assert ct.read_item("a", "x")["id"] == "a"


def test_activity_id_differs_between_requests(container):
    container.create_item({"id": "a", "pk": "x"})
    a1 = headers(container)["x-ms-activity-id"]
    container.read_item("a", "x")
    assert headers(container)["x-ms-activity-id"] != a1


def test_request_charge_is_numeric_on_every_item_operation(container):
    seen = []
    hook = lambda h, b: seen.append(float(h["x-ms-request-charge"]))
    container.create_item({"id": "a", "pk": "x"}, response_hook=hook)
    container.read_item("a", "x", response_hook=hook)
    container.upsert_item({"id": "a", "pk": "x", "v": 1}, response_hook=hook)
    container.delete_item("a", "x", response_hook=hook)
    assert len(seen) == 4 and all(v >= 0 for v in seen)


def test_response_hook_gets_body_of_create(container):
    got = []
    container.create_item({"id": "a", "pk": "x"}, response_hook=lambda h, b: got.append(b))
    assert got[0]["id"] == "a"


def test_database_account_read_exposes_consistency_policy(client):
    acct = client.get_database_account()
    assert acct.ConsistencyPolicy["defaultConsistencyLevel"] in ("Strong", "BoundedStaleness", "Session", "Eventual", "ConsistentPrefix")


def test_user_agent_and_version_headers_are_accepted(make_client):
    c = make_client(user_agent_suffix="azemu-tests")
    assert c.get_database_account().DatabasesLink == "/dbs/"
