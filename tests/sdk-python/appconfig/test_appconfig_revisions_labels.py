"""Revision history (GET /revisions) and labels (GET /labels)."""

import time
from datetime import datetime, timezone

import pytest
from azure.appconfiguration import ConfigurationSetting
from azure.core.exceptions import HttpResponseError

from appconfigutil import status_of


def _set(client, key, label=None, value="v", **kw):
    return client.set_configuration_setting(ConfigurationSetting(key=key, label=label, value=value, **kw))


# --- revisions -------------------------------------------------------------


def test_each_write_adds_a_revision(client, uniq):
    key = uniq("rev")
    for i in range(3):
        _set(client, key, value=f"v{i}")
    revs = list(client.list_revisions(key_filter=key))
    assert sorted(r.value for r in revs) == ["v0", "v1", "v2"]


def test_revisions_are_newest_first(client, uniq):
    # Revision time has one second resolution and ties inside a second are NOT in
    # write order on real Azure (recordings show them sorted by etag), so leave a
    # second between the writes.
    key = uniq("revord")
    _set(client, key, value="first")
    time.sleep(1.1)
    _set(client, key, value="second")
    time.sleep(1.1)
    _set(client, key, value="third")
    assert [r.value for r in client.list_revisions(key_filter=key)] == ["third", "second", "first"]


def test_revisions_carry_the_full_setting_of_that_time(client, uniq):
    key = uniq("revfull")
    a = _set(client, key, "L", "v1", content_type="text/plain", tags={"n": "1"})
    b = _set(client, key, "L", "v2", content_type="text/x", tags={"n": "2"})
    by_value = {r.value: r for r in client.list_revisions(key_filter=key, label_filter="L")}
    assert by_value["v1"].etag == a.etag and by_value["v2"].etag == b.etag
    assert (by_value["v1"].content_type, by_value["v1"].tags) == ("text/plain", {"n": "1"})
    assert (by_value["v2"].content_type, by_value["v2"].tags) == ("text/x", {"n": "2"})
    assert all(r.label == "L" and r.last_modified for r in by_value.values())


def test_revisions_are_kept_after_the_setting_is_deleted(client, uniq):
    key = uniq("revdel")
    _set(client, key, value="v1")
    _set(client, key, value="v2")
    client.delete_configuration_setting(key)
    assert sorted(r.value for r in client.list_revisions(key_filter=key)) == ["v1", "v2"]


def test_revisions_filter_by_exact_key_excludes_other_keys(client, uniq):
    a, b = uniq("ra"), uniq("rb")
    _set(client, a, value="a")
    _set(client, b, value="b")
    assert [r.key for r in client.list_revisions(key_filter=a)] == [a]


def test_revisions_filter_by_key_prefix(client, uniq):
    p = uniq("rp")
    _set(client, f"{p}-1")
    _set(client, f"{p}-2")
    _set(client, uniq("other"))
    assert sorted(r.key for r in client.list_revisions(key_filter=f"{p}-*")) == [f"{p}-1", f"{p}-2"]


def test_revisions_filter_by_label(client, uniq):
    key = uniq("rl")
    _set(client, key, None, "plain")
    _set(client, key, "A", "a1")
    _set(client, key, "A", "a2")
    _set(client, key, "B", "b1")
    assert sorted(r.value for r in client.list_revisions(key_filter=key, label_filter="A")) == ["a1", "a2"]
    assert [r.value for r in client.list_revisions(key_filter=key, label_filter="\0")] == ["plain"]
    assert len(list(client.list_revisions(key_filter=key))) == 4


def test_revisions_positional_key_and_label_arguments(client, uniq):
    key = uniq("rpos")
    _set(client, key, "A", "a")
    _set(client, key, "B", "b")
    assert [r.value for r in client.list_revisions(key, "B")] == ["b"]


def test_revisions_filter_by_tags(client, uniq):
    key = uniq("rt")
    _set(client, key, value="tagged", tags={"env": "prod"})
    _set(client, key, value="untagged")
    got = list(client.list_revisions(key_filter=key, tags_filter=["env=prod"]))
    assert [r.value for r in got] == ["tagged"]


def test_revisions_field_selection(client, uniq):
    key = uniq("rf")
    _set(client, key, value="v", content_type="text/plain")
    [rev] = list(client.list_revisions(key_filter=key, fields=["key", "content_type"]))
    assert rev.key == key and rev.content_type == "text/plain"
    assert rev.value is None and rev.etag is None


def test_revisions_accept_datetime_hides_later_revisions(client, uniq):
    key = uniq("rpit")
    _set(client, key, value="old")
    time.sleep(1.1)
    probe = datetime.now(timezone.utc)
    time.sleep(1.1)
    _set(client, key, value="new")
    assert [r.value for r in client.list_revisions(key_filter=key, accept_datetime=probe)] == ["old"]
    assert sorted(r.value for r in client.list_revisions(key_filter=key)) == ["new", "old"]


def test_revisions_of_a_store_with_nothing_is_empty(client, uniq):
    assert list(client.list_revisions(key_filter=uniq("none"))) == []


def test_revisions_page_past_100(client, uniq):
    # Recordings: revision pages hold ~100 items and link to the next page.
    key = uniq("rpg")
    for i in range(105):
        _set(client, key, value=f"v{i}")
    revs = list(client.list_revisions(key_filter=key))
    assert len(revs) == 105
    assert {r.value for r in revs} == {f"v{i}" for i in range(105)}


def test_raw_revisions_response_shape(raw, uniq):
    key = uniq("rraw")
    raw.put_kv(key, value="v1")
    raw.put_kv(key, value="v2")
    r = raw.request("GET", "/revisions", params={"key": key})
    assert r.status_code == 200
    assert r.headers["Content-Type"] == "application/vnd.microsoft.appconfig.kvset+json; charset=utf-8"
    assert r.headers["Accept-Ranges"] == "items"
    body = r.json()
    assert len(body["items"]) == 2
    assert r.headers["ETag"] == f'"{body["etag"]}"'


# --- labels ----------------------------------------------------------------


def test_list_labels_is_empty_without_settings(client):
    assert list(client.list_labels()) == []


def test_list_labels_returns_each_label_once_with_none_first(client, uniq):
    k1, k2 = uniq("l1"), uniq("l2")
    _set(client, k1, None)
    _set(client, k1, "beta")
    _set(client, k2, "beta")
    _set(client, k2, "alpha")
    assert [l.name for l in client.list_labels()] == [None, "alpha", "beta"]


def test_list_labels_without_unlabelled_settings_has_no_none_entry(client, uniq):
    _set(client, uniq("l"), "only")
    assert [l.name for l in client.list_labels()] == ["only"]


def test_list_labels_name_prefix_filter(client, uniq):
    key = uniq("lp")
    for label in ("test-1", "test-2", "prod"):
        _set(client, key, label)
    assert [l.name for l in client.list_labels(name="test*")] == ["test-1", "test-2"]


def test_list_labels_exact_name_filter(client, uniq):
    key = uniq("le")
    for label in ("prod", "prod2"):
        _set(client, key, label)
    assert [l.name for l in client.list_labels(name="prod")] == ["prod"]


def test_list_labels_name_filter_without_match_is_empty(client, uniq):
    _set(client, uniq("ln"), "present")
    assert list(client.list_labels(name="absent")) == []


def test_list_labels_pages_past_100(client, uniq):
    key = uniq("lpg")
    for i in range(120):
        _set(client, key, f"label-{i:03d}")
    names = [l.name for l in client.list_labels()]
    assert names == [f"label-{i:03d}" for i in range(120)]


def test_list_labels_with_an_invalid_filter_is_a_400(client):
    # Recordings: name="test'@*$!%" -> 400 problem+json (invalid-argument).
    with pytest.raises(HttpResponseError) as exc:
        list(client.list_labels(name="test'@*$!%"))
    assert status_of(exc) == 400


def test_raw_invalid_label_filter_is_problem_json(raw):
    r = raw.request("GET", "/labels", params={"name": "test'@*$!%"})
    assert r.status_code == 400
    assert r.headers["Content-Type"] == "application/problem+json; charset=utf-8"
    body = r.json()
    assert body["type"] == "https://azconfig.io/errors/invalid-argument"
    assert body["status"] == 400
    assert body["title"].startswith("Invalid request parameter")


def test_raw_labels_response_shape(raw, uniq):
    raw.put_kv(uniq(), label="x", value="v")
    r = raw.request("GET", "/labels")
    assert r.status_code == 200
    assert r.headers["Content-Type"] == "application/vnd.microsoft.appconfig.labelset+json; charset=utf-8"
    assert r.json() == {"items": [{"name": "x"}]}
