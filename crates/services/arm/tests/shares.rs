//! `Microsoft.Storage/storageAccounts/fileServices/shares`: the same shares as the Azure Files service. A share made through Resource
//! Manager is made there with its quota, access tier and metadata; a share made through the file API is listed by Resource Manager.

mod common;

use common::{full, Arm, SUB};
use serde_json::json;

fn acct(name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/{name}")
}

fn share(account: &str, name: &str) -> String {
    format!("{}/fileServices/default/shares/{name}", acct(account))
}

async fn account(a: &Arm, name: &str) {
    a.group("rg").await;
    let r = a.put(&acct(name), json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn a_share_made_through_resource_manager_is_a_share_of_the_file_service() {
    let a = full().await;
    account(&a, "acct1").await;
    let id = share("acct1", "docs");
    let r = a.put(&id, json!({"properties": {"shareQuota": 5, "accessTier": "Hot", "metadata": {"k": "v"}}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // the PUT answers with the properties it was given (share-put.json)
    let j = r.json();
    assert_eq!(j["type"], "Microsoft.Storage/storageAccounts/fileServices/shares");
    assert_eq!(j["properties"], json!({"accessTier": "Hot", "metadata": {"k": "v"}, "shareQuota": 5}));
    assert!(j.get("etag").is_none());

    // the service has it, with the quota, the tier and the metadata
    let url = a.dp("file", "acct1");
    let p = a.dp_send("GET", &format!("{url}/docs?restype=share"), &[], "").await;
    assert_eq!(p.status, 200, "{}", p.text());
    assert_eq!(p.header("x-ms-share-quota"), Some("5"));
    assert_eq!(p.header("x-ms-access-tier"), Some("Hot"));
    assert_eq!(p.header("x-ms-meta-k"), Some("v"));
    let made = a.dp_send("PUT", &format!("{url}/docs/a.txt"), &[("x-ms-type", "file"), ("x-ms-content-length", "3")], "").await;
    assert_eq!(made.status, 201, "{}", made.text());

    // a GET shows everything (share-get.json)
    let g = a.get(&id).await;
    assert_eq!(g.status, 200, "{}", g.text());
    let g = g.json();
    assert!(g["etag"].as_str().unwrap().starts_with('"'));
    let p = &g["properties"];
    assert_eq!((p["accessTier"].as_str(), p["enabledProtocols"].as_str()), (Some("Hot"), Some("SMB")));
    assert_eq!((p["leaseState"].as_str(), p["leaseStatus"].as_str()), (Some("available"), Some("unlocked")));
    assert_eq!((p["shareQuota"].as_u64(), &p["metadata"], &p["signedIdentifiers"]), (Some(5), &json!({"k": "v"}), &json!([])));
    assert!(p["lastModifiedTime"].as_str().unwrap().ends_with("Z"));
    assert!(p.get("provisioningState").is_none());

    // a list has no metadata and no access policies (share-list.json)
    let list = a.get(&format!("{}/fileServices/default/shares", acct("acct1"))).await.json();
    assert_eq!(list["value"].as_array().map(Vec::len), Some(1));
    assert!(list["value"][0]["properties"].get("metadata").is_none());
    assert_eq!(list["value"][0]["properties"]["shareQuota"], 5);

    // an update: ARM's values replace the service's
    let r = a.put(&id, json!({"properties": {"shareQuota": 10, "accessTier": "Cool", "metadata": {"k2": "v2"}}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let p = a.dp_send("GET", &format!("{url}/docs?restype=share"), &[], "").await;
    assert_eq!(
        (p.header("x-ms-share-quota"), p.header("x-ms-access-tier"), p.header("x-ms-meta-k2")),
        (Some("10"), Some("Cool"), Some("v2"))
    );
    assert_eq!(p.header("x-ms-meta-k"), None);
    // PATCH changes what it carries and keeps the rest
    let r = a.call("PATCH", &id, Some(json!({"properties": {"shareQuota": 20}}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let p = a.dp_send("GET", &format!("{url}/docs?restype=share"), &[], "").await;
    assert_eq!((p.header("x-ms-share-quota"), p.header("x-ms-meta-k2")), (Some("20"), Some("v2")));
    assert_eq!(a.get(&id).await.json()["properties"]["accessTier"], "Cool");

    // an access policy goes to the share's ACL
    let r = a
        .put(
            &id,
            json!({"properties": {"shareQuota": 20, "signedIdentifiers": [{"id": "p1", "accessPolicy": {"permission": "rl", "expiryTime": "2031-01-01T00:00:00.0000000Z"}}]}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let acl = a.dp_send("GET", &format!("{url}/docs?restype=share&comp=acl"), &[], "").await;
    assert!(acl.text().contains("<Id>p1</Id>") && acl.text().contains("<Permission>rl</Permission>"), "{}", acl.text());
    assert_eq!(a.get(&id).await.json()["properties"]["signedIdentifiers"][0]["id"], "p1");

    // delete removes it from the service
    assert_eq!(a.delete(&id).await.status, 200);
    let gone = a.dp_send("GET", &format!("{url}/docs?restype=share"), &[], "").await;
    assert_eq!(gone.status, 404);
    assert_eq!(a.get(&id).await.status, 404);
    // a share that is not there (share-get-missing.txt), and the delete of one answers 204
    let missing = a.get(&id).await;
    assert_eq!(missing.json()["error"]["code"], "ShareNotFound");
    assert_eq!(missing.json()["error"]["message"], "The specified share does not exist.");
    assert_eq!(a.delete(&id).await.status, 204);
}

#[tokio::test]
async fn a_share_made_through_the_file_api_is_listed_by_resource_manager() {
    let a = full().await;
    account(&a, "acct2").await;
    let url = a.dp("file", "acct2");
    let made = a
        .dp_send(
            "PUT",
            &format!("{url}/fromsdk?restype=share"),
            &[("x-ms-share-quota", "7"), ("x-ms-meta-owner", "team"), ("x-ms-access-tier", "Hot")],
            "",
        )
        .await;
    assert_eq!(made.status, 201, "{}", made.text());

    let list = a.get(&format!("{}/fileServices/default/shares", acct("acct2"))).await.json();
    let names: Vec<&str> = list["value"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, ["fromsdk"]);
    let g = a.get(&share("acct2", "fromsdk")).await.json();
    assert_eq!(g["id"], json!(share("acct2", "fromsdk")));
    assert_eq!((g["properties"]["shareQuota"].as_u64(), g["properties"]["accessTier"].as_str()), (Some(7), Some("Hot")));
    assert_eq!(g["properties"]["metadata"], json!({"owner": "team"}));

    // what changes in the service shows in Resource Manager
    let r = a.dp_send("PUT", &format!("{url}/fromsdk?restype=share&comp=properties"), &[("x-ms-share-quota", "9")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(a.get(&share("acct2", "fromsdk")).await.json()["properties"]["shareQuota"], 9);
    // a PUT of a share that the service had already made is an update
    let r = a.put(&share("acct2", "fromsdk"), json!({"properties": {"shareQuota": 11}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // and so is the delete of one
    assert_eq!(a.delete(&share("acct2", "fromsdk")).await.status, 200);
    assert_eq!(a.dp_send("GET", &format!("{url}/fromsdk?restype=share"), &[], "").await.status, 404);

    // a share deleted through the service is gone from the list
    a.dp_send("PUT", &format!("{url}/second?restype=share"), &[], "").await;
    assert_eq!(a.get(&format!("{}/fileServices/default/shares", acct("acct2"))).await.json()["value"].as_array().map(Vec::len), Some(1));
    a.dp_send("DELETE", &format!("{url}/second?restype=share"), &[], "").await;
    assert_eq!(a.get(&format!("{}/fileServices/default/shares", acct("acct2"))).await.json()["value"], json!([]));
    assert_eq!(a.get(&share("acct2", "second")).await.status, 404);
}

#[tokio::test]
async fn names_values_and_the_end_of_the_account() {
    let a = full().await;
    account(&a, "acct3").await;
    // share-put-badname.txt
    for bad in ["Upper", "ab", "-lead", "two--hyphens", "under_score"] {
        let r = a.put(&share("acct3", bad), json!({"properties": {}})).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.text());
        assert_eq!(r.json()["error"]["code"], "InvalidResourceName", "{bad}");
    }
    // the service refuses a tier or a quota it does not know, and ARM answers its error
    let r = a.put(&share("acct3", "ok1"), json!({"properties": {"accessTier": "Warm"}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = a.put(&share("acct3", "ok2"), json!({"properties": {"shareQuota": 0}})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(a.get(&share("acct3", "ok2")).await.status, 404, "a refused share is not kept");
    // no account, no share
    let r = a.put(&share("nothere", "docs"), json!({"properties": {}})).await;
    assert_eq!(r.status, 404, "{}", r.text());

    // fileServices/default keeps the retention of deleted shares that was written
    let fs = format!("{}/fileServices/default", acct("acct3"));
    let g = a.get(&fs).await.json();
    assert_eq!(g["properties"]["shareDeleteRetentionPolicy"], json!({"enabled": true, "days": 7}));
    let r = a.put(&fs, json!({"properties": {"shareDeleteRetentionPolicy": {"enabled": true, "days": 14}}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let g = a.get(&fs).await.json();
    assert_eq!(g["properties"]["shareDeleteRetentionPolicy"]["days"], 14);
    assert_eq!(g["properties"]["cors"], json!({"corsRules": []}), "what the request leaves out is kept");
    assert_eq!(a.put(&format!("{}/fileServices/other", acct("acct3")), json!({"properties": {}})).await.status, 400);

    // deleting the account takes its shares, the soft-deleted ones too
    let url = a.dp("file", "acct3");
    assert_eq!(a.put(&share("acct3", "keep"), json!({"properties": {"shareQuota": 1}})).await.status, 201);
    assert_eq!(a.put(&share("acct3", "dropped"), json!({"properties": {"shareQuota": 1}})).await.status, 201);
    assert_eq!(a.delete(&share("acct3", "dropped")).await.status, 200);
    let listed = a.dp_send("GET", &format!("{url}/?comp=list&include=deleted"), &[], "").await;
    assert!(listed.text().contains("<Deleted>true</Deleted>"), "{}", listed.text());
    assert_eq!(a.delete(&acct("acct3")).await.status, 200);
    let after = a.dp_send("GET", &format!("{url}/?comp=list&include=deleted"), &[], "").await;
    assert!(!after.text().contains("<Name>keep</Name>") && !after.text().contains("<Name>dropped</Name>"), "{}", after.text());
}
