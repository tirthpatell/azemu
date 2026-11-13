//! What a store keeps and what one request may cost: snapshots, revisions, filters, pages.

mod common;

use azemu_appconfig::Limits;
use common::*;
use serde_json::json;

fn link_of(r: &azemu_core::testing::TestResponse) -> Option<String> {
    let l = r.header("link")?;
    Some(l[l.find('<')? + 1..l.find('>')?].to_string())
}

// ---- Snapshots ------------------------------------------------------------------------------------------------

async fn snap(t: &T, name: &str, key: &str) -> azemu_core::testing::TestResponse {
    t.call("PUT", &format!("/snapshots/{name}"), &[("Content-Type", "application/json")], &json!({"filters":[{"key":key}]}).to_string())
        .await
}

#[tokio::test]
async fn a_store_holds_at_most_the_configured_number_of_snapshots() {
    let t = start_with(Limits { snapshots: 3, ..Limits::default() }).await;
    t.put_value("k", None, "v").await;
    for i in 0..3 {
        assert_eq!(snap(&t, &format!("s{i}"), "k").await.status, 201);
    }
    let r = snap(&t, "s3", "k").await;
    assert_eq!(r.status, 429);
    let b = r.json();
    assert_eq!(b["type"], "https://azconfig.io/errors/too-many-requests");
    assert_eq!(b["policy"], "Snapshot Count");
    // the old ones still work, and a name that is refused was not created
    assert_eq!(t.get("/snapshots/s2").await.status, 200);
    assert_eq!(t.get("/snapshots/s3").await.status, 404);
    // an empty snapshot counts too: nothing is free
    let t = start_with(Limits { snapshots: 2, ..Limits::default() }).await;
    assert_eq!(snap(&t, "a", "nothing").await.status, 201);
    assert_eq!(snap(&t, "b", "nothing").await.status, 201);
    assert_eq!(snap(&t, "c", "nothing").await.status, 429);
}

#[tokio::test]
async fn snapshot_storage_is_a_quota_of_the_store() {
    // each snapshot of a 40 KB setting is 41,000 bytes; 100,000 bytes hold two
    let t = start_with(Limits { snapshot_bytes: 100_000, ..Limits::default() }).await;
    t.put_value("big", None, &"x".repeat(40_000)).await;
    assert_eq!(snap(&t, "one", "big").await.status, 201);
    assert_eq!(snap(&t, "two", "big").await.status, 201);
    let r = snap(&t, "three", "big").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["policy"], "Snapshot Storage Quota");
    // a small snapshot still fits; another store has its own quota
    t.put_value("small", None, "v").await;
    assert_eq!(snap(&t, "tiny", "small").await.status, 201);
}

#[tokio::test]
async fn one_snapshot_is_at_most_1_mb() {
    let t = start().await;
    for i in 0..3 {
        t.put_value(&format!("k{i}"), None, &"x".repeat(400_000)).await;
    }
    let r = t
        .call("PUT", "/snapshots/two", &[("Content-Type", "application/json")], &json!({"filters":[{"key":"k0"},{"key":"k1"}]}).to_string())
        .await;
    assert_eq!(r.status, 201);
    let r =
        t.call("PUT", "/snapshots/three", &[("Content-Type", "application/json")], &json!({"filters":[{"key":"k*"}]}).to_string()).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["detail"].as_str().unwrap().contains("larger than the limit"));
    assert_eq!(t.get("/snapshots/three").await.status, 404);
}

#[tokio::test]
async fn a_snapshot_keeps_its_values_when_the_settings_change_or_go_away() {
    let t = start().await;
    t.put_value("k", None, "first").await;
    assert_eq!(snap(&t, "s", "k").await.status, 201);
    t.put_value("k", None, "second").await;
    t.delete("k", None).await;
    let r = t.get("/kv?snapshot=s").await;
    assert_eq!(r.json()["items"][0]["value"], "first");
}

// ---- Revisions ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn revisions_per_key_are_capped_and_the_newest_are_kept() {
    let t = start_with(Limits { revisions_per_key: 120, ..Limits::default() }).await;
    for i in 0..300 {
        t.put_value("k", None, &i.to_string()).await;
    }
    t.put_value("other", None, "o").await;
    let r = t.get("/revisions?key=k").await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 100);
    let next = link_of(&r).expect("100 items fill a page");
    assert_eq!(r.json()["items"][0]["value"], "299");
    let r2 = t.get(&next).await;
    let second = r2.json()["items"].as_array().unwrap().len();
    assert_eq!(second, 20, "120 revisions are kept, 100 on the first page");
    assert_eq!(r2.json()["items"][19]["value"], "180");
}

#[tokio::test]
async fn a_range_read_gets_one_page_whatever_the_client_asks_for() {
    let t = start().await;
    for i in 0..250 {
        t.put_value(&format!("k{i:03}"), None, "v").await;
    }
    let r = t.call("GET", "/revisions", &[("Range", "items=0-")], "").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 100);
    assert_eq!(r.header("content-range"), Some("items 0-99/250"));
    let r = t.call("GET", "/revisions", &[("Range", "items=0-100000")], "").await;
    assert_eq!(r.header("content-range"), Some("items 0-99/250"));
    let r = t.call("GET", "/revisions", &[("Range", "items=200-")], "").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("items 200-249/250"));
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 50);
}

#[tokio::test]
async fn history_is_trimmed_by_bytes_and_a_setting_that_cannot_fit_is_refused_with_429() {
    let t = start_with(Limits { store_bytes: 300_000, ..Limits::default() }).await;
    let big = "x".repeat(50_000);
    for _ in 0..40 {
        assert_eq!(t.put_value("k", None, &big).await.status, 200);
    }
    let kept = t.get("/revisions?key=k").await.json()["items"].as_array().unwrap().len();
    assert!((2..=6).contains(&kept), "{kept} revisions kept of 40");
    // different keys are current settings: they cannot be trimmed
    let mut refused = 0;
    for i in 0..10 {
        let r = t.put_value(&format!("c{i}"), None, &big).await;
        if r.status == 429 {
            refused += 1;
            assert_eq!(r.json()["policy"], "Storage Quota");
        }
    }
    assert!(refused >= 3, "{refused} refused");
    // reading, deleting and overwriting still work
    assert_eq!(t.get("/kv/k").await.status, 200);
    assert_eq!(t.delete("c0", None).await.status, 200);
}

// ---- Filters and the work of one request ------------------------------------------------------------------------

#[tokio::test]
async fn filters_are_limited_to_five_values_like_azure() {
    let t = start().await;
    t.put("k", None, json!({"value":"v","tags":{"a":"1","b":"2","c":"3","d":"4","e":"5","f":"6"}})).await;
    assert_eq!(t.get("/kv?tags=a%3D1&tags=b%3D2&tags=c%3D3&tags=d%3D4&tags=e%3D5").await.status, 200);
    let r = t.get("/kv?tags=a%3D1&tags=b%3D2&tags=c%3D3&tags=d%3D4&tags=e%3D5&tags=f%3D6").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["name"], "tags");
    assert_eq!(t.get("/kv?key=a,b,c,d,e").await.status, 200);
    let r = t.get("/kv?key=a,b,c,d,e,f").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["name"], "key");
    assert_eq!(t.get("/kv?label=a,b,c,d,e,f").await.status, 400);
    assert_eq!(t.get("/revisions?key=a,b,c,d,e,f").await.status, 400);
    assert_eq!(t.get("/keys?name=a,b,c,d,e,f").await.status, 400);
    assert_eq!(t.get("/labels?name=a,b,c,d,e,f").await.status, 400);
    let r = t
        .call("PUT", "/snapshots/x", &[("Content-Type", "application/json")], &json!({"filters":[{"key":"a,b,c,d,e,f"}]}).to_string())
        .await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn a_list_that_matches_little_in_a_big_store_stops_after_a_page_of_work_and_links_on() {
    // 30 items with 8,000 tags each: a tag filter looks at 240,000 tags, more than one request does; the answer says where to go on
    let t = start().await;
    for i in 0..30 {
        let mut tags: serde_json::Map<String, serde_json::Value> = (0..8000).map(|j| (format!("t{j:05}"), json!("v"))).collect();
        if i == 29 {
            tags.insert("wanted".into(), json!("yes"));
        }
        assert_eq!(t.put(&format!("item{i:02}"), None, json!({"value":"x","tags":tags})).await.status, 200);
    }
    let first = t.get("/kv?tags=wanted%3Dyes").await;
    assert_eq!(first.status, 200);
    assert_eq!(first.json()["items"].as_array().unwrap().len(), 0, "the work of one request ended before the matching item");
    let mut next = link_of(&first).expect("a link to go on");
    let mut found = Vec::new();
    for _ in 0..10 {
        let r = t.get(&next).await;
        found.extend(keys_of(&r));
        match link_of(&r) {
            Some(n) => next = n,
            None => break,
        }
    }
    assert_eq!(found, ["item29"]);
}

#[tokio::test]
async fn a_tag_filter_over_items_with_many_tags_is_fast() {
    // the review's case, scaled down: 6 items x 20,000 tags, 5 filters that are the last tags: 600,000 comparisons at most
    let t = start().await;
    for i in 0..6 {
        let tags: serde_json::Map<String, serde_json::Value> = (0..20_000).map(|j| (format!("t{j:05}"), json!("v"))).collect();
        t.put(&format!("item{i}"), None, json!({"value":"x","tags":tags})).await;
    }
    let q: Vec<String> = (0..5).map(|k| format!("tags=t{:05}%3Dv", 19_999 - k)).collect();
    let started = std::time::Instant::now();
    let r = t.get(&format!("/kv?{}", q.join("&"))).await;
    assert_eq!(keys_of(&r).len(), 6);
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
}
