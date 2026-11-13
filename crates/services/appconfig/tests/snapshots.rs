mod common;

use common::*;
use serde_json::{json, Value};

async fn create(t: &T, name: &str, body: Value) -> azemu_core::testing::TestResponse {
    t.call("PUT", &format!("/snapshots/{name}"), &[("Content-Type", "application/json")], &body.to_string()).await
}

async fn seeded() -> T {
    let t = start().await;
    t.put("app/a", None, json!({"value":"a1","content_type":"text/x","tags":{"t":"1"}})).await;
    t.put("app/b", Some("prod"), json!({"value":"b1"})).await;
    t.put("app/b", Some("dev"), json!({"value":"b-dev"})).await;
    t.put("other", None, json!({"value":"o"})).await;
    t
}

#[tokio::test]
async fn create_answers_201_provisioning_with_operation_location() {
    let t = seeded().await;
    let r = create(&t, "snap1", json!({"filters":[{"key":"app/*","label":"prod"}],"retention_period":3600})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.snapshot+json; charset=utf-8"));
    let b = r.json();
    let names: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["etag", "name", "status", "filters", "composition_type", "created", "size", "items_count", "tags", "retention_period"]
    );
    assert_eq!(b["status"], "provisioning");
    assert_eq!(b["size"], 0);
    assert_eq!(b["items_count"], 0);
    assert_eq!(b["composition_type"], "key");
    assert_eq!(b["filters"], json!([{"key":"app/*","label":"prod","tags":[]}]));
    assert_eq!(b["retention_period"], 3600);
    assert_eq!(b["tags"], json!({}));
    assert!(b["created"].as_str().unwrap().contains('.'), "created has 7 fraction digits while provisioning");
    assert_eq!(r.header("etag"), Some(format!("\"{}\"", b["etag"].as_str().unwrap()).as_str()));
    assert!(r.header("last-modified").is_some());
    assert_eq!(r.header("link"), Some("</kv?snapshot=snap1&api-version=2024-09-01>; rel=\"items\""));
    let op = r.header("operation-location").unwrap();
    assert_eq!(op, format!("{}/operations?snapshot=snap1&api-version=2024-09-01", t.base));
}

#[tokio::test]
async fn operation_endpoint_finishes_at_once() {
    let t = seeded().await;
    create(&t, "snap1", json!({"filters":[{"key":"*"}]})).await;
    let r = t.get("/operations?snapshot=snap1").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));
    let b = r.json();
    assert_eq!(b["status"], "Succeeded");
    assert_eq!(b["error"], Value::Null);
    assert_eq!(t.get("/operations?snapshot=nope").await.status, 404);
    assert_eq!(t.get("/operations").await.status, 400);
}

#[tokio::test]
async fn get_after_creation_is_ready_with_counts() {
    let t = seeded().await;
    let created = create(&t, "snap1", json!({"filters":[{"key":"app/*","label":"*"}],"composition_type":"key_label"})).await;
    let r = t.get("/snapshots/snap1").await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["status"], "ready");
    assert_eq!(b["items_count"], 3);
    assert_eq!(b["composition_type"], "key_label");
    assert_eq!(b["size"], 1000);
    assert_eq!(b["retention_period"], 2592000);
    assert!(!b["created"].as_str().unwrap().contains('.'));
    assert!(b.get("expires").is_none());
    // the etag does not change when provisioning ends
    assert_eq!(b["etag"], created.json()["etag"]);
    assert_eq!(r.header("link"), Some("</kv?snapshot=snap1&api-version=2024-09-01>; rel=\"items\""));
}

#[tokio::test]
async fn description_field_for_newer_versions() {
    let t = seeded().await;
    let r = t.call_v("2026-04-01", "PUT", "/snapshots/s", &[], &json!({"filters":[{"key":"*"}],"description":"hi"}).to_string()).await;
    assert_eq!(r.json()["description"], "hi");
    let names: Vec<String> = r.json().as_object().unwrap().keys().cloned().collect();
    assert_eq!(&names[..3], ["etag", "name", "description"]);
    assert!(t.get("/snapshots/s").await.json().get("description").is_none());
}

#[tokio::test]
async fn snapshot_content_is_frozen_at_creation() {
    let t = seeded().await;
    create(&t, "frozen", json!({"filters":[{"key":"app/*","label":"*"}],"composition_type":"key_label"})).await;
    // change the store afterwards
    t.put_value("app/a", None, "a2").await;
    t.delete("app/b", Some("dev")).await;
    t.put_value("app/new", None, "n").await;
    let r = t.get("/kv?snapshot=frozen").await;
    assert_eq!(r.status, 200);
    let items = r.json()["items"].as_array().unwrap().clone();
    let got: Vec<(String, Option<String>, String)> = items
        .iter()
        .map(|i| (i["key"].as_str().unwrap().into(), i["label"].as_str().map(Into::into), i["value"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        got,
        [
            ("app/a".to_string(), None, "a1".to_string()),
            ("app/b".to_string(), Some("dev".to_string()), "b-dev".to_string()),
            ("app/b".to_string(), Some("prod".to_string()), "b1".to_string()),
        ]
    );
    assert_eq!(items[0]["tags"], json!({"t":"1"}));
    assert_eq!(items[0]["content_type"], "text/x");
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.kvset+json; charset=utf-8"));
    assert!(r.header("etag").is_some());
    // $select works on snapshot items
    let r = t.get("/kv?snapshot=frozen&$select=key").await;
    assert_eq!(r.json()["items"][0], json!({"key":"app/a"}));
    assert_eq!(t.get("/kv?snapshot=missing").await.status, 404);
}

#[tokio::test]
async fn key_composition_keeps_one_setting_per_key() {
    let t = seeded().await;
    create(&t, "bykey", json!({"filters":[{"key":"app/b","label":"*"}],"composition_type":"key"})).await;
    let r = t.get("/kv?snapshot=bykey").await;
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(t.get("/snapshots/bykey").await.json()["items_count"], 1);
    // a later filter wins
    create(&t, "later", json!({"filters":[{"key":"app/b","label":"dev"},{"key":"app/b","label":"prod"}]})).await;
    let r = t.get("/kv?snapshot=later").await;
    assert_eq!(r.json()["items"][0]["label"], "prod");
}

#[tokio::test]
async fn a_filter_without_label_means_the_null_label() {
    let t = seeded().await;
    create(&t, "nolabel", json!({"filters":[{"key":"app/*"}]})).await;
    let r = t.get("/kv?snapshot=nolabel").await;
    assert_eq!(keys_of(&r), ["app/a"]);
}

#[tokio::test]
async fn tag_filters_in_snapshots() {
    let t = seeded().await;
    create(&t, "tagged", json!({"filters":[{"key":"*","label":"*","tags":["t=1"]}]})).await;
    assert_eq!(keys_of(&t.get("/kv?snapshot=tagged").await), ["app/a"]);
    let r = t.get("/snapshots/tagged").await;
    assert_eq!(r.json()["filters"][0]["tags"], json!(["t=1"]));
}

#[tokio::test]
async fn duplicate_name_is_409() {
    let t = seeded().await;
    assert_eq!(create(&t, "dup", json!({"filters":[{"key":"*"}]})).await.status, 201);
    let r = create(&t, "dup", json!({"filters":[{"key":"*"}]})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.header("content-type"), Some("application/problem+json; charset=utf-8"));
    assert_eq!(r.json()["status"], 409);
    assert_eq!(r.json()["name"], "dup");
}

#[tokio::test]
async fn invalid_creation_requests_are_400() {
    let t = seeded().await;
    assert_eq!(create(&t, "a", json!({})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[]})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[{"label":"x"}]})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[{"key":"*"},{"key":"*"},{"key":"*"},{"key":"*"}]})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[{"key":"*"}],"composition_type":"all"})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[{"key":"*"}],"retention_period":10})).await.status, 400);
    assert_eq!(create(&t, "a", json!({"filters":[{"key":"a*b"}]})).await.status, 400);
    assert_eq!(t.call("PUT", "/snapshots/a", &[], "nope").await.status, 400);
    assert_eq!(t.get("/snapshots/a").await.status, 404);
}

#[tokio::test]
async fn archive_and_recover_with_etag_conditions() {
    let t = seeded().await;
    let created = create(&t, "s", json!({"filters":[{"key":"*"}],"retention_period":3600})).await;
    let etag0 = created.header("etag").unwrap().to_string();
    let patch = |status: &'static str, headers: Vec<(&'static str, String)>| {
        let t = &t;
        async move {
            let mut h = vec![("Content-Type", "application/json")];
            let owned: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            h.extend(owned);
            t.call("PATCH", "/snapshots/s", &h, &json!({"status": status}).to_string()).await
        }
    };
    let r = patch("archived", vec![("If-Match", "\"wrong etag\"".into())]).await;
    assert_eq!(r.status, 412);
    assert_eq!(r.text(), "");
    assert_eq!(t.get("/snapshots/s").await.json()["status"], "ready");

    let r = patch("archived", vec![("If-Match", etag0.clone())]).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["status"], "archived");
    assert!(b["expires"].as_str().unwrap().contains('.'));
    assert_ne!(r.header("etag"), Some(etag0.as_str()));
    let names: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        names,
        ["etag", "name", "status", "filters", "composition_type", "created", "expires", "size", "items_count", "tags", "retention_period"]
    );
    assert_eq!(r.header("link"), Some("</kv?snapshot=s&api-version=2024-09-01>; rel=\"items\""));
    // an archived snapshot can still be read
    assert_eq!(t.get("/kv?snapshot=s").await.status, 200);

    let r = patch("ready", vec![]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["status"], "ready");
    assert!(r.json().get("expires").is_none());

    assert_eq!(patch("bogus", vec![]).await.status, 400);
    let r = t.call("PATCH", "/snapshots/missing", &[("Content-Type", "application/json")], r#"{"status":"archived"}"#).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn conditional_get_of_a_snapshot() {
    let t = seeded().await;
    let etag = create(&t, "s", json!({"filters":[{"key":"*"}]})).await.header("etag").unwrap().to_string();
    assert_eq!(t.call("GET", "/snapshots/s", &[("If-None-Match", &etag)], "").await.status, 304);
    assert_eq!(t.call("GET", "/snapshots/s", &[("If-Match", "\"x\"")], "").await.status, 412);
    assert_eq!(t.call("HEAD", "/snapshots/s", &[], "").await.status, 200);
}

#[tokio::test]
async fn list_snapshots_with_filters() {
    let t = seeded().await;
    assert_eq!(t.get("/snapshots").await.json(), json!({"items": []}));
    for n in ["alpha", "beta", "alpha2"] {
        create(&t, n, json!({"filters":[{"key":"*"}]})).await;
    }
    t.call("PATCH", "/snapshots/beta", &[("Content-Type", "application/json")], r#"{"status":"archived"}"#).await;
    let r = t.get("/snapshots").await;
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.snapshotset+json; charset=utf-8"));
    let names = |r: &azemu_core::testing::TestResponse| -> Vec<String> {
        r.json()["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(names(&r), ["alpha", "alpha2", "beta"]);
    assert!(r.json()["items"][0].get("etag").is_some());
    assert_eq!(names(&t.get("/snapshots?status=ready").await), ["alpha", "alpha2"]);
    assert_eq!(names(&t.get("/snapshots?status=archived").await), ["beta"]);
    assert_eq!(names(&t.get("/snapshots?status=ready,archived").await).len(), 3);
    assert_eq!(names(&t.get("/snapshots?name=alpha*").await), ["alpha", "alpha2"]);
    let r = t.get("/snapshots?$select=name,status").await;
    assert_eq!(r.json()["items"][0], json!({"name":"alpha","status":"ready"}));
    let r = t.call_v("2026-04-01", "GET", "/snapshots", &[], "").await;
    assert_eq!(r.header("x-ms-item-count"), Some("3"));
    assert_eq!(t.call("HEAD", "/snapshots", &[], "").await.status, 200);
}

#[tokio::test]
async fn snapshots_are_per_store() {
    let t = seeded().await;
    create(&t, "mine", json!({"filters":[{"key":"*"}]})).await;
    let other = t.base_of("elsewhere");
    let r = t.srv.send("GET", &format!("{other}/snapshots/mine?api-version={VERSION}"), &[AUTH], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn select_on_a_single_snapshot() {
    let t = seeded().await;
    create(&t, "s", json!({"filters":[{"key":"*"}]})).await;
    let r = t.get("/snapshots/s?$select=name,items_count").await;
    assert_eq!(r.json(), json!({"name":"s","items_count":2}));
}

/// Recording `test_appconfig_client.pyTestAppConfigurationClientAADtest_list_snapshot_configuration_settings.json`, entries 12 and 13, 14 and 16:
/// a ready snapshot of no item has `items_count: 0` and `size: 1000`.
#[tokio::test]
async fn an_empty_snapshot_has_the_minimum_size() {
    let t = seeded().await;
    create(&t, "empty", json!({"filters":[{"key":"nothing-like-this/*","label":"*"}]})).await;
    let b = t.get("/snapshots/empty").await.json();
    assert_eq!((b["status"].as_str(), b["items_count"].as_i64(), b["size"].as_i64()), (Some("ready"), Some(0), Some(1000)));
}
