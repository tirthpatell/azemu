mod common;
use common::*;
use std::time::Duration;

fn first_message(r: &azemu_core::testing::TestResponse) -> (String, String) {
    let b = r.text();
    (tag(&b, "MessageId").unwrap(), tag(&b, "PopReceipt").unwrap())
}

#[tokio::test]
async fn put_returns_201_with_enqueued_message_xml() {
    let q = start().await;
    q.create("putq").await;
    let r = q.put("putq", "hello", "").await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    let b = r.text();
    assert!(b.starts_with("\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><QueueMessagesList><QueueMessage><MessageId>"));
    // element order as Azure sends it
    let order: Vec<usize> = ["MessageId", "InsertionTime", "ExpirationTime", "PopReceipt", "TimeNextVisible"]
        .iter()
        .map(|t| b.find(&format!("<{t}>")).unwrap_or_else(|| panic!("missing {t}")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]));
    assert!(!b.contains("MessageText"));
    // insertion time is an RFC 1123 date
    assert!(tag(&b, "InsertionTime").unwrap().ends_with(" GMT"));
}

#[tokio::test]
async fn ttl_default_explicit_and_never() {
    let q = start().await;
    q.create("ttlq").await;
    let expiry =
        |r: &azemu_core::testing::TestResponse| azemu_core::time::parse_rfc1123(&tag(&r.text(), "ExpirationTime").unwrap()).unwrap();
    let inserted =
        |r: &azemu_core::testing::TestResponse| azemu_core::time::parse_rfc1123(&tag(&r.text(), "InsertionTime").unwrap()).unwrap();

    let r = q.put("ttlq", "a", "").await;
    assert_eq!(expiry(&r) - inserted(&r), azemu_core::time::Duration::days(7));
    let r = q.put("ttlq", "a", "messagettl=3600").await;
    assert_eq!(expiry(&r) - inserted(&r), azemu_core::time::Duration::hours(1));
    let r = q.put("ttlq", "a", "messagettl=-1").await;
    assert_eq!(tag(&r.text(), "ExpirationTime").unwrap(), "Fri, 31 Dec 9999 23:59:59 GMT");
    // longer than 7 days is fine on current versions (recording: messagettl=1073741824)
    let r = q.put("ttlq", "a", "messagettl=1073741824").await;
    assert_eq!(r.status, 201);
    assert_eq!(expiry(&r) - inserted(&r), azemu_core::time::Duration::seconds(1073741824));
    // huge values cap at year 9999
    let r = q.put("ttlq", "a", "messagettl=9223372036854775807").await;
    assert_eq!(tag(&r.text(), "ExpirationTime").unwrap(), "Fri, 31 Dec 9999 23:59:59 GMT");

    for bad in ["0", "-2", "abc"] {
        let r = q.put("ttlq", "a", &format!("messagettl={bad}")).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"), "{bad}");
    }
}

#[tokio::test]
async fn visibility_timeout_on_put_hides_message_and_is_validated() {
    let q = start().await;
    q.create("visq").await;
    let r = q.put("visq", "later", "visibilitytimeout=60").await;
    assert_eq!(r.status, 201);
    assert_ne!(tag(&r.text(), "TimeNextVisible"), tag(&r.text(), "InsertionTime"));
    assert_eq!(tags(&q.get("visq", "").await.text(), "MessageId").len(), 0);
    assert_eq!(tags(&q.peek("visq", "").await.text(), "MessageId").len(), 0);

    let r = q.put("visq", "x", "visibilitytimeout=604801").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"));
    let r = q.put("visq", "x", "visibilitytimeout=-1").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"));
    // visibility must be shorter than the time to live
    let r = q.put("visq", "x", "visibilitytimeout=10&messagettl=10").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
}

#[tokio::test]
async fn get_returns_content_receipt_and_dequeue_count() {
    let q = start().await;
    q.create("getq").await;
    let put = q.put("getq", "one", "").await;
    let r = q.get("getq", "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    let b = r.text();
    assert_eq!(tag(&b, "MessageId"), tag(&put.text(), "MessageId"));
    assert_eq!(tag(&b, "DequeueCount").as_deref(), Some("1"));
    assert_eq!(tag(&b, "MessageText").as_deref(), Some("one"));
    assert_ne!(tag(&b, "PopReceipt"), tag(&put.text(), "PopReceipt"), "getting a message issues a new receipt");
    let order: Vec<usize> =
        ["MessageId", "InsertionTime", "ExpirationTime", "PopReceipt", "TimeNextVisible", "DequeueCount", "MessageText"]
            .iter()
            .map(|t| b.find(&format!("<{t}>")).unwrap())
            .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{b}");
    // default visibility is 30 s
    let next = azemu_core::time::parse_rfc1123(&tag(&b, "TimeNextVisible").unwrap()).unwrap();
    let now = azemu_core::time::OffsetDateTime::now_utc();
    let secs = (next - now).whole_seconds();
    assert!((27..=31).contains(&secs), "visible in {secs} s");
    // now hidden
    assert!(q.get("getq", "").await.text().ends_with("<QueueMessagesList />"));
}

#[tokio::test]
async fn get_numofmessages_and_fifo_order() {
    let q = start().await;
    q.create("fifoq").await;
    for i in 0..5 {
        q.put("fifoq", &format!("m{i}"), "").await;
    }
    let b = q.get("fifoq", "numofmessages=3").await.text();
    assert_eq!(tags(&b, "MessageText"), ["m0", "m1", "m2"]);
    let b = q.get("fifoq", "numofmessages=32").await.text();
    assert_eq!(tags(&b, "MessageText"), ["m3", "m4"], "only visible messages are returned");

    for bad in ["0", "33", "-1"] {
        let r = q.get("fifoq", &format!("numofmessages={bad}")).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"), "{bad}");
        assert!(r.text().contains("<QueryParameterName>numofmessages</QueryParameterName>"));
        assert!(r.text().contains("<MaximumAllowed>32</MaximumAllowed>"));
    }
    let r = q.get("fifoq", "numofmessages=x").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
    let r = q.get("fifoq", "visibilitytimeout=0").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"));
    let r = q.get("fifoq", "visibilitytimeout=604801").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn peek_does_not_hide_or_count() {
    let q = start().await;
    q.create("peekq").await;
    q.put("peekq", "a", "").await;
    q.put("peekq", "b", "").await;
    for _ in 0..2 {
        let b = q.peek("peekq", "numofmessages=2").await.text();
        assert_eq!(tags(&b, "MessageText"), ["a", "b"]);
        assert_eq!(tags(&b, "DequeueCount"), ["0", "0"]);
        assert!(!b.contains("PopReceipt") && !b.contains("TimeNextVisible"));
    }
    assert_eq!(tags(&q.get("peekq", "numofmessages=2").await.text(), "MessageText").len(), 2);
}

#[tokio::test]
async fn visibility_zero_redelivers_and_dequeue_count_grows() {
    let q = start().await;
    q.create("redo").await;
    q.put("redo", "again", "").await;
    for expected in 1..=3 {
        let r = q.get("redo", "").await;
        assert_eq!(tag(&r.text(), "DequeueCount"), Some(expected.to_string()));
        let (id, receipt) = first_message(&r);
        let u = q.send("PUT", &format!("/redo/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt)), &[], "").await;
        assert_eq!(u.status, 204);
    }
}

#[tokio::test]
async fn update_changes_text_receipt_and_visibility() {
    let q = start().await;
    q.create("updq").await;
    q.put("updq", "old", "").await;
    let (id, receipt) = first_message(&q.get("updq", "").await);

    let r = q
        .send(
            "PUT",
            &format!("/updq/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt)),
            &[],
            &msg_body("new &amp; improved"),
        )
        .await;
    assert_eq!(r.status, 204);
    let new_receipt = r.header("x-ms-popreceipt").unwrap().to_string();
    assert_ne!(new_receipt, receipt);
    assert!(r.header("x-ms-time-next-visible").unwrap().ends_with(" GMT"));

    // the old receipt is dead
    let old = q.send("DELETE", &format!("/updq/messages/{id}?popreceipt={}", urlenc(&receipt)), &[], "").await;
    assert_eq!((old.status, error_code(&old).as_str()), (400, "PopReceiptMismatch"));
    let upd = q.send("PUT", &format!("/updq/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt)), &[], "").await;
    assert_eq!((upd.status, error_code(&upd).as_str()), (400, "PopReceiptMismatch"));

    let b = q.get("updq", "").await.text();
    assert_eq!(tag(&b, "MessageText").as_deref(), Some("new &amp; improved"));
    assert_eq!(tag(&b, "DequeueCount").as_deref(), Some("2"), "update does not touch the count");
    // the new receipt was replaced by the get: the update receipt is stale too
    let d = q.send("DELETE", &format!("/updq/messages/{id}?popreceipt={}", urlenc(&new_receipt)), &[], "").await;
    assert_eq!(d.status, 400);
}

#[tokio::test]
async fn update_requires_receipt_and_visibility() {
    let q = start().await;
    q.create("updreq").await;
    let put = q.put("updreq", "x", "").await;
    let (id, receipt) = first_message(&put);
    let r = q.send("PUT", &format!("/updreq/messages/{id}?visibilitytimeout=0"), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredQueryParameter"));
    assert!(r.text().contains("<QueryParameterName>popreceipt</QueryParameterName>"));
    let r = q.send("PUT", &format!("/updreq/messages/{id}?popreceipt={}", urlenc(&receipt)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredQueryParameter"));
    let r = q.send("PUT", &format!("/updreq/messages/{id}?popreceipt={}&visibilitytimeout=604801", urlenc(&receipt)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "OutOfRangeQueryParameterValue"));
    // the receipt from put works for update and delete
    let r = q.send("PUT", &format!("/updreq/messages/{id}?popreceipt={}&visibilitytimeout=30", urlenc(&receipt)), &[], "").await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn delete_message_cases() {
    let q = start().await;
    q.create("delq").await;
    q.put("delq", "a", "").await;
    q.put("delq", "b", "").await;
    let first = first_message(&q.get("delq", "").await);
    let second = first_message(&q.get("delq", "").await);

    // wrong receipt for an existing id
    let r = q.send("DELETE", &format!("/delq/messages/{}?popreceipt={}", first.0, urlenc(&second.1)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "PopReceiptMismatch"));
    // unknown id
    let r =
        q.send("DELETE", &format!("/delq/messages/00000000-0000-0000-0000-000000000000?popreceipt={}", urlenc(&second.1)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "MessageNotFound"));
    // missing receipt
    let r = q.send("DELETE", &format!("/delq/messages/{}", first.0), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredQueryParameter"));

    let r = q.send("DELETE", &format!("/delq/messages/{}?popreceipt={}", first.0, urlenc(&first.1)), &[], "").await;
    assert_eq!(r.status, 204);
    // gone for good, also a second delete
    let r = q.send("DELETE", &format!("/delq/messages/{}?popreceipt={}", first.0, urlenc(&first.1)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "MessageNotFound"));
    let r = q.send("GET", "/delq?comp=metadata", &[], "").await;
    assert_eq!(r.header("x-ms-approximate-messages-count"), Some("1"));
}

#[tokio::test]
async fn clear_removes_visible_and_hidden_messages() {
    let q = start().await;
    q.create("clrq").await;
    q.put("clrq", "a", "").await;
    q.put("clrq", "b", "visibilitytimeout=60").await;
    q.get("clrq", "").await;
    assert_eq!(q.send("DELETE", "/clrq/messages", &[], "").await.status, 204);
    let r = q.send("GET", "/clrq?comp=metadata", &[], "").await;
    assert_eq!(r.header("x-ms-approximate-messages-count"), Some("0"));
    assert!(q.peek("clrq", "").await.text().ends_with("<QueueMessagesList />"));
}

#[tokio::test]
async fn message_size_limit_is_64_kib_of_text() {
    let q = start().await;
    q.create("bigq").await;
    let r = q.put("bigq", &"a".repeat(65536), "").await;
    assert_eq!(r.status, 201, "exactly 64 KiB is accepted");
    let r = q.put("bigq", &"a".repeat(65537), "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (413, "RequestBodyTooLarge"));
    assert!(r.text().contains("<MaxLimit>65536</MaxLimit>"));
    // the limit counts bytes of text, not characters
    let r = q.put("bigq", &"é".repeat(32769), "").await;
    assert_eq!(r.status, 413);
    // and applies to update too
    q.put("bigq", "s", "").await;
    let (id, receipt) = first_message(&q.get("bigq", "numofmessages=1").await);
    let r = q
        .send(
            "PUT",
            &format!("/bigq/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt)),
            &[],
            &msg_body(&"b".repeat(65537)),
        )
        .await;
    assert_eq!(r.status, 413);
    // far larger than any message: refused without being read as XML
    let r = q.put("bigq", &"a".repeat(2_000_000), "").await;
    assert_eq!(r.status, 413);
}

#[tokio::test]
async fn invalid_xml_is_400_invalid_xml_document_with_position() {
    let q = start().await;
    q.create("xmlq").await;
    // control character: line 2 column 28 in the body the SDK sends
    let body = "<?xml version='1.0' encoding='utf-8'?>\n<QueueMessage><MessageText>\u{1}</MessageText></QueueMessage>";
    let r = q.send("POST", "/xmlq/messages", &[], body).await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"));
    let b = r.text();
    assert!(b.contains("<Message>XML specified is not syntactically valid.\nRequestId:"));
    assert_eq!(tag(&b, "LineNumber").as_deref(), Some("2"));
    assert_eq!(tag(&b, "LinePosition").as_deref(), Some("28"));
    assert_eq!(tag(&b, "Reason").as_deref(), Some("Error parsing Xml content"));

    for bad in [
        "",
        "not xml",
        "<QueueMessage><MessageText>unclosed</QueueMessage>",
        "<QueueMessage></QueueMessage>",
        "<Other><MessageText>x</MessageText></Other>",
        "<QueueMessage><MessageText>&bogus;</MessageText></QueueMessage>",
        "<QueueMessage><MessageText>&#1;</MessageText></QueueMessage>",
    ] {
        let r = q.send("POST", "/xmlq/messages", &[], bad).await;
        assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidXmlDocument"), "{bad:?}");
    }
    // a failed put stored nothing
    assert!(q.peek("xmlq", "").await.text().ends_with("<QueueMessagesList />"));
}

#[tokio::test]
async fn text_round_trips_exactly() {
    let q = start().await;
    q.create("textq").await;
    for text in [" mess\t age1\n", "héllo wörld 世界 \u{1f680} Ω", "&lt;b&gt; &amp;", ""] {
        q.send("DELETE", "/textq/messages", &[], "").await;
        let r = q.put("textq", text, "").await;
        assert_eq!(r.status, 201);
        let b = q.get("textq", "").await.text();
        let got = tag(&b, "MessageText").unwrap_or_default();
        assert_eq!(got, text, "stored text comes back as sent (escapes included)");
    }
    // entity references in the request are decoded before storing and escaped again
    q.send("DELETE", "/textq/messages", &[], "").await;
    q.put("textq", "a &lt; b &amp; c", "").await;
    let b = q.peek("textq", "").await.text();
    assert!(b.contains("<MessageText>a &lt; b &amp; c</MessageText>"));
}

#[tokio::test]
async fn unknown_routes_and_verbs() {
    let q = start().await;
    q.create("routeq").await;
    let r = q.send("POST", "/routeq", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (405, "UnsupportedHttpVerb"));
    let r = q.send("GET", "/routeq/messages/id/extra", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidUri"));
    let r = q.send("GET", "/routeq?comp=bogus", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "InvalidQueryParameterValue"));
    // OPTIONS is the CORS preflight; without the CORS headers it is a 400
    let r = q.send("OPTIONS", "/routeq/messages", &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (400, "MissingRequiredHeader"));
}

// --- visibility and expiry with real time (each waits at most 1.3 s) -------------------

#[tokio::test]
async fn message_returns_after_visibility_timeout() {
    let q = start().await;
    q.create("timeq").await;
    q.put("timeq", "t", "").await;
    let r = q.get("timeq", "visibilitytimeout=1").await;
    assert_eq!(tag(&r.text(), "DequeueCount").as_deref(), Some("1"));
    assert!(q.get("timeq", "").await.text().ends_with("<QueueMessagesList />"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = q.get("timeq", "").await;
    assert_eq!(tag(&r.text(), "MessageText").as_deref(), Some("t"));
    assert_eq!(tag(&r.text(), "DequeueCount").as_deref(), Some("2"));
}

#[tokio::test]
async fn message_with_initial_delay_appears_later() {
    let q = start().await;
    q.create("delayq").await;
    q.put("delayq", "d", "visibilitytimeout=1").await;
    assert!(q.peek("delayq", "").await.text().ends_with("<QueueMessagesList />"));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(tag(&q.peek("delayq", "").await.text(), "MessageText").as_deref(), Some("d"));
}

#[tokio::test]
async fn expired_messages_vanish_everywhere() {
    let q = start().await;
    q.create("expq").await;
    q.put("expq", "short", "messagettl=1").await;
    q.put("expq", "long", "").await;
    q.put("expq", "hidden-short", "messagettl=2&visibilitytimeout=1").await;
    let c = |r: azemu_core::testing::TestResponse| r.header("x-ms-approximate-messages-count").unwrap().to_string();
    assert_eq!(c(q.send("GET", "/expq?comp=metadata", &[], "").await), "3");
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(tags(&q.peek("expq", "numofmessages=10").await.text(), "MessageText"), ["long"]);
    assert_eq!(c(q.send("GET", "/expq?comp=metadata", &[], "").await), "1");
    assert_eq!(tags(&q.get("expq", "numofmessages=10").await.text(), "MessageText"), ["long"]);
}

#[tokio::test]
async fn expired_message_cannot_be_updated_or_deleted() {
    let q = start().await;
    q.create("expq2").await;
    let put = q.put("expq2", "x", "messagettl=1").await;
    let (id, receipt) = first_message(&put);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = q.send("DELETE", &format!("/expq2/messages/{id}?popreceipt={}", urlenc(&receipt)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "MessageNotFound"));
    let r = q.send("PUT", &format!("/expq2/messages/{id}?popreceipt={}&visibilitytimeout=0", urlenc(&receipt)), &[], "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "MessageNotFound"));
}
