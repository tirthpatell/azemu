//! The inspector of App Configuration (`/_azemu/inspect/appconfig/...`).

mod common;

use azemu_core::inspect::walk::walk;
use common::*;
use serde_json::json;

async fn fill(t: &T) {
    t.put_value("app/color", None, "blue").await;
    t.put_value("app/color", Some("prod"), "red").await;
    t.put_value("app/color", Some("prod"), "green").await;
    t.put_value("app/size", None, "10").await;
    t.put_value("db/host", Some("dev"), "localhost").await;
    t.put(
        "flag/beta",
        None,
        json!({"value": "{\"enabled\":true}", "content_type": "application/vnd.microsoft.appconfig.ff+json", "tags": {"team": "x"}}),
    )
    .await;
    t.put_value("gone", None, "bye").await;
    t.delete("gone", None).await;
    let body = json!({"filters": [{"key": "app/*"}]}).to_string();
    let r = t.call("PUT", "/snapshots/release-1", &[("Content-Type", "application/json")], &body).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

async fn insp(t: &T, p: &str) -> azemu_core::testing::TestResponse {
    t.srv.send("GET", &format!("{}/_azemu/inspect{p}", t.srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn the_whole_tree_can_be_walked_and_paging_ends() {
    let t = start().await;
    fill(&t).await;
    // a second store
    t.srv
        .send(
            "PUT",
            &format!("{}/kv/x?api-version={VERSION}", t.base_of("other")),
            &[AUTH, ("Content-Type", "application/json")],
            json!({"value": "1"}).to_string(),
        )
        .await;
    for limit in [1, 2, 50] {
        let nodes = walk(&t.srv, "/appconfig", limit).await;
        let kinds: Vec<&str> = nodes.iter().map(|n| n.node["kind"].as_str().unwrap()).collect();
        for k in ["service", "store", "keyvalues", "revisions", "snapshots", "snapshot"] {
            assert!(kinds.contains(&k), "no {k} among {kinds:?}");
        }
    }
    assert_eq!(insp(&t, "/appconfig").await.json()["properties"]["accounts"], 2);
}

#[tokio::test]
async fn key_values_filter_by_key_and_label_and_revisions_keep_the_old_values() {
    let t = start().await;
    fill(&t).await;
    let r = insp(&t, "/appconfig/teststore/keyvalues?limit=2").await.json();
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    assert!(r["next"].is_string());
    let names = |v: &serde_json::Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| format!("{}|{}", i["name"].as_str().unwrap(), i["label"].as_str().unwrap_or("-")))
            .collect()
    };
    let all = insp(&t, "/appconfig/teststore/keyvalues").await.json();
    assert_eq!(names(&all), ["app/color|-", "app/color|prod", "app/size|-", "db/host|dev", "flag/beta|-"]);
    assert_eq!(all["items"][1]["value"], "green");
    assert_eq!(all["items"][4]["tags"]["team"], "x");
    let app = insp(&t, "/appconfig/teststore/keyvalues?key=app%2F").await.json();
    assert_eq!(names(&app), ["app/color|-", "app/color|prod", "app/size|-"]);
    let prod = insp(&t, "/appconfig/teststore/keyvalues?label=prod").await.json();
    assert_eq!(names(&prod), ["app/color|prod"]);
    let none = insp(&t, "/appconfig/teststore/keyvalues?label=(null)").await.json();
    assert_eq!(names(&none).len(), 3);
    // the history of one key and label: the two values, newest first, the older one marked as replaced
    let rev = insp(&t, "/appconfig/teststore/revisions?key=app%2Fcolor&label=prod").await.json();
    let rows = rev["items"].as_array().unwrap();
    assert_eq!(rows.iter().map(|r| r["value"].as_str().unwrap()).collect::<Vec<_>>(), ["green", "red"]);
    assert!(rows[0]["replacedOrDeleted"].is_null() && rows[1]["replacedOrDeleted"].is_string());
    // the deleted key still has a revision
    let gone = insp(&t, "/appconfig/teststore/revisions?key=gone").await.json();
    assert_eq!(gone["items"].as_array().unwrap().len(), 1);
    // the snapshot holds the settings of its filter
    let items = insp(&t, "/appconfig/teststore/snapshots/release-1/items").await.json();
    assert_eq!(items["items"].as_array().unwrap().len(), 2, "the two with the null label");
}

#[tokio::test]
async fn looking_changes_nothing_and_a_missing_store_is_not_made() {
    let t = start().await;
    fill(&t).await;
    let view = || async { (t.get("/kv").await.text(), t.get("/revisions").await.text(), t.get("/snapshots").await.text()) };
    let before = view().await;
    walk(&t.srv, "", 1).await;
    assert_eq!(before, view().await);
    for p in ["/appconfig/ghost", "/appconfig/ghost/keyvalues", "/appconfig/teststore/snapshots/none", "/appconfig/teststore/nothing"] {
        assert_eq!(insp(&t, p).await.status, 404, "{p}");
    }
    assert_eq!(insp(&t, "/appconfig").await.json()["properties"]["accounts"], 1);
}
