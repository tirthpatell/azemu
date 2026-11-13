mod common;
use azemu_core::serde_json::json;
use common::*;

async fn with_table() -> Env {
    let e = env().await;
    assert_eq!(e.create_table("tbl").await.status, 201);
    e
}

#[tokio::test]
async fn insert_returns_the_entity_with_etag_and_location() {
    let e = with_table().await;
    let r = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r k","n":1}"#).await;
    assert_eq!(r.status, 201);
    let etag = r.header("etag").unwrap().to_string();
    assert!(etag.starts_with("W/\"datetime'") && etag.contains("%3A") && etag.ends_with("Z'\""), "{etag}");
    assert_eq!(r.header("location"), Some(format!("{}/tbl(PartitionKey='p',RowKey='r%20k')", e.base).as_str()));
    let j = r.json();
    assert_eq!(j["odata.metadata"], format!("{}/$metadata#tbl/@Element", e.base));
    assert_eq!(j["odata.etag"], etag);
    assert_eq!(j["n"], 1);
    // Timestamp is the etag's time
    let ts = j["Timestamp"].as_str().unwrap();
    assert!(etag.contains(&ts.replace(':', "%3A")), "{ts} vs {etag}");
}

#[tokio::test]
async fn prefer_return_no_content_on_insert() {
    let e = with_table().await;
    let r = e.send("POST", "/tbl", &[JSON, ("Prefer", "return-no-content")], r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    assert_eq!(r.status, 204);
    assert!(r.body.is_empty());
    assert_eq!(r.header("preference-applied"), Some("return-no-content"));
    assert!(r.header("etag").is_some() && r.header("location").is_some() && r.header("dataserviceid").is_some());
}

#[tokio::test]
async fn payload_formats() {
    let e = with_table().await;
    let body = r#"{"PartitionKey":"p","RowKey":"r","s":"x","i":5,"f":2.5,"b":true,"big@odata.type":"Edm.Int64","big":"9007199254740993","d@odata.type":"Edm.DateTime","d":"2020-01-02T03:04:05.1234567Z","g@odata.type":"Edm.Guid","g":"C9DA6455-213D-42C9-9A79-3E9149A57833","bin@odata.type":"Edm.Binary","bin":"YmluYXJ5","nan@odata.type":"Edm.Double","nan":"NaN","gone":null}"#;
    assert_eq!(e.insert("tbl", body).await.status, 201);

    let minimal = e.get("tbl", "p", "r").await.json();
    assert_eq!(minimal["big"], "9007199254740993");
    assert_eq!(minimal["big@odata.type"], "Edm.Int64");
    assert_eq!(minimal["d"], "2020-01-02T03:04:05.1234567Z");
    assert_eq!(minimal["g"], "c9da6455-213d-42c9-9a79-3e9149a57833");
    assert_eq!(minimal["bin@odata.type"], "Edm.Binary");
    assert_eq!(minimal["nan"], "NaN");
    assert_eq!(minimal["nan@odata.type"], "Edm.Double");
    assert_eq!(minimal["f"], 2.5);
    assert_eq!(minimal["i"], 5);
    assert!(minimal.get("i@odata.type").is_none() && minimal.get("f@odata.type").is_none());
    assert!(minimal.get("gone").is_none());
    assert!(minimal.get("odata.type").is_none());

    let path = "/tbl(PartitionKey='p',RowKey='r')";
    let full = e.send("GET", path, &[("Accept", "application/json;odata=fullmetadata")], "").await.json();
    assert_eq!(full["odata.type"], "acct.tbl");
    assert_eq!(full["odata.id"], format!("{}{}", e.base, path));
    assert_eq!(full["odata.editLink"], "tbl(PartitionKey='p',RowKey='r')");
    assert_eq!(full["Timestamp@odata.type"], "Edm.DateTime");

    let none = e.send("GET", path, &[("Accept", "application/json;odata=nometadata")], "").await.json();
    assert!(none.as_object().unwrap().keys().all(|k| !k.contains("odata")), "{none}");
    assert_eq!(none["big"], "9007199254740993");
    assert!(none["Timestamp"].is_string());
}

#[tokio::test]
async fn doubles_that_are_whole_numbers_stay_doubles() {
    let e = with_table().await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","a":5.0,"b":3,"c@odata.type":"Edm.Double","c":5}"#).await;
    let raw = e.get("tbl", "p", "r").await.text();
    assert!(raw.contains("\"a\":5.0") && raw.contains("\"b\":3,") && raw.contains("\"c\":5.0"), "{raw}");
}

#[tokio::test]
async fn property_order_is_kept_and_non_ascii_is_escaped() {
    let e = with_table().await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","zeta":1,"alpha":"é","mid":2}"#).await;
    let raw = e.get("tbl", "p", "r").await.text();
    let (z, a, m) = (raw.find("zeta").unwrap(), raw.find("alpha").unwrap(), raw.find("mid").unwrap());
    assert!(z < a && a < m, "{raw}");
    assert!(raw.contains("\"alpha\":\"\\u00e9\""), "{raw}");
}

#[tokio::test]
async fn insert_errors() {
    let e = with_table().await;
    let ok = r#"{"PartitionKey":"p","RowKey":"r"}"#;
    assert_eq!(e.insert("tbl", ok).await.status, 201);
    let r = e.insert("tbl", ok).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "EntityAlreadyExists"));
    assert_eq!(msg(&r).lines().next(), Some("The specified entity already exists."));
    for (body, want_code, want_status) in [
        (r#"{"RowKey":"r"}"#, "PropertiesNeedValue", 400),
        (r#"{"PartitionKey":"p"}"#, "PropertiesNeedValue", 400),
        (r#"{"PartitionKey":"p","RowKey":1}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"a/b","RowKey":"r"}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"a#b","RowKey":"r"}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"p","RowKey":"a\tb"}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","123":1}"#, "PropertyNameInvalid", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","a b":1}"#, "PropertyNameInvalid", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","d@odata.type":"Edm.DateTime","d":"not a date"}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","n@odata.type":"Edm.Int64","n":"9223372036854775808"}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","o":{"a":1}}"#, "InvalidInput", 400),
        (r#"{"PartitionKey":"p","RowKey":"x","n":2147483648}"#, "InvalidInput", 400),
        ("[1]", "InvalidInput", 400),
        ("", "InvalidInput", 400),
    ] {
        let r = e.insert("tbl", body).await;
        assert_eq!((r.status, code(&r).as_str()), (want_status, want_code), "{body}");
    }
    assert_eq!(e.row_keys("tbl", "").await, ["r"]);
}

#[tokio::test]
async fn size_and_count_limits() {
    let e = with_table().await;
    let long_name = "a".repeat(256);
    let r = e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"r","{long_name}":1}}"#)).await;
    assert_eq!(code(&r), "PropertyNameTooLong");
    assert_eq!(e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"r","{}":1}}"#, "a".repeat(255))).await.status, 201);

    let s = "x".repeat(32 * 1024);
    assert_eq!(e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"s","s":"{s}"}}"#)).await.status, 201);
    let r = e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"s2","s":"{s}x"}}"#)).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "PropertyValueTooLarge"));

    let many: String = (0..253).map(|i| format!(r#","k{i}":{i}"#)).collect();
    let r = e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"m"{many}}}"#)).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "TooManyProperties"));
    let ok: String = (0..252).map(|i| format!(r#","k{i}":{i}"#)).collect();
    assert_eq!(e.insert("tbl", &format!(r#"{{"PartitionKey":"p","RowKey":"m"{ok}}}"#)).await.status, 201);

    assert_eq!(e.insert("tbl", &format!(r#"{{"PartitionKey":"{}","RowKey":"r"}}"#, "k".repeat(512))).await.status, 201);
    assert_eq!(e.insert("tbl", &format!(r#"{{"PartitionKey":"{}","RowKey":"r"}}"#, "k".repeat(513))).await.status, 400);
}

#[tokio::test]
async fn get_entity_and_select() {
    let e = with_table().await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","a":1,"b":2,"c":3}"#).await;
    let r = e.send("GET", "/tbl(PartitionKey='p',RowKey='r')?$select=a,%20c", &[], "").await;
    assert_eq!(r.status, 200);
    let raw = r.text();
    let at = |k: &str| raw.find(&format!("\"{k}\"")).unwrap_or(usize::MAX);
    assert!(at("odata.metadata") < at("odata.etag") && at("odata.etag") < at("a") && at("a") < at("c"), "{raw}");
    assert!(!raw.contains("\"b\"") && !raw.contains("PartitionKey") && !raw.contains("Timestamp"), "{raw}");
    let r = e.get("tbl", "p", "nope").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "ResourceNotFound"));
    assert_eq!(msg(&r).lines().next(), Some("The specified resource does not exist."));
}

#[tokio::test]
async fn keys_with_quotes_unicode_and_empty_values() {
    let e = with_table().await;
    for (pk, rk) in [("O''Brien", "x"), ("héllo 你好", "r"), ("", ""), ("100%25", "a+b"), ("p", "with%20space")] {
        let body = azemu_core::serde_json::json!({"PartitionKey": pk.replace("''", "'").replace("%25", "%").replace("%20", " "), "RowKey": rk.replace("%20", " "), "v": 1});
        let r = e.insert("tbl", &body.to_string()).await;
        assert_eq!(r.status, 201, "{pk}/{rk}");
        let enc = |s: &str| s.replace('é', "%C3%A9").replace(' ', "%20").replace("你好", "%E4%BD%A0%E5%A5%BD");
        let r = e.send("GET", &format!("/tbl(PartitionKey='{}',RowKey='{}')", enc(pk), enc(rk)), &[], "").await;
        assert_eq!(r.status, 200, "{pk}/{rk}: {}", r.text());
    }
}

#[tokio::test]
async fn update_merge_replace_and_upsert() {
    let e = with_table().await;
    let first = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","a":1,"b":2}"#).await.header("etag").unwrap().to_string();
    let path = "/tbl(PartitionKey='p',RowKey='r')";

    let r = e.send("PATCH", path, &[JSON, ("If-Match", "*")], r#"{"b":20,"c":30}"#).await;
    assert_eq!(r.status, 204);
    assert_ne!(r.header("etag").unwrap(), first);
    let j = e.get("tbl", "p", "r").await.json();
    assert_eq!((j["a"].clone(), j["b"].clone(), j["c"].clone()), (json!(1), json!(20), json!(30)));

    let r = e.send("PUT", path, &[JSON, ("If-Match", "*")], r#"{"b":5}"#).await;
    assert_eq!(r.status, 204);
    let j = e.get("tbl", "p", "r").await.json();
    assert!(j.get("a").is_none() && j.get("c").is_none() && j["b"] == 5);

    // MERGE is the same verb as PATCH
    let r = e.send("MERGE", path, &[JSON, ("If-Match", "*")], r#"{"z":1}"#).await;
    assert_eq!(r.status, 204);
    assert_eq!(e.get("tbl", "p", "r").await.json()["b"], 5);

    // without If-Match a missing entity is created, in both modes
    for (m, rk) in [("PUT", "u1"), ("MERGE", "u2"), ("PATCH", "u3")] {
        let r = e.send(m, &format!("/tbl(PartitionKey='p',RowKey='{rk}')"), &[JSON], r#"{"v":1}"#).await;
        assert_eq!(r.status, 204, "{m}");
        assert_eq!(e.get("tbl", "p", rk).await.json()["v"], 1);
    }
}

#[tokio::test]
async fn update_conditions_and_missing_entities() {
    let e = with_table().await;
    let created = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","v":1}"#).await.header("etag").unwrap().to_string();
    let path = "/tbl(PartitionKey='p',RowKey='r')";
    let stale = "W/\"datetime'2012-06-15T22%3A51%3A44.9662825Z'\"";

    for m in ["PUT", "PATCH"] {
        let r = e.send(m, path, &[JSON, ("If-Match", stale)], r#"{"v":2}"#).await;
        assert_eq!((r.status, code(&r).as_str()), (412, "UpdateConditionNotSatisfied"), "{m}");
        assert_eq!(msg(&r).lines().next(), Some("The update condition specified in the request was not satisfied."));
    }
    assert_eq!(e.get("tbl", "p", "r").await.json()["v"], 1);
    let r = e.send("PUT", path, &[JSON, ("If-Match", &created)], r#"{"v":2}"#).await;
    assert_eq!(r.status, 204);
    // the old etag is stale now
    assert_eq!(e.send("PUT", path, &[JSON, ("If-Match", &created)], r#"{"v":3}"#).await.status, 412);

    // 404 wins over a failed condition
    for m in ["PUT", "PATCH"] {
        let r = e.send(m, "/tbl(PartitionKey='p',RowKey='none')", &[JSON, ("If-Match", stale)], r#"{"v":2}"#).await;
        assert_eq!((r.status, code(&r).as_str()), (404, "ResourceNotFound"), "{m}");
        let r = e.send(m, "/tbl(PartitionKey='p',RowKey='none')", &[JSON, ("If-Match", "*")], r#"{"v":2}"#).await;
        assert_eq!(r.status, 404);
    }
    assert_eq!(e.get("tbl", "p", "none").await.status, 404);
}

#[tokio::test]
async fn every_write_changes_the_etag_even_without_changes() {
    let e = with_table().await;
    let a = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","v":1}"#).await.header("etag").unwrap().to_string();
    let path = "/tbl(PartitionKey='p',RowKey='r')";
    let b = e.send("PUT", path, &[JSON, ("If-Match", "*")], r#"{"v":1}"#).await.header("etag").unwrap().to_string();
    let c = e.send("PUT", path, &[JSON, ("If-Match", "*")], r#"{"v":1}"#).await.header("etag").unwrap().to_string();
    assert!(a != b && b != c && a != c);
    assert_eq!(e.get("tbl", "p", "r").await.header("etag"), Some(c.as_str()));
}

#[tokio::test]
async fn body_keys_must_match_the_url() {
    let e = with_table().await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    let r = e.send("PUT", "/tbl(PartitionKey='p',RowKey='r')", &[JSON], r#"{"PartitionKey":"other","RowKey":"r"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
}

#[tokio::test]
async fn delete_entity() {
    let e = with_table().await;
    let etag = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r"}"#).await.header("etag").unwrap().to_string();
    let path = "/tbl(PartitionKey='p',RowKey='r')";
    // If-Match is required
    let r = e.send("DELETE", path, &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
    let r = e.send("DELETE", path, &[("If-Match", "W/\"datetime'2012-06-15T22%3A51%3A44.9662825Z'\"")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (412, "UpdateConditionNotSatisfied"));
    assert_eq!(e.send("DELETE", path, &[("If-Match", &etag)], "").await.status, 204);
    let r = e.send("DELETE", path, &[("If-Match", "*")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "ResourceNotFound"));
    // a missing table is TableNotFound for entity deletes
    let r = e.send("DELETE", "/nothere(PartitionKey='p',RowKey='r')", &[("If-Match", "*")], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "TableNotFound"));
}

#[tokio::test]
async fn x_http_method_tunnels_through_post() {
    let e = with_table().await;
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","a":1}"#).await;
    let path = "/tbl(PartitionKey='p',RowKey='r')";
    let r = e.send("POST", path, &[JSON, ("X-HTTP-Method", "MERGE"), ("If-Match", "*")], r#"{"b":2}"#).await;
    assert_eq!(r.status, 204);
    let j = e.get("tbl", "p", "r").await.json();
    assert_eq!((j["a"].clone(), j["b"].clone()), (json!(1), json!(2)));
    let r = e.send("POST", path, &[("X-HTTP-Method", "DELETE"), ("If-Match", "*")], "").await;
    assert_eq!(r.status, 204);
    assert_eq!(e.get("tbl", "p", "r").await.status, 404);
    let r = e.send("POST", path, &[JSON, ("X-HTTP-Method", "GET")], "{}").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "XMethodIncorrectValue"));
}

#[tokio::test]
async fn timestamp_in_the_payload_is_ignored() {
    let e = with_table().await;
    let r = e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","Timestamp":"2001-01-01T00:00:00Z"}"#).await;
    assert_ne!(r.json()["Timestamp"], "2001-01-01T00:00:00Z");
}

#[tokio::test]
async fn bad_key_predicates_and_odd_urls() {
    let e = with_table().await;
    let r = e.send("GET", "/tbl(PartitionKey='p')", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
    assert!(
        msg(&r).contains("number of keys specified in the URI does not match number of key properties for the resource 'acct.tbl'"),
        "{}",
        msg(&r)
    );
    // a batch below a table path, and service operations below a table path
    let r = e.send("POST", "/tbl/$batch", &[BATCH_CT], "").await;
    assert_eq!((r.status, code(&r).as_str()), (415, "MediaTypeNotSupported"));
    let r = e.send("GET", "/tbl/?restype=service&comp=properties", &[], "").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("<m:code>InvalidQueryParameterValue</m:code>"));
    let r = e.send("POST", "/tbl/Tables", &[JSON], r#"{"TableName":"tbl"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "PropertiesNeedValue"));
    let r = e.send("POST", "/nothere/Tables", &[JSON], r#"{"TableName":"x"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (404, "TableNotFound"));
}

#[tokio::test]
async fn request_ids_and_client_ids_are_echoed() {
    let e = with_table().await;
    let r = e.send("GET", "/Tables", &[("x-ms-client-request-id", "abc-123"), ("x-ms-version", "2019-02-02")], "").await;
    assert_eq!(r.header("x-ms-client-request-id"), Some("abc-123"));
    assert_eq!(r.header("x-ms-version"), Some("2019-02-02"));
    assert!(r.header("x-ms-request-id").is_some());
    let r = e.get("tbl", "p", "missing").await;
    assert!(r.text().contains(r.header("x-ms-request-id").unwrap()), "the error body carries the request id");
}

#[tokio::test]
async fn data_service_version_is_answered_to_clients_that_send_it() {
    let e = with_table().await;
    let r = e.send("GET", "/Tables", &[("DataServiceVersion", "3.0"), ("MaxDataServiceVersion", "3.0;NetFx")], "").await;
    assert_eq!(r.header("dataserviceversion"), Some("3.0;"));
    let r = e.send("DELETE", "/Tables('tbl')", &[("DataServiceVersion", "3.0")], "").await;
    assert_eq!((r.status, r.header("dataserviceversion")), (204, Some("1.0;")));
    let r = e.send("GET", "/Tables", &[], "").await;
    assert_eq!(r.header("dataserviceversion"), None);
}
