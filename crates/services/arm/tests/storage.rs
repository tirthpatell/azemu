mod common;

use common::{full, SUB};
use serde_json::json;

fn acct(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.Storage/storageAccounts/{name}")
}

#[tokio::test]
async fn account_container_queue_table_reach_the_data_plane() {
    let a = full().await;
    a.group("rg").await;
    let r = a.put(&acct("rg", "acct1"), json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let j = r.json();
    assert_eq!(j["name"], "acct1");
    assert_eq!(j["type"], "Microsoft.Storage/storageAccounts");
    assert_eq!(j["sku"]["tier"], "Standard");
    assert_eq!(j["properties"]["provisioningState"], "Succeeded");
    let port = a.srv.port();
    assert_eq!(j["properties"]["primaryEndpoints"]["blob"], format!("http://acct1.blob.core.localhost:{port}/"));
    assert_eq!(j["properties"]["primaryEndpoints"]["queue"], format!("http://acct1.queue.core.localhost:{port}/"));
    assert_eq!(j["properties"]["primaryEndpoints"]["table"], format!("http://acct1.table.core.localhost:{port}/"));
    assert_eq!(j["properties"]["primaryEndpoints"]["dfs"], format!("http://acct1.dfs.core.localhost:{port}/"));

    // container
    let c = format!("{}/blobServices/default/containers/photos", acct("rg", "acct1"));
    let r = a.put(&c, json!({"properties": {"publicAccess": "None", "metadata": {"team": "a"}}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["type"], "Microsoft.Storage/storageAccounts/blobServices/containers");
    assert_eq!(a.put(&c, json!({"properties": {}})).await.status, 200);
    // the Blob service sees it
    let url = a.dp("blob", "acct1");
    let r = a.dp_send("PUT", &format!("{url}/photos/hello.txt"), &[("x-ms-blob-type", "BlockBlob")], "hello").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = a.dp_send("GET", &format!("{url}/photos/hello.txt"), &[], "").await;
    assert_eq!(r.text(), "hello");
    let r = a.dp_send("GET", &format!("{url}/photos?restype=container"), &[], "").await;
    assert_eq!(r.status, 200);

    // queue
    let q = format!("{}/queueServices/default/queues/jobs", acct("rg", "acct1"));
    let r = a.put(&q, json!({"properties": {"metadata": {"k": "v"}}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let qurl = a.dp("queue", "acct1");
    let r = a.dp_send("POST", &format!("{qurl}/jobs/messages"), &[], "<QueueMessage><MessageText>hi</MessageText></QueueMessage>").await;
    assert_eq!(r.status, 201, "{}", r.text());

    // table
    let t = format!("{}/tableServices/default/tables/people", acct("rg", "acct1"));
    let r = a.put(&t, json!({})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["properties"]["tableName"], "people");
    let turl = a.dp("table", "acct1");
    let r = a
        .dp_send(
            "POST",
            &format!("{turl}/people"),
            &[("Content-Type", "application/json"), ("Accept", "application/json;odata=nometadata")],
            r#"{"PartitionKey":"p","RowKey":"r","v":1}"#,
        )
        .await;
    assert!(r.status == 201 || r.status == 204, "{} {}", r.status, r.text());

    // lists
    let r = a.get(&format!("{}/blobServices/default/containers", acct("rg", "acct1"))).await.json();
    assert_eq!(r["value"].as_array().unwrap().len(), 1);
    let r = a.get(&format!("{}/blobServices/default", acct("rg", "acct1"))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["name"], "default");

    // delete a container through ARM: gone from the data plane
    assert_eq!(a.delete(&c).await.status, 200);
    let r = a.dp_send("GET", &format!("{url}/photos/hello.txt"), &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(a.delete(&q).await.status, 204);
    assert_eq!(a.delete(&t).await.status, 204);
    let r = a.dp_send("GET", &format!("{qurl}/jobs/messages"), &[], "").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn keys_and_name_check() {
    let a = full().await;
    a.group("rg").await;
    a.put(&acct("rg", "acct2"), json!({"location": "eastus", "sku": {"name": "Standard_GRS"}, "kind": "StorageV2"})).await;
    let r = a.post(&format!("{}/listKeys", acct("rg", "acct2")), json!({})).await;
    assert_eq!(r.status, 200);
    let k = r.json();
    assert_eq!(k["keys"][0]["keyName"], "key1");
    assert_eq!(k["keys"][0]["permissions"], "FULL");
    let key1 = k["keys"][0]["value"].as_str().unwrap().to_string();
    assert_eq!(key1.len(), 88);
    let r = a.post(&format!("{}/regenerateKey", acct("rg", "acct2")), json!({"keyName": "key1"})).await.json();
    assert_ne!(r["keys"][0]["value"].as_str().unwrap(), key1);
    let r = a
        .post(
            &format!("/subscriptions/{SUB}/providers/Microsoft.Storage/checkNameAvailability"),
            json!({"name": "acct2", "type": "Microsoft.Storage/storageAccounts"}),
        )
        .await
        .json();
    assert_eq!(r["nameAvailable"], false);
    assert_eq!(r["reason"], "AlreadyExists");
    let r = a
        .post(
            &format!("/subscriptions/{SUB}/providers/Microsoft.Storage/checkNameAvailability"),
            json!({"name": "freshname", "type": "Microsoft.Storage/storageAccounts"}),
        )
        .await
        .json();
    assert_eq!(r["nameAvailable"], true);
    let r = a
        .post(
            &format!("/subscriptions/{SUB}/providers/Microsoft.Storage/checkNameAvailability"),
            json!({"name": "Bad_Name", "type": "Microsoft.Storage/storageAccounts"}),
        )
        .await
        .json();
    assert_eq!(r["reason"], "AccountNameInvalid");
    let r = a.put(&acct("rg", "BadName"), json!({"location": "eastus"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "AccountNameInvalid");
    // another group cannot take the name
    a.group("rg-b").await;
    let r = a.put(&acct("rg-b", "acct2"), json!({"location": "eastus"})).await;
    assert_eq!(r.status, 409);
}

#[tokio::test]
async fn deleting_the_account_or_group_removes_data() {
    let a = full().await;
    a.group("rgd").await;
    a.put(&acct("rgd", "acct3"), json!({"location": "eastus"})).await;
    a.put(&format!("{}/blobServices/default/containers/cont1", acct("rgd", "acct3")), json!({})).await;
    let url = a.dp("blob", "acct3");
    assert_eq!(a.dp_send("PUT", &format!("{url}/cont1/b.txt"), &[("x-ms-blob-type", "BlockBlob")], "x").await.status, 201);
    // delete only the account
    assert_eq!(a.delete(&acct("rgd", "acct3")).await.status, 200);
    assert_eq!(a.dp_send("GET", &format!("{url}/cont1/b.txt"), &[], "").await.status, 404);
    // a group delete takes the rest
    a.put(&acct("rgd", "acct4"), json!({"location": "eastus"})).await;
    a.put(&format!("{}/queueServices/default/queues/queue1", acct("rgd", "acct4")), json!({})).await;
    let qurl = a.dp("queue", "acct4");
    assert_eq!(a.dp_send("GET", &format!("{qurl}/queue1?comp=metadata"), &[], "").await.status, 200);
    assert_eq!(a.delete(&format!("/subscriptions/{SUB}/resourceGroups/rgd")).await.status, 202);
    assert_eq!(a.dp_send("GET", &format!("{qurl}/queue1?comp=metadata"), &[], "").await.status, 404);
}

#[tokio::test]
async fn child_needs_parent_and_unbuilt_child_is_501() {
    let a = full().await;
    a.group("rge").await;
    let r = a.put(&format!("{}/blobServices/default/containers/c", acct("rge", "nonexistent1")), json!({})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "ParentResourceNotFound");
    a.put(&acct("rge", "acct5"), json!({"location": "eastus"})).await;
    let r = a.put(&format!("{}/fileServices/default/shares/s1", acct("rge", "acct5")), json!({})).await;
    assert_eq!(r.status, 501);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("fileServices/shares"));
}

#[tokio::test]
async fn reset_empties_both_sides() {
    let a = full().await;
    a.group("rgr").await;
    a.put(&acct("rgr", "acct6"), json!({"location": "eastus"})).await;
    a.put(&format!("{}/blobServices/default/containers/cont1", acct("rgr", "acct6")), json!({})).await;
    a.srv.reset();
    assert_eq!(a.get(&acct("rgr", "acct6")).await.status, 404);
    let url = a.dp("blob", "acct6");
    assert_eq!(a.dp_send("GET", &format!("{url}/cont1?restype=container"), &[], "").await.status, 404);
}

#[tokio::test]
async fn updating_a_container_back_to_private_reaches_the_blob_service() {
    // the update path sent "none" as the access level, which the Blob service refuses; ARM ignored the refusal and said Succeeded
    let a = full().await;
    a.group("rg").await;
    let r = a.put(&acct("rg", "acct7"), json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2", "properties": {"allowBlobPublicAccess": true}})).await;
    assert_eq!(r.status, 200);
    let c = format!("{}/blobServices/default/containers/pub", acct("rg", "acct7"));
    assert_eq!(a.put(&c, json!({"properties": {"publicAccess": "Blob"}})).await.status, 201);
    let url = a.dp("blob", "acct7");
    let target = format!("{url}/pub?restype=container");
    let props = || a.dp_send("GET", &target, &[], "");
    assert_eq!(props().await.header("x-ms-blob-public-access"), Some("blob"));
    let r = a.put(&c, json!({"properties": {"publicAccess": "None", "metadata": {"k": "v"}}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let p = props().await;
    assert_eq!(p.header("x-ms-blob-public-access"), None, "the container is private again");
    assert_eq!(p.header("x-ms-meta-k"), Some("v"));
}

/// `allowBlobPublicAccess` reaches the Blob service: it is false unless the request says true (as on Azure), and while it is
/// false an anonymous request is refused with 409 PublicAccessNotPermitted and a container cannot be made public.
#[tokio::test]
async fn allow_blob_public_access_decides_whether_containers_can_be_public() {
    let a = full().await;
    a.group("rg").await;
    let body = |allow: Option<bool>| {
        let mut b = json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"});
        if let Some(v) = allow {
            b["properties"] = json!({"allowBlobPublicAccess": v});
        }
        b
    };
    for (name, allow) in [("openacct", Some(true)), ("closedacct", None)] {
        let r = a.put(&acct("rg", name), body(allow)).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["properties"]["allowBlobPublicAccess"], json!(allow.unwrap_or(false)));
    }
    // an account that allows it: a public container is read with no credential
    let c = format!("{}/blobServices/default/containers/photos", acct("rg", "openacct"));
    let r = a.put(&c, json!({"properties": {"publicAccess": "Blob"}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let url = a.dp("blob", "openacct");
    let r = a.dp_send("PUT", &format!("{url}/photos/p.txt"), &[("x-ms-blob-type", "BlockBlob")], "pic").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let anon = |u: String| {
        let srv = &a.srv;
        async move { srv.send("GET", &u, &[], "").await }
    };
    assert_eq!(anon(format!("{url}/photos/p.txt")).await.text(), "pic");
    // the account that does not: refused, and no public container
    let url = a.dp("blob", "closedacct");
    let r = a.dp_send("PUT", &format!("{url}/photos?restype=container"), &[], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = anon(format!("{url}/photos/p.txt")).await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (409, Some("PublicAccessNotPermitted")), "{}", r.text());
    let c = format!("{}/blobServices/default/containers/shared", acct("rg", "closedacct"));
    let r = a.put(&c, json!({"properties": {"publicAccess": "Container"}})).await;
    assert!(r.status >= 400, "{} {}", r.status, r.text());
    // turning it on later is what the management API does
    let r = a.put(&acct("rg", "closedacct"), body(Some(true))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a.put(&c, json!({"properties": {"publicAccess": "Container"}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}
