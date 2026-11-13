"""Entity CRUD: create / get / update / upsert / delete, etags and optimistic concurrency."""

import re
from datetime import datetime, timezone

import pytest
from azure.core import MatchConditions
from azure.core.exceptions import (
    HttpResponseError,
    ResourceExistsError,
    ResourceModifiedError,
    ResourceNotFoundError,
)
from azure.data.tables import TableEntity, UpdateMode

from tableutil import code

ETAG_RE = re.compile(r"""^W/"datetime'\d{4}-\d\d-\d\dT\d\d%3A\d\d%3A\d\d(\.\d+)?Z'"$""")


def ent(**props):
    return {"PartitionKey": "p", "RowKey": "r", **props}


STALE_ETAG = "W/\"datetime'2012-06-15T22%3A51%3A44.9662825Z'\""


# -- create ---------------------------------------------------------------------


def test_create_entity_returns_etag(table):
    resp = table.create_entity(ent(a=1))
    assert ETAG_RE.match(resp["etag"])


def test_create_then_get_round_trips_properties(table):
    table.create_entity(ent(name="Ada", age=36, active=True))
    got = table.get_entity("p", "r")
    assert got["PartitionKey"] == "p"
    assert got["RowKey"] == "r"
    assert (got["name"], got["age"], got["active"]) == ("Ada", 36, True)


def test_create_entity_accepts_table_entity_object(table):
    e = TableEntity()
    e["PartitionKey"], e["RowKey"], e["x"] = "p", "r", "from-table-entity"
    table.create_entity(e)
    assert table.get_entity("p", "r")["x"] == "from-table-entity"


def test_create_entity_with_only_keys_has_no_other_properties(table):
    table.create_entity(ent())
    got = table.get_entity("p", "r")
    assert dict(got) == {"PartitionKey": "p", "RowKey": "r"}


def test_create_duplicate_entity_raises_entity_already_exists(table):
    table.create_entity(ent(v=1))
    with pytest.raises(ResourceExistsError) as exc:
        table.create_entity(ent(v=2))
    assert exc.value.status_code == 409
    assert code(exc.value) == "EntityAlreadyExists"
    assert table.get_entity("p", "r")["v"] == 1


def test_same_row_key_in_different_partitions_are_different_entities(table):
    table.create_entity({"PartitionKey": "p1", "RowKey": "r", "v": 1})
    table.create_entity({"PartitionKey": "p2", "RowKey": "r", "v": 2})
    assert table.get_entity("p1", "r")["v"] == 1
    assert table.get_entity("p2", "r")["v"] == 2


def test_create_entity_without_partition_key_is_rejected(table):
    # Service: 400 PropertiesNeedValue; the SDK turns that into a ValueError.
    with pytest.raises(ValueError, match="PartitionKey"):
        table.create_entity({"RowKey": "r"})


def test_create_entity_without_row_key_is_rejected(table):
    with pytest.raises(ValueError, match="RowKey"):
        table.create_entity({"PartitionKey": "p"})


def test_create_entity_in_missing_table_raises_table_not_found(service, table_name):
    client = service.get_table_client(table_name)
    with pytest.raises(ResourceNotFoundError) as exc:
        client.create_entity(ent())
    assert exc.value.status_code == 404
    assert code(exc.value) == "TableNotFound"


# -- get ------------------------------------------------------------------------


def test_get_missing_entity_raises_resource_not_found(table):
    with pytest.raises(ResourceNotFoundError) as exc:
        table.get_entity("p", "nope")
    assert exc.value.status_code == 404
    assert code(exc.value) == "ResourceNotFound"


def test_get_entity_from_missing_table_raises_table_not_found(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        service.get_table_client(table_name).get_entity("p", "r")
    assert code(exc.value) == "TableNotFound"


def test_get_entity_with_wrong_partition_is_not_found(table):
    table.create_entity(ent())
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("other", "r")


def test_get_entity_select_returns_only_selected_properties(table):
    table.create_entity(ent(a=1, b=2, c=3))
    got = table.get_entity("p", "r", select=["a", "c"])
    assert dict(got) == {"a": 1, "c": 3}


def test_get_entity_select_can_include_keys(table):
    table.create_entity(ent(a=1, b=2))
    got = table.get_entity("p", "r", select=["PartitionKey", "b"])
    assert dict(got) == {"PartitionKey": "p", "b": 2}


def test_get_entity_select_string_form(table):
    table.create_entity(ent(a=1, b=2))
    got = table.get_entity("p", "r", select="b")
    assert dict(got) == {"b": 2}


# -- metadata and etags -----------------------------------------------------------


def test_entity_metadata_has_etag_and_timestamp(table):
    table.create_entity(ent())
    got = table.get_entity("p", "r")
    assert ETAG_RE.match(got.metadata["etag"])
    ts = got.metadata["timestamp"]
    assert isinstance(ts, datetime)
    assert ts.tzinfo is not None
    assert abs((datetime.now(timezone.utc) - ts).total_seconds()) < 3600


def test_timestamp_is_not_a_regular_property(table):
    table.create_entity(ent())
    assert "Timestamp" not in table.get_entity("p", "r")


def test_get_entity_etag_equals_the_one_returned_by_create(table):
    created = table.create_entity(ent())
    assert table.get_entity("p", "r").metadata["etag"] == created["etag"]


def test_update_entity_changes_the_etag(table):
    created = table.create_entity(ent(v=1))
    updated = table.update_entity(ent(v=2))
    assert updated["etag"] != created["etag"]
    assert table.get_entity("p", "r").metadata["etag"] == updated["etag"]


def test_merge_changes_the_etag(table):
    created = table.create_entity(ent(v=1))
    merged = table.update_entity(ent(w=2), mode=UpdateMode.MERGE)
    assert merged["etag"] != created["etag"]


def test_upsert_changes_the_etag_of_an_existing_entity(table):
    created = table.create_entity(ent(v=1))
    upserted = table.upsert_entity(ent(v=2))
    assert upserted["etag"] != created["etag"]


def test_update_with_identical_content_still_changes_the_etag(table):
    created = table.create_entity(ent(v=1))
    updated = table.update_entity(ent(v=1))
    assert updated["etag"] != created["etag"]


def test_timestamp_moves_forward_after_update(table):
    table.create_entity(ent(v=1))
    before = table.get_entity("p", "r").metadata["timestamp"]
    table.update_entity(ent(v=2))
    after = table.get_entity("p", "r").metadata["timestamp"]
    assert after > before


def test_get_does_not_change_the_etag(table):
    table.create_entity(ent())
    first = table.get_entity("p", "r").metadata["etag"]
    second = table.get_entity("p", "r").metadata["etag"]
    assert first == second


# -- update (replace / merge) -----------------------------------------------------


def test_update_entity_default_mode_is_merge(table):
    table.create_entity(ent(a=1, b=2))
    table.update_entity(ent(b=20, c=30))
    got = table.get_entity("p", "r")
    assert (got["a"], got["b"], got["c"]) == (1, 20, 30)


def test_update_entity_merge_keeps_properties_not_mentioned(table):
    table.create_entity(ent(keep="yes", change="old"))
    table.update_entity(ent(change="new"), mode=UpdateMode.MERGE)
    got = table.get_entity("p", "r")
    assert got["keep"] == "yes"
    assert got["change"] == "new"


def test_update_entity_merge_can_change_a_property_type(table):
    table.create_entity(ent(a=1))
    table.update_entity(ent(a="now a string"), mode=UpdateMode.MERGE)
    assert table.get_entity("p", "r")["a"] == "now a string"


def test_update_entity_replace_drops_properties_not_mentioned(table):
    table.create_entity(ent(a=1, b=2))
    table.update_entity(ent(b=20), mode=UpdateMode.REPLACE)
    got = table.get_entity("p", "r")
    assert dict(got) == {"PartitionKey": "p", "RowKey": "r", "b": 20}


def test_update_entity_replace_with_only_keys_clears_all_properties(table):
    table.create_entity(ent(a=1, b=2))
    table.update_entity(ent(), mode=UpdateMode.REPLACE)
    assert dict(table.get_entity("p", "r")) == {"PartitionKey": "p", "RowKey": "r"}


def test_update_entity_leaves_other_entities_alone(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r1", "v": 1})
    table.create_entity({"PartitionKey": "p", "RowKey": "r2", "v": 1})
    table.update_entity({"PartitionKey": "p", "RowKey": "r1", "v": 2})
    assert table.get_entity("p", "r2")["v"] == 1


@pytest.mark.parametrize("mode", [UpdateMode.MERGE, UpdateMode.REPLACE])
def test_update_missing_entity_raises_resource_not_found(table, mode):
    with pytest.raises(ResourceNotFoundError) as exc:
        table.update_entity(ent(a=1), mode=mode)
    assert exc.value.status_code == 404
    assert code(exc.value) == "ResourceNotFound"


@pytest.mark.parametrize("mode", [UpdateMode.MERGE, UpdateMode.REPLACE])
def test_update_missing_entity_does_not_create_it(table, mode):
    with pytest.raises(ResourceNotFoundError):
        table.update_entity(ent(a=1), mode=mode)
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", "r")


@pytest.mark.parametrize("mode", [UpdateMode.MERGE, UpdateMode.REPLACE])
def test_update_with_stale_etag_on_missing_entity_is_404_not_412(table, mode):
    with pytest.raises(ResourceNotFoundError) as exc:
        table.update_entity(ent(a=1), mode=mode, etag=STALE_ETAG,
                            match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 404


def test_update_in_missing_table_raises_table_not_found(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        service.get_table_client(table_name).update_entity(ent(a=1))
    assert code(exc.value) == "TableNotFound"


# -- upsert -------------------------------------------------------------------------


def test_upsert_merge_creates_a_missing_entity(table):
    table.upsert_entity(ent(a=1), mode=UpdateMode.MERGE)
    assert table.get_entity("p", "r")["a"] == 1


def test_upsert_replace_creates_a_missing_entity(table):
    table.upsert_entity(ent(a=1), mode=UpdateMode.REPLACE)
    assert table.get_entity("p", "r")["a"] == 1


def test_upsert_default_mode_is_merge(table):
    table.create_entity(ent(a=1, b=2))
    table.upsert_entity(ent(b=20))
    got = table.get_entity("p", "r")
    assert (got["a"], got["b"]) == (1, 20)


def test_upsert_merge_keeps_unmentioned_properties(table):
    table.create_entity(ent(a=1, b=2))
    table.upsert_entity(ent(c=3), mode=UpdateMode.MERGE)
    got = table.get_entity("p", "r")
    assert (got["a"], got["b"], got["c"]) == (1, 2, 3)


def test_upsert_replace_drops_unmentioned_properties(table):
    table.create_entity(ent(a=1, b=2))
    table.upsert_entity(ent(c=3), mode=UpdateMode.REPLACE)
    assert dict(table.get_entity("p", "r")) == {"PartitionKey": "p", "RowKey": "r", "c": 3}


def test_upsert_returns_an_etag_that_matches_the_stored_entity(table):
    resp = table.upsert_entity(ent(a=1))
    assert table.get_entity("p", "r").metadata["etag"] == resp["etag"]


def test_upsert_twice_never_conflicts(table):
    table.upsert_entity(ent(a=1))
    table.upsert_entity(ent(a=2))
    assert table.get_entity("p", "r")["a"] == 2


def test_upsert_in_missing_table_raises_table_not_found(service, table_name):
    with pytest.raises(ResourceNotFoundError) as exc:
        service.get_table_client(table_name).upsert_entity(ent(a=1))
    assert code(exc.value) == "TableNotFound"


# -- delete ---------------------------------------------------------------------------


def test_delete_entity_removes_it(table):
    table.create_entity(ent())
    table.delete_entity("p", "r")
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", "r")


def test_delete_entity_accepts_an_entity_argument(table):
    e = ent(a=1)
    table.create_entity(e)
    table.delete_entity(e)
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", "r")


def test_delete_missing_entity_is_not_an_error_in_the_sdk(table):
    # The service answers 404 ResourceNotFound; the SDK swallows it.
    table.delete_entity("p", "never-existed")


def test_delete_entity_in_missing_table_is_not_an_error_in_the_sdk(service, table_name):
    service.get_table_client(table_name).delete_entity("p", "r")


def test_delete_entity_leaves_other_entities(table):
    table.create_entity({"PartitionKey": "p", "RowKey": "r1"})
    table.create_entity({"PartitionKey": "p", "RowKey": "r2"})
    table.delete_entity("p", "r1")
    assert table.get_entity("p", "r2")["RowKey"] == "r2"


def test_deleted_entity_can_be_created_again(table):
    table.create_entity(ent(v=1))
    table.delete_entity("p", "r")
    table.create_entity(ent(v=2))
    assert table.get_entity("p", "r")["v"] == 2


# -- optimistic concurrency -------------------------------------------------------------


@pytest.mark.parametrize("mode", [UpdateMode.MERGE, UpdateMode.REPLACE])
def test_update_with_current_etag_succeeds(table, mode):
    created = table.create_entity(ent(v=1))
    table.update_entity(ent(v=2), mode=mode, etag=created["etag"],
                        match_condition=MatchConditions.IfNotModified)
    assert table.get_entity("p", "r")["v"] == 2


@pytest.mark.parametrize("mode", [UpdateMode.MERGE, UpdateMode.REPLACE])
def test_update_with_stale_etag_raises_update_condition_not_satisfied(table, mode):
    created = table.create_entity(ent(v=1))
    table.update_entity(ent(v=2))  # moves the etag on
    with pytest.raises(ResourceModifiedError) as exc:
        table.update_entity(ent(v=3), mode=mode, etag=created["etag"],
                            match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 412
    assert code(exc.value) == "UpdateConditionNotSatisfied"
    assert table.get_entity("p", "r")["v"] == 2


def test_update_with_made_up_etag_raises_412(table):
    table.create_entity(ent(v=1))
    with pytest.raises(ResourceModifiedError) as exc:
        table.update_entity(ent(v=2), etag=STALE_ETAG, match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 412


def test_failed_conditional_update_does_not_change_the_etag(table):
    created = table.create_entity(ent(v=1))
    with pytest.raises(ResourceModifiedError):
        table.update_entity(ent(v=2), etag=STALE_ETAG, match_condition=MatchConditions.IfNotModified)
    assert table.get_entity("p", "r").metadata["etag"] == created["etag"]


def test_update_after_conditional_success_makes_old_etag_stale(table):
    created = table.create_entity(ent(v=1))
    table.update_entity(ent(v=2), etag=created["etag"], match_condition=MatchConditions.IfNotModified)
    with pytest.raises(ResourceModifiedError):
        table.update_entity(ent(v=3), etag=created["etag"], match_condition=MatchConditions.IfNotModified)


def test_update_without_etag_is_unconditional(table):
    table.create_entity(ent(v=1))
    table.update_entity(ent(v=2))
    table.update_entity(ent(v=3))
    assert table.get_entity("p", "r")["v"] == 3


def test_delete_with_current_etag_succeeds(table):
    created = table.create_entity(ent())
    table.delete_entity("p", "r", etag=created["etag"], match_condition=MatchConditions.IfNotModified)
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", "r")


def test_delete_with_stale_etag_raises_update_condition_not_satisfied(table):
    created = table.create_entity(ent(v=1))
    table.update_entity(ent(v=2))
    with pytest.raises(ResourceModifiedError) as exc:
        table.delete_entity("p", "r", etag=created["etag"], match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 412
    assert code(exc.value) == "UpdateConditionNotSatisfied"
    assert table.get_entity("p", "r")["v"] == 2


def test_delete_with_made_up_etag_raises_412_and_keeps_the_entity(table):
    table.create_entity(ent())
    with pytest.raises(ResourceModifiedError) as exc:
        table.delete_entity("p", "r", etag=STALE_ETAG, match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 412
    assert table.get_entity("p", "r")["RowKey"] == "r"


def test_delete_entity_object_with_metadata_etag_and_match_condition(table):
    table.create_entity(ent(v=1))
    got = table.get_entity("p", "r")
    table.update_entity(ent(v=2))  # makes got's etag stale
    with pytest.raises(ResourceModifiedError):
        table.delete_entity(got, match_condition=MatchConditions.IfNotModified)
    fresh = table.get_entity("p", "r")
    table.delete_entity(fresh, match_condition=MatchConditions.IfNotModified)
    with pytest.raises(ResourceNotFoundError):
        table.get_entity("p", "r")


def test_delete_with_stale_etag_on_missing_entity_is_not_an_error(table):
    # 404 wins over the failed precondition, and the SDK swallows 404 on delete.
    table.delete_entity("p", "r", etag=STALE_ETAG, match_condition=MatchConditions.IfNotModified)


def test_upsert_ignores_etags_on_the_entity(table):
    table.create_entity(ent(v=1))
    table.upsert_entity(ent(v=2))
    table.upsert_entity(ent(v=3))
    assert table.get_entity("p", "r")["v"] == 3


def test_two_clients_racing_on_the_same_etag_only_one_wins(service, table):
    created = table.create_entity(ent(v=0))
    other = service.get_table_client(table.table_name)
    table.update_entity(ent(v=1), etag=created["etag"], match_condition=MatchConditions.IfNotModified)
    with pytest.raises(HttpResponseError) as exc:
        other.update_entity(ent(v=2), etag=created["etag"], match_condition=MatchConditions.IfNotModified)
    assert exc.value.status_code == 412
    assert table.get_entity("p", "r")["v"] == 1
