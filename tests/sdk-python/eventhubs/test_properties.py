"""get_eventhub_properties / get_partition_ids / get_partition_properties on producer and consumer clients.

Sources: SDK `_client_base._get_eventhub_properties` / `_get_partition_properties` (keys and types), Microsoft's
`livetest/synctests/test_properties.py` (partition_ids == ["0", "1"], error types), `utils.utc_from_timestamp` (the "no time"
sentinel). The management exchange itself is described in docs/sdk-notes.md.
"""

from datetime import datetime, timedelta, timezone

import pytest
from azure.eventhub import EventData
from azure.eventhub.exceptions import ConnectError

import ehutil
from ehutil import make_consumer, make_producer, read_from_start, send_n

PARTITION_KEYS = {
    "eventhub_name", "id", "beginning_sequence_number", "last_enqueued_sequence_number",
    "last_enqueued_offset", "last_enqueued_time_utc", "is_empty",
}


@pytest.fixture(params=["producer", "consumer"])
def client(request, emu, hub, track):
    """The same assertions run on both client types."""
    if request.param == "producer":
        return track(make_producer(emu, hub))
    return track(make_consumer(emu, hub))


@pytest.fixture(params=["producer", "consumer"])
def fresh_client(request, emu, fresh_hub, track):
    """Like `client`, on a hub nothing was ever sent to (the shared hubs of an Azure run are not empty)."""
    if request.param == "producer":
        return track(make_producer(emu, fresh_hub))
    return track(make_consumer(emu, fresh_hub))


def last_enqueued(client, pids=None):
    return {p: client.get_partition_properties(p)["last_enqueued_sequence_number"] for p in (pids or client.get_partition_ids())}


# -- event hub ------------------------------------------------------------------------------------------------------
def test_eventhub_properties_name_and_partition_ids(client, hub):
    props = client.get_eventhub_properties()
    assert props["eventhub_name"] == hub
    assert props["partition_ids"] == ["0", "1", "2", "3"]


def test_eventhub_properties_has_exactly_the_documented_keys(client):
    assert set(client.get_eventhub_properties()) == {"eventhub_name", "created_at", "partition_ids"}


def test_eventhub_properties_created_at_is_a_recent_utc_datetime(client):
    created = client.get_eventhub_properties()["created_at"]
    assert isinstance(created, datetime)
    assert created.tzinfo is not None and created.utcoffset() == timedelta(0)
    assert abs(datetime.now(timezone.utc) - created) < timedelta(hours=1)


def test_partition_ids_are_strings(client):
    ids = client.get_partition_ids()
    assert ids == ["0", "1", "2", "3"]
    assert all(isinstance(p, str) for p in ids)


def test_get_partition_ids_matches_eventhub_properties(client):
    assert client.get_partition_ids() == client.get_eventhub_properties()["partition_ids"]


@pytest.mark.parametrize("count", [1, 2, 8, 32])
def test_partition_ids_follow_the_partition_count(emu, new_hub, track, count):
    name = new_hub(count)
    client = track(make_producer(emu, name))
    assert client.get_partition_ids() == [str(i) for i in range(count)]


def test_two_hubs_do_not_share_partition_counts(emu, new_hub, track):
    a, b = new_hub(2), new_hub(5)
    assert track(make_producer(emu, a)).get_partition_ids() == ["0", "1"]
    assert track(make_producer(emu, b)).get_partition_ids() == ["0", "1", "2", "3", "4"]


def test_eventhub_name_in_connection_string_entity_path(emu, hub, track):
    from azure.eventhub import EventHubProducerClient

    client = track(EventHubProducerClient.from_connection_string(ehutil.conn_str(emu, hub), **ehutil.CLIENT_KW))
    assert client.get_eventhub_properties()["eventhub_name"] == hub


# -- partition properties ---------------------------------------------------------------------------------------------
def test_partition_properties_has_exactly_the_documented_keys(client):
    assert set(client.get_partition_properties("0")) == PARTITION_KEYS


def test_partition_properties_name_and_id(client, hub):
    props = client.get_partition_properties("2")
    assert props["eventhub_name"] == hub
    assert props["id"] == "2"


def test_empty_partition_is_empty(fresh_client):
    props = fresh_client.get_partition_properties("0")
    assert props["is_empty"] is True
    assert isinstance(props["last_enqueued_offset"], str)


def test_empty_partition_sequence_numbers_are_minus_one(fresh_client):
    # ASSUMED (see docs/sdk-notes.md): a partition that never held an event reports -1; the SDK only documents the "no time" sentinel.
    props = fresh_client.get_partition_properties("0")
    assert props["last_enqueued_sequence_number"] == -1


def test_empty_partition_last_enqueued_time_is_the_no_time_sentinel(fresh_client):
    # The SDK maps the AMQP "never" timestamp (year 1) to datetime.min (utils.utc_from_timestamp); ASSUMED that the service sends it.
    assert fresh_client.get_partition_properties("0")["last_enqueued_time_utc"] == datetime.min.replace(tzinfo=timezone.utc)


def test_all_partitions_of_a_fresh_hub_are_empty(fresh_client):
    assert all(fresh_client.get_partition_properties(p)["is_empty"] for p in fresh_client.get_partition_ids())


def test_partition_is_not_empty_after_a_send(emu, hub, producer):
    send_n(producer, 1, partition_id="1")
    props = producer.get_partition_properties("1")
    assert props["is_empty"] is False


def test_only_the_partition_that_received_events_is_not_empty(emu, hub, producer):
    # (on a shared hub other partitions may hold older events, so look at what changed)
    before = last_enqueued(producer)
    send_n(producer, 2, partition_id="2")
    after = last_enqueued(producer)
    assert {p: after[p] - before[p] for p in after} == {"0": 0, "1": 0, "2": 2, "3": 0}
    assert producer.get_partition_properties("2")["is_empty"] is False


def test_last_enqueued_values_match_the_last_event(emu, hub, producer):
    send_n(producer, 3, partition_id="0")
    events = read_from_start(emu, hub, 3, partition_id="0")
    props = producer.get_partition_properties("0")
    last = events[-1]
    assert props["last_enqueued_sequence_number"] == last.sequence_number
    assert props["last_enqueued_offset"] == last.offset
    assert props["last_enqueued_time_utc"] == last.enqueued_time


def test_beginning_sequence_number_is_the_first_events_sequence_number(emu, hub, producer):
    send_n(producer, 3, partition_id="0")
    events = read_from_start(emu, hub, 3, partition_id="0")
    begin = producer.get_partition_properties("0")["beginning_sequence_number"]
    if ehutil.baseline_of(hub, "0") == -1:  # nothing was in the partition before this test
        assert begin == events[0].sequence_number
    else:  # a shared hub: older events are still retained
        assert begin <= events[0].sequence_number


def test_last_enqueued_sequence_number_advances_with_each_event(emu, hub, producer):
    send_n(producer, 2, partition_id="3")
    first = producer.get_partition_properties("3")["last_enqueued_sequence_number"]
    send_n(producer, 5, prefix="more", partition_id="3")
    assert producer.get_partition_properties("3")["last_enqueued_sequence_number"] == first + 5


def test_sequence_span_equals_event_count_minus_one(emu, hub, producer):
    before = last_enqueued(producer, ["1"])["1"]
    send_n(producer, 7, partition_id="1")
    props = producer.get_partition_properties("1")
    assert props["last_enqueued_sequence_number"] - before == 7
    if before == -1:
        assert props["last_enqueued_sequence_number"] - props["beginning_sequence_number"] == 6


def test_last_enqueued_time_is_recent_and_in_utc(emu, hub, producer):
    send_n(producer, 1, partition_id="0")
    t = producer.get_partition_properties("0")["last_enqueued_time_utc"]
    assert t.utcoffset() == timedelta(0)
    assert abs(datetime.now(timezone.utc) - t) < timedelta(minutes=2)


def test_partition_properties_seen_by_consumer_client_after_producer_send(emu, hub, producer, track):
    before = last_enqueued(producer, ["2"])["2"]
    send_n(producer, 4, partition_id="2")
    consumer = track(make_consumer(emu, hub))
    props = consumer.get_partition_properties("2")
    assert props["is_empty"] is False
    assert props["last_enqueued_sequence_number"] - before == 4


def test_last_enqueued_offset_is_a_string_of_digits_after_a_send(emu, hub, producer):
    send_n(producer, 1, partition_id="0")
    off = producer.get_partition_properties("0")["last_enqueued_offset"]
    assert isinstance(off, str) and off.lstrip("-").isdigit()


# -- the reply as it comes off the wire ----------------------------------------------------------------------------------------
def test_eventhub_reply_keys_and_types(emu, hub, producer):
    # Checked against Azure: the keys and the Python types the SDKs read
    info = ehutil.raw_management_reply(producer)
    assert info[b"name"] == hub.encode()
    assert info[b"type"] == b"com.microsoft:eventhub"
    assert info[b"partition_count"] == 4 and info[b"partition_ids"] == [b"0", b"1", b"2", b"3"]
    assert isinstance(info[b"created_at"], int)  # milliseconds since 1970
    assert info[b"georeplication_factor"] == 1


def test_empty_partition_reply_values(emu, fresh_hub, track):
    # The values of a partition that never held an event; Azure answers exactly these (the docs left them open)
    info = ehutil.raw_management_reply(track(make_producer(emu, fresh_hub)), "0")
    assert info[b"type"] == b"com.microsoft:partition" and info[b"partition"] == b"0"
    assert info[b"begin_sequence_number"] == -1 and info[b"last_enqueued_sequence_number"] == -1
    assert info[b"last_enqueued_offset"] == b"-1"
    assert info[b"last_enqueued_time_utc"] == -62135596800000  # 0001-01-01
    assert info[b"is_partition_empty"] is True


def test_partition_reply_epoch_fields(emu, hub, producer):
    # both are -1 on a real namespace (November 2026); the Rust SDK reads last_enqueued_sequence_number_epoch
    info = ehutil.raw_management_reply(producer, "0")
    assert info[b"begin_sequence_number_epoch"] == -1
    assert info[b"last_enqueued_sequence_number_epoch"] == -1


# -- errors ---------------------------------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", ["XYZ", "-1", "4"])
def test_unknown_partition_id_is_an_error(client, bad):
    # The SDK has no client-side check for properties; any management status >= 400 becomes ConnectError (see docs/sdk-notes.md).
    with pytest.raises(ConnectError):
        client.get_partition_properties(bad)


@pytest.mark.no_autocreate
def test_properties_of_a_missing_hub_raise_connect_error(emu, track):
    # Microsoft's test_get_properties_with_connect_error: a hub that does not exist -> ConnectError (management 404).
    client = track(make_producer(emu, ehutil.uniq("missing")))
    with pytest.raises(ConnectError):
        client.get_eventhub_properties()
