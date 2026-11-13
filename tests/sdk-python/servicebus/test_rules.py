"""Subscription rules: SQL filters, correlation filters, True/False filters, rule actions, several rules.

Sources: docs "Topic filters and actions" (rules without actions are OR-ed into ONE message; every rule WITH an action
produces its own copy carrying a `RuleName` property), docs "SQL filter syntax", Microsoft's mgmt rule tests.
Each case creates a subscription without `$Default`, adds one rule, sends one message and checks what arrives.
"""

import pytest
from azure.servicebus import ServiceBusMessage
from azure.servicebus.management import (
    CorrelationRuleFilter,
    FalseRuleFilter,
    SqlRuleAction,
    SqlRuleFilter,
    TrueRuleFilter,
)

from sbutil import bodies, drain_sub


@pytest.fixture
def run(sb, admin, new_topic, new_sub):
    """run(filter, message_kwargs, action=None, extra_rules=()) -> list of received messages (completed)."""

    def go(rule_filter, msg_kwargs=None, action=None, extra_rules=(), body="x"):
        topic = new_topic()
        sub = new_sub(topic, remove_default=True)
        admin.create_rule(topic, sub, "r", filter=rule_filter, action=action)
        for name, f, a in extra_rules:
            admin.create_rule(topic, sub, name, filter=f, action=a)
        with sb.get_topic_sender(topic) as s:
            s.send_messages(ServiceBusMessage(body, **(msg_kwargs or {})))
        return drain_sub(sb, topic, sub, max_wait=1)

    return go


def props(**kw):
    return {"application_properties": kw}


# -- SQL filters ----------------------------------------------------------------------------------
@pytest.mark.parametrize(
    "expr, kwargs, matches",
    [
        ("color = 'red'", props(color="red"), True),
        ("color = 'red'", props(color="blue"), False),
        ("color = 'red'", props(other="red"), False),
        ("price > 10", props(price=15), True),
        ("price > 10", props(price=10), False),
        ("price >= 10", props(price=10), True),
        ("price < 10", props(price=5), True),
        ("price < 10", props(price=10), False),
        ("price <= 10", props(price=10), True),
        ("price <> 10", props(price=11), True),
        ("price <> 10", props(price=10), False),
        ("price != 10", props(price=11), True),
        ("price = 2.5", props(price=2.5), True),
        ("flag = true", props(flag=True), True),
        ("flag = true", props(flag=False), False),
        ("price * 2 > 20", props(price=11), True),
        ("price + 1 = 11", props(price=10), True),
        ("1 = 1", {}, True),
        ("1 = 0", {}, False),
    ],
    ids=lambda v: v if isinstance(v, str) else None,
)
def test_sql_filter_comparison(run, expr, kwargs, matches):
    got = run(SqlRuleFilter(expr), kwargs)
    assert bool(got) is matches


@pytest.mark.parametrize(
    "expr, kwargs, matches",
    [
        ("a = 1 AND b = 2", props(a=1, b=2), True),
        ("a = 1 AND b = 2", props(a=1, b=3), False),
        ("a = 1 AND b = 2", props(a=1), False),
        ("a = 1 OR b = 2", props(a=9, b=2), True),
        ("a = 1 OR b = 2", props(a=9, b=9), False),
        ("NOT (a = 1)", props(a=2), True),
        ("NOT (a = 1)", props(a=1), False),
        ("(a = 1 OR b = 1) AND c = 3", props(b=1, c=3), True),
        ("(a = 1 OR b = 1) AND c = 3", props(b=1, c=4), False),
    ],
    ids=lambda v: v if isinstance(v, str) else None,
)
def test_sql_filter_boolean_logic(run, expr, kwargs, matches):
    assert bool(run(SqlRuleFilter(expr), kwargs)) is matches


@pytest.mark.parametrize(
    "expr, kwargs, matches",
    [
        ("color IN ('red', 'green')", props(color="green"), True),
        ("color IN ('red', 'green')", props(color="blue"), False),
        ("n IN (1, 2, 3)", props(n=2), True),
        ("n IN (1, 2, 3)", props(n=4), False),
        ("name LIKE 'abc%'", props(name="abcdef"), True),
        ("name LIKE 'abc%'", props(name="xabc"), False),
        ("name LIKE '%def'", props(name="abcdef"), True),
        ("name LIKE 'a_c'", props(name="abc"), True),
        ("name LIKE 'a_c'", props(name="abbc"), False),
        ("color IS NULL", {}, True),
        ("color IS NULL", props(color="x"), False),
        ("color IS NOT NULL", props(color="x"), True),
        ("color IS NOT NULL", {}, False),
        ("EXISTS(color)", props(color="x"), True),
        ("EXISTS(color)", {}, False),
        ("NOT EXISTS(color)", {}, True),
    ],
    ids=lambda v: v if isinstance(v, str) else None,
)
def test_sql_filter_in_like_null_exists(run, expr, kwargs, matches):
    assert bool(run(SqlRuleFilter(expr), kwargs)) is matches


@pytest.mark.parametrize(
    "expr, kwargs, matches",
    [
        ("sys.Label = 'invoice'", {"subject": "invoice"}, True),
        ("sys.Label = 'invoice'", {"subject": "order"}, False),
        ("sys.CorrelationId = 'c1'", {"correlation_id": "c1"}, True),
        ("sys.CorrelationId = 'c1'", {"correlation_id": "c2"}, False),
        ("sys.MessageId = 'm1'", {"message_id": "m1"}, True),
        ("sys.To = 'dest'", {"to": "dest"}, True),
        ("sys.ReplyTo = 'back'", {"reply_to": "back"}, True),
        ("sys.ContentType = 'text/plain'", {"content_type": "text/plain"}, True),
        ("sys.ReplyToSessionId = 'rs'", {"reply_to_session_id": "rs"}, True),
        ("sys.Label LIKE 'inv%'", {"subject": "invoice-7"}, True),
        ("sys.Label IN ('a', 'b')", {"subject": "b"}, True),
        ("sys.Label IS NULL", {}, True),
        ("sys.Label = 'x' AND color = 'red'", {"subject": "x", **props(color="red")}, True),
        ("user.color = 'red'", props(color="red"), True),
    ],
    ids=lambda v: v if isinstance(v, str) else None,
)
def test_sql_filter_system_properties(run, expr, kwargs, matches):
    assert bool(run(SqlRuleFilter(expr), kwargs)) is matches


@pytest.mark.parametrize(
    "param, value, msg_value, matches",
    [
        ("str", "red", "red", True),
        ("str", "red", "blue", False),
        ("int", 5, 5, True),
        ("int", 5, 6, False),
        ("bool", True, True, True),
        ("float", 1.5, 1.5, True),
    ],
)
def test_sql_filter_with_parameter(run, param, value, msg_value, matches):
    assert bool(run(SqlRuleFilter("p = @v", parameters={"@v": value}), props(p=msg_value))) is matches


# -- true / false / correlation -----------------------------------------------------------------------
def test_true_filter_matches_everything(run):
    assert len(run(TrueRuleFilter(), props(a=1))) == 1


def test_false_filter_matches_nothing(run):
    assert run(FalseRuleFilter(), props(a=1)) == []


@pytest.mark.parametrize(
    "field, value, kwargs, matches",
    [
        ("correlation_id", "c1", {"correlation_id": "c1"}, True),
        ("correlation_id", "c1", {"correlation_id": "c2"}, False),
        ("correlation_id", "c1", {}, False),
        ("message_id", "m1", {"message_id": "m1"}, True),
        ("message_id", "m1", {"message_id": "m2"}, False),
        ("to", "dest", {"to": "dest"}, True),
        ("to", "dest", {"to": "other"}, False),
        ("reply_to", "back", {"reply_to": "back"}, True),
        ("reply_to", "back", {"reply_to": "x"}, False),
        ("label", "invoice", {"subject": "invoice"}, True),
        ("label", "invoice", {"subject": "Invoice"}, False),
        ("session_id", "s1", {"session_id": "s1"}, True),
        ("session_id", "s1", {"session_id": "s2"}, False),
        ("reply_to_session_id", "rs", {"reply_to_session_id": "rs"}, True),
        ("reply_to_session_id", "rs", {"reply_to_session_id": "zz"}, False),
        ("content_type", "text/plain", {"content_type": "text/plain"}, True),
        ("content_type", "text/plain", {"content_type": "text/html"}, False),
    ],
    ids=lambda v: v if isinstance(v, str) else None,
)
def test_correlation_filter_single_field(run, field, value, kwargs, matches):
    assert bool(run(CorrelationRuleFilter(**{field: value}), kwargs)) is matches


def test_correlation_filter_on_user_property(run):
    f = CorrelationRuleFilter(properties={"color": "red"})
    assert len(run(f, props(color="red"))) == 1


def test_correlation_filter_on_user_property_mismatch(run):
    f = CorrelationRuleFilter(properties={"color": "red"})
    assert run(f, props(color="blue")) == []


def test_correlation_filter_on_int_property(run):
    assert len(run(CorrelationRuleFilter(properties={"n": 5}), props(n=5))) == 1


def test_correlation_filter_fields_are_anded(run):
    f = CorrelationRuleFilter(correlation_id="c", label="l")
    assert len(run(f, {"correlation_id": "c", "subject": "l"})) == 1


def test_correlation_filter_and_fails_if_one_field_differs(run):
    f = CorrelationRuleFilter(correlation_id="c", label="l")
    assert run(f, {"correlation_id": "c", "subject": "other"}) == []


def test_correlation_filter_is_case_sensitive(run):
    assert run(CorrelationRuleFilter(correlation_id="ABC"), {"correlation_id": "abc"}) == []


# -- actions ------------------------------------------------------------------------------------------
def test_action_sets_user_property(run):
    got = run(TrueRuleFilter(), props(a=1), action=SqlRuleAction("SET priority = 'high'"))
    assert len(got) == 1
    assert got[0].application_properties[b"priority"] == b"high"


def test_action_sets_system_property(run):
    got = run(TrueRuleFilter(), {"subject": "old"}, action=SqlRuleAction("SET sys.Label = 'new'"))
    assert got[0].subject == "new"


def test_action_removes_property(run):
    got = run(TrueRuleFilter(), props(secret="s", keep="k"), action=SqlRuleAction("REMOVE secret"))
    assert b"secret" not in got[0].application_properties
    assert got[0].application_properties[b"keep"] == b"k"


def test_action_computes_from_existing_property(run):
    got = run(TrueRuleFilter(), props(n=5), action=SqlRuleAction("SET n2 = n * 2"))
    assert got[0].application_properties[b"n2"] == 10


def test_action_with_several_statements(run):
    got = run(TrueRuleFilter(), props(a=1), action=SqlRuleAction("SET b = 2; SET c = 'x'; REMOVE a"))
    p = got[0].application_properties
    assert p[b"b"] == 2 and p[b"c"] == b"x" and b"a" not in p


def test_action_with_parameter(run):
    got = run(TrueRuleFilter(), {}, action=SqlRuleAction("SET p = @v", parameters={"@v": 7}))
    assert got[0].application_properties[b"p"] == 7


def test_rule_with_action_adds_rule_name_property(run):
    # docs: each rule with an action produces a copy that has a RuleName property
    got = run(TrueRuleFilter(), {}, action=SqlRuleAction("SET z = 1"))
    assert got[0].application_properties[b"RuleName"] == b"r"


def test_action_does_not_run_when_filter_does_not_match(run):
    got = run(SqlRuleFilter("a = 1"), props(a=2), action=SqlRuleAction("SET z = 1"))
    assert got == []


# -- several rules -----------------------------------------------------------------------------------
def test_two_matching_rules_without_actions_give_one_message(run):
    got = run(SqlRuleFilter("a = 1"), props(a=1), extra_rules=[("r2", SqlRuleFilter("a > 0"), None)])
    assert len(got) == 1


def test_matching_rule_with_action_and_plain_rule_give_two_messages(run):
    got = run(
        SqlRuleFilter("a = 1"),
        props(a=1),
        extra_rules=[("with-action", TrueRuleFilter(), SqlRuleAction("SET tagged = 1"))],
    )
    assert len(got) == 2
    assert sorted(b"tagged" in m.application_properties for m in got) == [False, True]


def test_two_matching_rules_with_actions_give_two_copies(run):
    got = run(
        TrueRuleFilter(),
        {},
        action=SqlRuleAction("SET v = 1"),
        extra_rules=[("r2", TrueRuleFilter(), SqlRuleAction("SET v = 2"))],
    )
    assert sorted(m.application_properties[b"v"] for m in got) == [1, 2]
    assert sorted(m.application_properties[b"RuleName"] for m in got) == [b"r", b"r2"]


def test_only_the_matching_rule_counts(run):
    got = run(SqlRuleFilter("a = 1"), props(a=2), extra_rules=[("r2", SqlRuleFilter("a = 2"), None)])
    assert len(got) == 1


def test_subscription_without_rules_receives_nothing(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, remove_default=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("x"))
    assert drain_sub(sb, topic, sub, max_wait=1) == []


def test_default_rule_plus_extra_rule_still_delivers_once(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic)
    admin.create_rule(topic, sub, "extra", filter=SqlRuleFilter("a = 1"))
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("x", application_properties={"a": 1}))
    assert len(drain_sub(sb, topic, sub, max_wait=1)) == 1


def test_removing_default_rule_stops_delivery(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic)
    admin.delete_rule(topic, sub, "$Default")
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("x"))
    assert drain_sub(sb, topic, sub, max_wait=1) == []


def test_rule_added_later_only_applies_to_later_messages(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, remove_default=True)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("before"))
        admin.create_rule(topic, sub, "late", filter=TrueRuleFilter())
        s.send_messages(ServiceBusMessage("after"))
    assert bodies(drain_sub(sb, topic, sub, max_wait=1)) == ["after"]


def test_updated_rule_filter_changes_what_is_delivered(sb, admin, new_topic, new_sub):
    topic = new_topic()
    sub = new_sub(topic, remove_default=True)
    admin.create_rule(topic, sub, "r", filter=SqlRuleFilter("a = 1"))
    rule = admin.get_rule(topic, sub, "r")
    rule.filter = SqlRuleFilter("a = 2")
    admin.update_rule(topic, sub, rule)
    with sb.get_topic_sender(topic) as s:
        s.send_messages(ServiceBusMessage("one", application_properties={"a": 1}))
        s.send_messages(ServiceBusMessage("two", application_properties={"a": 2}))
    assert bodies(drain_sub(sb, topic, sub, max_wait=1)) == ["two"]


def test_two_subscriptions_with_different_filters_split_the_messages(sb, new_topic, new_sub, admin):
    topic = new_topic()
    red = new_sub(topic, remove_default=True)
    blue = new_sub(topic, remove_default=True)
    admin.create_rule(topic, red, "r", filter=SqlRuleFilter("color = 'red'"))
    admin.create_rule(topic, blue, "r", filter=CorrelationRuleFilter(properties={"color": "blue"}))
    with sb.get_topic_sender(topic) as s:
        for i, c in enumerate(["red", "blue", "red", "green", "blue"]):
            s.send_messages(ServiceBusMessage(f"m{i}", application_properties={"color": c}))
    assert bodies(drain_sub(sb, topic, red, max_wait=1)) == ["m0", "m2"]
    assert bodies(drain_sub(sb, topic, blue, max_wait=1)) == ["m1", "m4"]
