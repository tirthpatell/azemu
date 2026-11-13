mod common;
use common::*;

#[tokio::test]
async fn create_list_delete_table() {
    let e = env().await;
    let r = e.create_table("MyTable").await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["TableName"], "MyTable");
    assert_eq!(r.json()["odata.metadata"], format!("{}/$metadata#Tables/@Element", e.base));
    assert_eq!(r.header("location"), Some(format!("{}/Tables('MyTable')", e.base).as_str()));
    assert!(r.header("x-ms-request-id").is_some());
    assert_eq!(r.header("server"), Some("Windows-Azure-Table/1.0 Microsoft-HTTPAPI/2.0"));

    let r = e.send("GET", "/Tables", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["value"], azemu_core::serde_json::json!([{"TableName": "MyTable"}]));
    // Azure writes a space after each semicolon in the header of a whole answer (recording
    // `test_table.pyTestTabletest_create_table_fail_on_exist.json`, entry 2).
    assert_eq!(r.header("content-type"), Some("application/json; odata=minimalmetadata; streaming=true; charset=utf-8"));

    let r = e.send("DELETE", "/Tables('mytable')", &[], "").await;
    assert_eq!(r.status, 204);
    assert!(r.body.is_empty());
    assert_eq!(e.send("GET", "/Tables", &[], "").await.json()["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn names_are_case_insensitive_and_keep_their_case() {
    let e = env().await;
    assert_eq!(e.create_table("CaSe1").await.status, 201);
    let r = e.create_table("case1").await;
    assert_eq!((r.status, code(&r).as_str()), (409, "TableAlreadyExists"));
    assert_eq!(msg(&r).lines().next(), Some("The table specified already exists."));
    let r = e.send("GET", "/Tables", &[], "").await;
    assert_eq!(r.json()["value"][0]["TableName"], "CaSe1");
    // entity operations find the table whatever case the URL uses
    assert_eq!(e.insert("CASE1", r#"{"PartitionKey":"p","RowKey":"r"}"#).await.status, 201);
}

#[tokio::test]
async fn table_name_rules_use_the_recorded_codes() {
    let e = env().await;
    for bad in ["1table", "my_table", "my-table", "my.table"] {
        let r = e.create_table(bad).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"), "{bad}");
    }
    assert_eq!(msg(&e.create_table("my_table").await).lines().next(), Some("The specifed resource name contains invalid characters."));
    for bad in ["aa", &"a".repeat(64)] {
        let r = e.create_table(bad).await;
        assert_eq!((r.status, code(&r).as_str()), (400, "OutOfRangeInput"), "{bad}");
    }
    assert_eq!(e.create_table("abc").await.status, 201);
    assert_eq!(e.create_table(&format!("a{}", "b".repeat(62))).await.status, 201);
    // the same rules apply to entity operations
    let r = e.insert("1table", r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidResourceName"));
    let r = e.send("GET", "/aa?comp=acl", &[], "").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("<m:code>OutOfRangeInput</m:code>"));
}

#[tokio::test]
async fn missing_table_codes() {
    let e = env().await;
    let r = e.send("DELETE", "/Tables('nothere')", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "ResourceNotFound"));
    let r = e.insert("nothere", r#"{"PartitionKey":"p","RowKey":"r"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (404, "TableNotFound"));
    assert_eq!(msg(&r).lines().next(), Some("The table specified does not exist."));
    let r = e.send("GET", "/nothere()", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (404, "TableNotFound"));
    let r = e.send("GET", "/nothere?comp=acl", &[], "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("<m:code>TableNotFound</m:code>"));
}

#[tokio::test]
async fn create_without_a_name_needs_a_value() {
    let e = env().await;
    let r = e.send("POST", "/Tables", &[JSON], "{}").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "PropertiesNeedValue"));
    let r = e.send("POST", "/Tables", &[JSON], "not json").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
}

#[tokio::test]
async fn prefer_return_no_content_and_payload_levels() {
    let e = env().await;
    let r = e.send("POST", "/Tables", &[JSON, ("Prefer", "return-no-content")], r#"{"TableName":"quiet"}"#).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("preference-applied"), Some("return-no-content"));
    assert!(r.body.is_empty());

    let r = e.send("POST", "/Tables", &[JSON, ("Accept", "application/json;odata=nometadata")], r#"{"TableName":"plain"}"#).await;
    assert_eq!(r.json(), azemu_core::serde_json::json!({"TableName": "plain"}));
    assert_eq!(r.header("content-type"), Some("application/json; odata=nometadata; streaming=true; charset=utf-8"));

    let r = e.send("GET", "/Tables", &[("Accept", "application/json;odata=fullmetadata")], "").await;
    let first = &r.json()["value"][0];
    assert_eq!(first["odata.type"], "acct.Tables");
    assert_eq!(first["odata.id"], format!("{}/Tables('plain')", e.base));
    assert_eq!(first["odata.editLink"], "Tables('plain')");
}

#[tokio::test]
async fn query_tables_filter_top_and_continuation() {
    let e = env().await;
    for n in ["tb1", "tb2", "tb3", "tb4", "other"] {
        e.create_table(n).await;
    }
    let names = |r: &azemu_core::testing::TestResponse| -> Vec<String> {
        r.json()["value"].as_array().unwrap().iter().map(|t| t["TableName"].as_str().unwrap().to_string()).collect()
    };
    let r = e.send("GET", "/Tables?$filter=TableName%20ge%20'tb'%20and%20TableName%20lt%20'tc'&$top=3", &[], "").await;
    assert_eq!(names(&r), ["tb1", "tb2", "tb3"]);
    let token = r.header("x-ms-continuation-NextTableName").unwrap().to_string();
    assert!(token.starts_with("1!"));
    let enc = token.replace('!', "%21");
    let r = e
        .send("GET", &format!("/Tables?$filter=TableName%20ge%20'tb'%20and%20TableName%20lt%20'tc'&$top=3&NextTableName={enc}"), &[], "")
        .await;
    assert_eq!(names(&r), ["tb4"]);
    assert!(r.header("x-ms-continuation-NextTableName").is_none());

    let r = e.send("GET", "/Tables?$filter=TableName%20eq%20'other'", &[], "").await;
    assert_eq!(names(&r), ["other"]);
    let r = e.send("GET", "/Tables?$filter=aaa%20bbb%20ccc", &[], "").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
    assert_eq!(msg(&r).lines().next(), Some("Syntax error at position 7 in 'aaa bbb ccc'."));
}

#[tokio::test]
async fn accounts_are_separate_and_any_account_works() {
    let e = env().await;
    e.create_table("shared").await;
    let other = e.srv.url("table", "other");
    let r = e.srv.send("GET", &format!("{other}/Tables"), &[AUTH], "").await;
    assert_eq!(r.json()["value"].as_array().unwrap().len(), 0);
    let r = e.srv.send("POST", &format!("{other}/Tables"), &[AUTH, JSON], r#"{"TableName":"shared"}"#).await;
    assert_eq!(r.status, 201);
}

#[tokio::test]
async fn reset_drops_everything() {
    let e = env().await;
    e.create_table("gone").await;
    e.srv.reset();
    assert_eq!(e.send("GET", "/Tables", &[], "").await.json()["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn unbuilt_operations_answer_501_and_name_themselves() {
    let e = env().await;
    e.create_table("tbl").await;
    for (m, p) in [("HEAD", "/Tables"), ("PUT", "/Tables"), ("GET", "/?comp=list"), ("DELETE", "/tbl")] {
        let r = e.send(m, p, &[], "").await;
        assert_eq!(r.status, 501, "{m} {p}: {}", r.text());
        if m != "HEAD" {
            assert!(r.text().contains("NotImplemented"), "{m} {p}");
            assert!(r.text().contains(&format!("table: {m} ")), "{}", r.text());
        }
    }
}
