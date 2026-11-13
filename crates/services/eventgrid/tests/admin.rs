mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn topic_crud_for_all_three_kinds() {
    let t = Eg::new().await;
    let r = t.put_json("/topics/custom1", &json!({"inputSchema":"EventGridSchema"})).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["kind"], "custom");
    assert_eq!(r.json()["inputSchema"], "EventGridSchema");
    assert_eq!(t.put_json("/topics/custom1", &json!({"inputSchema":"CustomEventSchema"})).await.status, 200);
    assert_eq!(t.get_admin("/topics/custom1").await.json()["inputSchema"], "CustomEventSchema");

    assert_eq!(t.put_json("/topics/dom1", &json!({"kind":"domain"})).await.status, 201);
    assert_eq!(t.put_json("/topics/dt?domain=dom1", &json!({})).await.status, 201);
    assert_eq!(t.put_json("/topics/dt?domain=dom1", &json!({})).await.status, 200);

    // namespace topic; the namespace is created, `default` when not named
    assert_eq!(t.put_json("/topics/nt?namespace=nsx", &json!({})).await.status, 201);
    assert_eq!(t.put_json("/topics/dflt", &json!({"kind":"namespace"})).await.status, 201);
    let v = t.get_admin("/topics/dflt").await.json();
    assert_eq!(v["kind"], "namespaceTopic");
    assert_eq!(v["namespace"], "default");

    let list = t.get_admin("/topics").await.json();
    let names: Vec<String> = list["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| format!("{}:{}", x["kind"].as_str().unwrap(), x["name"].as_str().unwrap()))
        .collect();
    for expect in ["custom:custom1", "domain:dom1", "domainTopic:dt", "namespaceTopic:nt", "namespaceTopic:dflt"] {
        assert!(names.contains(&expect.to_string()), "{names:?}");
    }

    // a kind cannot be changed under the same name
    assert_eq!(t.put_json("/topics/custom1", &json!({"kind":"domain"})).await.status, 409);
    assert_eq!(t.srv.request("DELETE", &t.admin("/topics/custom1")).send().await.status, 204);
    assert_eq!(t.get_admin("/topics/custom1").await.status, 404);
    assert_eq!(t.srv.request("DELETE", &t.admin("/topics/nt?namespace=nsx")).send().await.status, 204);
    assert_eq!(t.get_admin("/topics/nt?namespace=nsx").await.status, 404);
    assert_eq!(t.srv.request("DELETE", &t.admin("/topics/nothing")).send().await.status, 404);
}

#[tokio::test]
async fn bad_admin_input_is_a_400_with_a_message() {
    let t = Eg::new().await;
    t.put_json("/topics/c", &json!({})).await;
    t.put_json("/topics/n?namespace=ns", &json!({})).await;
    let bad = |path: &'static str, body: serde_json::Value| {
        let t = &t;
        async move {
            let r = t.put_json(path, &body).await;
            assert_eq!(r.status, 400, "{path} {body}: {}", r.text());
            assert!(!r.json()["error"]["message"].as_str().unwrap().is_empty());
        }
    };
    // pull on a custom topic
    bad("/topics/c/subscriptions/s", json!({"deliveryMode":"pull"})).await;
    bad("/topics/c/subscriptions/s", json!({})).await;
    // push without or with a bad URL
    bad("/topics/c/subscriptions/s", json!({"deliveryMode":"push"})).await;
    bad("/topics/c/subscriptions/s", json!({"webhookUrl":"ftp://x/y"})).await;
    bad("/topics/c/subscriptions/s", json!({"webhookUrl":"not a url"})).await;
    // a pull subscription with a URL
    bad("/topics/n/subscriptions/s?namespace=ns", json!({"deliveryMode":"pull","webhookUrl":"http://h/x"})).await;
    // numbers
    bad("/topics/n/subscriptions/s?namespace=ns", json!({"lockDurationSeconds": 0})).await;
    bad("/topics/n/subscriptions/s?namespace=ns", json!({"maxDeliveryCount": 0})).await;
    // filters
    bad(
        "/topics/n/subscriptions/s?namespace=ns",
        json!({"filter":{"advancedFilters":[{"operatorType":"Bogus","key":"k","values":["a"]}]}}),
    )
    .await;
    bad(
        "/topics/n/subscriptions/s?namespace=ns",
        json!({"filter":{"advancedFilters":[{"operatorType":"StringIn","key":"k","values":[1]}]}}),
    )
    .await;
    // not JSON
    let r = t.srv.request("PUT", &t.admin("/topics/n/subscriptions/s?namespace=ns")).body("{").send().await;
    assert_eq!(r.status, 400);
    // topic does not exist
    assert_eq!(t.put_json("/topics/ghost/subscriptions/s", &json!({})).await.status, 404);
    // the failed attempts created nothing
    assert_eq!(t.get_admin("/topics/n/subscriptions?namespace=ns").await.json()["value"], json!([]));
}

#[tokio::test]
async fn subscription_crud_and_listing() {
    let t = Eg::new().await;
    t.put_json("/topics/jobs?namespace=ns", &json!({})).await;
    let r = t
        .put_json(
            "/topics/jobs/subscriptions/a?namespace=ns",
            &json!({"lockDurationSeconds": 30, "maxDeliveryCount": 4, "eventTimeToLiveSeconds": 600,
                    "filter": {"includedEventTypes": ["X"], "subjectBeginsWith": "/p", "isSubjectCaseSensitive": true,
                               "advancedFilters": [{"operatorType":"BoolEquals","key":"data.ok","value":true}]}}),
        )
        .await;
    assert_eq!(r.status, 201);
    let v = r.json();
    assert_eq!(v["name"], "a");
    assert_eq!(v["deliveryMode"], "pull");
    assert_eq!(v["lockDurationSeconds"], 30.0);
    assert_eq!(v["maxDeliveryCount"], 4);
    assert_eq!(v["filter"]["advancedFilters"][0]["operatorType"], "BoolEquals");
    assert_eq!(v["validation"]["status"], "NotRequired");
    assert_eq!(v["stats"]["available"], 0);
    t.put_json("/topics/jobs/subscriptions/b?namespace=ns", &json!({})).await;
    let list = t.get_admin("/topics/jobs/subscriptions?namespace=ns").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 2);
    // replacing keeps the queue
    t.ns_publish("ns", "jobs", json!([ce("1", "X")])).await;
    let r = t.put_json("/topics/jobs/subscriptions/b?namespace=ns", &json!({"maxDeliveryCount": 2})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["stats"]["available"], 1);
    assert_eq!(r.json()["maxDeliveryCount"], 2);
    assert_eq!(t.srv.request("DELETE", &t.admin("/topics/jobs/subscriptions/a?namespace=ns")).send().await.status, 204);
    assert_eq!(t.get_admin("/topics/jobs/subscriptions/a?namespace=ns").await.status, 404);
    // the queue can be inspected
    let ev = t.get_admin("/topics/jobs/subscriptions/b/events?namespace=ns").await.json();
    assert_eq!(ev["value"][0]["event"]["id"], "1");
    assert_eq!(ev["value"][0]["state"], "available");
}

#[tokio::test]
async fn namespace_topics_are_found_in_the_default_namespace_without_a_parameter() {
    let t = Eg::new().await;
    // creating a subscription through the plain path works for a topic of `default`
    t.put_json("/topics/jobs", &json!({"kind":"namespace"})).await;
    assert_eq!(t.put_json("/topics/jobs/subscriptions/s", &json!({})).await.status, 201);
    t.ns_publish("default", "jobs", json!([ce("1", "t")])).await;
    let r = t.receive("default", "jobs", "s", "maxWaitTime=1").await;
    assert_eq!(r.json()["value"][0]["event"]["id"], "1");
}

#[tokio::test]
async fn status_and_unknown_admin_paths() {
    let t = Eg::new().await;
    let v = t.get_admin("").await.json();
    assert_eq!(v["autocreate"], true);
    assert!(v["topics"].is_array());
    let r = t.srv.request("PATCH", &t.admin("/topics/x")).send().await;
    assert_eq!(r.status, 501);
    assert_eq!(r.json()["error"]["code"], "NotImplemented");
    // routed through core: the admin prefix works on the gateway host too
    assert_eq!(t.srv.send("GET", &format!("{}/_azemu/health", t.srv.gateway_url()), &[], "").await.status, 200);
}
