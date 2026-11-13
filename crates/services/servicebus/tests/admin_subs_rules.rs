mod admin_common;

use admin_common::*;
use azemu_servicebus::rules::RuleFilter;

const ATOM_ENTRY: &str = "application/atom+xml; type=entry; charset=utf-8";

async fn with_topic() -> Admin {
    let a = admin().await;
    assert_eq!(a.topic("t", "").await.status, 201);
    a
}

#[tokio::test]
async fn create_subscription_with_defaults_matches_the_recording() {
    let a = with_topic().await;
    let r = a.sub("t", "s", "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    assert!(r.header("etag").is_some(), "the subscription create answer has an ETag");
    let b = r.text();
    assert_order(
        &b,
        &[
            "LockDuration",
            "RequiresSession",
            "DefaultMessageTimeToLive",
            "DeadLetteringOnMessageExpiration",
            "DeadLetteringOnFilterEvaluationExceptions",
            "MessageCount",
            "MaxDeliveryCount",
            "EnableBatchedOperations",
            "Status",
            "CreatedAt",
            "UpdatedAt",
            "AccessedAt",
            "AutoDeleteOnIdle",
            "EntityAvailabilityStatus",
            "IsClientAffine",
        ],
    );
    assert!(!b.contains("<author>") && !b.contains("CountDetails"));
    assert_eq!(tag(&b, "AccessedAt").unwrap(), "0001-01-01T00:00:00");
    assert_eq!(tag(&b, "DeadLetteringOnFilterEvaluationExceptions").unwrap(), "true");
    assert_eq!(tag(&b, "LockDuration").unwrap(), "PT1M");
    assert_eq!(tag(&b, "IsClientAffine").unwrap(), "false");
    assert!(tag(&b, "CreatedAt").unwrap().len() == "2024-09-04T18:19:20.8376905Z".len(), "seven fraction digits");
    assert!(b.contains("/t/subscriptions/s?api-version=2021-05</id>"), "{b}");

    let g = a.get("/t/subscriptions/s?enrich=false&api-version=2021-05").await;
    assert_eq!(g.status, 200);
    assert!(g.header("etag").is_some());
    let b = g.text();
    assert_order(&b, &["AccessedAt", "CountDetails", "AutoDeleteOnIdle"]);
    assert!(b.contains("<CountDetails xmlns:d3p1=\"http://schemas.microsoft.com/netservices/2011/06/servicebus\"><d3p1:ActiveMessageCount>0</d3p1:ActiveMessageCount>"), "{b}");
    assert_ne!(tag(&b, "AccessedAt").unwrap(), "0001-01-01T00:00:00", "a read subscription shows an access time");
}

#[tokio::test]
async fn subscription_settings_and_forwarding_round_trip() {
    let a = with_topic().await;
    a.queue("target", "").await;
    let host = format!("default.servicebus.localhost:{}", a.srv.port());
    let r = a
        .sub(
            "t",
            "s",
            &format!(
                "<LockDuration>PT13S</LockDuration><RequiresSession>true</RequiresSession><DefaultMessageTimeToLive>PT11M</DefaultMessageTimeToLive>\
                 <DeadLetteringOnMessageExpiration>true</DeadLetteringOnMessageExpiration><DeadLetteringOnFilterEvaluationExceptions>false</DeadLetteringOnFilterEvaluationExceptions>\
                 <MaxDeliveryCount>14</MaxDeliveryCount><Status>ReceiveDisabled</Status><ForwardTo>sb://{host}/target</ForwardTo>\
                 <ForwardDeadLetteredMessagesTo>sb://{host}/target</ForwardDeadLetteredMessagesTo><AutoDeleteOnIdle>PT10M</AutoDeleteOnIdle><UserMetadata>meta</UserMetadata>"
            ),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let b = a.get("/t/subscriptions/s").await.text();
    for (n, v) in [
        ("LockDuration", "PT13S"),
        ("RequiresSession", "true"),
        ("DefaultMessageTimeToLive", "PT11M"),
        ("DeadLetteringOnMessageExpiration", "true"),
        ("DeadLetteringOnFilterEvaluationExceptions", "false"),
        ("MaxDeliveryCount", "14"),
        ("Status", "ReceiveDisabled"),
        ("AutoDeleteOnIdle", "PT10M"),
        ("UserMetadata", "meta"),
    ] {
        assert_eq!(tag(&b, n).as_deref(), Some(v), "{n}");
    }
    assert_eq!(tag(&b, "ForwardTo").unwrap(), format!("sb://{host}/target"));
    // recorded order: ForwardTo after Status, ForwardDeadLetteredMessagesTo after the counts
    assert_order(
        &b,
        &["Status", "ForwardTo", "CreatedAt", "AccessedAt", "CountDetails", "ForwardDeadLetteredMessagesTo", "AutoDeleteOnIdle"],
    );
    let s = a.shared.state.lock().ns("default").topic("t").unwrap().subs["s"].core.settings.clone();
    assert_eq!(s.forward_to.as_deref(), Some("target"));
    assert!(s.requires_session && !s.dead_letter_on_filter_exception);
}

#[tokio::test]
async fn new_subscription_has_the_default_rule_as_a_true_filter() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    {
        // in the engine it is the SQL filter `1=1`
        let mut st = a.shared.state.lock();
        let rules = &st.ns("default").topic("t").unwrap().subs["s"].rules;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "$Default");
        assert!(matches!(rules[0].filter, RuleFilter::Sql { .. }));
    }
    let r = a.get("/t/subscriptions/s/rules?$skip=0&$top=100&api-version=2021-05").await;
    assert_eq!(r.status, 200);
    let b = r.text();
    assert!(b.contains("<title type=\"text\">Rules</title>"));
    assert!(b.contains("<RuleDescription xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\" xmlns:i=\"http://www.w3.org/2001/XMLSchema-instance\"><Filter i:type=\"TrueFilter\"><SqlExpression>1=1</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter><Action i:type=\"EmptyRuleAction\"/>"), "{b}");
    assert!(b.contains("<Name>$Default</Name>"));
    let g = a.get("/t/subscriptions/s/rules/$Default?enrich=false").await;
    assert_eq!(g.status, 200);
    assert!(g.text().contains("<Filter i:type=\"TrueFilter\">"));
}

#[tokio::test]
async fn subscription_errors() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    let r = a.sub("t", "S", "").await;
    assert_eq!(r.status, 409);
    assert!(r.header("etag").is_some());
    assert!(r.text().contains("<Detail>The messaging entity 'default:Topic:t|S' already exists. To know more visit https://aka.ms/sbResourceMgrExceptions.  TrackingId:"), "{}", r.text());
    let r = a.sub("no-such-topic", "s", "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("Entity 'default:Topic:no-such-topic' was not found."), "{}", r.text());
    let r = a.get("/t/subscriptions/nope").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("Entity 'default:Topic:t|nope' was not found."), "{}", r.text());
    assert_eq!(a.delete("/t/subscriptions/nope").await.status, 404);
    assert_eq!(a.delete("/nope/subscriptions/nope").await.status, 404);
    assert_eq!(a.get("/nope/subscriptions").await.status, 404);
    // a queue description is not a subscription description
    assert_eq!(a.create("/t/subscriptions/x", &entry("QueueDescription", "")).await.status, 400);
    // lock duration: the subscription text has no SubCode prefix
    let r = a.sub("t", "x", "<LockDuration>P25D</LockDuration>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("<Detail>The supplied lock time exceeds the allowed maximum of '5' minutes."), "{}", r.text());
    assert_eq!(a.get("/t/subscriptions/x").await.status, 404);
    // same name in two topics
    a.topic("t2", "").await;
    assert_eq!(a.sub("t2", "s", "").await.status, 201);
}

#[tokio::test]
async fn subscription_update_and_delete() {
    let a = with_topic().await;
    a.sub("t", "s", "<MaxDeliveryCount>14</MaxDeliveryCount>").await;
    let u = a
        .update(
            "/t/subscriptions/s",
            &entry("SubscriptionDescription", "<LockDuration>PT2M</LockDuration><MaxDeliveryCount>21</MaxDeliveryCount>"),
        )
        .await;
    assert_eq!(u.status, 200, "{}", u.text());
    let b = a.get("/t/subscriptions/s").await.text();
    assert_eq!(tag(&b, "LockDuration").unwrap(), "PT2M");
    assert_eq!(tag(&b, "MaxDeliveryCount").unwrap(), "21");
    assert_eq!(a.update("/t/subscriptions/iewdm", &entry("SubscriptionDescription", "")).await.status, 404);
    let r = a.update("/t/subscriptions/s", &entry("SubscriptionDescription", "<RequiresSession>true</RequiresSession>")).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The value for the RequiresSession property of an existing Subscription cannot be changed."), "{}", r.text());
    assert_eq!(a.update("/t/subscriptions/s", &entry("SubscriptionDescription", "<LockDuration>P25D</LockDuration>")).await.status, 400);
    // rules and settings survive an update
    a.rule("t", "s", "r1", "<Filter xsi:type=\"SqlFilter\"><SqlExpression>a = 1</SqlExpression></Filter>").await;
    a.update("/t/subscriptions/s", &entry("SubscriptionDescription", "")).await;
    assert_eq!(a.get("/t/subscriptions/s/rules").await.text().matches("<entry ").count(), 2);
    let d = a.delete("/t/subscriptions/s").await;
    assert_eq!(d.status, 200);
    assert!(d.header("etag").is_some() && d.body.is_empty());
    assert_eq!(a.get("/t/subscriptions/s").await.status, 404);
    assert!(!a.get("/t/subscriptions").await.text().contains("<entry"));
}

#[tokio::test]
async fn subscription_list_is_per_topic_and_pages() {
    let a = with_topic().await;
    a.topic("t2", "").await;
    for n in ["a", "b", "c"] {
        a.sub("t", n, "").await;
    }
    a.sub("t2", "z", "").await;
    let r = a.get("/t/subscriptions?$skip=0&$top=100&api-version=2021-05").await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().contains("type=feed"));
    let b = r.text();
    assert_eq!(titles(&b), vec!["Subscriptions", "a", "b", "c"]);
    assert!(b.contains("<entry xml:base=\"") && b.contains("<link rel=\"self\" href=\"subscriptions/a?api-version=2021-05\"/>"), "{b}");
    let p = a.get("/t/subscriptions?$skip=1&$top=1").await.text();
    assert_eq!(titles(&p), vec!["Subscriptions", "b"]);
    assert!(p.contains("rel=\"next\"") && p.contains("$skip=2&amp;$top=1"));
    assert_eq!(titles(&a.get("/t2/subscriptions").await.text()), vec!["Subscriptions", "z"]);
}

#[tokio::test]
async fn sql_rule_round_trip_and_state() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    let body = "<Filter xsi:type=\"SqlFilter\"><SqlExpression>Priority = @param1 AND Level = @param2</SqlExpression><CompatibilityLevel>20</CompatibilityLevel>\
        <Parameters><KeyValueOfstringanyType><Key>@param1</Key><Value xsi:type=\"d6p1:string\">str1</Value></KeyValueOfstringanyType>\
        <KeyValueOfstringanyType><Key>@param2</Key><Value xsi:type=\"d6p1:int\">1</Value></KeyValueOfstringanyType></Parameters>\
        <RequiresPreprocessing>true</RequiresPreprocessing></Filter><Action xsi:type=\"EmptyRuleAction\" /><Name>test_rule_2</Name>";
    let r = a.rule("t", "s", "test_rule_2", body).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    assert!(r.header("etag").is_some());
    let b = r.text();
    assert!(b.contains("<Filter i:type=\"SqlFilter\"><SqlExpression>Priority = @param1 AND Level = @param2</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><Parameters>"), "{b}");
    assert!(
        b.contains("<Key>@param1</Key><Value i:type=\"d6p1:string\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">str1</Value>"),
        "{b}"
    );
    assert!(b.contains("d6p1:int\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">1</Value></KeyValueOfstringanyType></Parameters><RequiresPreprocessing>true</RequiresPreprocessing></Filter><Action i:type=\"EmptyRuleAction\"/><CreatedAt>"), "{b}");
    assert!(b.contains("<published>") && b.contains("<link rel=\"self\" href="));
    // the engine has a compiled SQL filter with the parameters stored without "@"
    {
        let mut st = a.shared.state.lock();
        let rules = &st.ns("default").topic("t").unwrap().subs["s"].rules;
        match &rules[1].filter {
            RuleFilter::Sql { expression, parameters, .. } => {
                assert_eq!(expression, "Priority = @param1 AND Level = @param2");
                assert_eq!(parameters.len(), 2);
                assert!(parameters.contains_key("param1"));
            }
            other => panic!("{other:?}"),
        }
    }
    let g = a.get("/t/subscriptions/s/rules/test_rule_2?enrich=false").await.text();
    assert!(g.contains("<Value i:type=\"d6p1:int\""), "{g}");
    // an invalid expression is refused
    let r = a.rule("t", "s", "bad", "<Filter xsi:type=\"SqlFilter\"><SqlExpression>a ==== b</SqlExpression></Filter>").await;
    assert_eq!(r.status, 400);
    assert_eq!(a.get("/t/subscriptions/s/rules/bad").await.status, 404);
}

#[tokio::test]
async fn true_false_and_correlation_filters() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    let t = a.rule("t", "s", "t", "<Filter xsi:type=\"TrueFilter\"><SqlExpression>1=1</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><RequiresPreprocessing>true</RequiresPreprocessing></Filter><Action xsi:type=\"EmptyRuleAction\" />").await;
    assert_eq!(t.status, 201);
    assert!(t
        .text()
        .contains("<Filter i:type=\"TrueFilter\"><SqlExpression>1=1</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter>"));
    let f = a
        .rule(
            "t",
            "s",
            "f",
            "<Filter xsi:type=\"FalseFilter\"><SqlExpression>1=0</SqlExpression><CompatibilityLevel>20</CompatibilityLevel></Filter>",
        )
        .await;
    assert!(f.text().contains("<Filter i:type=\"FalseFilter\"><SqlExpression>1=0</SqlExpression>"), "{}", f.text());
    // no filter at all means a true filter
    let n = a.rule("t", "s", "n", "").await;
    assert_eq!(n.status, 201);
    assert!(n.text().contains("i:type=\"TrueFilter\""));
    let c = a
        .rule(
            "t", "s", "c",
            "<Filter xsi:type=\"CorrelationFilter\"><CorrelationId>testcid</CorrelationId><MessageId>m</MessageId><To>to</To><ReplyTo>rt</ReplyTo><Label>l</Label>\
             <SessionId>sid</SessionId><ReplyToSessionId>rsid</ReplyToSessionId><ContentType>ct</ContentType><Properties>\
             <KeyValueOfstringanyType><Key>key_string</Key><Value xsi:type=\"d6p1:string\">str1</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>key_int</Key><Value xsi:type=\"d6p1:int\">2</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>key_long</Key><Value xsi:type=\"d6p1:long\">2147483650</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>key_bool</Key><Value xsi:type=\"d6p1:boolean\">false</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>key_datetime</Key><Value xsi:type=\"d6p1:dateTime\">2020-07-05T11:12:13</Value></KeyValueOfstringanyType>\
             <KeyValueOfstringanyType><Key>key_duration</Key><Value xsi:type=\"d6p1:duration\">P1DT2H3M</Value></KeyValueOfstringanyType></Properties></Filter>",
        )
        .await;
    assert_eq!(c.status, 201, "{}", c.text());
    let b = a.get("/t/subscriptions/s/rules/c").await.text();
    assert_order(
        &b,
        &["CorrelationId", "MessageId", "To", "ReplyTo", "Label", "SessionId", "ReplyToSessionId", "ContentType", "Properties"],
    );
    for needle in [
        "<Key>key_string</Key><Value i:type=\"d6p1:string\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">str1</Value>",
        "<Key>key_int</Key><Value i:type=\"d6p1:int\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">2</Value>",
        "<Key>key_long</Key><Value i:type=\"d6p1:long\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">2147483650</Value>",
        "<Key>key_bool</Key><Value i:type=\"d6p1:boolean\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">false</Value>",
        "<Key>key_datetime</Key><Value i:type=\"d6p1:dateTime\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">2020-07-05T11:12:13</Value>",
        "<Key>key_duration</Key><Value i:type=\"d6p1:duration\" xmlns:d6p1=\"http://schemas.microsoft.com/2003/10/Serialization/\">P1DT2H3M</Value>",
    ] {
        assert!(b.contains(needle), "missing {needle} in {b}");
    }
    let mut st = a.shared.state.lock();
    let rules = &st.ns("default").topic("t").unwrap().subs["s"].rules;
    assert!(matches!(rules[1].filter, RuleFilter::True) && matches!(rules[2].filter, RuleFilter::False));
    assert!(matches!(rules[4].filter, RuleFilter::Correlation(_)));
    assert!(matches!(rules[0].filter, RuleFilter::Sql { .. }), "$Default stays the engine's SQL filter");
}

#[tokio::test]
async fn rule_action_round_trip() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    let r = a
        .rule(
            "t", "s", "r",
            "<Filter xsi:type=\"TrueFilter\"/><Action xsi:type=\"SqlRuleAction\"><SqlExpression>SET Priority = @param</SqlExpression><CompatibilityLevel>20</CompatibilityLevel>\
             <Parameters><KeyValueOfstringanyType><Key>@param</Key><Value xsi:type=\"d6p1:dateTime\">2020-07-05T11:12:13</Value></KeyValueOfstringanyType></Parameters>\
             <RequiresPreprocessing>true</RequiresPreprocessing></Action>",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(r.text().contains("<Action i:type=\"SqlRuleAction\"><SqlExpression>SET Priority = @param</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><Parameters><KeyValueOfstringanyType><Key>@param</Key><Value i:type=\"d6p1:dateTime\""), "{}", r.text());
    {
        let mut st = a.shared.state.lock();
        let rule = &st.ns("default").topic("t").unwrap().subs["s"].rules[1];
        assert_eq!(rule.action.as_ref().unwrap().expression, "SET Priority = @param");
    }
    // an action without parameters: the GET and the list show an empty <Parameters/> as recorded
    a.rule("t", "s", "r2", "<Action xsi:type=\"SqlRuleAction\"><SqlExpression>SET x = 1</SqlExpression></Action>").await;
    let g = a.get("/t/subscriptions/s/rules/r2").await.text();
    assert!(g.contains("<RequiresPreprocessing>true</RequiresPreprocessing><Parameters/></Action>"), "{g}");
    let bad = a.rule("t", "s", "r3", "<Action xsi:type=\"SqlRuleAction\"><SqlExpression>SET ====</SqlExpression></Action>").await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn rule_create_get_update_delete_errors() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    let sql = |e: &str| {
        format!("<Filter xsi:type=\"SqlFilter\"><SqlExpression>{e}</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><RequiresPreprocessing>true</RequiresPreprocessing></Filter><Action xsi:type=\"EmptyRuleAction\" />")
    };
    assert_eq!(a.rule("t", "s", "rule", &sql("Priority = 'low'")).await.status, 201);
    let dup = a.rule("t", "s", "RULE", &sql("Priority = 'low'")).await;
    assert_eq!(dup.status, 409);
    assert!(dup.header("etag").is_some());
    assert!(dup.text().contains("The messaging entity 'default:Topic:t|s|RULE' already exists."), "{}", dup.text());
    assert_eq!(a.rule("t", "s", "$Default", "").await.status, 409);
    assert_eq!(a.rule("t", "nope", "r", "").await.status, 404);
    assert_eq!(a.rule("nope", "s", "r", "").await.status, 404);
    // update
    let p = "/t/subscriptions/s/rules/rule?api-version=2021-05";
    let u = a
        .update(p, &entry("RuleDescription", "<Filter xsi:type=\"CorrelationFilter\"><CorrelationId>testcid</CorrelationId></Filter><Action xsi:type=\"SqlRuleAction\"><SqlExpression>SET Priority = 'low'</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><RequiresPreprocessing>true</RequiresPreprocessing></Action><CreatedAt>2024-09-04T18:18:52.410181Z</CreatedAt><Name>rule</Name>"))
        .await;
    assert_eq!(u.status, 200, "{}", u.text());
    assert!(u.header("etag").is_some());
    let g = a.get("/t/subscriptions/s/rules/rule").await.text();
    assert!(
        g.contains("<Filter i:type=\"CorrelationFilter\"><CorrelationId>testcid</CorrelationId></Filter><Action i:type=\"SqlRuleAction\">"),
        "{g}"
    );
    assert!(!g.contains("2024-09-04T18:18:52"), "CreatedAt is the server's");
    let miss = a.update("/t/subscriptions/s/rules/iewdm", &entry("RuleDescription", &sql("a = 1"))).await;
    assert_eq!(miss.status, 404);
    assert!(miss.text().contains("Entity 'default:Topic:t|s|iewdm' was not found. To know more visit"), "{}", miss.text());
    assert_eq!(a.update("/t/subscriptions/s/rules/", &entry("RuleDescription", &sql("a = 1"))).await.status, 400);
    // get/delete missing
    assert_eq!(a.get("/t/subscriptions/s/rules/nope").await.status, 404);
    assert_eq!(a.delete("/t/subscriptions/s/rules/nope").await.status, 404);
    assert_eq!(a.get("/t/subscriptions/nope/rules").await.status, 404);
    assert_eq!(a.create("/t/subscriptions/s/rules/x", &entry("QueueDescription", "")).await.status, 400);
}

#[tokio::test]
async fn rules_keep_creation_order_and_deleting_the_default_is_allowed() {
    let a = with_topic().await;
    a.sub("t", "s", "").await;
    a.sub("t", "other", "").await;
    for (n, e) in [("test_rule_1", "low"), ("test_rule_2", "middle"), ("test_rule_3", "high")] {
        let r =
            a.rule("t", "s", n, &format!("<Filter xsi:type=\"SqlFilter\"><SqlExpression>Priority = '{e}'</SqlExpression></Filter>")).await;
        assert_eq!(r.status, 201);
    }
    async fn list(a: &Admin, sub: &str) -> Vec<String> {
        titles(&a.get(&format!("/t/subscriptions/{sub}/rules?$skip=0&$top=100&api-version=2021-05")).await.text())
    }
    assert_eq!(list(&a, "s").await[1..].to_vec(), vec!["$Default", "test_rule_1", "test_rule_2", "test_rule_3"]);
    assert_eq!(a.delete("/t/subscriptions/s/rules/test_rule_2").await.status, 200);
    let l = a.get("/t/subscriptions/s/rules").await;
    let b = l.text();
    assert!(b.contains("<Filter i:type=\"SqlFilter\"><SqlExpression>Priority = 'low'</SqlExpression><CompatibilityLevel>20</CompatibilityLevel><RequiresPreprocessing>true</RequiresPreprocessing><Parameters/></Filter>"), "{b}");
    assert_eq!(titles(&b)[1..].to_vec(), vec!["$Default", "test_rule_1", "test_rule_3"]);
    assert_eq!(a.delete("/t/subscriptions/s/rules/$Default").await.status, 200);
    assert_eq!(list(&a, "s").await[1..].to_vec(), vec!["test_rule_1", "test_rule_3"]);
    assert_eq!(list(&a, "other").await[1..].to_vec(), vec!["$Default"], "rules are per subscription");
    let p = a.get("/t/subscriptions/s/rules?$skip=1&$top=1").await.text();
    assert_eq!(titles(&p)[1..].to_vec(), vec!["test_rule_3"]);
    assert!(!p.contains("rel=\"next\""));
}

#[tokio::test]
async fn list_entry_ids_repeat_the_casing_of_the_request_path() {
    // the JavaScript SDK asks for /Subscriptions and /Rules and finds the names by matching them exactly
    let a = with_topic().await;
    assert_eq!(a.sub("t", "s", "").await.status, 201);
    for (path, expected) in [
        ("/t/Subscriptions?api-version=2024-05", "/t/Subscriptions/s?api-version=2024-05</id>"),
        ("/t/subscriptions?api-version=2024-05", "/t/subscriptions/s?api-version=2024-05</id>"),
    ] {
        let b = a.get(path).await.text();
        assert!(b.contains(expected), "{b}");
        let href = if path.contains("Subscriptions") { "href=\"Subscriptions/s?" } else { "href=\"subscriptions/s?" };
        assert!(b.contains(href), "{b}");
    }
    let b = a.get("/t/Subscriptions/s/Rules?api-version=2024-05").await.text();
    assert!(b.contains("/t/Subscriptions/s/Rules/$Default?api-version=2024-05</id>"), "{b}");
    assert!(b.contains("href=\"Rules/$Default?"), "{b}");
    let b = a.get("/t/subscriptions/s/rules?api-version=2021-05").await.text();
    assert!(b.contains("/t/subscriptions/s/rules/$Default?api-version=2021-05</id>"), "{b}");
}

#[tokio::test]
async fn default_rule_description_replaces_the_default_rule() {
    // what the JavaScript SDK sends for `defaultRuleOptions`
    let a = with_topic().await;
    let r = a
        .sub(
            "t",
            "red",
            "<DefaultRuleDescription><Filter xmlns:p4=\"http://www.w3.org/2001/XMLSchema-instance\" p4:type=\"SqlFilter\"><SqlExpression>color = 'red'</SqlExpression></Filter>\
             <Action xmlns:p4=\"http://www.w3.org/2001/XMLSchema-instance\" p4:type=\"SqlRuleAction\"><SqlExpression>SET seen = 1</SqlExpression></Action><Name>only-red</Name></DefaultRuleDescription>",
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    let rules = a.shared.state.lock().ns("default").topic("t").unwrap().subs["red"].rules.clone();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "only-red");
    assert!(matches!(rules[0].filter, RuleFilter::Sql { .. }), "{:?}", rules[0].filter);
    assert!(rules[0].action.is_some());
    assert_eq!(a.get("/t/subscriptions/red/rules/only-red").await.status, 200);
    assert_eq!(a.get("/t/subscriptions/red/rules/$Default").await.status, 404);
    // without a name the rule is called $Default
    let r = a.sub("t", "blue", "<DefaultRuleDescription><Filter p4:type=\"CorrelationFilter\" xmlns:p4=\"http://www.w3.org/2001/XMLSchema-instance\"><Label>b</Label></Filter></DefaultRuleDescription>").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let rules = a.shared.state.lock().ns("default").topic("t").unwrap().subs["blue"].rules.clone();
    assert_eq!(rules[0].name, "$Default");
    assert!(matches!(rules[0].filter, RuleFilter::Correlation(_)));
}
