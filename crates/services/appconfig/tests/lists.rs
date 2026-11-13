mod common;

use common::*;
use serde_json::json;

fn link_of(r: &azemu_core::testing::TestResponse) -> Option<String> {
    let l = r.header("link")?;
    let start = l.find('<')? + 1;
    let end = l.find('>')?;
    assert!(l.ends_with("rel=\"next\""), "{l}");
    Some(l[start..end].to_string())
}

async fn seed(t: &T, prefix: &str, n: usize) -> Vec<String> {
    let mut keys = Vec::new();
    for i in 0..n {
        let k = format!("{prefix}-{i:04}");
        t.put_value(&k, None, &format!("v{i}")).await;
        keys.push(k);
    }
    keys
}

#[tokio::test]
async fn list_orders_by_key_in_byte_order_then_label_null_first() {
    let t = start().await;
    for k in ["m-2x", "m-10x", "m-1x", "m-100x", "m-0x", "m-11x"] {
        t.put_value(k, None, "v").await;
    }
    let r = t.get("/kv?key=m-*").await;
    assert_eq!(keys_of(&r), ["m-0x", "m-100x", "m-10x", "m-11x", "m-1x", "m-2x"]);
    for l in ["b", "a"] {
        t.put_value("z", Some(l), "v").await;
    }
    t.put_value("z", None, "v").await;
    let r = t.get("/kv?key=z").await;
    assert_eq!(labels_of(&r), [None, Some("a".into()), Some("b".into())]);
}

#[tokio::test]
async fn list_response_shape() {
    let t = start().await;
    t.put_value("a", None, "v").await;
    let r = t.get("/kv").await;
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.kvset+json; charset=utf-8"));
    let b = r.json();
    let names: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(names, ["etag", "items"]);
    assert_eq!(r.header("etag"), Some(format!("\"{}\"", b["etag"].as_str().unwrap()).as_str()));
    assert!(r.header("link").is_none());
    assert!(r.header("x-ms-item-count").is_none(), "2024-09-01 has no item count header");
    let r = t.call_v("2026-04-01", "GET", "/kv", &[], "").await;
    assert_eq!(r.header("x-ms-item-count"), Some("1"));
    let empty = t.call("GET", "/kv?key=nothing", &[], "").await;
    assert_eq!(empty.json()["items"], json!([]));
}

#[tokio::test]
async fn key_filter_grammar_over_http() {
    let t = start().await;
    for k in ["app-alpha.json", "app-beta.json", "app-gamma.yaml", "other", "a,b*c", "a,bXc", "a"] {
        t.put_value(k, None, "v").await;
    }
    let q = |f: &str| format!("/kv?key={}", enc(f));
    assert_eq!(keys_of(&t.get(&q("other")).await), ["other"]);
    assert_eq!(keys_of(&t.get(&q("app-*")).await), ["app-alpha.json", "app-beta.json", "app-gamma.yaml"]);
    assert_eq!(keys_of(&t.get(&q("*.json")).await), ["app-alpha.json", "app-beta.json"]);
    assert_eq!(keys_of(&t.get(&q("*gam*")).await), ["app-gamma.yaml"]);
    assert_eq!(keys_of(&t.get(&q("other,app-beta.json")).await), ["app-beta.json", "other"]);
    assert_eq!(keys_of(&t.get(&q("a\\,b\\*c")).await), ["a,b*c"]);
    assert_eq!(keys_of(&t.get(&q("APP-*")).await), Vec::<String>::new());
    assert_eq!(t.get(&q("*")).await.json()["items"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn invalid_filter_is_the_recorded_400() {
    let t = start().await;
    let r = t.get("/labels?name=test%27%40%2A%24%21%25").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.header("content-type"), Some("application/problem+json; charset=utf-8"));
    assert_eq!(
        r.json(),
        json!({
            "type": "https://azconfig.io/errors/invalid-argument",
            "title": "Invalid request parameter 'label'",
            "name": "label",
            "detail": "label(6): Invalid character",
            "pos": 6,
            "status": 400
        })
    );
    let r = t.get("/kv?key=a*b").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["name"], "key");
    assert_eq!(r.json()["pos"], 1);
}

#[tokio::test]
async fn label_filters() {
    let t = start().await;
    for l in [None, Some("dev"), Some("dev2"), Some("prod")] {
        t.put_value("k", l, "v").await;
    }
    let labels = |q: &'static str| {
        let t = &t;
        async move { labels_of(&t.get(&format!("/kv?key=k&label={q}")).await) }
    };
    assert_eq!(labels("dev").await, [Some("dev".to_string())]);
    assert_eq!(labels("%00").await, [None]);
    assert_eq!(labels("*").await.len(), 4);
    assert_eq!(labels("dev*").await, [Some("dev".to_string()), Some("dev2".to_string())]);
    assert_eq!(labels("%00%2Cprod").await, [None, Some("prod".to_string())]);
    assert_eq!(labels("nope").await, Vec::<Option<String>>::new());
    // no label parameter at all: every label
    assert_eq!(labels_of(&t.get("/kv?key=k").await).len(), 4);
}

#[tokio::test]
async fn tags_filter() {
    let t = start().await;
    t.put("a", None, json!({"tags":{"env":"prod","team":"x"}})).await;
    t.put("b", None, json!({"tags":{"env":"prod","team":"y"}})).await;
    t.put("c", None, json!({"tags":{"env":"dev"}})).await;
    t.put("d", None, json!({})).await;
    assert_eq!(keys_of(&t.get("/kv?tags=env%3Dprod").await), ["a", "b"]);
    assert_eq!(keys_of(&t.get("/kv?tags=env%3Dprod&tags=team%3Dx").await), ["a"]);
    assert_eq!(keys_of(&t.get("/kv?tags=env%3Dnone").await), Vec::<String>::new());
    assert_eq!(t.get("/kv?tags=novalue").await.status, 400);
}

#[tokio::test]
async fn select_fields_in_lists() {
    let t = start().await;
    t.put("k", Some("L"), json!({"value":"secret","content_type":"text/x"})).await;
    let r = t.get("/kv?$select=key,content_type").await;
    assert_eq!(r.json()["items"][0], json!({"key":"k","content_type":"text/x"}));
}

#[tokio::test]
async fn pages_of_100_with_matching_link_header_and_next_link() {
    let t = start().await;
    let keys = seed(&t, "pg", 230).await;
    let first = t.get("/kv?key=pg*").await;
    let b = first.json();
    assert_eq!(b["items"].as_array().unwrap().len(), 100);
    let link = link_of(&first).expect("link on a full page");
    assert!(link.starts_with("/kv?"), "{link}");
    assert!(link.contains("key=pg*"), "{link}");
    assert!(link.contains("api-version=2024-09-01"), "{link}");
    assert!(link.contains("&after="), "{link}");
    assert_eq!(b["@nextLink"], link);

    let mut all = keys_of(&first);
    let mut next = Some(link);
    let mut sizes = vec![100];
    while let Some(l) = next {
        let r = t.srv.send("GET", &format!("{}{}", t.base, l), &[AUTH], "").await;
        assert_eq!(r.status, 200);
        sizes.push(keys_of(&r).len());
        all.extend(keys_of(&r));
        next = link_of(&r);
    }
    assert_eq!(sizes, [100, 100, 30]);
    assert_eq!(all, keys);
}

#[tokio::test]
async fn a_full_last_page_links_to_an_empty_page() {
    // as recorded: no look-ahead, 200 settings give pages of 100, 100 and 0
    let t = start().await;
    seed(&t, "ex", 200).await;
    let p1 = t.get("/kv?key=ex*").await;
    let p2 = t.srv.send("GET", &format!("{}{}", t.base, link_of(&p1).unwrap()), &[AUTH], "").await;
    assert_eq!(keys_of(&p2).len(), 100);
    let p3 = t.srv.send("GET", &format!("{}{}", t.base, link_of(&p2).unwrap()), &[AUTH], "").await;
    assert_eq!(keys_of(&p3).len(), 0);
    assert!(link_of(&p3).is_none());
    assert!(p3.json().get("@nextLink").is_none());
}

#[tokio::test]
async fn cursor_is_base64_of_key_newline_label() {
    let t = start().await;
    seed(&t, "cu", 101).await;
    let r = t.get("/kv?key=cu*").await;
    let link = link_of(&r).unwrap();
    let after = link.rsplit("after=").next().unwrap().replace("%3D", "=");
    // base64 of "cu-0099\n"
    assert_eq!(after, "Y3UtMDA5OQo=");
}

#[tokio::test]
async fn page_etag_conditional_requests() {
    let t = start().await;
    seed(&t, "et", 120).await;
    let p1 = t.get("/kv?key=et*").await;
    let etag = p1.header("etag").unwrap().to_string();
    let link = link_of(&p1).unwrap();
    // unchanged page: 304 with the Link header and no body
    let r = t.call("GET", "/kv?key=et*", &[("If-None-Match", &etag)], "").await;
    assert_eq!(r.status, 304);
    assert_eq!(r.text(), "");
    assert_eq!(link_of(&r), Some(link.clone()));
    assert!(r.header("etag").is_none());
    // the unquoted etag works too
    let r = t.call("GET", "/kv?key=et*", &[("If-None-Match", etag.trim_matches('"'))], "").await;
    assert_eq!(r.status, 304);
    // page 2 has its own etag
    let p2 = t.srv.send("GET", &format!("{}{}", t.base, link), &[AUTH], "").await;
    assert_ne!(p2.header("etag"), p1.header("etag"));
    // changing a setting on page 2 changes only page 2's etag
    t.put_value("et-0119", None, "changed").await;
    let p1b = t.get("/kv?key=et*").await;
    let p2b = t.srv.send("GET", &format!("{}{}", t.base, link), &[AUTH], "").await;
    assert_eq!(p1b.header("etag"), p1.header("etag"));
    assert_ne!(p2b.header("etag"), p2.header("etag"));
    let r = t.srv.send("GET", &format!("{}{}", t.base, link), &[AUTH, ("If-None-Match", p2.header("etag").unwrap())], "").await;
    assert_eq!(r.status, 200);
    // If-Match on a page is not enforced (recordings: a setting's etag in If-Match gives 200)
    let r = t.call("GET", "/kv?key=et*", &[("If-Match", "\"anything\"")], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn head_kv_has_the_page_headers_and_no_body() {
    let t = start().await;
    seed(&t, "hd", 120).await;
    let g = t.get("/kv?key=hd*").await;
    let h = t.call("HEAD", "/kv?key=hd*", &[], "").await;
    assert_eq!(h.status, 200);
    assert_eq!(h.text(), "");
    assert_eq!(h.header("etag"), g.header("etag"));
    assert_eq!(h.header("link"), g.header("link"));
    assert_eq!(h.header("content-type"), g.header("content-type"));
    let h = t.call("HEAD", "/kv?key=hd*", &[("If-None-Match", g.header("etag").unwrap())], "").await;
    assert_eq!(h.status, 304);
}

#[tokio::test]
async fn revisions_are_newest_first_even_within_one_instant() {
    let t = start().await;
    for i in 0..6 {
        t.put_value("r", None, &format!("v{i}")).await;
    }
    t.put_value("other", None, "o").await;
    let r = t.get("/revisions?key=r").await;
    assert_eq!(r.header("accept-ranges"), Some("items"));
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.kvset+json; charset=utf-8"));
    let values: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|i| i["value"].as_str().unwrap().to_string()).collect();
    assert_eq!(values, ["v5", "v4", "v3", "v2", "v1", "v0"]);
    // across keys, newest first
    let all = t.get("/revisions").await;
    assert_eq!(keys_of(&all)[0], "other");
}

#[tokio::test]
async fn revisions_keep_deleted_settings_and_record_locks() {
    let t = start().await;
    t.put_value("r", None, "v1").await;
    t.put_value("r", None, "v2").await;
    t.call("PUT", "/locks/r", &[], "").await;
    t.call("DELETE", "/locks/r", &[], "").await;
    t.delete("r", None).await;
    let r = t.get("/revisions?key=r").await.json();
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["locked"], false);
    assert_eq!(items[1]["locked"], true);
    assert_eq!(items[3]["value"], "v1");
}

#[tokio::test]
async fn revisions_filters_select_and_paging() {
    let t = start().await;
    t.put("a", Some("x"), json!({"value":"1","tags":{"env":"prod"}})).await;
    t.put("a", Some("y"), json!({"value":"2"})).await;
    t.put("b", None, json!({"value":"3"})).await;
    assert_eq!(keys_of(&t.get("/revisions?key=a").await).len(), 2);
    assert_eq!(keys_of(&t.get("/revisions?label=x").await), ["a"]);
    assert_eq!(keys_of(&t.get("/revisions?label=%00").await), ["b"]);
    assert_eq!(keys_of(&t.get("/revisions?tags=env%3Dprod").await), ["a"]);
    let r = t.get("/revisions?key=b&$select=key,value").await;
    assert_eq!(r.json()["items"][0], json!({"key":"b","value":"3"}));
    // paging past 100
    for i in 0..105 {
        t.put_value("many", None, &format!("v{i}")).await;
    }
    let p1 = t.get("/revisions?key=many").await;
    assert_eq!(keys_of(&p1).len(), 100);
    let link = link_of(&p1).unwrap();
    assert!(link.starts_with("/revisions?"));
    let p2 = t.srv.send("GET", &format!("{}{}", t.base, link), &[AUTH], "").await;
    assert_eq!(keys_of(&p2).len(), 5);
    assert!(link_of(&p2).is_none());
}

#[tokio::test]
async fn revisions_range_header() {
    let t = start().await;
    for i in 0..5 {
        t.put_value("r", None, &format!("v{i}")).await;
    }
    let r = t.call("GET", "/revisions?key=r", &[("Range", "items=1-2")], "").await;
    assert_eq!(r.status, 206);
    assert_eq!(r.header("content-range"), Some("items 1-2/5"));
    let values: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|i| i["value"].as_str().unwrap().to_string()).collect();
    assert_eq!(values, ["v3", "v2"]);
    let r = t.call("GET", "/revisions?key=r", &[("Range", "items=9-10")], "").await;
    assert_eq!(r.status, 416);
}

#[tokio::test]
async fn labels_list() {
    let t = start().await;
    assert_eq!(t.get("/labels").await.json(), json!({"items": []}));
    t.put_value("k1", None, "v").await;
    t.put_value("k1", Some("beta"), "v").await;
    t.put_value("k2", Some("beta"), "v").await;
    t.put_value("k2", Some("alpha"), "v").await;
    let r = t.get("/labels").await;
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.labelset+json; charset=utf-8"));
    assert_eq!(r.json(), json!({"items": [{"name": null}, {"name": "alpha"}, {"name": "beta"}]}));
    assert_eq!(t.get("/labels?name=al*").await.json(), json!({"items": [{"name": "alpha"}]}));
    assert_eq!(t.get("/labels?name=%00").await.json(), json!({"items": [{"name": null}]}));
    assert_eq!(t.get("/labels?$select=name&name=beta").await.json(), json!({"items": [{"name": "beta"}]}));
    let r = t.call_v("2026-04-01", "GET", "/labels", &[], "").await;
    assert_eq!(r.header("x-ms-item-count"), Some("3"));
}

#[tokio::test]
async fn labels_page_past_100() {
    let t = start().await;
    for i in 0..120 {
        t.put_value("k", Some(&format!("label-{i:03}")), "v").await;
    }
    let p1 = t.get("/labels").await;
    assert_eq!(p1.json()["items"].as_array().unwrap().len(), 100);
    let link = link_of(&p1).unwrap();
    assert!(link.starts_with("/labels?"));
    let p2 = t.srv.send("GET", &format!("{}{}", t.base, link), &[AUTH], "").await;
    assert_eq!(p2.json()["items"].as_array().unwrap().len(), 20);
}

#[tokio::test]
async fn keys_list() {
    let t = start().await;
    t.put_value("b", None, "v").await;
    t.put_value("a", Some("x"), "v").await;
    t.put_value("a", Some("y"), "v").await;
    t.put_value("app", None, "v").await;
    let r = t.get("/keys").await;
    assert_eq!(r.header("content-type"), Some("application/vnd.microsoft.appconfig.keyset+json; charset=utf-8"));
    assert_eq!(r.json(), json!({"items": [{"name":"a"},{"name":"app"},{"name":"b"}]}));
    assert_eq!(t.get("/keys?name=a*").await.json(), json!({"items": [{"name":"a"},{"name":"app"}]}));
    assert_eq!(t.call("HEAD", "/keys", &[], "").await.status, 200);
    assert_eq!(t.get("/keys?name=a*b").await.status, 400);
}

#[tokio::test]
async fn head_for_labels_and_revisions() {
    let t = start().await;
    t.put_value("k", Some("l"), "v").await;
    for p in ["/labels", "/revisions", "/keys", "/snapshots"] {
        let r = t.call("HEAD", p, &[], "").await;
        assert_eq!(r.status, 200, "{p}");
        assert_eq!(r.text(), "");
    }
}
