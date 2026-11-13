//! Deletes through ARM either remove everything or say why not, and a group delete does not race a create.

mod common;

use common::{full, Arm, SUB};
use serde_json::json;

fn acct(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/Microsoft.Storage/storageAccounts/{name}")
}

async fn account(a: &Arm, rg: &str, name: &str) {
    let r = a.put(&acct(rg, name), json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

async fn blob(a: &Arm, account: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> azemu_core::testing::TestResponse {
    a.dp_send(method, &format!("{}{path}", a.dp("blob", account)), headers, body).await
}

async fn lease_container(a: &Arm, account: &str, name: &str) {
    assert_eq!(blob(a, account, "PUT", &format!("/{name}?restype=container"), &[], "").await.status, 201);
    let r = blob(
        a,
        account,
        "PUT",
        &format!("/{name}?restype=container&comp=lease"),
        &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")],
        "",
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn deleting_a_leased_container_through_arm_fails_and_the_container_stays() {
    let a = full().await;
    a.group("rg").await;
    account(&a, "rg", "acct1").await;
    let c = format!("{}/blobServices/default/containers/leased", acct("rg", "acct1"));
    assert_eq!(a.put(&c, json!({"properties": {}})).await.status, 201);
    let r = blob(
        &a,
        "acct1",
        "PUT",
        "/leased?restype=container&comp=lease",
        &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")],
        "",
    )
    .await;
    assert_eq!(r.status, 201);
    let lease_id = r.header("x-ms-lease-id").unwrap().to_string();
    // the data plane refuses, so ARM refuses: a conflict with the code the data plane gave
    let r = a.delete(&c).await;
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "LeaseIdMissing");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("leased"), "{}", r.text());
    // nothing was lost: ARM still has it, the data plane still has it
    assert_eq!(a.get(&c).await.status, 200);
    assert_eq!(blob(&a, "acct1", "GET", "/leased?restype=container", &[], "").await.status, 200);
    // after the lease is released the same delete works
    let r = blob(
        &a,
        "acct1",
        "PUT",
        "/leased?restype=container&comp=lease",
        &[("x-ms-lease-action", "release"), ("x-ms-lease-id", &lease_id)],
        "",
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(a.delete(&c).await.status, 200);
    assert_eq!(a.get(&c).await.status, 404);
    assert_eq!(blob(&a, "acct1", "GET", "/leased?restype=container", &[], "").await.status, 404);
}

#[tokio::test]
async fn deleting_an_account_removes_leased_containers_too() {
    let a = full().await;
    a.group("rg").await;
    account(&a, "rg", "acct2").await;
    lease_container(&a, "acct2", "leased").await;
    assert_eq!(blob(&a, "acct2", "PUT", "/plain?restype=container", &[], "").await.status, 201);
    assert_eq!(a.delete(&acct("rg", "acct2")).await.status, 200);
    assert_eq!(a.get(&acct("rg", "acct2")).await.status, 404);
    // the same name again starts empty
    account(&a, "rg", "acct2").await;
    let r = blob(&a, "acct2", "GET", "/?comp=list", &[], "").await;
    assert!(!r.text().contains("<Name>"), "left over: {}", r.text());
}

/// A Queue service that lists three queues on two pages and refuses to delete one of them.
struct FakeQueue {
    deleted: parking_lot::Mutex<Vec<String>>,
    refuse: parking_lot::Mutex<bool>,
}

#[azemu_core::async_trait]
impl azemu_core::Service for FakeQueue {
    fn name(&self) -> &'static str {
        "queue"
    }
    fn mounts(&self) -> Vec<azemu_core::Mount> {
        vec![azemu_core::Mount::AccountHost("queue")]
    }
    fn reset(&self) {}
    async fn handle(&self, req: azemu_core::Request) -> azemu_core::Response {
        use azemu_core::resp::Resp;
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => {
                let (names, next) = match req.query.get("marker") {
                    None => ("<Queue><Name>q1</Name></Queue><Queue><Name>stuck</Name></Queue>", "m1"),
                    Some(_) => ("<Queue><Name>q3</Name></Queue>", ""),
                };
                let xml = format!("<?xml version=\"1.0\"?><EnumerationResults><Queues>{names}</Queues><NextMarker>{next}</NextMarker></EnumerationResults>");
                Resp::new(200u16).header("Content-Type", "application/xml").body(xml.into_bytes()).build()
            }
            ("DELETE", p) => {
                let name = p.trim_start_matches('/').to_string();
                if name == "stuck" && *self.refuse.lock() {
                    return Resp::new(409u16)
                        .header("x-ms-error-code", "QueueBeingDeleted")
                        .body(
                            b"<Error><Code>QueueBeingDeleted</Code><Message>The specified queue is being deleted.</Message></Error>"
                                .to_vec(),
                        )
                        .build();
                }
                self.deleted.lock().push(name);
                Resp::new(204u16).build()
            }
            _ => Resp::new(404u16).build(),
        }
    }
}

#[tokio::test]
async fn an_account_whose_delete_fails_in_a_service_is_kept_with_the_error_and_every_page_is_read() {
    let fake = std::sync::Arc::new(FakeQueue { deleted: Default::default(), refuse: parking_lot::Mutex::new(true) });
    let q: std::sync::Arc<dyn azemu_core::Service> = fake.clone();
    let svc = azemu_arm::ArmService::with_strict(azemu_arm::Backends { queue: Some(q), ..Default::default() }, false);
    let a = Arm { srv: azemu_core::testing::TestServer::start(vec![std::sync::Arc::new(svc)]).await };
    a.group("rg").await;
    account(&a, "rg", "acct3").await;
    let r = a.delete(&acct("rg", "acct3")).await;
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "QueueBeingDeleted");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("acct3"));
    // both pages were read and everything that could be deleted was (q3 is on the second page)
    let mut done = fake.deleted.lock().clone();
    done.sort();
    assert_eq!(done, ["q1", "q3"]);
    // ARM still has the account, and a group delete answers the same error and keeps the group
    assert_eq!(a.get(&acct("rg", "acct3")).await.status, 200);
    let g = format!("/subscriptions/{SUB}/resourcegroups/rg");
    let r = a.delete(&g).await;
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(a.get(&g).await.status, 200);
    assert_eq!(a.get(&acct("rg", "acct3")).await.status, 200);
    // the group is open for writes again after a failed delete
    account(&a, "rg", "acct5").await;
    // once the service lets go, the same delete works
    *fake.refuse.lock() = false;
    assert_eq!(a.delete(&g).await.status, 202);
    assert_eq!(a.get(&g).await.status, 404);
    assert_eq!(a.get(&acct("rg", "acct3")).await.status, 404);
    assert!(fake.deleted.lock().contains(&"stuck".to_string()));
}

#[tokio::test]
async fn an_account_with_more_than_one_page_of_containers_is_emptied_completely() {
    // the Blob list has 5000 names a page; the delete used to read the first page only
    let a = full().await;
    a.group("rg").await;
    account(&a, "rg", "acct4").await;
    let started = std::time::Instant::now();
    for i in 0..5_010 {
        let r = blob(&a, "acct4", "PUT", &format!("/c{i:05}?restype=container"), &[], "").await;
        assert_eq!(r.status, 201);
    }
    eprintln!("created 5010 containers in {:?}", started.elapsed());
    let r = blob(&a, "acct4", "GET", "/?comp=list", &[], "").await;
    assert!(r.text().contains("<NextMarker>c"), "the list has a second page");
    assert_eq!(a.delete(&acct("rg", "acct4")).await.status, 200);
    account(&a, "rg", "acct4").await;
    let r = blob(&a, "acct4", "GET", "/?comp=list", &[], "").await;
    assert!(!r.text().contains("<Name>"), "containers left: {}", &r.text()[..r.text().len().min(300)]);
}

// ---- a group delete and a create that overlap ---------------------------------------------------------------------------

/// A Queue service whose create of a queue waits until the test lets it go.
struct SlowQueue {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    deleted: parking_lot::Mutex<Vec<String>>,
}

#[azemu_core::async_trait]
impl azemu_core::Service for SlowQueue {
    fn name(&self) -> &'static str {
        "queue"
    }
    fn mounts(&self) -> Vec<azemu_core::Mount> {
        vec![azemu_core::Mount::AccountHost("queue")]
    }
    fn reset(&self) {}
    async fn handle(&self, req: azemu_core::Request) -> azemu_core::Response {
        use azemu_core::resp::Resp;
        match (req.method.as_str(), req.path.as_str()) {
            ("PUT", "/slowq") => {
                self.entered.notify_one();
                self.release.notified().await;
                Resp::new(201u16).build()
            }
            ("PUT", _) => Resp::new(201u16).build(),
            ("GET", "/") => Resp::new(200u16)
                .header("Content-Type", "application/xml")
                .body(b"<EnumerationResults><Queues><Queue><Name>slowq</Name></Queue></Queues><NextMarker /></EnumerationResults>".to_vec())
                .build(),
            ("DELETE", p) => {
                self.deleted.lock().push(p.trim_start_matches('/').to_string());
                Resp::new(204u16).build()
            }
            _ => Resp::new(404u16).build(),
        }
    }
}

#[tokio::test]
async fn a_create_that_is_running_when_the_group_delete_starts_is_deleted_with_the_group_and_a_later_one_is_refused() {
    let fake = std::sync::Arc::new(SlowQueue { entered: Default::default(), release: Default::default(), deleted: Default::default() });
    let q: std::sync::Arc<dyn azemu_core::Service> = fake.clone();
    let svc = azemu_arm::ArmService::with_strict(azemu_arm::Backends { queue: Some(q), ..Default::default() }, false);
    let a = Arm { srv: azemu_core::testing::TestServer::start(vec![std::sync::Arc::new(svc)]).await };
    a.group("rg").await;
    account(&a, "rg", "acct6").await;
    let g = format!("/subscriptions/{SUB}/resourcegroups/rg");
    let slow = format!("{}/queueServices/default/queues/slowq", acct("rg", "acct6"));
    let other = format!("{}/queueServices/default/queues/other", acct("rg", "acct6"));
    let create = a.put(&slow, json!({"properties": {}}));
    let orchestrate = async {
        fake.entered.notified().await; // the create is inside the service, holding its place in the group
        let delete = a.delete(&g);
        let later = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await; // the delete has marked the group by now
            let r = a.put(&other, json!({"properties": {}})).await;
            let r2 = a.put(&g, json!({"location": "eastus"})).await;
            fake.release.notify_one(); // let the first create finish
            (r, r2)
        };
        tokio::join!(delete, later)
    };
    let (created, (deleted, (refused, refused_group))) = tokio::join!(create, orchestrate);
    assert_eq!(created.status, 200, "the create that was already running finishes: {}", created.text());
    assert_eq!(refused.status, 409, "{}", refused.text());
    assert_eq!(refused.json()["error"]["code"], "ResourceGroupBeingDeleted");
    assert_eq!(refused_group.status, 409, "{}", refused_group.text());
    assert_eq!(deleted.status, 202, "{}", deleted.text());
    // the group and everything in it are gone: nothing of the create is left, in ARM or in the service
    assert_eq!(a.get(&g).await.status, 404);
    assert_eq!(a.get(&slow).await.status, 404);
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/rg/resources")).await.status, 404);
    assert!(fake.deleted.lock().contains(&"slowq".to_string()), "{:?}", fake.deleted.lock());
    // a create after the delete answered fails: there is no group
    assert_eq!(a.put(&other, json!({"properties": {}})).await.status, 404);
}
