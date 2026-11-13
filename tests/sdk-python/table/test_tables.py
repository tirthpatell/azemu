"""Table create / delete / list / query / name validation."""

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.data.tables import TableClient, TableServiceClient

from tableutil import code


def names(items):
    return [t.name for t in items]


# -- create -------------------------------------------------------------------


def test_create_table_returns_client_for_that_table(service, table_name):
    client = service.create_table(table_name)
    assert isinstance(client, TableClient)
    assert client.table_name == table_name


def test_create_table_makes_it_listable(service, table_name):
    service.create_table(table_name)
    assert table_name in names(service.list_tables())


def test_create_table_twice_raises_table_already_exists(service, table_name):
    service.create_table(table_name)
    with pytest.raises(ResourceExistsError) as exc:
        service.create_table(table_name)
    assert exc.value.status_code == 409
    assert code(exc.value) == "TableAlreadyExists"


def test_table_client_create_table_twice_raises_table_already_exists(service, table_name):
    client = service.get_table_client(table_name)
    client.create_table()
    with pytest.raises(ResourceExistsError) as exc:
        client.create_table()
    assert code(exc.value) == "TableAlreadyExists"


def test_create_table_if_not_exists_creates_missing_table(service, table_name):
    client = service.create_table_if_not_exists(table_name)
    assert client.table_name == table_name
    assert table_name in names(service.list_tables())


def test_create_table_if_not_exists_is_idempotent(service, table_name):
    service.create_table(table_name)
    client = service.create_table_if_not_exists(table_name)
    assert client.table_name == table_name
    assert names(service.query_tables(f"TableName eq '{table_name}'")) == [table_name]


def test_table_client_create_table_if_not_exists_keeps_existing_data(service, table_name):
    client = service.create_table(table_name)
    client.create_entity({"PartitionKey": "p", "RowKey": "r", "v": 1})
    service.create_table_if_not_exists(table_name)
    assert client.get_entity("p", "r")["v"] == 1


def test_table_names_are_case_insensitive_for_creation(service, uniq):
    name = uniq("Mixed")
    service.create_table(name)
    with pytest.raises(ResourceExistsError) as exc:
        service.create_table(name.lower())
    assert code(exc.value) == "TableAlreadyExists"


def test_table_name_keeps_the_case_it_was_created_with(service, uniq):
    name = uniq("CaSe")
    service.create_table(name)
    assert name in names(service.list_tables())


def test_table_name_may_contain_digits_after_first_letter(service, uniq):
    name = "a" + uniq("1")[1:] + "9"
    service.create_table(name)
    assert name in names(service.list_tables())


def test_table_name_of_exactly_three_characters_is_allowed(service):
    import uuid
    # 3 chars is the minimum; use a random one to avoid collisions on shared targets.
    name = "z" + uuid.uuid4().hex[:2]
    service.create_table_if_not_exists(name)
    assert name in names(service.list_tables())


def test_table_name_of_exactly_63_characters_is_allowed(service, uniq):
    name = "n" + uniq("x")[1:] + "a" * (63 - 13)
    assert len(name) == 63
    service.create_table(name)
    assert name in names(service.list_tables())


# The SDK turns the service's InvalidResourceName / OutOfRangeInput answers for
# storage accounts into a ValueError that explains the naming rules. (The exact
# service codes are checked over raw HTTP in test_raw_http.py.)
AZURITE_NAME_GAP = "Azurite answers table-name errors with an empty error code, so the SDK cannot map them"


@pytest.mark.not_on_azurite(reason=AZURITE_NAME_GAP)
@pytest.mark.parametrize("bad", ["1table", "my_table", "my-table", "my table", "my.table"])
def test_create_table_with_invalid_characters_is_rejected(service, bad):
    with pytest.raises(ValueError, match="alphanumeric"):
        service.create_table(bad)


@pytest.mark.not_on_azurite(reason=AZURITE_NAME_GAP)
@pytest.mark.parametrize("bad", ["aa", "a" * 64])
def test_create_table_with_bad_length_is_rejected(service, bad):
    with pytest.raises(ValueError, match="3-63"):
        service.create_table(bad)


@pytest.mark.not_on_azurite(reason=AZURITE_NAME_GAP)
def test_entity_operation_on_invalid_table_name_is_rejected(service):
    client = service.get_table_client("1table")
    with pytest.raises(ValueError, match="alphanumeric"):
        client.create_entity({"PartitionKey": "foo", "RowKey": "bar"})


# -- delete -------------------------------------------------------------------


def test_delete_table_removes_it_from_listing(service, table_name):
    service.create_table(table_name)
    service.delete_table(table_name)
    assert table_name not in names(service.list_tables())


def test_table_client_delete_table_removes_it(service, table_name):
    client = service.create_table(table_name)
    client.delete_table()
    assert names(service.query_tables(f"TableName eq '{table_name}'")) == []


def test_delete_missing_table_does_not_raise_in_the_sdk(service, table_name):
    # The service answers 404 ResourceNotFound; the SDK swallows it.
    service.delete_table(table_name)
    service.get_table_client(table_name).delete_table()


def test_deleted_table_rejects_entity_operations_with_table_not_found(service, table_name):
    client = service.create_table(table_name)
    service.delete_table(table_name)
    with pytest.raises(ResourceNotFoundError) as exc:
        client.create_entity({"PartitionKey": "p", "RowKey": "r"})
    assert exc.value.status_code == 404
    assert code(exc.value) == "TableNotFound"


# -- list / query ---------------------------------------------------------------


def test_list_tables_returns_table_items_with_name(service, table_name):
    service.create_table(table_name)
    items = [t for t in service.list_tables() if t.name == table_name]
    assert len(items) == 1


def test_list_tables_contains_every_created_table(service, uniq):
    created = [uniq("lst") for _ in range(4)]
    for n in created:
        service.create_table(n)
    listed = set(names(service.list_tables()))
    assert set(created) <= listed


def test_list_tables_is_sorted_by_name(service, uniq):
    prefix = uniq("srt")
    created = [prefix + s for s in ("c", "a", "b")]
    for n in created:
        service.create_table(n)
    got = names(service.query_tables(f"TableName ge '{prefix}' and TableName lt '{prefix}z'"))
    assert got == sorted(created)


def test_query_tables_eq_filter_returns_exactly_that_table(service, uniq):
    a, b = uniq("qa"), uniq("qb")
    service.create_table(a)
    service.create_table(b)
    assert names(service.query_tables(f"TableName eq '{a}'")) == [a]


def test_query_tables_eq_filter_for_missing_table_returns_nothing(service, uniq):
    assert names(service.query_tables(f"TableName eq '{uniq('none')}'")) == []


def test_query_tables_or_filter_returns_both(service, uniq):
    a, b, c = uniq("oa"), uniq("ob"), uniq("oc")
    for n in (a, b, c):
        service.create_table(n)
    got = names(service.query_tables(f"TableName eq '{a}' or TableName eq '{b}'"))
    assert sorted(got) == sorted([a, b])


def test_query_tables_range_filter(service, uniq):
    prefix = uniq("rng")
    for s in ("a", "b", "c", "d"):
        service.create_table(prefix + s)
    got = names(service.query_tables(f"TableName gt '{prefix}a' and TableName le '{prefix}c'"))
    assert got == [prefix + "b", prefix + "c"]


def test_query_tables_with_parameters(service, uniq):
    a = uniq("pa")
    service.create_table(a)
    service.create_table(uniq("pb"))
    got = names(service.query_tables("TableName eq @name", parameters={"name": a}))
    assert got == [a]


def test_query_tables_results_per_page_pages_through_everything(service, uniq):
    prefix = uniq("pg")
    created = [f"{prefix}{i}" for i in range(5)]
    for n in created:
        service.create_table(n)
    flt = f"TableName ge '{prefix}' and TableName lt '{prefix}z'"
    pages = list(service.query_tables(flt, results_per_page=2).by_page())
    assert [len(list(p)) for p in pages] == [2, 2, 1] or sum(len(list(p)) for p in pages) == 5
    flat = names(service.query_tables(flt, results_per_page=2))
    assert flat == created


def test_query_tables_page_sizes_are_two_two_one(service, uniq):
    prefix = uniq("ps")
    for i in range(5):
        service.create_table(f"{prefix}{i}")
    flt = f"TableName ge '{prefix}' and TableName lt '{prefix}z'"
    sizes = [len(list(p)) for p in service.query_tables(flt, results_per_page=2).by_page()]
    assert sizes == [2, 2, 1]


def test_query_tables_continuation_token_resumes_where_the_page_ended(service, uniq):
    prefix = uniq("ct")
    created = [f"{prefix}{i}" for i in range(4)]
    for n in created:
        service.create_table(n)
    flt = f"TableName ge '{prefix}' and TableName lt '{prefix}z'"
    pager = service.query_tables(flt, results_per_page=2).by_page()
    first = names(next(pager))
    token = pager.continuation_token
    assert first == created[:2]
    assert token
    resumed = service.query_tables(flt, results_per_page=2).by_page(continuation_token=token)
    assert names(next(resumed)) == created[2:]


def test_list_tables_results_per_page_limits_page_size(service, uniq):
    for _ in range(3):
        service.create_table(uniq("lp"))
    first_page = list(next(service.list_tables(results_per_page=2).by_page()))
    assert len(first_page) == 2


def test_service_client_from_connection_string_works_without_table_endpoint_path(conn_str):
    svc = TableServiceClient.from_connection_string(conn_str)
    assert svc.account_name
