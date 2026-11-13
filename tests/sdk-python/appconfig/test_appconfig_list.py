"""list_configuration_settings: filters, field selection, paging, ordering, point-in-time reads."""

import time
from datetime import datetime, timezone
from email.utils import format_datetime

import pytest
from azure.appconfiguration import ConfigurationSetting, ConfigurationSettingFields
from azure.core.exceptions import HttpResponseError

from appconfigutil import next_link, seed


def _keys(items):
    return [s.key for s in items]


def _set(client, key, label=None, value="v", **kw):
    return client.set_configuration_setting(ConfigurationSetting(key=key, label=label, value=value, **kw))


# --- basics ----------------------------------------------------------------


def test_list_on_a_store_with_nothing_returns_nothing(client):
    assert list(client.list_configuration_settings()) == []


def test_list_returns_every_setting_with_all_fields(client, uniq):
    p = uniq("all")
    _set(client, f"{p}-a", "L", "va", content_type="text/plain", tags={"t": "1"})
    _set(client, f"{p}-b", None, "vb")
    got = {s.key: s for s in client.list_configuration_settings()}
    assert set(got) == {f"{p}-a", f"{p}-b"}
    a = got[f"{p}-a"]
    assert (a.label, a.value, a.content_type, a.tags, a.read_only) == ("L", "va", "text/plain", {"t": "1"}, False)
    assert a.etag and a.last_modified
    assert got[f"{p}-b"].label is None


def test_list_results_are_ordered_by_key_in_ordinal_order(client, uniq):
    # Recordings: multi_0, multi_100, multi_10, multi_11 ... (plain byte order, not natural order).
    p = uniq("ord")
    keys = [f"{p}-{n}x" for n in (2, 10, 1, 100, 0, 11)]
    for k in keys:
        _set(client, k)
    listed = _keys(client.list_configuration_settings(key_filter=f"{p}-*"))
    assert listed == sorted(keys)
    assert listed == [f"{p}-{n}x" for n in (0, 100, 10, 11, 1, 2)]


def test_list_results_are_ordered_by_key_then_label_with_no_label_first(client, uniq):
    key = uniq("lblord")
    for label in ("b", None, "a"):
        _set(client, key, label)
    assert [s.label for s in client.list_configuration_settings(key_filter=key)] == [None, "a", "b"]


# --- key filter ------------------------------------------------------------


def test_key_filter_exact_returns_all_labels_of_that_key_only(client, uniq):
    p = uniq("kf")
    _set(client, f"{p}", None)
    _set(client, f"{p}", "L")
    _set(client, f"{p}-other")
    got = list(client.list_configuration_settings(key_filter=p))
    assert [(s.key, s.label) for s in got] == [(p, None), (p, "L")]


def test_key_filter_prefix_wildcard(client, uniq):
    p = uniq("pre")
    for suffix in ("a", "b", "c"):
        _set(client, f"{p}/{suffix}")
    _set(client, f"x{p}/a")
    assert _keys(client.list_configuration_settings(key_filter=f"{p}/*")) == [f"{p}/a", f"{p}/b", f"{p}/c"]


def test_key_filter_star_matches_everything(client, uniq):
    keys = [uniq("any") for _ in range(3)]
    for k in keys:
        _set(client, k)
    assert sorted(_keys(client.list_configuration_settings(key_filter="*"))) == sorted(keys)


def test_key_filter_suffix_and_contains_wildcards(client, uniq):
    p = uniq("sfx")
    _set(client, f"{p}-alpha.json")
    _set(client, f"{p}-beta.json")
    _set(client, f"{p}-gamma.yaml")
    assert _keys(client.list_configuration_settings(key_filter=f"*{p}-alpha.json")) == [f"{p}-alpha.json"]
    assert _keys(client.list_configuration_settings(key_filter="*.json")) == [
        f"{p}-alpha.json",
        f"{p}-beta.json",
    ]
    assert _keys(client.list_configuration_settings(key_filter=f"*{p}-gam*")) == [f"{p}-gamma.yaml"]


def test_key_filter_comma_separated_list_is_an_or(client, uniq):
    a, b, c = uniq("a"), uniq("b"), uniq("c")
    for k in (a, b, c):
        _set(client, k)
    assert sorted(_keys(client.list_configuration_settings(key_filter=f"{a},{c}"))) == sorted([a, c])


def test_key_filter_list_can_mix_exact_and_prefix(client, uniq):
    a, p = uniq("exact"), uniq("pfx")
    _set(client, a)
    _set(client, f"{p}-1")
    _set(client, f"{p}-2")
    _set(client, uniq("zzz"))
    got = _keys(client.list_configuration_settings(key_filter=f"{a},{p}*"))
    assert got == sorted([a, f"{p}-1", f"{p}-2"])


def test_key_filter_with_escaped_comma_and_star_matches_the_literal_characters(client, uniq):
    p = uniq("esc")
    weird = f"{p},a*b"
    _set(client, weird)
    _set(client, f"{p},aXb")  # would match an unescaped wildcard
    _set(client, f"{p}")  # would match an unescaped comma split
    assert _keys(client.list_configuration_settings(key_filter=f"{p}\\,a\\*b")) == [weird]


def test_key_filter_without_match_is_empty(client, uniq):
    _set(client, uniq("present"))
    assert list(client.list_configuration_settings(key_filter=uniq("absent"))) == []


def test_key_filter_is_case_sensitive(client, uniq):
    p = uniq("Case")
    _set(client, p)
    assert list(client.list_configuration_settings(key_filter=p.lower())) == []


# --- label filter ----------------------------------------------------------


@pytest.fixture
def labelled(client, uniq):
    """One key under four label states: none, 'dev', 'dev2', 'prod'."""
    key = uniq("lf")
    for label in (None, "dev", "dev2", "prod"):
        _set(client, key, label, value=f"v-{label}")
    return key


def test_label_filter_default_returns_all_labels(client, labelled):
    assert len(list(client.list_configuration_settings(key_filter=labelled))) == 4


def test_label_filter_exact(client, labelled):
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="dev"))
    assert [s.label for s in got] == ["dev"]


def test_label_filter_nul_means_no_label(client, labelled):
    # Recordings: label_filter="\0" goes over the wire as label=%00.
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="\0"))
    assert [s.label for s in got] == [None]


def test_label_filter_star_returns_all_labels_including_none(client, labelled):
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="*"))
    assert [s.label for s in got] == [None, "dev", "dev2", "prod"]


def test_label_filter_prefix(client, labelled):
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="dev*"))
    assert [s.label for s in got] == ["dev", "dev2"]


def test_label_filter_comma_separated_list(client, labelled):
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="dev,prod"))
    assert [s.label for s in got] == ["dev", "prod"]


def test_label_filter_list_with_nul_includes_the_unlabelled_one(client, labelled):
    got = list(client.list_configuration_settings(key_filter=labelled, label_filter="\0,prod"))
    assert [s.label for s in got] == [None, "prod"]


def test_label_filter_with_escaped_reserved_characters(client, uniq):
    # Recordings (test_list_configuration_settings_reserved_chars): label "x_*, \1" found with an escaped filter.
    key = uniq("rsv")
    label = "lbl_*, \\1"
    _set(client, key, label)
    _set(client, key, "lbl_plain")
    got = list(client.list_configuration_settings(key_filter=key, label_filter="lbl_\\*\\, \\\\1"))
    assert [s.label for s in got] == [label]


def test_label_filter_without_match_is_empty(client, labelled):
    assert list(client.list_configuration_settings(key_filter=labelled, label_filter="nope")) == []


# --- tags filter -----------------------------------------------------------


def test_tags_filter_returns_only_settings_with_that_tag(client, uniq):
    p = uniq("tg")
    _set(client, f"{p}-1", tags={"env": "prod", "team": "a"})
    _set(client, f"{p}-2", tags={"env": "dev"})
    _set(client, f"{p}-3")
    got = client.list_configuration_settings(key_filter=f"{p}-*", tags_filter=["env=prod"])
    assert _keys(got) == [f"{p}-1"]


def test_tags_filter_with_two_tags_requires_both(client, uniq):
    p = uniq("tg2")
    _set(client, f"{p}-1", tags={"env": "prod", "team": "a"})
    _set(client, f"{p}-2", tags={"env": "prod", "team": "b"})
    got = client.list_configuration_settings(key_filter=f"{p}-*", tags_filter=["env=prod", "team=a"])
    assert _keys(got) == [f"{p}-1"]


def test_tags_filter_without_match_is_empty(client, uniq):
    key = uniq("tg3")
    _set(client, key, tags={"env": "prod"})
    assert list(client.list_configuration_settings(key_filter=key, tags_filter=["env=invalid"])) == []


# --- field selection -------------------------------------------------------


def test_fields_selection_returns_only_the_selected_fields(client, uniq):
    # Recordings: $select=key,content_type -> items contain exactly those two fields.
    key = uniq("sel")
    _set(client, key, "L", "secret value", content_type="text/plain", tags={"a": "b"})
    [got] = list(client.list_configuration_settings(key_filter=key, fields=["key", "content_type"]))
    assert got.key == key
    assert got.content_type == "text/plain"
    assert got.value is None
    assert got.label is None
    assert got.etag is None
    assert got.last_modified is None


def test_fields_selection_accepts_the_enum_and_read_only_alias(client, uniq):
    key = uniq("sel2")
    _set(client, key, value="the value")
    [got] = list(
        client.list_configuration_settings(
            key_filter=key, fields=[ConfigurationSettingFields.KEY, ConfigurationSettingFields.VALUE, "read_only"]
        )
    )
    assert (got.key, got.value) == (key, "the value")
    assert got.read_only is False
    assert got.etag is None


# --- point in time ---------------------------------------------------------


def test_list_accept_datetime_before_anything_existed_is_empty(client, uniq):
    _set(client, uniq("old"))
    assert list(client.list_configuration_settings(accept_datetime=datetime(2000, 4, 1, 9, 30, 45, tzinfo=timezone.utc))) == []


def test_list_accept_datetime_accepts_an_rfc1123_string(client, uniq):
    _set(client, uniq("old"))
    stamp = format_datetime(datetime(2000, 4, 1, 9, 30, 45, tzinfo=timezone.utc), usegmt=True)
    assert stamp == "Sat, 01 Apr 2000 09:30:45 GMT"
    assert list(client.list_configuration_settings(accept_datetime=stamp)) == []


def test_list_accept_datetime_shows_the_store_as_it_was(client, uniq):
    p = uniq("pit")
    _set(client, f"{p}-a", value="a-old")
    _set(client, f"{p}-b", value="b-old")
    time.sleep(1.1)
    probe = datetime.now(timezone.utc)
    time.sleep(1.1)
    _set(client, f"{p}-a", value="a-new")
    client.delete_configuration_setting(f"{p}-b")
    _set(client, f"{p}-c", value="c-new")
    then = {s.key: s.value for s in client.list_configuration_settings(key_filter=f"{p}-*", accept_datetime=probe)}
    now = {s.key: s.value for s in client.list_configuration_settings(key_filter=f"{p}-*")}
    assert then == {f"{p}-a": "a-old", f"{p}-b": "b-old"}
    assert now == {f"{p}-a": "a-new", f"{p}-c": "c-new"}


# --- paging ----------------------------------------------------------------


def test_list_over_one_page_returns_every_setting(client, uniq):
    keys = seed(client, uniq("pg"), 230)
    assert _keys(client.list_configuration_settings(key_filter="*")) == keys


def test_pages_hold_at_most_100_settings(client, uniq):
    # Recordings: 100 per page, then the remainder.
    seed(client, uniq("pgsz"), 230)
    sizes = [len(list(page)) for page in client.list_configuration_settings().by_page()]
    assert sizes == [100, 100, 30]


def test_exactly_100_settings_come_back_complete(client, uniq):
    keys = seed(client, uniq("pg100"), 100)
    assert _keys(client.list_configuration_settings()) == keys


def test_paging_keeps_the_filters_on_later_pages(client, uniq):
    p = uniq("pgf")
    wanted = seed(client, f"{p}-yes", 120)
    seed(client, f"{p}-no", 30)
    assert _keys(client.list_configuration_settings(key_filter=f"{p}-yes*")) == wanted


def test_paging_with_field_selection_keeps_the_selection(client, uniq):
    seed(client, uniq("pgsel"), 120)
    got = list(client.list_configuration_settings(fields=["key"]))
    assert len(got) == 120
    assert all(s.value is None and s.key for s in got)


def test_raw_first_page_links_to_the_next_page(raw, client, uniq):
    p = uniq("lnk")
    keys = seed(client, p, 120)
    first = raw.request("GET", "/kv", params={"key": f"{p}*"})
    assert first.status_code == 200
    body = first.json()
    assert len(body["items"]) == 100
    link = next_link(first)
    assert link and link.startswith("/kv?") and "after=" in link
    assert body["@nextLink"] == link
    second = raw.request("GET", "/kv", params=_query_of(link))
    assert second.status_code == 200
    got = [i["key"] for i in body["items"]] + [i["key"] for i in second.json()["items"]]
    assert got == keys
    assert "@nextLink" not in second.json()
    assert next_link(second) is None


def _query_of(link):
    from urllib.parse import parse_qsl, urlsplit

    return dict(parse_qsl(urlsplit(link).query, keep_blank_values=True))


def test_raw_last_page_has_no_next_link(raw, client, uniq):
    p = uniq("last")
    seed(client, p, 3)
    r = raw.request("GET", "/kv", params={"key": f"{p}*"})
    assert "@nextLink" not in r.json()
    assert next_link(r) is None


# --- etag based change detection (by_page match_conditions) ------------------


def _collect_etags(paged):
    pager = paged.by_page()
    etags = []
    for page in pager:
        list(page)
        etags.append(pager.etag)
    return etags


def test_page_etags_are_stable_while_nothing_changes(client, uniq):
    p = uniq("et")
    seed(client, p, 120)
    first = _collect_etags(client.list_configuration_settings(key_filter=f"{p}*"))
    second = _collect_etags(client.list_configuration_settings(key_filter=f"{p}*"))
    assert len(first) == 2
    assert first == second
    assert first[0] != first[1]


def test_by_page_with_match_conditions_skips_unchanged_pages(client, uniq):
    p = uniq("mon")
    seed(client, p, 120)
    etags = _collect_etags(client.list_configuration_settings(key_filter=f"{p}*"))
    changed = list(client.list_configuration_settings(key_filter=f"{p}*").by_page(match_conditions=etags))
    assert changed == []


def test_by_page_with_match_conditions_returns_only_the_changed_page(client, uniq):
    p = uniq("mon2")
    keys = seed(client, p, 120)
    etags = _collect_etags(client.list_configuration_settings(key_filter=f"{p}*"))
    target = keys[-1]  # lives on page 2
    _set(client, target, value="changed")
    pages = [list(pg) for pg in client.list_configuration_settings(key_filter=f"{p}*").by_page(match_conditions=etags)]
    assert len(pages) == 1
    assert [s.key for s in pages[0]][-1] == target
    assert pages[0][-1].value == "changed"


def test_by_page_with_match_conditions_reports_a_change_on_the_first_page(client, uniq):
    p = uniq("mon3")
    keys = seed(client, p, 120)
    etags = _collect_etags(client.list_configuration_settings(key_filter=f"{p}*"))
    _set(client, keys[0], value="changed")
    pages = [list(pg) for pg in client.list_configuration_settings(key_filter=f"{p}*").by_page(match_conditions=etags)]
    assert len(pages) == 1
    assert pages[0][0].value == "changed"
    assert len(pages[0]) == 100


# --- check_configuration_settings (HEAD /kv) -----------------------------------


def test_check_configuration_settings_pages_have_etags_and_no_items(client, uniq):
    p = uniq("hd")
    seed(client, p, 120)
    pager = client.check_configuration_settings(key_filter=f"{p}*").by_page()
    seen = []
    for page in pager:
        assert list(page) == []
        seen.append(pager.etag)
    assert len(seen) == 2 and all(seen)


def test_check_configuration_settings_detects_a_changed_page(client, uniq):
    p = uniq("hd2")
    keys = seed(client, p, 120)
    pager = client.check_configuration_settings(key_filter=f"{p}*").by_page()
    etags = []
    for _ in pager:
        etags.append(pager.etag)
    unchanged = list(client.check_configuration_settings(key_filter=f"{p}*").by_page(match_conditions=etags))
    assert unchanged == []
    _set(client, keys[-1], value="changed")
    changed = list(client.check_configuration_settings(key_filter=f"{p}*").by_page(match_conditions=etags))
    assert len(changed) == 1


# --- limits -------------------------------------------


def test_more_than_five_tag_filters_or_comma_values_are_refused(client, uniq):
    # Learn, REST API key-value: "limited to 5 tag filters" and "(limited to 5 CSV)"
    p = uniq("lim")
    _set(client, f"{p}-1", tags={"a": "1"})
    five = [f"t{i}=1" for i in range(5)]
    assert list(client.list_configuration_settings(key_filter=f"{p}-1", tags_filter=five)) == []
    with pytest.raises(HttpResponseError) as e:
        list(client.list_configuration_settings(key_filter=f"{p}-1", tags_filter=five + ["t5=1"]))
    assert e.value.status_code == 400
    with pytest.raises(HttpResponseError) as e:
        list(client.list_configuration_settings(key_filter=",".join(f"k{i}" for i in range(6))))
    assert e.value.status_code == 400


def test_a_key_keeps_its_revisions_and_every_revision_list_is_paged_by_100(client, uniq):
    key = uniq("rev")
    for i in range(130):
        _set(client, key, value=str(i))
    revs = list(client.list_revisions(key_filter=key))
    assert len(revs) == 130
    assert revs[0].value == "129" and revs[-1].value == "0"
    pages = [len(list(p)) for p in client.list_revisions(key_filter=key).by_page()]
    assert pages == [100, 30]
