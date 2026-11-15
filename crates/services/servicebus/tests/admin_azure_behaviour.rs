//! Administration API behaviour that was checked against a real Standard namespace in November 2026.
mod admin_common;

use admin_common::*;

fn sql(expr: &str, params: &str) -> String {
    format!(
        "<Filter xsi:type=\"SqlFilter\"><SqlExpression>{expr}</SqlExpression><Parameters>{params}</Parameters></Filter><Action xsi:type=\"EmptyRuleAction\" />"
    )
}

#[tokio::test]
async fn forwarding_targets_must_exist_with_the_services_texts() {
    let a = admin().await;
    a.queue("dest", "").await;
    a.topic("topic", "").await;
    let host = a.base.trim_start_matches("http://").trim_start_matches("https://").to_string();
    // a queue: a full address that does not exist, a bare name
    let r = a.queue("src", &format!("<ForwardTo>sb://{host}/nothing</ForwardTo>")).await;
    assert_eq!(r.status, 400);
    assert!(
        r.text()
            .contains(&format!("SubCode=40400. The auto forwarding destination entity sb://{host}/nothing does not exist. TrackingId:")),
        "{}",
        r.text()
    );
    let r = a.queue("src", "<ForwardTo>dest</ForwardTo>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("SubCode=40000. Absolute URI must be provided.&#xD;\nParameter name: ForwardTo"), "{}", r.text());
    let r = a.queue("src", "<ForwardDeadLetteredMessagesTo>dest</ForwardDeadLetteredMessagesTo>").await;
    assert!(r.text().contains("Parameter name: ForwardDeadLetteredMessagesTo"), "{}", r.text());
    // a queue and a topic are targets
    for target in ["dest", "topic"] {
        let r = a.queue(&format!("to-{target}"), &format!("<ForwardTo>sb://{host}/{target}</ForwardTo>")).await;
        assert_eq!(r.status, 201, "{}", r.text());
        assert!(r.text().contains(&format!("<ForwardTo>sb://{host}/{target}</ForwardTo>")), "{}", r.text());
    }
    // an update names the entity
    let r = a.update("/to-dest", &entry("QueueDescription", &format!("<ForwardTo>sb://{host}/gone</ForwardTo>"))).await;
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains(&format!(
            "The auto forwarding destination &quot;sb://{host}/gone&quot; on entity &quot;default:Queue:to-dest&quot; does not exist. ."
        )),
        "{}",
        r.text()
    );
    // a subscription
    let r = a.sub("topic", "s", &format!("<ForwardTo>sb://{host}/gone</ForwardTo>")).await;
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains(&format!(
            "The auto forwarding destination &quot;sb://{host}/gone&quot; on entity &quot;default:Topic:topic|s&quot; does not exist. ."
        )),
        "{}",
        r.text()
    );
    let r = a.sub("topic", "s", "<ForwardTo>dest</ForwardTo>").await;
    assert!(r.text().contains("BR0012The specified value is in an invalid format.&#xD;\nParameter name: ForwardTo."), "{}", r.text());
    assert_eq!(a.sub("topic", "ok", &format!("<ForwardTo>sb://{host}/dest</ForwardTo>")).await.status, 201);
}

#[tokio::test]
async fn an_update_with_a_stale_etag_is_refused() {
    let a = admin().await;
    a.queue("q", "").await;
    a.topic("t", "").await;
    a.sub("t", "s", "").await;
    let stale = [("If-Match", "\"123\"")];
    let r = a.req("PUT", "/q?api-version=2021-05", &entry("QueueDescription", ""), &stale).await;
    assert_eq!(r.status, 412);
    assert_eq!(r.text(), "");
    let r = a.req("PUT", "/t/subscriptions/s?api-version=2021-05", &entry("SubscriptionDescription", ""), &stale).await;
    assert_eq!(r.status, 500);
    assert!(r.text().contains("The service was unable to process the request; please retry the operation."), "{}", r.text());
    // the ETag of the entity (quoted or not) and `*` are accepted
    for quoted in [false, true] {
        let etag = a.get("/q?api-version=2021-05").await.header("etag").unwrap().to_string();
        let value = if quoted { format!("\"{etag}\"") } else { etag };
        let r = a.req("PUT", "/q?api-version=2021-05", &entry("QueueDescription", ""), &[("If-Match", value.as_str())]).await;
        assert_eq!(r.status, 200, "{value}: {}", r.text());
    }
    let r = a.req("PUT", "/q?api-version=2021-05", &entry("QueueDescription", ""), &[("If-Match", "*")]).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn reads_below_a_topic_that_is_not_there_answer_the_list_of_services() {
    let a = admin().await;
    for path in [
        "/nope/subscriptions",
        "/nope/subscriptions/s",
        "/nope/subscriptions/s/rules",
        "/nope/subscriptions/s/rules/r",
        "/$nothing",
        "/$Resources/Subscriptions",
    ] {
        let r = a.get(&format!("{path}?api-version=2021-05")).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(r.text().contains("Publicly Listed Services"), "{path}: {}", r.text());
    }
    // writes are not recognized
    let r = a.rule("nope", "s", "r", "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("The incoming request is not recognized as a namespace policy put request."), "{}", r.text());
    // a subscription that is not there, below a topic that is
    a.topic("t", "").await;
    let r = a.rule("t", "nosub", "r", "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("The messaging entity 'default:Topic:t|nosub|r' could not be found."), "{}", r.text());
    // deleting what is not there
    a.sub("t", "s", "").await;
    a.delete("/t/subscriptions/s").await;
    let r = a.delete("/t/subscriptions/s").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("The messaging entity 'default:Topic:t|s' could not be found. To know more visit"), "{}", r.text());
}

#[tokio::test]
async fn paging_values_and_api_versions_are_checked() {
    let a = admin().await;
    for (q, name) in [("$top=-1", "top"), ("$top=abc", "top"), ("$skip=-1", "skip")] {
        let r = a.get(&format!("/$Resources/queues?{q}&api-version=2021-05")).await;
        assert_eq!(r.status, 400, "{q}");
        assert!(
            r.text().contains(&format!("Parameter '{name}' should be non-negative integer.&#xD;\nParameter name: {name}.")),
            "{}",
            r.text()
        );
    }
    let r = a.get("/$Resources/queues?api-version=1999-01").await;
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains(
            "The api-version in the query string is not supported. Either remove it from the Uri or use one of '2012-03,2012-08,"
        ),
        "{}",
        r.text()
    );
    assert_eq!(a.get("/$Resources/queues").await.status, 200, "no api-version is fine");
}

#[tokio::test]
async fn status_values_that_an_entity_cannot_have_and_other_limits() {
    let a = admin().await;
    let r = a.topic("t", "<Status>ReceiveDisabled</Status>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("Entity 'Status' cannot be set to 'ReceiveDisabled' for 'default:Topic:t'."), "{}", r.text());
    a.topic("t", "").await;
    let r = a.sub("t", "s", "<Status>SendDisabled</Status>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("BR0012Entity 'Status' cannot be set to 'SendDisabled' for 'default:Topic:t|s'."), "{}", r.text());
    assert_eq!(a.sub("t", "s", "<Status>ReceiveDisabled</Status>").await.status, 201);
    assert_eq!(a.queue("q", "<Status>Creating</Status>").await.status, 201);
    assert_eq!(a.queue("q1", "<RequiresSession>1</RequiresSession>").await.status, 201);
    // a subscription name over 50 characters, a rule name with a space
    let r = a.sub("t", &"x".repeat(51), "").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The entity name 'Subscription' cannot exceed '50' characters."), "{}", r.text());
    let r = a.rule("t", "s", "a%20b", "").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("contains character(s) that is not allowed by Service Bus."), "{}", r.text());
}

#[tokio::test]
async fn rules_that_azure_refuses() {
    let a = admin().await;
    a.topic("t", "").await;
    a.sub("t", "s", "").await;
    async fn one(a: &Admin, p: &str, text: &str) {
        let r = a.rule("t", "s", "r", p).await;
        assert_eq!(r.status, 400, "{text}: {}", r.text());
        assert!(r.text().contains(text), "{text}: {}", r.text());
    }
    one(
        &a,
        &sql("a = @p", ""),
        "A parameter (@p) used in the expression specified in SqlExpression is missing from the Parameters property.",
    )
    .await;
    let q = "<KeyValueOfstringanyType><Key>@q</Key><Value i:type=\"d6p1:int\" xmlns:d6p1=\"http://www.w3.org/2001/XMLSchema\">1</Value></KeyValueOfstringanyType>";
    one(
        &a,
        &sql("a = 1", q),
        "The specified parameter (@q) in Parameters property was not used in the expression specified in SqlExpression.",
    )
    .await;
    one(&a, &sql("", ""), "The property 'SqlExpression' is null or empty.").await;
    one(&a, &sql("foo(a) = 1", ""), "The specified function name (foo) is unknown. Make sure the function name is correct.").await;
    one(&a, &sql("sys.Bogus = 'x'", ""), "The specified property Bogus does not exist, or it is not a gettable property of the Microsoft.ServiceBus.Messaging.BrokeredMessage class.").await;
    one(&a, &sql("a ==", ""), "There was an error parsing the SQL expression. [Token line=1, column=3, Token in error= =, Additional details= syntax error, unexpected EQ]").await;
    one(&a, &sql("a = 1 AND", ""), "There was an error parsing the SQL expression. [Token line=2, column=0, Token in error=  , Additional details= syntax error, unexpected EOF]").await;
    one(&a, &sql("TRUE", ""), "syntax error, unexpected EOF").await;
    one(&a, &sql("a LIKE 5", ""), "Expression of type 'System.Int64' cannot be used for parameter of type 'System.Object'").await;
    one(&a, &sql(&"a = 1 AND ".repeat(120), ""), "The length of the filter statement is 1199, which exceeds the maximum length of 1024")
        .await;
    let action = |e: &str| {
        format!("<Filter xsi:type=\"TrueFilter\"/><Action xsi:type=\"SqlRuleAction\"><SqlExpression>{e}</SqlExpression></Action>")
    };
    one(&a, &action("SET a = 1, b = 2"), "Unrecognized character. ','").await;
    one(&a, &action("SET sys.Bogus = 'x'"), "The specified property (Bogus) does not exist or it is not a settable property of the Microsoft.ServiceBus.Messaging.BrokeredMessage class.").await;
    one(&a, "<Filter xsi:type=\"CorrelationFilter\"/>", "At least one system or user property must be set for a correlation filter.").await;
    // taken: `REMOVE sys.Label`
    let r = a.rule("t", "s", "removes", &action("REMOVE sys.Label")).await;
    assert_eq!(r.status, 201, "{}", r.text());
}
