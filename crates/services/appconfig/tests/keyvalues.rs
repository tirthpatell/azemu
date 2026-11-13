mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn put_returns_the_setting_with_recorded_headers() {
    let t = start().await;
    let r = t
        .put_h(
            "k1",
            Some("dev"),
            json!({"key":"k1","label":"ignored","value":"v","content_type":"text/plain","tags":{"a":"b","c":"d"}}),
            &[("x-ms-client-request-id", "client-1")],
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.kv+json; charset=utf-8"));
    let b = r.json();
    // field order as recorded
    let names: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(names, ["etag", "key", "label", "content_type", "value", "tags", "locked", "last_modified"]);
    assert_eq!(b["label"], "dev", "the label comes from the query, not the body");
    assert_eq!(b["locked"], false);
    assert_eq!(b["tags"], json!({"a":"b","c":"d"}));
    let etag = b["etag"].as_str().unwrap();
    assert_eq!(etag.len(), 43);
    assert_eq!(r.header("etag"), Some(format!("\"{etag}\"").as_str()));
    assert!(r.header("last-modified").unwrap().ends_with("GMT"));
    assert!(b["last_modified"].as_str().unwrap().ends_with("+00:00"));
    assert_eq!(r.header("x-ms-client-request-id"), Some("client-1"));
    assert!(r.header("x-ms-request-id").is_some());
    assert!(r.header("x-ms-correlation-request-id").is_some());
    assert_eq!(r.header("strict-transport-security"), Some("max-age=31536000; includeSubDomains"));
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    let sync = r.header("sync-token").unwrap();
    assert!(sync.starts_with("zAJw6V16=") && sync.contains(";sn="), "{sync}");
}

#[tokio::test]
async fn sync_token_number_increases() {
    let t = start().await;
    let sn =
        |r: &azemu_core::testing::TestResponse| -> u64 { r.header("sync-token").unwrap().rsplit("sn=").next().unwrap().parse().unwrap() };
    let a = t.get("/kv").await;
    let b = t.get("/kv").await;
    assert!(sn(&b) > sn(&a));
}

#[tokio::test]
async fn setting_without_fields_has_nulls_and_empty_tags() {
    let t = start().await;
    let r = t.put("bare", None, json!({})).await;
    let b = r.json();
    assert_eq!(b["label"], serde_json::Value::Null);
    assert_eq!(b["value"], serde_json::Value::Null);
    assert_eq!(b["content_type"], serde_json::Value::Null);
    assert_eq!(b["tags"], json!({}));
    let r = t.put_value("empty", None, "").await;
    assert_eq!(r.json()["value"], "");
}

#[tokio::test]
async fn description_only_with_newer_api_versions() {
    let t = start().await;
    let r = t.put("d", None, json!({"value":"v"})).await;
    assert!(r.json().get("description").is_none(), "2024-09-01 has no description");
    let r = t.call_v("2026-04-01", "GET", "/kv/d", &[], "").await;
    assert_eq!(r.json()["description"], serde_json::Value::Null);
    let r = t.call_v("2023-11-01", "GET", "/kv/d", &[], "").await;
    assert!(r.json().get("description").is_none());
    let r =
        t.call_v("2026-04-01", "PUT", "/kv/d2", &[("Content-Type", "application/json")], r#"{"value":"x","description":"hello"}"#).await;
    assert_eq!(r.json()["description"], "hello");
}

#[tokio::test]
async fn null_label_and_named_label_are_different_settings() {
    let t = start().await;
    t.put_value("k", None, "plain").await;
    t.put_value("k", Some("L"), "labelled").await;
    assert_eq!(t.get("/kv/k").await.json()["value"], "plain");
    assert_eq!(t.get("/kv/k?label=L").await.json()["value"], "labelled");
    // %00 is the null label too
    assert_eq!(t.get("/kv/k?label=%00").await.json()["value"], "plain");
    assert_eq!(t.get("/kv/k?label=other").await.status, 404);
    t.delete("k", None).await;
    assert_eq!(t.get("/kv/k").await.status, 404);
    assert_eq!(t.get("/kv/k?label=L").await.status, 200);
}

#[tokio::test]
async fn get_missing_is_404_with_empty_body() {
    let t = start().await;
    let r = t.get("/kv/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.text(), "");
}

#[tokio::test]
async fn keys_with_slashes_and_special_characters() {
    let t = start().await;
    t.put_value("app/db:conn string", None, "x").await;
    let r = t.get(&format!("/kv/{}", enc("app/db:conn string"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["key"], "app/db:conn string");
    // an unencoded slash in the path is part of the key as well
    let r = t.get("/kv/app/db:conn%20string").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn invalid_keys_are_400() {
    let t = start().await;
    assert_eq!(t.put_value(".", None, "x").await.status, 400);
    assert_eq!(t.put_value("..", None, "x").await.status, 400);
}

#[tokio::test]
async fn etag_changes_on_every_write() {
    let t = start().await;
    let a = t.put_value("k", None, "same").await.json()["etag"].as_str().unwrap().to_string();
    let b = t.put_value("k", None, "same").await.json()["etag"].as_str().unwrap().to_string();
    assert_ne!(a, b);
}

#[tokio::test]
async fn if_none_match_star_makes_add_fail_with_412_and_empty_body() {
    let t = start().await;
    let r = t.put_h("k", None, json!({"value":"1"}), &[("If-None-Match", "*")]).await;
    assert_eq!(r.status, 200);
    let r = t.put_h("k", None, json!({"value":"2"}), &[("If-None-Match", "*")]).await;
    assert_eq!(r.status, 412);
    assert_eq!(r.text(), "");
    assert_eq!(r.header("content-length"), Some("0"));
    assert_eq!(t.get("/kv/k").await.json()["value"], "1");
}

#[tokio::test]
async fn if_match_on_put() {
    let t = start().await;
    let etag = t.put_value("k", None, "1").await.json()["etag"].as_str().unwrap().to_string();
    let r = t.put_h("k", None, json!({"value":"x"}), &[("If-Match", "\"wrong\"")]).await;
    assert_eq!(r.status, 412);
    let quoted = format!("\"{etag}\"");
    let r = t.put_h("k", None, json!({"value":"2"}), &[("If-Match", &quoted)]).await;
    assert_eq!(r.status, 200);
    // If-Match on a missing setting
    let r = t.put_h("missing", None, json!({"value":"x"}), &[("If-Match", "*")]).await;
    assert_eq!(r.status, 412);
    // the unquoted etag the SDK sends in some calls
    let new_etag = r.header("etag").map(str::to_string);
    assert!(new_etag.is_none());
    let cur = t.get("/kv/k").await.json()["etag"].as_str().unwrap().to_string();
    let r = t.put_h("k", None, json!({"value":"3"}), &[("If-Match", &cur)]).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn conditional_get() {
    let t = start().await;
    let etag = t.put_value("k", None, "v").await.header("etag").unwrap().to_string();
    let r = t.call("GET", "/kv/k", &[("If-None-Match", &etag)], "").await;
    assert_eq!(r.status, 304);
    assert_eq!(r.text(), "");
    let r = t.call("GET", "/kv/k", &[("If-None-Match", "\"other\"")], "").await;
    assert_eq!(r.status, 200);
    let r = t.call("GET", "/kv/k", &[("If-Match", "\"wrong etag\"")], "").await;
    assert_eq!(r.status, 412);
    let r = t.call("GET", "/kv/k", &[("If-Match", &etag)], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn head_single_setting_has_headers_and_no_body() {
    let t = start().await;
    let etag = t.put_value("k", None, "v").await.header("etag").unwrap().to_string();
    let r = t.call("HEAD", "/kv/k", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("etag"), Some(etag.as_str()));
    assert_eq!(r.text(), "");
}

#[tokio::test]
async fn select_limits_fields() {
    let t = start().await;
    t.put("k", None, json!({"value":"v","content_type":"text/x"})).await;
    let r = t.get("/kv/k?$select=key,content_type").await;
    assert_eq!(r.json(), json!({"key":"k","content_type":"text/x"}));
    assert_eq!(t.get("/kv/k?$select=bogus").await.status, 400);
}

#[tokio::test]
async fn delete_returns_setting_or_204() {
    let t = start().await;
    let put = t.put("k", Some("L"), json!({"value":"v","tags":{"a":"b"}})).await;
    let r = t.delete("k", Some("L")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["etag"], put.json()["etag"]);
    assert_eq!(r.json()["tags"], json!({"a":"b"}));
    let r = t.delete("k", Some("L")).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.text(), "");
    assert_eq!(t.delete("never", None).await.status, 204);
}

#[tokio::test]
async fn delete_with_if_match() {
    let t = start().await;
    let etag = t.put_value("k", None, "v").await.header("etag").unwrap().to_string();
    let r = t.call("DELETE", "/kv/k", &[("If-Match", "\"wrong etag\"")], "").await;
    assert_eq!(r.status, 412);
    assert_eq!(t.get("/kv/k").await.status, 200);
    let r = t.call("DELETE", "/kv/k", &[("If-Match", &etag)], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn locks_block_writes_and_deletes_with_the_recorded_409() {
    let t = start().await;
    let before = t.put_value("k", Some("L"), "v").await;
    let r = t.call("PUT", "/locks/k?label=L", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["locked"], true);
    assert_ne!(r.json()["etag"], before.json()["etag"]);
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.kv+json; charset=utf-8"));

    let w = t.put_value("k", Some("L"), "w").await;
    assert_eq!(w.status, 409);
    assert_eq!(w.header("content-type"), Some("application/problem+json; charset=utf-8"));
    assert_eq!(
        w.json(),
        json!({
            "type": "https://azconfig.io/errors/key-locked",
            "title": "Modifing key 'k' is not allowed",
            "name": "k",
            "detail": "The key is read-only. To allow modification unlock it first.",
            "status": 409
        })
    );
    assert_eq!(t.delete("k", Some("L")).await.status, 409);
    // reading is fine and shows the lock
    assert_eq!(t.get("/kv/k?label=L").await.json()["locked"], true);
    // other labels are not locked
    assert_eq!(t.put_value("k", None, "free").await.status, 200);

    let r = t.call("DELETE", "/locks/k?label=L", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["locked"], false);
    assert_eq!(t.put_value("k", Some("L"), "w").await.status, 200);
}

#[tokio::test]
async fn lock_conditions_and_missing_setting() {
    let t = start().await;
    t.put_value("k", None, "v").await;
    let r = t.call("PUT", "/locks/k", &[("If-Match", "\"wrong etag\"")], "").await;
    assert_eq!(r.status, 412);
    assert_eq!(t.get("/kv/k").await.json()["locked"], false);
    let r = t.call("PUT", "/locks/missing", &[], "").await;
    assert_eq!(r.status, 404);
    let r = t.call("DELETE", "/locks/missing", &[], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn lock_survives_a_conditional_overwrite_attempt_order() {
    // a failing condition is reported before the lock
    let t = start().await;
    t.put_value("k", None, "v").await;
    t.call("PUT", "/locks/k", &[], "").await;
    let r = t.put_h("k", None, json!({"value":"x"}), &[("If-Match", "\"wrong\"")]).await;
    assert_eq!(r.status, 412);
}

#[tokio::test]
async fn feature_flags_and_secret_references_round_trip_untouched() {
    let t = start().await;
    let ff_value = r#"{"id":"Beta","description":"","enabled":true,"conditions":{"client_filters":[{"name":"Microsoft.Percentage","parameters":{"Value":50}}]}}"#;
    let ff_ct = "application/vnd.microsoft.appconfig.ff+json;charset=utf-8";
    let key = ".appconfig.featureflag/Beta";
    let r = t.put(key, Some("prod"), json!({"value": ff_value, "content_type": ff_ct})).await;
    assert_eq!(r.status, 200);
    let g = t.get(&format!("/kv/{}?label=prod", enc(key))).await.json();
    assert_eq!(g["value"], ff_value);
    assert_eq!(g["content_type"], ff_ct);
    assert_eq!(g["key"], key);

    let ref_value = r#"{"uri":"https://myvault.vault.azure.net/secrets/mysecret"}"#;
    let ref_ct = "application/vnd.microsoft.appconfig.keyvaultref+json;charset=utf-8";
    t.put("db/secret", None, json!({"value": ref_value, "content_type": ref_ct})).await;
    let g = t.get("/kv/db%2Fsecret").await.json();
    assert_eq!(g["value"], ref_value);
    assert_eq!(g["content_type"], ref_ct);

    // listed with the key prefix filter (how clients find flags)
    let l = t.get("/kv?key=.appconfig.featureflag%2F*").await.json();
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
    assert_eq!(l["items"][0]["value"], ff_value);
}

#[tokio::test]
async fn bad_bodies_are_400() {
    let t = start().await;
    let h = [("Content-Type", "application/json")];
    assert_eq!(t.call("PUT", "/kv/k", &h, "not json").await.status, 400);
    assert_eq!(t.call("PUT", "/kv/k", &h, "[1]").await.status, 400);
    assert_eq!(t.call("PUT", "/kv/k", &h, r#"{"value": 5}"#).await.status, 400);
    assert_eq!(t.call("PUT", "/kv/k", &h, r#"{"tags": {"a": 1}}"#).await.status, 400);
}

#[tokio::test]
async fn stores_are_separate_and_reset_clears_them() {
    let t = start().await;
    t.put_value("k", None, "v").await;
    let other = t.base_of("otherstore");
    let r = t.srv.send("GET", &format!("{other}/kv/k?api-version={VERSION}"), &[AUTH], "").await;
    assert_eq!(r.status, 404);
    t.srv.reset();
    assert_eq!(t.get("/kv/k").await.status, 404);
}

#[tokio::test]
async fn accept_datetime_reads_the_past() {
    let t = start().await;
    t.put_value("k", None, "old").await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let probe = azemu_core::time::rfc1123(azemu_core::time::OffsetDateTime::now_utc());
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    t.put_value("k", None, "new").await;
    t.put_value("other", None, "later").await;
    let r = t.call("GET", "/kv/k", &[("Accept-Datetime", &probe)], "").await;
    assert_eq!(r.json()["value"], "old");
    assert_eq!(r.header("memento-datetime"), Some(probe.as_str()));
    assert_eq!(t.get("/kv/k").await.json()["value"], "new");
    let l = t.call("GET", "/kv", &[("Accept-Datetime", &probe)], "").await;
    assert_eq!(keys_of(&l), ["k"]);
    assert_eq!(l.header("memento-datetime"), Some(probe.as_str()));
    // before anything existed
    let r = t.call("GET", "/kv/k", &[("Accept-Datetime", "Sat, 01 Apr 2000 09:30:45 GMT")], "").await;
    assert_eq!(r.status, 404);
    // the Python str(datetime) form
    let r = t.call("GET", "/kv", &[("Accept-Datetime", "2000-04-01 09:30:45+00:00")], "").await;
    assert_eq!(keys_of(&r), Vec::<String>::new());
    // a deleted setting is still there before its deletion
    t.delete("k", None).await;
    let r = t.call("GET", "/kv/k", &[("Accept-Datetime", &probe)], "").await;
    assert_eq!(r.json()["value"], "old");
    assert_eq!(t.call("GET", "/kv/k", &[("Accept-Datetime", "garbage")], "").await.status, 400);
}
