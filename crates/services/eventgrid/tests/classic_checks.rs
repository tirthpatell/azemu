//! What a custom topic checks on `POST /api/events` before it looks at an event, and the answers of its other paths.
//! The expected texts are what a real topic answered when they were tried (November 2026, `docs/services/eventgrid.md`,
//! "Checked against Azure"); the key and the names below are made up for the test.

mod common;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_eventgrid::{EventGridService, Options};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::*;
use serde_json::{json, Value};
use std::sync::Arc;

const KEY1: &str = "a2V5LW9uZS0wMTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM=";
const KEY2: &str = "a2V5LXR3by0wMTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM=";
const RESOURCE_ID: &str =
    "/subscriptions/11111111-1111-1111-1111-111111111111/resourceGroups/rg/providers/Microsoft.EventGrid/topics/keyed";

async fn keyed() -> Eg {
    let t = Eg::new().await;
    let body = json!({"inputSchema": "EventGridSchema", "keys": [KEY1, KEY2], "resourceId": RESOURCE_ID});
    assert_eq!(t.put_json("/topics/keyed", &body).await.status, 201);
    t
}

/// The message without the `Report '...'` sentence at its end.
fn message(r: &TestResponse) -> String {
    let m = r.json()["error"]["message"].as_str().unwrap_or("").to_string();
    m.split(" Report '").next().unwrap_or("").to_string()
}

async fn post(t: &Eg, topic: &str, headers: &[(&str, &str)], query: &str, body: String) -> TestResponse {
    let q = if query.is_empty() { String::new() } else { format!("?{query}") };
    let mut b = t.srv.request("POST", &format!("{}/api/events{q}", t.host_url(topic))).header("Content-Type", "application/json");
    for (k, v) in headers {
        b = b.header(k, v);
    }
    b.body(body).send().await
}

fn one() -> String {
    json!([eg("1", "T", "/s")]).to_string()
}

#[tokio::test]
async fn a_wrong_key_is_401_and_a_right_one_passes() {
    let t = keyed().await;
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", KEY1)], "", one()).await.status, 200);
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", KEY2)], "", one()).await.status, 200);
    // the key is trimmed
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", &format!("{KEY1} "))], "", one()).await.status, 200);
    // a key that is valid base64 but not this topic's
    let r = post(&t, "keyed", &[("aeg-sas-key", "wrongkey")], "", one()).await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["code"], "Unauthorized");
    assert_eq!(r.json()["error"]["details"][0]["code"], "Unauthorized");
    assert_eq!(message(&r), "The request authorization key is not authorized for KEYED.EVENTGRID.LOCALHOST.");
    assert!(r.json()["error"]["message"].as_str().unwrap().ends_with(" (UTC)' to our forums for assistance or raise a support ticket."));
    // a key that is not base64 gets the reason
    let r = post(&t, "keyed", &[("aeg-sas-key", "wrongkey1")], "", one()).await;
    assert_eq!(r.status, 401);
    assert!(message(&r).starts_with("The request authorization key is not authorized for KEYED.EVENTGRID.LOCALHOST. This is due to the reason: The input is not a valid Base-64 string"));
    // the upper case spelling of a key is another key
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", &KEY1.to_uppercase())], "", one()).await.status, 401);
    // an empty key is a 400
    let r = post(&t, "keyed", &[("aeg-sas-key", "")], "", one()).await;
    assert_eq!(r.status, 400);
    assert_eq!(message(&r), "Request must have a value for aeg-sas-key.");
    assert_eq!(r.json()["error"]["details"][0]["code"], "InvalidSas");
}

#[tokio::test]
async fn the_key_header_wins_over_a_token() {
    let t = keyed().await;
    // a good key with a bad token passes; a bad key with a good token does not
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", KEY1), ("aeg-sas-token", "garbage")], "", one()).await.status, 200);
    let good = sas(&t, KEY1, 60);
    assert_eq!(post(&t, "keyed", &[("aeg-sas-key", "wrongkey"), ("aeg-sas-token", &good)], "", one()).await.status, 401);
}

#[tokio::test]
async fn no_credential_and_other_schemes() {
    let t = keyed().await;
    let r = post(&t, "keyed", &[], "", one()).await;
    assert_eq!(r.status, 401);
    assert_eq!(message(&r), "Request must contain one of the following authorization signature: aeg-sas-token, aeg-sas-key.");
    for scheme in ["SharedAccessKey abc", "Basic abc"] {
        let r = post(&t, "keyed", &[("Authorization", scheme)], "", one()).await;
        assert_eq!(r.status, 401, "{scheme}");
        assert_eq!(message(&r), "Request has an unsupported Authorization scheme (must be SharedAccessSignature or Bearer).");
    }
    // a bearer token is not looked at (there is no tenant to ask), as the README says
    assert_eq!(post(&t, "keyed", &[("Authorization", "Bearer anything")], "", one()).await.status, 200);
}

fn sas(t: &Eg, key: &str, minutes: i64) -> String {
    let host = t.host_url("keyed");
    let resource = format!("{host}/api/events?apiVersion=2018-01-01");
    let expiry = azemu_core::time::OffsetDateTime::now_utc() + azemu_core::time::Duration::minutes(minutes);
    // the Python SDK writes the expiry as str(datetime)
    let e = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        expiry.year(),
        u8::from(expiry.month()),
        expiry.day(),
        expiry.hour(),
        expiry.minute(),
        expiry.second()
    );
    let enc = |s: &str| {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect::<String>()
    };
    let unsigned = format!("r={}&e={}", enc(&resource), enc(&e));
    let sig = azemu_core::storage::sig::hmac_sha256(&STANDARD.decode(key).unwrap(), unsigned.as_bytes());
    format!("{unsigned}&s={}", enc(&STANDARD.encode(sig)))
}

#[tokio::test]
async fn a_signed_token_is_checked_against_the_keys() {
    let t = keyed().await;
    for key in [KEY1, KEY2] {
        let token = sas(&t, key, 60);
        assert_eq!(post(&t, "keyed", &[("aeg-sas-token", &token)], "", one()).await.status, 200);
        assert_eq!(post(&t, "keyed", &[("Authorization", &format!("SharedAccessSignature {token}"))], "", one()).await.status, 200);
    }
    // signed with another key
    let other = STANDARD.encode(b"another key of thirty-two bytes!!");
    let r = post(&t, "keyed", &[("aeg-sas-token", &sas(&t, &other, 60))], "", one()).await;
    assert_eq!(r.status, 401);
    assert_eq!(message(&r), "Invalid signature. The request authorization signature was not signed with any of the topic keys.");
    // over: a token must have more than five minutes left (ten minutes minus five of grace)
    let r = post(&t, "keyed", &[("aeg-sas-token", &sas(&t, KEY1, 2))], "", one()).await;
    assert_eq!(r.status, 401);
    assert!(message(&r).starts_with("Expired. e:"), "{}", message(&r));
    assert!(message(&r).ends_with(" grace period for clock skew . aeg-sas-token."), "{}", message(&r));
    assert_eq!(post(&t, "keyed", &[("aeg-sas-token", &sas(&t, KEY1, 6))], "", one()).await.status, 200);
    // made for another host
    let token = sas(&t, KEY1, 60).replace("keyed.eventgrid.localhost", "other.eventgrid.localhost");
    let r = post(&t, "keyed", &[("aeg-sas-token", &token)], "", one()).await;
    assert_eq!(r.status, 401);
    assert!(message(&r).starts_with("The authorization signature does not apply to resource: "));
}

#[tokio::test]
async fn a_token_that_is_not_the_format_is_400() {
    let t = keyed().await;
    let r = post(&t, "keyed", &[("aeg-sas-token", "garbage")], "", one()).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["details"][0]["code"], "InvalidSas");
    assert_eq!(
        message(&r),
        "Malformed value for aeg-sas-token. Valid format :r=UrlEncodedResource&e=UrlEncodedUtcExpiration&s=UrlEncodedSignature. Input sas string: garbage."
    );
    for bad in ["r=x&e=y", "r=x&e=tomorrow&s=zzz", "r=x&e=11%2F20%2F2030+00%3A00%3A00&s=zzz"] {
        let r = post(&t, "keyed", &[("aeg-sas-token", bad)], "", one()).await;
        assert_eq!(r.status, 400, "{bad}");
        assert_eq!(
            message(&r),
            format!("Invalid value in aeg-sas-token. Valid format :r=UrlEncodedResource&e=UrlEncodedUtcExpiration&s=UrlEncodedSignature. Input sas string: {bad}.")
        );
    }
}

#[tokio::test]
async fn a_topic_without_keys_takes_any_credential() {
    let t = Eg::new().await;
    assert_eq!(t.put_json("/topics/open", &json!({})).await.status, 201);
    assert_eq!(post(&t, "open", &[("aeg-sas-key", "whatever")], "", one()).await.status, 200);
    assert_eq!(post(&t, "open", &[("aeg-sas-token", "garbage")], "", one()).await.status, 200);
    // keys can be added later and taken away again
    assert_eq!(t.put_json("/topics/open", &json!({"keys": [KEY1]})).await.status, 200);
    assert_eq!(post(&t, "open", &[("aeg-sas-key", "whatever")], "", one()).await.status, 401);
    assert_eq!(t.put_json("/topics/open", &json!({})).await.status, 200);
    assert_eq!(post(&t, "open", &[("aeg-sas-key", "whatever")], "", one()).await.status, 401, "a PUT without keys leaves them as they are");
    assert_eq!(t.put_json("/topics/open", &json!({"keys": []})).await.status, 200);
    assert_eq!(post(&t, "open", &[("aeg-sas-key", "whatever")], "", one()).await.status, 200);
}

#[tokio::test]
async fn keys_from_the_config_file_and_from_a_restart() {
    let dir = std::env::temp_dir().join(format!("azemu-eg-keys-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("eventgrid.json");
    std::fs::write(&cfg, json!({"topics": [{"name": "filed", "keys": [KEY1]}]}).to_string()).unwrap();
    let opts = Options { config_path: Some(cfg), ..fast_options() };
    let t = Eg { srv: TestServer::start(vec![Arc::new(EventGridService::with_options(opts))]).await };
    assert_eq!(post(&t, "filed", &[("aeg-sas-key", KEY1)], "", one()).await.status, 200);
    assert_eq!(post(&t, "filed", &[("aeg-sas-key", KEY2)], "", one()).await.status, 401);
    // keys are saved with the topic
    let data = dir.join("data");
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let t = Eg { srv: TestServer::start_persistent(vec![svc], &data).await };
    assert_eq!(t.put_json("/topics/kept", &json!({"keys": [KEY2]})).await.status, 201);
    t.srv.shutdown().await.unwrap();
    let svc = Arc::new(EventGridService::with_options(fast_options()));
    let t = Eg { srv: TestServer::start_persistent(vec![svc], &data).await };
    assert_eq!(post(&t, "kept", &[("aeg-sas-key", KEY2)], "", one()).await.status, 200);
    assert_eq!(post(&t, "kept", &[("aeg-sas-key", KEY1)], "", one()).await.status, 401);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn api_version_is_2018_01_01_or_not_a_version() {
    let t = keyed().await;
    let key = [("aeg-sas-key", KEY1)];
    for ok in ["", "api-version=2018-01-01", "api-version=2018-1-1", "api-version=banana"] {
        assert_eq!(post(&t, "keyed", &key, ok, one()).await.status, 200, "{ok}");
    }
    for bad in ["2099-01-01", "1999-01-01", "2017-06-15-preview", "2019-06-01", "2018-01-02", "2018-01-01-preview", "2"] {
        let r = post(&t, "keyed", &key, &format!("api-version={bad}"), one()).await;
        assert_eq!(r.status, 400, "{bad}");
        let j: Value = r.json();
        assert_eq!(j["error"]["code"], "UnsupportedApiVersion");
        assert!(j["error"]["innerError"].is_null());
        assert!(j["error"].get("details").is_none());
        assert_eq!(
            j["error"]["message"],
            format!(
                "The HTTP resource that matches the request URI '{}/api/events' does not support the API version '{bad}'.",
                t.host_url("keyed")
            )
        );
    }
    // the version is looked at before the key
    let r = post(&t, "keyed", &[("aeg-sas-key", "wrongkey")], "api-version=2099-01-01", one()).await;
    assert_eq!(r.json()["error"]["code"], "UnsupportedApiVersion");
}

fn event_of_size(size: usize) -> String {
    let mut e = json!({"id":"1","subject":"/s","eventType":"t","eventTime":"2026-11-14T00:00:00Z","dataVersion":"1","data":{"s":""}});
    let have = serde_json::to_vec(&e).unwrap().len();
    e["data"]["s"] = json!("x".repeat(size - have));
    assert_eq!(serde_json::to_vec(&e).unwrap().len(), size);
    format!("[{e}]")
}

#[tokio::test]
async fn one_event_may_be_about_a_megabyte() {
    let t = keyed().await;
    let key = [("aeg-sas-key", KEY1)];
    assert_eq!(post(&t, "keyed", &key, "", event_of_size(1_048_577)).await.status, 200);
    assert_eq!(post(&t, "keyed", &key, "", event_of_size(1_049_430)).await.status, 200);
    let r = post(&t, "keyed", &key, "", event_of_size(1_049_431)).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["code"], "RequestEntityTooLarge");
    assert_eq!(r.json()["error"]["details"][0]["code"], "PayloadTooLarge");
    assert_eq!(message(&r), "The maximum size for the JSON content (1049600) has been exceeded.");
    // one small event next to a big one does not make the big one smaller
    let big = event_of_size(1_049_431);
    let two = format!("[{},{}]", &one()[1..one().len() - 1], &big[1..big.len() - 1]);
    assert_eq!(post(&t, "keyed", &key, "", two).await.status, 413);
}

#[tokio::test]
async fn the_topic_property_must_be_empty_or_the_topics_own_resource_id() {
    let t = keyed().await;
    let key = [("aeg-sas-key", KEY1)];
    let with_topic = |topic: &str| {
        let mut e = eg("1", "T", "/s");
        e["topic"] = json!(topic);
        json!([e]).to_string()
    };
    assert_eq!(post(&t, "keyed", &key, "", with_topic("")).await.status, 200);
    assert_eq!(post(&t, "keyed", &key, "", with_topic(RESOURCE_ID)).await.status, 200);
    let r = post(&t, "keyed", &key, "", with_topic("/x/y")).await;
    assert_eq!(r.status, 401);
    assert_eq!(
        message(&r),
        format!("Property 'topic' was found to be set to '/x/y', but was expected to either be null/empty or be set to {RESOURCE_ID}.")
    );
}

#[tokio::test]
async fn other_paths_and_methods_of_a_topic_host() {
    let t = keyed().await;
    let host = t.host_url("keyed");
    let key = ("aeg-sas-key", KEY1);
    for method in ["GET", "PUT", "DELETE"] {
        let r = t.srv.request(method, &format!("{host}/api/events")).header(key.0, key.1).send().await;
        assert_eq!(r.status, 405, "{method}");
        assert_eq!(r.header("allow"), Some("OPTIONS, POST"));
        assert_eq!(message(&r), ".");
        assert_eq!(r.json()["error"]["code"], "MethodNotAllowed");
    }
    let r = t.srv.request("OPTIONS", &format!("{host}/api/events")).send().await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("allow"), Some("POST, OPTIONS"));
    for path in ["/", "/api"] {
        let r = t.srv.request("POST", &format!("{host}{path}")).header(key.0, key.1).body(one()).send().await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.json()["error"]["code"], "NotFound");
        assert_eq!(r.json()["error"]["details"][0]["code"], "ResourceNotFound");
        let encoded = host.replace(':', "%3A").replace('/', "%2F");
        assert_eq!(
            message(&r),
            format!("No HTTP resource was found that matches the request URI '{encoded}{}'.", path.replace('/', "%2F"))
        );
    }
    // a trailing slash and another case reach the publish endpoint
    assert_eq!(t.srv.request("POST", &format!("{host}/api/events/")).header(key.0, key.1).body(one()).send().await.status, 200);
    assert_eq!(t.srv.request("POST", &format!("{host}/api/Events")).header(key.0, key.1).body(one()).send().await.status, 200);
}

#[tokio::test]
async fn a_topic_fixed_to_the_event_grid_schema_reads_events_as_azure_does() {
    let t = keyed().await;
    let key = [("aeg-sas-key", KEY1)];
    let say = |r: &TestResponse| (r.status, message(r), r.json()["error"]["details"][0]["code"].as_str().unwrap_or("").to_string());
    // an unknown property, a missing one, a number where text goes, no data version
    let mut e = eg("1", "T", "/s");
    e["color"] = json!("red");
    let r = post(&t, "keyed", &key, "", json!([e]).to_string()).await;
    assert_eq!(
        say(&r),
        (400, "This resource is configured for 'EventGridEvent' schema. Received unexpected property 'color' in JSON which does not conform to the expected schema.".into(), "InputJsonInvalid".into())
    );
    let mut e = eg("1", "T", "/s");
    e.as_object_mut().unwrap().remove("subject");
    let r = post(&t, "keyed", &key, "", json!([e]).to_string()).await;
    assert_eq!(say(&r).1, "This resource is configured for 'EventGridEvent' schema and requires 'subject' property to be set.");
    let mut e = eg("1", "T", "/s");
    e["id"] = json!(5);
    e.as_object_mut().unwrap().remove("dataVersion");
    assert_eq!(post(&t, "keyed", &key, "", json!([e]).to_string()).await.status, 200);
    let mut e = eg("1", "T", "/s");
    e["eventTime"] = json!("yesterday");
    assert_eq!(
        say(&post(&t, "keyed", &key, "", json!([e]).to_string()).await).1,
        "The event time property 'eventTime' was not a valid date/time."
    );
    // not an array
    let r = post(&t, "keyed", &key, "", eg("1", "T", "/s").to_string()).await;
    assert!(say(&r).1.ends_with("Token Expected: StartArray, Actual Token Received: StartObject."));
    let r = post(&t, "keyed", &key, "", "[]".into()).await;
    assert!(say(&r).1.ends_with("Token Expected: StartObject, Actual Token Received: EndArray."));
    let r = post(&t, "keyed", &key, "", String::new()).await;
    assert_eq!(say(&r).1, "Unexpected end when reading JSON.");
    // a CloudEvent is refused whatever the content type says, an Event Grid event is taken with a CloudEvents content type
    let r = post(&t, "keyed", &key, "", json!([ce("1", "t")]).to_string()).await;
    assert!(say(&r).1.contains("Received unexpected property 'specversion'"), "{}", say(&r).1);
    let r = t
        .srv
        .request("POST", &format!("{}/api/events", t.host_url("keyed")))
        .header(key[0].0, key[0].1)
        .header("Content-Type", "application/cloudevents-batch+json; charset=utf-8")
        .body(one())
        .send()
        .await;
    assert_eq!(r.status, 200);
}
