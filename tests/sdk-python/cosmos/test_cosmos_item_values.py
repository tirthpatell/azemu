"""What goes in comes out: JSON value fidelity."""

import pytest

from cosmosutil import run


def rt(container, **props):
    """Create then read an item; return the stored properties."""
    container.create_item({"id": "doc", "pk": "x", **props})
    return container.read_item("doc", "x")


def test_nested_documents_round_trip(container):
    nested = {"a": {"b": {"c": {"d": [1, {"e": "f"}]}}}}
    assert rt(container, **nested)["a"] == nested["a"]


def test_integers_stay_integers(container):
    got = rt(container, n=42, zero=0, neg=-7)
    assert got["n"] == 42 and isinstance(got["n"], int)
    assert isinstance(got["zero"], int) and got["neg"] == -7


def test_floats_stay_floats(container):
    got = rt(container, f=1.5, g=0.1, h=-2.25)
    assert got["f"] == 1.5 and got["g"] == 0.1 and got["h"] == -2.25
    assert isinstance(got["f"], float)


def test_float_with_zero_fraction_compares_equal_to_int(container):
    assert rt(container, f=3.0)["f"] == 3


def test_large_integers_up_to_2_pow_53(container):
    big = 2**53
    assert rt(container, big=big, neg=-big)["big"] == big


def test_very_small_and_very_large_doubles(container):
    got = rt(container, tiny=1e-300, huge=1e300)
    assert got["tiny"] == 1e-300 and got["huge"] == 1e300


def test_booleans(container):
    got = rt(container, t=True, f=False)
    assert got["t"] is True and got["f"] is False


def test_null_values_are_kept(container):
    got = rt(container, nothing=None)
    assert "nothing" in got and got["nothing"] is None


def test_empty_string_is_kept(container):
    assert rt(container, s="")["s"] == ""


def test_empty_array_and_empty_object_are_kept(container):
    got = rt(container, a=[], o={})
    assert got["a"] == [] and got["o"] == {}


def test_arrays_keep_order_and_mixed_types(container):
    arr = [3, "two", None, True, {"k": 1}, [1, 2], 1.5]
    assert rt(container, arr=arr)["arr"] == arr


def test_nested_arrays(container):
    arr = [[1, 2], [3, [4, 5]], []]
    assert rt(container, arr=arr)["arr"] == arr


def test_unicode_strings(container):
    s = "café 中文 русский \U0001F600 ü"
    assert rt(container, s=s)["s"] == s


def test_unicode_property_names(container):
    got = rt(container, **{"été": 1, "中": 2})
    assert got["été"] == 1 and got["中"] == 2


def test_string_escapes_and_control_characters(container):
    s = 'quote " backslash \\ slash / newline \n tab \t'
    assert rt(container, s=s)["s"] == s


def test_property_names_with_spaces_and_symbols(container):
    got = rt(container, **{"with space": 1, "dash-ed": 2, "dot.ted": 3, "$dollar": 4})
    assert (got["with space"], got["dash-ed"], got["dot.ted"], got["$dollar"]) == (1, 2, 3, 4)


def test_property_order_is_preserved(container):
    got = rt(container, z=1, a=2, m=3)
    user_keys = [k for k in got if not k.startswith("_")]
    assert user_keys == ["id", "pk", "z", "a", "m"]


def test_long_string_value(container):
    s = "x" * 100_000
    assert rt(container, s=s)["s"] == s


def test_deeply_nested_document(container):
    d = cur = {}
    for _ in range(20):
        cur["n"] = {}
        cur = cur["n"]
    cur["leaf"] = 1
    got = rt(container, root=d)["root"]
    for _ in range(20):
        got = got["n"]
    assert got["leaf"] == 1


def test_many_properties(container):
    props = {f"p{i}": i for i in range(300)}
    got = rt(container, **props)
    assert all(got[k] == v for k, v in props.items())


def test_id_with_spaces_and_unicode_can_be_read(container):
    for i in ("with space", "café", "中文", "percent%20sign", "a.b-c_d"):
        container.create_item({"id": i, "pk": "x"})
        assert container.read_item(i, "x")["id"] == i


def test_id_is_case_sensitive(container):
    container.create_item({"id": "Abc", "pk": "x"})
    container.create_item({"id": "abc", "pk": "x"})
    assert len(list(container.read_all_items())) == 2


def test_system_property_names_in_body_are_ignored_or_overwritten(container):
    got = rt(container, _ts=1, _etag="fake")
    assert got["_ts"] != 1 and got["_etag"] != "fake"


def test_numbers_are_queryable_by_value_regardless_of_int_or_float_form(container):
    container.create_item({"id": "a", "pk": "x", "n": 2})
    container.create_item({"id": "b", "pk": "x", "n": 2.0})
    assert sorted(i["id"] for i in run(container, "SELECT c.id FROM c WHERE c.n = 2")) == ["a", "b"]
