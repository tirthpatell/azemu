"""Administration client (HTTPS): subscription rules (create / get / update / delete / list).

Expected values from Microsoft's test_mgmt_rules.py and its recordings: a new subscription has one rule `$Default` with
a TrueRuleFilter; typed correlation properties (str, int, long, bool, datetime, timedelta) round trip.
"""

from datetime import datetime, timedelta

import pytest
from azure.core.exceptions import HttpResponseError, ResourceExistsError, ResourceNotFoundError
from azure.servicebus.management import (
    CorrelationRuleFilter,
    FalseRuleFilter,
    RuleProperties,
    SqlRuleAction,
    SqlRuleFilter,
    TrueRuleFilter,
)

INT32_MAX = 2**31 - 1


@pytest.fixture
def ts(admin, new_topic, new_sub):
    topic = new_topic()
    return topic, new_sub(topic)


def test_new_subscription_has_one_default_true_rule(admin, ts):
    topic, sub = ts
    rules = list(admin.list_rules(topic, sub))
    assert [r.name for r in rules] == ["$Default"]
    assert type(rules[0].filter) is TrueRuleFilter
    assert rules[0].action is None


def test_get_default_rule(admin, ts):
    topic, sub = ts
    rule = admin.get_rule(topic, sub, "$Default")
    assert rule.name == "$Default"
    assert type(rule.filter) is TrueRuleFilter


def test_create_rule_without_filter_defaults_to_true_filter(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r")
    assert type(admin.get_rule(topic, sub, "r").filter) is TrueRuleFilter


def test_create_rule_returns_rule_properties(admin, ts):
    topic, sub = ts
    rule = admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a = 1"))
    assert isinstance(rule, RuleProperties)
    assert rule.name == "r"


def test_create_true_and_false_filter_rules(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "t", filter=TrueRuleFilter())
    admin.create_rule(topic, sub, "f", filter=FalseRuleFilter())
    assert type(admin.get_rule(topic, sub, "t").filter) is TrueRuleFilter
    assert type(admin.get_rule(topic, sub, "f").filter) is FalseRuleFilter


def test_sql_filter_round_trip_with_parameters(admin, ts):
    topic, sub = ts
    admin.create_rule(
        topic, sub, "r", filter=SqlRuleFilter("Priority = @param1 AND Level = @param2", parameters={"@param1": "str1", "@param2": 1})
    )
    f = admin.get_rule(topic, sub, "r").filter
    assert type(f) is SqlRuleFilter
    assert f.sql_expression == "Priority = @param1 AND Level = @param2"
    assert f.parameters["@param1"] == "str1"
    assert f.parameters["@param2"] == 1


def test_correlation_filter_round_trip_with_typed_properties(admin, ts):
    topic, sub = ts
    flt = CorrelationRuleFilter(
        correlation_id="testcid",
        message_id="mid",
        to="to",
        reply_to="rt",
        label="lbl",
        session_id="sid",
        reply_to_session_id="rsid",
        content_type="ct",
        properties={
            "key_string": "str1",
            "key_int": 2,
            "key_long": INT32_MAX + 3,
            "key_bool": False,
            "key_datetime": datetime(2020, 7, 5, 11, 12, 13),
            "key_duration": timedelta(days=1, hours=2, minutes=3),
        },
    )
    admin.create_rule(topic, sub, "r", filter=flt)
    got = admin.get_rule(topic, sub, "r").filter
    assert type(got) is CorrelationRuleFilter
    assert (got.correlation_id, got.message_id, got.to, got.reply_to) == ("testcid", "mid", "to", "rt")
    assert (got.label, got.session_id, got.reply_to_session_id, got.content_type) == ("lbl", "sid", "rsid", "ct")
    p = got.properties
    assert p["key_string"] == "str1" and p["key_int"] == 2 and p["key_long"] == INT32_MAX + 3
    assert p["key_bool"] is False
    assert p["key_datetime"] == datetime(2020, 7, 5, 11, 12, 13)
    assert p["key_duration"] == timedelta(days=1, hours=2, minutes=3)


def test_rule_action_round_trip(admin, ts):
    topic, sub = ts
    admin.create_rule(
        topic, sub, "r", filter=TrueRuleFilter(),
        action=SqlRuleAction("SET Priority = @param", parameters={"@param": datetime(2020, 7, 5, 11, 12, 13)}),
    )
    a = admin.get_rule(topic, sub, "r").action
    assert a.sql_expression == "SET Priority = @param"
    assert a.parameters["@param"] == datetime(2020, 7, 5, 11, 12, 13)


def test_rule_has_created_at(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r")
    assert admin.get_rule(topic, sub, "r").created_at_utc is not None


def test_create_duplicate_rule_raises_resource_exists(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r")
    with pytest.raises(ResourceExistsError):
        admin.create_rule(topic, sub, "r")


def test_create_second_default_rule_raises_resource_exists(admin, ts):
    topic, sub = ts
    with pytest.raises(ResourceExistsError):
        admin.create_rule(topic, sub, "$Default")


def test_get_and_delete_missing_rule_raise_resource_not_found(admin, ts):
    topic, sub = ts
    with pytest.raises(ResourceNotFoundError):
        admin.get_rule(topic, sub, "nope")
    with pytest.raises(ResourceNotFoundError):
        admin.delete_rule(topic, sub, "nope")


def test_create_rule_on_missing_subscription_raises_resource_not_found(admin, new_topic):
    topic = new_topic()
    with pytest.raises(ResourceNotFoundError):
        admin.create_rule(topic, "no-such-sub", "r")


def test_list_and_delete_rules_keep_creation_order(admin, ts):
    topic, sub = ts
    for name, expr in (("test_rule_1", "Priority = 'low'"), ("test_rule_2", "Priority = 'middle'"), ("test_rule_3", "Priority = 'high'")):
        admin.create_rule(topic, sub, name, filter=SqlRuleFilter(expr))
    assert len(list(admin.list_rules(topic, sub))) == 4
    admin.delete_rule(topic, sub, "test_rule_2")
    rules = list(admin.list_rules(topic, sub))
    assert [r.name for r in rules] == ["$Default", "test_rule_1", "test_rule_3"]
    assert rules[1].filter.sql_expression == "Priority = 'low'"
    assert rules[2].filter.sql_expression == "Priority = 'high'"
    admin.delete_rule(topic, sub, "test_rule_1")
    admin.delete_rule(topic, sub, "test_rule_3")
    assert [r.name for r in admin.list_rules(topic, sub)] == ["$Default"]


def test_default_rule_can_be_deleted(admin, ts):
    topic, sub = ts
    admin.delete_rule(topic, sub, "$Default")
    assert list(admin.list_rules(topic, sub)) == []


def test_update_rule_filter_and_action(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a = 1"))
    rule = admin.get_rule(topic, sub, "r")
    rule.filter = CorrelationRuleFilter(correlation_id="cid")
    rule.action = SqlRuleAction("SET x = 1")
    admin.update_rule(topic, sub, rule)
    got = admin.get_rule(topic, sub, "r")
    assert type(got.filter) is CorrelationRuleFilter and got.filter.correlation_id == "cid"
    assert got.action.sql_expression == "SET x = 1"


def test_update_rule_removes_action(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r", action=SqlRuleAction("SET x = 1"))
    rule = admin.get_rule(topic, sub, "r")
    rule.action = None
    admin.update_rule(topic, sub, rule)
    assert admin.get_rule(topic, sub, "r").action is None


def test_update_rule_from_dict(admin, ts):
    topic, sub = ts
    admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a = 1"))
    admin.update_rule(topic, sub, {"name": "r", "filter": SqlRuleFilter("a = 2"), "action": None, "created_at_utc": None})
    assert admin.get_rule(topic, sub, "r").filter.sql_expression == "a = 2"


def test_update_missing_rule_is_refused(admin, ts):
    topic, sub = ts
    with pytest.raises(HttpResponseError):
        admin.update_rule(topic, sub, {"name": "nope", "filter": TrueRuleFilter(), "action": None, "created_at_utc": None})


def test_update_rule_with_wrong_argument_types_raises_type_error(admin, ts):
    topic, sub = ts
    with pytest.raises(TypeError):
        admin.update_rule(topic, sub, None)
    with pytest.raises(TypeError):
        admin.update_rule(topic, sub, Exception("x"))


def test_rule_properties_cannot_be_constructed_directly():
    with pytest.raises(TypeError):
        RuleProperties("randomname")


def test_rules_are_per_subscription(admin, new_topic, new_sub):
    topic = new_topic()
    a, b = new_sub(topic), new_sub(topic)
    admin.create_rule(topic, a, "only-a")
    assert [r.name for r in admin.list_rules(topic, a)] == ["$Default", "only-a"]
    assert [r.name for r in admin.list_rules(topic, b)] == ["$Default"]
