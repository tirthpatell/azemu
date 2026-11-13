mod common;

use azemu_eventgrid::Options;
use common::*;
use serde_json::json;

#[tokio::test]
async fn event_grid_schema_batch_is_accepted_with_recorded_headers() {
    let t = Eg::new().await;
    let r = t.publish_eg("topic1", json!([eg("1", "A.B", "/s"), eg("2", "A.B", "/s2")])).await;
    assert_eq!(r.status, 200);
    assert!(r.body.is_empty());
    assert_eq!(r.header("api-supported-versions"), Some("2018-01-01"));
    assert_eq!(r.header("server"), Some("Microsoft-HTTPAPI/2.0"));
    assert_eq!(r.header("strict-transport-security"), Some("max-age=31536000; includeSubDomains"));
    assert_eq!(r.header("content-length"), Some("0"));
    assert!(r.header("x-ms-request-id").is_some());
    assert!(r.header("date").is_some());
}

#[tokio::test]
async fn missing_credential_is_401_with_the_recorded_body_shape() {
    let t = Eg::new().await;
    let url = format!("{}/api/events?api-version=2018-01-01", t.host_url("llawtopic"));
    let r = t.srv.request("POST", &url).header("Content-Type", "application/json").body("[]").send().await;
    assert_eq!(r.status, 401);
    let j = r.json();
    assert_eq!(j["error"]["code"], "Unauthorized");
    assert_eq!(j["error"]["details"][0]["code"], "Unauthorized");
    assert!(j["error"]["message"].as_str().unwrap().contains("LLAWTOPIC.EVENTGRID.LOCALHOST"));
}

#[tokio::test]
async fn any_of_the_three_credential_forms_is_accepted() {
    let t = Eg::new().await;
    let url = format!("{}/api/events?api-version=2018-01-01", t.host_url("t"));
    for (k, v) in [
        ("aeg-sas-key", "abc"),
        ("aeg-sas-token", "r=x&e=y&s=z"),
        ("Authorization", "SharedAccessSignature r=x&e=y&s=z"),
        ("Authorization", "Bearer eyJ.fake.token"),
        ("Authorization", "SharedAccessKey abc"),
    ] {
        let r = t
            .srv
            .request("POST", &url)
            .header(k, v)
            .header("Content-Type", "application/json")
            .body(json!([eg("1", "T", "s")]).to_string())
            .send()
            .await;
        assert_eq!(r.status, 200, "{k}: {v}");
    }
}

#[tokio::test]
async fn cloud_events_batch_single_and_data_forms() {
    let t = Eg::new().await;
    // the shapes in the recordings: data string, dict, none, data_base64, extensions
    let batch = json!([
        {"specversion":"1.0","id":"1","source":"/s","type":"t","time":"2024-05-23T22:06:22.029989+00:00","data":"hello"},
        {"specversion":"1.0","id":"2","source":"/s","type":"t","data":{"message":"Hello World!"}},
        {"specversion":"1.0","id":"3","source":"/s","type":"t"},
        {"specversion":"1.0","id":"4","source":"/s","type":"t","data_base64":"aGVsbG8gd29ybGQ="},
        {"specversion":"1.0","id":"5","source":"/s","type":"t","data":"x","ext1":"extension","reasoncode":204}
    ]);
    assert_eq!(t.publish_ce("ce", batch).await.status, 200);
    let single = t.publish("ce", "application/cloudevents+json; charset=utf-8", &ce("6", "t")).await;
    assert_eq!(single.status, 200);
}

#[tokio::test]
async fn cloud_events_binary_content_mode() {
    let t = Eg::new().await;
    let url = format!("{}/api/events?api-version=2018-01-01", t.host_url("bin"));
    let r = t
        .srv
        .request("POST", &url)
        .header(KEY.0, KEY.1)
        .header("ce-specversion", "1.0")
        .header("ce-id", "7")
        .header("ce-source", "/s")
        .header("ce-type", "t.bin")
        .header("Content-Type", "application/json")
        .body(r#"{"a":1}"#)
        .send()
        .await;
    assert_eq!(r.status, 200);
    // a binary event without ce-id is rejected
    let r = t
        .srv
        .request("POST", &url)
        .header(KEY.0, KEY.1)
        .header("ce-specversion", "1.0")
        .header("ce-source", "/s")
        .header("ce-type", "t.bin")
        .body("x")
        .send()
        .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn custom_schema_accepts_any_objects() {
    let t = Eg::new().await;
    let body = json!([{"customSubject":"sample","customEventType":"sample.event","customId":"1234","customData":"sample data"}]);
    assert_eq!(t.publish_eg("custom", body).await.status, 200);
    assert_eq!(t.publish_eg("custom", json!({"single":"object"})).await.status, 200);
}

#[tokio::test]
async fn event_grid_schema_validation_errors() {
    let t = Eg::new().await;
    // eventType present => the topic is treated as Event Grid schema, so the missing subject is an error
    let mut e = eg("1", "T", "s");
    e.as_object_mut().unwrap().remove("subject");
    let r = t.publish_eg("val", json!([e])).await;
    assert_eq!(r.status, 400);
    let j = r.json();
    assert_eq!(j["error"]["code"], "BadRequest");
    let msg = j["error"]["message"].as_str().unwrap();
    assert!(msg.contains("'EventGridEvent' schema") && msg.contains("'subject'") && msg.contains("[0]"), "{msg}");
    assert_eq!(j["error"]["details"][0]["code"], "InputJsonInvalid");

    let mut bad_time = eg("1", "T", "s");
    bad_time["eventTime"] = json!("not a time");
    assert_eq!(t.publish_eg("val", json!([bad_time])).await.status, 400);

    // an object instead of an array, on a topic fixed to the Event Grid schema
    t.put_json("/topics/egonly", &json!({"inputSchema":"EventGridSchema"})).await;
    assert_eq!(t.publish_eg("egonly", eg("1", "T", "s")).await.status, 400);
    // custom shaped events on a topic fixed to the Event Grid schema
    assert_eq!(t.publish_eg("egonly", json!([{"customId":"1"}])).await.status, 400);
    // empty array
    assert_eq!(t.publish_eg("egonly", json!([])).await.status, 400);
}

#[tokio::test]
async fn cloud_event_validation_errors() {
    let t = Eg::new().await;
    for bad in [
        json!({"specversion":"1.0","id":"1","type":"t"}),
        json!({"specversion":"0.3","id":"1","source":"s","type":"t"}),
        json!({"specversion":"1.0","id":"1","source":"s","type":"t","data":1,"data_base64":"aGk="}),
        json!({"specversion":"1.0","id":"1","source":"s","type":"t","time":"nope"}),
    ] {
        let r = t.publish_ce("cev", json!([bad])).await;
        assert_eq!(r.status, 400, "{bad}");
        assert!(r.json()["error"]["message"].as_str().unwrap().contains("CloudEventSchemaV1_0"));
    }
    // a batch content type with a single object, and the other way round
    assert_eq!(t.publish("cev", "application/cloudevents-batch+json", &ce("1", "t")).await.status, 400);
    assert_eq!(t.publish("cev", "application/cloudevents+json", &json!([ce("1", "t")])).await.status, 400);
}

#[tokio::test]
async fn invalid_json_is_400() {
    let t = Eg::new().await;
    let url = format!("{}/api/events", t.host_url("bad"));
    let r = t.srv.request("POST", &url).header(KEY.0, KEY.1).body("{not json").send().await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "BadRequest");
    let r = t.srv.request("POST", &url).header(KEY.0, KEY.1).body("").send().await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn payload_over_the_recorded_limit_is_413() {
    let t = Eg::new().await;
    let big = "x".repeat(1_600_000);
    let r = t
        .publish_eg(
            "big",
            json!([{"id":"1","subject":"s","eventType":"t","eventTime":"2024-05-23T22:06:35Z","dataVersion":"1","data":big}]),
        )
        .await;
    assert_eq!(r.status, 413);
    let j = r.json();
    assert_eq!(j["error"]["code"], "RequestEntityTooLarge");
    assert_eq!(j["error"]["message"], "The maximum size (1536000) has been exceeded.");
    assert_eq!(j["error"]["details"][0]["code"], "PayloadTooLarge");
    // just under the limit is fine
    let ok = "x".repeat(1_400_000);
    assert_eq!(
        t.publish_eg(
            "big",
            json!([{"id":"1","subject":"s","eventType":"t","eventTime":"2024-05-23T22:06:35Z","dataVersion":"1","data":ok}])
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn partner_channel_header_is_accepted() {
    let t = Eg::new().await;
    let url = format!("{}/api/events?api-version=2018-01-01", t.host_url("partner"));
    let r = t
        .srv
        .request("POST", &url)
        .header(KEY.0, KEY.1)
        .header("aeg-channel-name", "mychannel")
        .header("Content-Type", "application/cloudevents-batch+json; charset=utf-8")
        .body(json!([ce("1", "t")]).to_string())
        .send()
        .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn domain_events_route_by_topic_field_and_need_one() {
    let t = Eg::new().await;
    assert_eq!(t.put_json("/topics/mydomain", &json!({"kind":"domain"})).await.status, 201);
    let mut e = eg("1", "T", "s");
    e["topic"] = json!("orders");
    assert_eq!(t.publish_eg("mydomain", json!([e])).await.status, 200);
    // the domain topic was created on first use
    let r = t.get_admin("/topics/orders?domain=mydomain").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["published"], 1);
    // no topic field on a domain
    let r = t.publish_eg("mydomain", json!([eg("2", "T", "s")])).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("domain topic"));
}

#[tokio::test]
async fn autocreate_off_gives_not_found_for_unknown_topics() {
    let t = Eg::with(Options { autocreate: false, ..fast_options() }).await;
    let r = t.publish_eg("ghost", json!([eg("1", "T", "s")])).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "NotFound");
    // once declared it works
    assert_eq!(t.put_json("/topics/ghost", &json!({})).await.status, 201);
    assert_eq!(t.publish_eg("ghost", json!([eg("1", "T", "s")])).await.status, 200);
}

#[tokio::test]
async fn classic_publish_on_a_namespace_host_is_not_found() {
    let t = Eg::new().await;
    assert_eq!(t.ns_publish("ns", "topic", json!([ce("1", "t")])).await.status, 200);
    let r = t.publish_eg("ns", json!([eg("1", "T", "s")])).await;
    assert_eq!(r.status, 404);
    // and a namespace operation on a custom topic host
    t.publish_eg("plain", json!([eg("1", "T", "s")])).await;
    assert_eq!(t.ns_publish("plain", "topic", json!([ce("1", "t")])).await.status, 404);
}

#[tokio::test]
async fn unbuilt_operations_answer_501_naming_them() {
    let t = Eg::new().await;
    let r = t.srv.request("DELETE", &format!("{}/api/other", t.host_url("t"))).header(KEY.0, KEY.1).send().await;
    assert_eq!(r.status, 501);
    assert_eq!(r.json()["error"]["code"], "NotImplemented");
    assert_eq!(r.json()["error"]["message"], "eventgrid: DELETE /api/other");
    let r =
        t.srv.request("POST", &format!("{}/topics/t/eventsubscriptions/s:frobnicate", t.host_url("n"))).header(KEY.0, KEY.1).send().await;
    assert_eq!(r.status, 501);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("frobnicate"));
    let r = t.srv.request("GET", &format!("{}/api/events", t.host_url("t"))).header(KEY.0, KEY.1).send().await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn reset_drops_topics() {
    let t = Eg::new().await;
    t.publish_eg("gone", json!([eg("1", "T", "s")])).await;
    assert!(t.get_admin("/topics/gone").await.status == 200);
    t.srv.reset();
    assert_eq!(t.get_admin("/topics/gone").await.status, 404);
}
