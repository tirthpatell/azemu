"""Shared access signatures: account SAS and table SAS (permissions, expiry, ranges, stored policy)."""

import pytest
from azure.core.credentials import AzureNamedKeyCredential, AzureSasCredential
from azure.core.exceptions import HttpResponseError, ResourceNotFoundError
from azure.data.tables import (
    TableAccessPolicy,
    TableClient,
    TableSasPermissions,
    TableServiceClient,
    generate_table_sas,
)

from tableutil import code, future, past

SAS_DENIED = ("AuthorizationPermissionMismatch", "AuthorizationFailure", "AuthorizationResourceTypeMismatch")
SAS_BAD = ("AuthenticationFailed", "AuthorizationFailure")


@pytest.fixture
def named_key(account_name, account_key):
    return AzureNamedKeyCredential(account_name, account_key)


@pytest.fixture
def seeded(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    return table


@pytest.fixture
def sas_client(service, table_name):
    """Factory: a TableClient authenticated with the given SAS token."""

    def make(token):
        return TableClient(endpoint=service.url, table_name=table_name, credential=AzureSasCredential(token))

    return make


def table_sas(named_key, table_name, perms="r", **kwargs):
    kwargs.setdefault("expiry", future())
    return generate_table_sas(named_key, table_name, permission=TableSasPermissions.from_string(perms), **kwargs)


# -- account SAS ------------------------------------------------------------------------


def test_account_sas_with_read_can_query_and_get(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="r"))
    assert [e["RowKey"] for e in client.query_entities("PartitionKey eq 'p'")] == ["r"]
    assert client.get_entity("p", "r")["v"] == 1


def test_account_sas_with_add_can_create_entities(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="a"))
    client.create_entity({"PartitionKey": "p", "RowKey": "new", "v": 2})
    assert seeded.get_entity("p", "new")["v"] == 2


def test_account_sas_with_update_can_update_entities(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="u"))
    client.update_entity({"PartitionKey": "p", "RowKey": "r", "v": 5})
    assert seeded.get_entity("p", "r")["v"] == 5


def test_account_sas_with_delete_can_delete_entities(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="d"))
    client.delete_entity("p", "r")
    with pytest.raises(ResourceNotFoundError):
        seeded.get_entity("p", "r")


def test_account_sas_upsert_needs_add_and_update(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="au"))
    client.upsert_entity({"PartitionKey": "p", "RowKey": "r", "v": 9})
    client.upsert_entity({"PartitionKey": "p", "RowKey": "fresh", "v": 1})
    assert seeded.get_entity("p", "r")["v"] == 9
    assert seeded.get_entity("p", "fresh")["v"] == 1


def test_account_sas_without_add_cannot_create(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="r"))
    with pytest.raises(HttpResponseError) as exc:
        client.create_entity({"PartitionKey": "p", "RowKey": "new"})
    assert exc.value.status_code == 403
    assert code(exc.value) in SAS_DENIED


def test_account_sas_without_read_cannot_query(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="a"))
    with pytest.raises(HttpResponseError) as exc:
        list(client.query_entities("PartitionKey eq 'p'"))
    assert exc.value.status_code == 403
    assert code(exc.value) in SAS_DENIED


def test_account_sas_without_update_cannot_update(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="rad"))
    with pytest.raises(HttpResponseError) as exc:
        client.update_entity({"PartitionKey": "p", "RowKey": "r", "v": 2})
    assert exc.value.status_code == 403
    assert seeded.get_entity("p", "r")["v"] == 1


def test_account_sas_without_delete_cannot_delete(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="rau"))
    with pytest.raises(HttpResponseError) as exc:
        client.delete_entity("p", "r")
    assert exc.value.status_code == 403
    assert seeded.get_entity("p", "r")["v"] == 1


def test_expired_account_sas_is_403_authorization_failure(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="r", start=past(120), expiry=past(60)))
    with pytest.raises(HttpResponseError) as exc:
        list(client.query_entities("PartitionKey eq 'p'"))
    assert exc.value.status_code == 403
    assert code(exc.value) == "AuthorizationFailure"


def test_account_sas_that_starts_in_the_future_is_403(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="r", start=future(60), expiry=future(120)))
    with pytest.raises(HttpResponseError) as exc:
        list(client.query_entities("PartitionKey eq 'p'"))
    assert exc.value.status_code == 403


def test_account_sas_with_tampered_signature_is_403(seeded, account_sas, sas_client):
    token = account_sas(permission="r")
    bad = token[:-6] + ("AAAAAA" if not token.endswith("AAAAAA") else "BBBBBB")
    with pytest.raises(HttpResponseError) as exc:
        list(sas_client(bad).query_entities("PartitionKey eq 'p'"))
    assert exc.value.status_code == 403
    assert code(exc.value) in SAS_BAD


def test_account_sas_with_widened_permissions_is_403(seeded, account_sas, sas_client):
    token = account_sas(permission="r")
    widened = token.replace("sp=r", "sp=rau")
    assert widened != token
    with pytest.raises(HttpResponseError) as exc:
        sas_client(widened).create_entity({"PartitionKey": "p", "RowKey": "x"})
    assert exc.value.status_code == 403


def test_account_sas_for_wrong_resource_type_is_403(seeded, account_sas, sas_client):
    # Entity operations need the Object resource type ("o").
    client = sas_client(account_sas(permission="r", resource_types="c"))
    with pytest.raises(HttpResponseError) as exc:
        list(client.query_entities("PartitionKey eq 'p'"))
    assert exc.value.status_code == 403


def test_account_sas_works_for_transactions_with_add_and_update(seeded, account_sas, sas_client):
    client = sas_client(account_sas(permission="au"))
    client.submit_transaction([
        ("create", {"PartitionKey": "p", "RowKey": "t1"}),
        ("update", {"PartitionKey": "p", "RowKey": "r", "v": 3}),
    ])
    assert seeded.get_entity("p", "t1")["RowKey"] == "t1"
    assert seeded.get_entity("p", "r")["v"] == 3


def test_account_sas_service_client_can_query_tables(service, table_name, account_sas):
    service.create_table(table_name)
    sas_service = TableServiceClient(endpoint=service.url, credential=AzureSasCredential(account_sas(permission="rl", resource_types="sco")))
    assert table_name in [t.name for t in sas_service.query_tables(f"TableName eq '{table_name}'")]


# -- table SAS --------------------------------------------------------------------------------


def test_table_sas_read_can_query(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "r"))
    assert [e["RowKey"] for e in client.list_entities()] == ["r"]


def test_table_sas_add_can_create(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "a"))
    client.create_entity({"PartitionKey": "p", "RowKey": "new"})
    assert seeded.get_entity("p", "new")["RowKey"] == "new"


def test_table_sas_update_can_update(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "u"))
    client.update_entity({"PartitionKey": "p", "RowKey": "r", "v": 7})
    assert seeded.get_entity("p", "r")["v"] == 7


def test_table_sas_delete_can_delete(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "d"))
    client.delete_entity("p", "r")
    assert list(seeded.list_entities()) == []


def test_table_sas_without_permission_is_403(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "r"))
    with pytest.raises(HttpResponseError) as exc:
        client.create_entity({"PartitionKey": "p", "RowKey": "new"})
    assert exc.value.status_code == 403
    assert code(exc.value) in SAS_DENIED


def test_expired_table_sas_is_403(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name, "r", start=past(120), expiry=past(60)))
    with pytest.raises(HttpResponseError) as exc:
        list(client.list_entities())
    assert exc.value.status_code == 403
    assert code(exc.value) == "AuthorizationFailure"


def test_table_sas_for_another_table_is_403(seeded, service, named_key, table_name, uniq, sas_client):
    other = uniq("other")
    service.create_table(other)
    token = table_sas(named_key, other, "r")
    with pytest.raises(HttpResponseError) as exc:
        list(sas_client(token).list_entities())
    assert exc.value.status_code == 403


def test_table_sas_with_upper_case_table_name_still_works(seeded, named_key, table_name, sas_client):
    client = sas_client(table_sas(named_key, table_name.upper(), "r"))
    assert [e["RowKey"] for e in client.list_entities()] == ["r"]


def test_table_sas_inside_partition_and_row_key_range_allows_add(seeded, named_key, table_name, sas_client):
    token = table_sas(named_key, table_name, "a", start_pk="test", start_rk="test1", end_pk="test", end_rk="test1")
    sas_client(token).create_entity({"PartitionKey": "test", "RowKey": "test1", "v": 1})
    assert seeded.get_entity("test", "test1")["v"] == 1


def test_table_sas_outside_partition_and_row_key_range_denies_add(seeded, named_key, table_name, sas_client):
    token = table_sas(named_key, table_name, "a", start_pk="test", start_rk="test1", end_pk="test", end_rk="test1")
    with pytest.raises(HttpResponseError) as exc:
        sas_client(token).create_entity({"PartitionKey": "other", "RowKey": "x"})
    assert exc.value.status_code == 403
    assert code(exc.value) == "AuthorizationFailure"


def test_table_sas_partition_range_limits_reads(seeded, named_key, table_name, sas_client):
    seeded.create_entity({"PartitionKey": "q", "RowKey": "r", "v": 2})
    token = table_sas(named_key, table_name, "r", start_pk="q", end_pk="q")
    client = sas_client(token)
    assert [e["PartitionKey"] for e in client.query_entities("PartitionKey eq 'q'")] == ["q"]
    with pytest.raises(HttpResponseError) as exc:
        client.get_entity("p", "r")
    assert exc.value.status_code == 403


def test_table_sas_from_stored_access_policy(seeded, named_key, table_name, sas_client):
    seeded.set_table_access_policy({"readers": TableAccessPolicy(
        permission=TableSasPermissions(read=True), start=past(60), expiry=future(60))})
    token = generate_table_sas(named_key, table_name, policy_id="readers")
    client = sas_client(token)
    assert [e["RowKey"] for e in client.list_entities()] == ["r"]
    with pytest.raises(HttpResponseError) as exc:
        client.create_entity({"PartitionKey": "p", "RowKey": "nope"})
    assert exc.value.status_code == 403


def test_table_sas_for_unknown_stored_policy_is_403(seeded, named_key, table_name, sas_client):
    token = generate_table_sas(named_key, table_name, policy_id="does-not-exist")
    with pytest.raises(HttpResponseError) as exc:
        list(sas_client(token).list_entities())
    assert exc.value.status_code == 403
