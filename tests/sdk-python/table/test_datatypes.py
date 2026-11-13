"""Property types: what goes in through the SDK comes back out unchanged."""

import math
import uuid
from datetime import datetime, timedelta, timezone

import pytest
from azure.core.exceptions import HttpResponseError
from azure.data.tables import EdmType, EntityProperty, TableEntity
from azure.data.tables._deserialize import TablesEntityDatetime

from tableutil import code

KEYS = {"PartitionKey": "p", "RowKey": "r"}


def roundtrip(table, **props):
    table.create_entity({**KEYS, **props})
    return table.get_entity("p", "r")


# -- strings -------------------------------------------------------------------


def test_string_round_trips(table):
    assert roundtrip(table, s="hello")["s"] == "hello"


def test_empty_string_is_kept_as_empty_string(table):
    got = roundtrip(table, s="")
    assert "s" in got
    assert got["s"] == ""


def test_string_with_only_spaces_keeps_its_spaces(table):
    assert roundtrip(table, s="   ")["s"] == "   "


def test_string_keeps_leading_and_trailing_spaces(table):
    assert roundtrip(table, s="  text  ")["s"] == "  text  "


def test_unicode_string_round_trips(table):
    text = "héllo wörld 你好 ꀕ 🙂 é́"
    assert roundtrip(table, s=text)["s"] == text


def test_string_with_json_special_characters_round_trips(table):
    text = 'quote " backslash \\ newline \n tab \t slash / <tag> & \u0000'.replace("\u0000", "")
    assert roundtrip(table, s=text)["s"] == text


def test_numeric_looking_string_stays_a_string(table):
    got = roundtrip(table, s="123", t="true", u="1.5")
    assert (got["s"], got["t"], got["u"]) == ("123", "true", "1.5")


def test_long_string_of_32k_characters_round_trips(table):
    text = "x" * 32 * 1024
    assert roundtrip(table, s=text)["s"] == text


def test_string_property_over_64kb_is_rejected(table):
    # 32K UTF-16 code units (64 KiB) is the limit; one more is too large.
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({**KEYS, "s": "x" * (32 * 1024 + 1)})
    assert exc.value.status_code == 400
    assert code(exc.value) == "PropertyValueTooLarge"


# -- integers -----------------------------------------------------------------


def test_int_round_trips_as_int(table):
    got = roundtrip(table, n=42)
    assert got["n"] == 42
    assert type(got["n"]) is int


@pytest.mark.parametrize("n", [0, -1, 2147483647, -2147483648])
def test_int32_boundaries_round_trip(table, n):
    got = roundtrip(table, n=n)
    assert got["n"] == n
    assert type(got["n"]) is int


def test_explicit_int32_property_round_trips_as_plain_int(table):
    got = roundtrip(table, n=EntityProperty(7, EdmType.INT32))
    assert got["n"] == 7
    assert type(got["n"]) is int


def test_int64_round_trips_as_entity_property(table):
    got = roundtrip(table, big=EntityProperty(2**40, EdmType.INT64))
    assert isinstance(got["big"], EntityProperty)
    assert got["big"].edm_type == EdmType.INT64
    assert got["big"].value == 2**40


@pytest.mark.parametrize("n", [2**63 - 1, -(2**63), 2147483648, -2147483649, 0])
def test_int64_boundaries_round_trip(table, n):
    got = roundtrip(table, big=EntityProperty(n, EdmType.INT64))
    assert got["big"].value == n


def test_small_value_stored_as_int64_stays_int64(table):
    got = roundtrip(table, big=EntityProperty(5, EdmType.INT64))
    assert got["big"].edm_type == EdmType.INT64
    assert got["big"].value == 5


@pytest.mark.parametrize("n", [2**31, -(2**31) - 1])
def test_int_outside_int32_is_rejected_by_the_sdk_before_sending(table, n):
    with pytest.raises(TypeError):
        table.create_entity({**KEYS, "n": n})


@pytest.mark.parametrize("n", [2**31, -(2**31) - 1])
def test_explicit_int32_out_of_range_is_rejected_by_the_sdk(table, n):
    with pytest.raises(TypeError):
        table.create_entity({**KEYS, "n": EntityProperty(n, EdmType.INT32)})


@pytest.mark.parametrize("n", [2**63, -(2**63) - 1])
def test_explicit_int64_out_of_range_is_rejected_by_the_sdk(table, n):
    with pytest.raises(TypeError):
        table.create_entity({**KEYS, "n": EntityProperty(n, EdmType.INT64)})


# -- floats -------------------------------------------------------------------


def test_float_round_trips_as_float(table):
    got = roundtrip(table, f=3.25)
    assert got["f"] == 3.25
    assert type(got["f"]) is float


def test_whole_number_float_stays_a_float(table):
    got = roundtrip(table, f=5.0)
    assert got["f"] == 5.0
    assert type(got["f"]) is float


def test_tiny_and_huge_floats_round_trip(table):
    got = roundtrip(table, a=0.0, b=1e-300, c=1.7976931348623157e308, d=-2.5e-10)
    assert got["a"] == 0.0
    assert got["b"] == 1e-300
    assert got["c"] == 1.7976931348623157e308
    assert got["d"] == -2.5e-10


def test_float_keeps_full_double_precision(table):
    value = 0.1 + 0.2
    assert roundtrip(table, f=value)["f"] == value


def test_explicit_double_property_with_int_value_comes_back_as_float(table):
    got = roundtrip(table, f=EntityProperty(5, EdmType.DOUBLE))
    assert got["f"] == 5.0
    assert type(got["f"]) is float


def test_nan_round_trips(table):
    assert math.isnan(roundtrip(table, f=float("nan"))["f"])


def test_infinities_round_trip(table):
    got = roundtrip(table, up=float("inf"), down=float("-inf"))
    assert got["up"] == math.inf
    assert got["down"] == -math.inf


# -- bool ---------------------------------------------------------------------


@pytest.mark.parametrize("flag", [True, False])
def test_bool_round_trips(table, flag):
    got = roundtrip(table, b=flag)
    assert got["b"] is flag


# -- datetime -----------------------------------------------------------------


def test_utc_datetime_round_trips(table):
    when = datetime(2021, 6, 8, 2, 10, 9, 123456, tzinfo=timezone.utc)
    got = roundtrip(table, d=when)["d"]
    assert got == when
    assert got.tzinfo is not None
    assert got.utcoffset() == timedelta(0)


def test_datetime_with_other_timezone_comes_back_as_the_same_instant_in_utc(table):
    tz = timezone(timedelta(hours=5, minutes=30))
    when = datetime(2021, 6, 8, 12, 0, 0, tzinfo=tz)
    got = roundtrip(table, d=when)["d"]
    assert got == when
    assert got.utcoffset() == timedelta(0)
    assert got.hour == 6 and got.minute == 30


def test_datetime_with_whole_seconds_round_trips(table):
    when = datetime(1970, 10, 4, tzinfo=timezone.utc)
    assert roundtrip(table, d=when)["d"] == when


def test_datetime_milliseconds_round_trip(table):
    when = datetime(2011, 11, 4, 0, 5, 23, 283000, tzinfo=timezone.utc)
    assert roundtrip(table, d=when)["d"] == when


def test_datetime_comes_back_as_tables_entity_datetime(table):
    got = roundtrip(table, d=datetime(2020, 1, 1, tzinfo=timezone.utc))["d"]
    assert isinstance(got, TablesEntityDatetime)
    assert isinstance(got, datetime)


@pytest.mark.not_on_azurite(reason="Azurite echoes datetimes as written (.283000Z); Azure drops trailing zeros (.283Z)")
def test_datetime_service_value_drops_trailing_zeros(table):
    got = roundtrip(table, d=datetime(2011, 11, 4, 0, 5, 23, 283000, tzinfo=timezone.utc))["d"]
    assert got.tables_service_value == "2011-11-04T00:05:23.283Z"


@pytest.mark.not_on_azurite(reason="Azurite echoes datetimes as written (.000000Z); Azure drops a zero fraction entirely")
def test_datetime_without_fraction_has_no_fraction_in_service_value(table):
    got = roundtrip(table, d=datetime(2011, 11, 4, 0, 5, 23, tzinfo=timezone.utc))["d"]
    assert got.tables_service_value == "2011-11-04T00:05:23Z"


def test_datetime_string_with_seven_digit_precision_round_trips_exactly(table):
    # Azure stores 100ns ticks; python datetime only has microseconds, so the
    # SDK exposes the exact text as tables_service_value.
    got = roundtrip(table, d=EntityProperty("2020-01-02T03:04:05.1234567Z", EdmType.DATETIME))["d"]
    assert got.tables_service_value == "2020-01-02T03:04:05.1234567Z"
    assert got.microsecond == 123456


def test_round_tripped_datetime_keeps_sub_microsecond_digits_when_written_back(table):
    table.create_entity({**KEYS, "d": EntityProperty("2020-01-02T03:04:05.1234567Z", EdmType.DATETIME)})
    got = table.get_entity("p", "r")
    table.upsert_entity({**KEYS, "d": got["d"]}, mode="replace")
    again = table.get_entity("p", "r")["d"]
    assert again.tables_service_value == "2020-01-02T03:04:05.1234567Z"


def test_datetime_far_past_and_future_round_trip(table):
    got = roundtrip(table, old=datetime(1601, 1, 1, tzinfo=timezone.utc),
                    new=datetime(9999, 12, 31, 23, 59, 59, tzinfo=timezone.utc))
    assert got["old"] == datetime(1601, 1, 1, tzinfo=timezone.utc)
    assert got["new"] == datetime(9999, 12, 31, 23, 59, 59, tzinfo=timezone.utc)


# -- guid / binary ------------------------------------------------------------------


def test_uuid_round_trips_as_uuid(table):
    value = uuid.uuid4()
    got = roundtrip(table, g=value)["g"]
    assert got == value
    assert isinstance(got, uuid.UUID)


def test_bytes_round_trip_as_bytes(table):
    got = roundtrip(table, b=b"\x00\x01\xfe\xffbinary")["b"]
    assert got == b"\x00\x01\xfe\xffbinary"
    assert isinstance(got, bytes)


def test_empty_bytes_round_trip(table):
    assert roundtrip(table, b=b"")["b"] == b""


def test_large_binary_round_trips(table):
    data = bytes(range(256)) * 200  # 51200 bytes, under the 64 KiB property limit
    assert roundtrip(table, b=data)["b"] == data


# -- None and property names ------------------------------------------------------------


def test_none_value_drops_the_property(table):
    got = roundtrip(table, kept="yes", gone=None)
    assert got["kept"] == "yes"
    assert "gone" not in got


def test_replace_with_none_removes_an_existing_property(table):
    table.create_entity({**KEYS, "a": 1, "b": 2})
    table.upsert_entity({**KEYS, "a": 1, "b": None}, mode="replace")
    assert "b" not in table.get_entity("p", "r")


def test_unicode_property_name_round_trips(table):
    got = roundtrip(table, **{"啊齄丂狛狜": "ꀕ"})
    assert got["啊齄丂狛狜"] == "ꀕ"


def test_property_name_with_underscore_and_digits_round_trips(table):
    got = roundtrip(table, **{"my_prop_2": 1, "_leading": 2})
    assert got["my_prop_2"] == 1
    assert got["_leading"] == 2


def test_property_names_are_case_sensitive(table):
    got = roundtrip(table, **{"Name": "upper", "name": "lower"})
    assert got["Name"] == "upper"
    assert got["name"] == "lower"


@pytest.mark.not_on_azurite(reason="Azurite accepts property names that start with a digit")
def test_property_name_that_starts_with_a_digit_is_rejected(table):
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({**KEYS, "123": 456})
    assert exc.value.status_code == 400
    assert code(exc.value) == "PropertyNameInvalid"


@pytest.mark.not_on_azurite(reason="Azurite does not enforce the 255 character property name limit")
def test_property_name_longer_than_255_characters_is_rejected(table):
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({**KEYS, "a" * 256: "v"})
    assert exc.value.status_code == 400
    assert code(exc.value) == "PropertyNameTooLong"


def test_property_name_of_255_characters_is_allowed(table):
    name = "a" * 255
    assert roundtrip(table, **{name: "v"})[name] == "v"


def test_252_custom_properties_are_allowed(table):
    props = {f"key{i}": f"value{i}" for i in range(252)}
    got = roundtrip(table, **props)
    assert got["key0"] == "value0"
    assert got["key251"] == "value251"
    assert len([k for k in got if k.startswith("key")]) == 252


@pytest.mark.not_on_azurite(reason="Azurite does not enforce the 252 custom property limit")
def test_253_custom_properties_raise_too_many_properties(table):
    props = {f"key{i}": f"value{i}" for i in range(253)}
    with pytest.raises(HttpResponseError) as exc:
        table.create_entity({**KEYS, **props})
    assert exc.value.status_code == 400
    assert code(exc.value) == "TooManyProperties"


# -- mixing types ---------------------------------------------------------------------


def test_entity_with_every_type_round_trips_together(table):
    g = uuid.uuid4()
    when = datetime(2020, 5, 6, 7, 8, 9, 10000, tzinfo=timezone.utc)
    got = roundtrip(
        table, s="str", i=1, big=EntityProperty(1 << 50, EdmType.INT64), f=2.5, flag=True,
        d=when, g=g, b=b"\x01\x02", empty="", gone=None,
    )
    assert got["s"] == "str" and got["i"] == 1 and got["f"] == 2.5 and got["flag"] is True
    assert got["big"].value == 1 << 50
    assert got["d"] == when and got["g"] == g and got["b"] == b"\x01\x02" and got["empty"] == ""
    assert "gone" not in got


def test_property_can_change_type_on_replace(table):
    table.create_entity({**KEYS, "v": 1})
    table.upsert_entity({**KEYS, "v": uuid.UUID(int=5)}, mode="replace")
    assert table.get_entity("p", "r")["v"] == uuid.UUID(int=5)


def test_types_survive_a_query_as_well_as_a_get(table):
    g = uuid.uuid4()
    table.create_entity({**KEYS, "g": g, "b": b"xy", "big": EntityProperty(9, EdmType.INT64)})
    got = list(table.query_entities("PartitionKey eq 'p'"))[0]
    assert got["g"] == g
    assert got["b"] == b"xy"
    assert got["big"].value == 9


def test_table_entity_is_a_dict(table):
    got = roundtrip(table, a=1)
    assert isinstance(got, TableEntity)
    assert isinstance(got, dict)
