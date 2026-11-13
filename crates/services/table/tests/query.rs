mod common;
use common::*;

async fn people() -> Env {
    let e = env().await;
    e.create_table("people").await;
    let rows = [
        r#"{"PartitionKey":"eu","RowKey":"001","name":"Alice","age":30,"score":1.5,"on":true,"born@odata.type":"Edm.DateTime","born":"2000-01-01T00:00:00Z","uid@odata.type":"Edm.Guid","uid":"c9da6455-213d-42c9-9a79-3e9149a57833","blob@odata.type":"Edm.Binary","blob":"YQ==","big@odata.type":"Edm.Int64","big":"10000000000","note":"n1"}"#,
        r#"{"PartitionKey":"eu","RowKey":"002","name":"bob","age":25,"score":2.5,"on":false,"born@odata.type":"Edm.DateTime","born":"1995-06-15T12:30:00Z","big@odata.type":"Edm.Int64","big":"5"}"#,
        r#"{"PartitionKey":"eu","RowKey":"010","name":"Carol","age":35,"score":-0.5,"on":true,"big@odata.type":"Edm.Int64","big":"-7","note":"n3"}"#,
        r#"{"PartitionKey":"us","RowKey":"001","name":"Dave","age":40,"score":10.0,"on":false}"#,
        r#"{"PartitionKey":"us","RowKey":"002","name":"O'Brien","age":22,"on":true,"note":""}"#,
        r#"{"PartitionKey":"asia","RowKey":"100","name":"eve","age":30,"on":true}"#,
    ];
    for r in rows {
        assert_eq!(e.insert("people", r).await.status, 201);
    }
    e
}

fn enc(f: &str) -> String {
    f.replace('%', "%25").replace(' ', "%20").replace('\'', "%27").replace('+', "%2B").replace('&', "%26").replace('#', "%23")
}

async fn ids(e: &Env, filter: &str) -> Vec<String> {
    let r = e.send("GET", &format!("/people()?$filter={}", enc(filter)), &[], "").await;
    assert_eq!(r.status, 200, "{filter}: {}", r.text());
    r.json()["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| format!("{}/{}", x["PartitionKey"].as_str().unwrap(), x["RowKey"].as_str().unwrap()))
        .collect()
}

#[tokio::test]
async fn results_are_ordered_by_partition_then_row_key() {
    let e = people().await;
    let r = e.send("GET", "/people()", &[], "").await;
    let got: Vec<String> = r.json()["value"].as_array().unwrap().iter().map(|x| format!("{}/{}", x["PartitionKey"], x["RowKey"])).collect();
    assert_eq!(got, ["\"asia\"/\"100\"", "\"eu\"/\"001\"", "\"eu\"/\"002\"", "\"eu\"/\"010\"", "\"us\"/\"001\"", "\"us\"/\"002\""]);
    assert_eq!(r.json()["odata.metadata"], format!("{}/$metadata#people", e.base));
    assert!(r.json()["value"][0].get("odata.metadata").is_none(), "metadata is on the envelope only");
    assert!(r.json()["value"][0]["odata.etag"].is_string());
}

#[tokio::test]
async fn row_keys_sort_ordinally() {
    let e = env().await;
    e.create_table("ord").await;
    for rk in ["9", "10", "1", "100", "B", "a", "_", "Z", "é", "\u{1f642}", "\u{fffd}"] {
        e.insert("ord", &format!(r#"{{"PartitionKey":"p","RowKey":"{rk}"}}"#)).await;
    }
    assert_eq!(e.row_keys("ord", "").await, ["1", "10", "100", "9", "B", "Z", "_", "a", "é", "\u{1f642}", "\u{fffd}"]);
}

#[tokio::test]
async fn comparison_operators_and_types() {
    let e = people().await;
    assert_eq!(ids(&e, "name eq 'Alice'").await, ["eu/001"]);
    assert_eq!(ids(&e, "name eq 'alice'").await, Vec::<String>::new());
    assert_eq!(ids(&e, "age gt 30").await, ["eu/010", "us/001"]);
    assert_eq!(ids(&e, "age le 25").await, ["eu/002", "us/002"]);
    assert_eq!(ids(&e, "age ne 30").await, ["eu/002", "eu/010", "us/001", "us/002"]);
    assert_eq!(ids(&e, "name lt 'a'").await, ["eu/001", "eu/010", "us/001", "us/002"]);
    assert_eq!(ids(&e, "score ge 2.5").await, ["eu/002", "us/001"]);
    assert_eq!(ids(&e, "score eq 10").await, ["us/001"]);
    assert_eq!(ids(&e, "on eq false").await, ["eu/002", "us/001"]);
    assert_eq!(ids(&e, "big eq 10000000000L").await, ["eu/001"]);
    assert_eq!(ids(&e, "big lt 0L").await, ["eu/010"]);
    assert_eq!(ids(&e, "big gt 4").await, ["eu/001", "eu/002"]);
    assert_eq!(ids(&e, "born ge datetime'2000-01-01T00:00:00Z'").await, ["eu/001"]);
    assert_eq!(ids(&e, "born lt datetime'2000-01-01T00:00:00.0000001Z'").await, ["eu/001", "eu/002"]);
    assert_eq!(ids(&e, "uid eq guid'C9DA6455-213D-42C9-9A79-3E9149A57833'").await, ["eu/001"]);
    assert_eq!(ids(&e, "blob eq X'61'").await, ["eu/001"]);
    assert_eq!(ids(&e, "blob eq binary'61'").await, ["eu/001"]);
    assert_eq!(ids(&e, "name eq 'O''Brien'").await, ["us/002"]);
    assert_eq!(ids(&e, "note eq ''").await, ["us/002"]);
    assert_eq!(ids(&e, "Timestamp ge datetime'2000-01-01T00:00:00Z'").await.len(), 6);
}

#[tokio::test]
async fn boolean_logic() {
    let e = people().await;
    assert_eq!(ids(&e, "PartitionKey eq 'eu' and age gt 28").await, ["eu/001", "eu/010"]);
    assert_eq!(ids(&e, "name eq 'Alice' or name eq 'Dave'").await, ["eu/001", "us/001"]);
    assert_eq!(ids(&e, "not (age gt 30)").await, ["asia/100", "eu/001", "eu/002", "us/002"]);
    assert_eq!(ids(&e, "name eq 'Dave' or PartitionKey eq 'eu' and age eq 25").await, ["eu/002", "us/001"]);
    assert_eq!(ids(&e, "(name eq 'Dave' or PartitionKey eq 'eu') and age eq 25").await, ["eu/002"]);
    assert_eq!(ids(&e, "not (PartitionKey eq 'eu' and (age lt 30 or age gt 30))").await, ["asia/100", "eu/001", "us/001", "us/002"]);
}

#[tokio::test]
async fn missing_and_mistyped_properties_do_not_match() {
    let e = people().await;
    assert_eq!(ids(&e, "note eq 'n1'").await, ["eu/001"]);
    assert_eq!(ids(&e, "note ge 'n'").await, ["eu/001", "eu/010"]);
    assert_eq!(ids(&e, "ghost eq 'x'").await.len(), 0);
    assert_eq!(ids(&e, "ghost ne 'x'").await.len(), 0);
    assert_eq!(ids(&e, "ghost eq 'x' or PartitionKey eq 'us'").await, ["us/001", "us/002"]);
    assert_eq!(ids(&e, "age eq '30'").await.len(), 0);
    assert_eq!(ids(&e, "name gt 5").await.len(), 0);
}

#[tokio::test]
async fn invalid_filters_are_400_invalid_input() {
    let e = people().await;
    for bad in [
        "aaa bbb ccc",
        "name eq",
        "(name eq 'x'",
        "name eq 'x')",
        "name equals 'x'",
        "eq 'x'",
        "name eq 'x' and",
        "and name eq 'x'",
        "name eq 'unterminated",
        "name eq 'O'Brien'",
        "age eq 1x",
        "born eq datetime'nope'",
    ] {
        let r = e.send("GET", &format!("/people()?$filter={}", enc(bad)), &[], "").await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"), "{bad}");
        assert!(msg(&r).starts_with("Syntax error at position"), "{bad}: {}", msg(&r));
    }
    // even when the table is empty
    e.create_table("empty").await;
    let r = e.send("GET", "/empty()?$filter=aaa%20bbb%20ccc", &[], "").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn select_and_top() {
    let e = people().await;
    let r = e.send("GET", "/people()?$select=name,age&$filter=RowKey%20eq%20'001'%20and%20PartitionKey%20eq%20'eu'", &[], "").await;
    let first = &r.json()["value"][0];
    let keys: Vec<&String> = first.as_object().unwrap().keys().filter(|k| !k.starts_with("odata")).collect();
    assert_eq!(keys.len(), 2);
    assert_eq!((first["name"].clone(), first["age"].clone()), (azemu_core::serde_json::json!("Alice"), azemu_core::serde_json::json!(30)));
    let r = e.send("GET", "/people()?$top=2", &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 2);
    assert_eq!(e.send("GET", "/people()?$top=0", &[], "").await.status, 400);
    assert_eq!(e.send("GET", "/people()?$top=abc", &[], "").await.status, 400);
}

#[tokio::test]
async fn paging_with_continuation_headers() {
    let e = people().await;
    let r = e.send("GET", "/people()?$top=4", &[], "").await;
    let pk = r.header("x-ms-continuation-NextPartitionKey").unwrap().to_string();
    let rk = r.header("x-ms-continuation-NextRowKey").unwrap().to_string();
    assert_eq!((pk.as_str(), rk.as_str()), ("1!4!dXM-", "1!4!MDAx"));
    let r2 = e
        .send(
            "GET",
            &format!("/people()?$top=4&NextPartitionKey={}&NextRowKey={}", pk.replace('!', "%21"), rk.replace('!', "%21")),
            &[],
            "",
        )
        .await;
    let rest = r2.json()["value"].as_array().unwrap().len();
    assert_eq!(rest, 2);
    assert!(r2.header("x-ms-continuation-NextPartitionKey").is_none());
    // an exact multiple has no empty trailing page
    let r = e.send("GET", "/people()?$top=6", &[], "").await;
    assert!(r.header("x-ms-continuation-NextPartitionKey").is_none());
    // a filter pages inside the filtered set
    let r = e.send("GET", "/people()?$top=2&$filter=on%20eq%20true", &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 2);
    assert!(r.header("x-ms-continuation-NextPartitionKey").is_some());
}

#[tokio::test]
async fn a_page_holds_at_most_1000_entities() {
    let e = env().await;
    e.create_table("big").await;
    e.seed("big", 1001).await;
    let r = e.send("GET", "/big()", &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 1000);
    assert_eq!(r.header("x-ms-continuation-NextRowKey"), Some("1!8!MTAwMA--"));
    let r = e.send("GET", "/big()?$top=5000", &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 1000);
    let r = e.send("GET", "/big()?NextPartitionKey=1!4!cA--&NextRowKey=1!8!MTAwMA--", &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn filter_literals_with_awkward_characters() {
    let e = env().await;
    e.create_table("odd").await;
    for (rk, s) in [("1", "a+b c%20d&e=f"), ("2", "a b c d"), ("3", "x' or name ne 'x"), ("4", "q?\"\\{}<>%")] {
        let body = azemu_core::serde_json::json!({"PartitionKey": "p", "RowKey": rk, "s": s});
        e.insert("odd", &body.to_string()).await;
    }
    let q = |lit: &str| format!("/odd()?$filter=s%20eq%20'{}'", lit);
    let r = e.send("GET", &q("a%2Bb%20c%2520d%26e%3Df"), &[], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 1);
    let r = e.send("GET", &q("x''%20or%20name%20ne%20''x"), &[], "").await;
    assert_eq!(r.json()["value"][0]["RowKey"], "3");
}

#[tokio::test]
async fn plus_in_the_query_string_is_a_space() {
    // The Rust Tables crate sends `$filter=age+ge+30`; the other SDKs send `%20`; a literal plus is `%2B`.
    let e = people().await;
    let get = |q: &str| {
        let url = format!("/people()?{q}");
        let e = &e;
        async move {
            let r = e.send("GET", &url, &[], "").await;
            assert_eq!(r.status, 200, "{url}: {}", r.text());
            r.json()["value"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| format!("{}/{}", x["PartitionKey"].as_str().unwrap(), x["RowKey"].as_str().unwrap()))
                .collect::<Vec<_>>()
        }
    };
    let want = ["asia/100", "eu/001", "eu/010", "us/001"];
    assert_eq!(get("$filter=age+ge+30").await, want);
    assert_eq!(get("%24filter=age+ge+30").await, want, "the name is form-encoded too");
    assert_eq!(get("$filter=age%20ge%2030").await, want);
    assert_eq!(get("$filter=age+ge+30&$top=2").await.len(), 2);
    // a literal plus in a value: sent as %2B it matches the entity; sent as + it is a space and matches nothing
    assert_eq!(e.insert("people", r#"{"PartitionKey":"eu","RowKey":"plus","name":"a+b"}"#).await.status, 201);
    assert_eq!(get("$filter=name+eq+'a%2Bb'").await, ["eu/plus"]);
    assert!(get("$filter=name+eq+'a+b'").await.is_empty());
}
