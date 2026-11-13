//! Saving to a data directory and starting again from it (`docs/services/persistence.md`).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use azemu_core::testing::TestServer;
use common::{full_persistent, Arm, SUB};
use serde_json::{json, Value};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-arm-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn rid(rg: &str, ns: &str, rest: &str) -> String {
    format!("/subscriptions/{SUB}/resourceGroups/{rg}/providers/{ns}/{rest}")
}

fn files(data: &Path) -> Vec<(String, std::time::SystemTime, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut v: Vec<_> = std::fs::read_dir(data.join("arm"))
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            (e.file_name().to_string_lossy().into_owned(), m.modified().unwrap(), m.ino())
        })
        .collect();
    v.sort();
    v
}

fn dep(rg: &str, name: &str) -> String {
    format!("/subscriptions/{SUB}/resourcegroups/{rg}/providers/Microsoft.Resources/deployments/{name}")
}

async fn make_things(a: &Arm) {
    let r = a
        .put(
            &format!("/subscriptions/{SUB}/resourcegroups/rg1"),
            json!({"location": "westeurope", "tags": {"team": "x", "env": "dev"}, "managedBy": "me"}),
        )
        .await;
    assert!(r.status == 200 || r.status == 201, "{}", r.text());
    a.group("rg2").await;
    let r = a
        .put(
            &rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01"),
            json!({"location": "eastus", "sku": {"name": "Standard"}, "tags": {"k": "v"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .put(
            &rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/queues/orders"),
            json!({"properties": {"lockDuration": "PT30S", "maxDeliveryCount": 3}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    a.put(&rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/topics/events"), json!({"properties": {}})).await;
    a.put(
        &rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/topics/events/subscriptions/s1"),
        json!({"properties": {"maxDeliveryCount": 5}}),
    )
    .await;
    let r = a.put(&rid("rg1", "Microsoft.EventHub", "namespaces/ehns01"), json!({"location": "eastus", "sku": {"name": "Standard"}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r =
        a.put(&rid("rg1", "Microsoft.EventHub", "namespaces/ehns01/eventhubs/hub1"), json!({"properties": {"partitionCount": 3}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a
        .put(
            &rid("rg2", "Microsoft.Storage", "storageAccounts/stor1"),
            json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    // tags at the group scope and a subscription-wide tag name
    let scope = format!("/subscriptions/{SUB}/resourceGroups/rg2/providers/Microsoft.Resources/tags/default");
    assert_eq!(a.put(&scope, json!({"properties": {"tags": {"x": "1"}}})).await.status, 200);
    a.put(&format!("/subscriptions/{SUB}/tagNames/costcenter"), json!({})).await;
    // a deployment at group scope
    let tpl = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [{"type": "Microsoft.ServiceBus/namespaces/queues", "apiVersion": "2022-10-01-preview", "name": "sbns01/from-template", "properties": {}}],
        "outputs": {"n": {"type": "string", "value": "sbns01"}}
    });
    let r = a.put(&dep("rg1", "d1"), json!({"properties": {"mode": "Incremental", "template": tpl}})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

/// Everything a client can read through ARM, with the port of the server taken out (endpoints in the bodies name it).
async fn look(a: &Arm) -> Value {
    let mut out = serde_json::Map::new();
    let port = format!(":{}", a.srv.port());
    for path in [
        "/subscriptions".to_string(),
        format!("/subscriptions/{SUB}/resourcegroups"),
        format!("/subscriptions/{SUB}/resourcegroups/rg1"),
        format!("/subscriptions/{SUB}/resourcegroups/rg2"),
        format!("/subscriptions/{SUB}/resources"),
        format!("/subscriptions/{SUB}/resourcegroups/rg1/resources"),
        format!("/subscriptions/{SUB}/resourcegroups/rg2/resources"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/queues"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/queues/orders"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/queues/from-template"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/topics/events/subscriptions/s1"),
        rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/authorizationRules"),
        rid("rg1", "Microsoft.EventHub", "namespaces/ehns01/eventhubs/hub1"),
        rid("rg2", "Microsoft.Storage", "storageAccounts/stor1"),
        format!("/subscriptions/{SUB}/resourceGroups/rg2/providers/Microsoft.Resources/tags/default"),
        format!("/subscriptions/{SUB}/tagNames"),
        format!("/subscriptions/{SUB}/resourcegroups/rg1/providers/Microsoft.Resources/deployments"),
        dep("rg1", "d1"),
    ] {
        let r = a.get(&path).await;
        let text = r.text().replace(&port, ":PORT");
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        out.insert(path, json!({"status": r.status, "body": body}));
    }
    Value::Object(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn groups_resources_tags_deployments_and_the_data_plane_come_back() {
    let data = dir("all");
    let (a, _arm, _all) = full_persistent(&data).await;
    make_things(&a).await;
    let before = look(&a).await;
    assert_eq!(before[&format!("/subscriptions/{SUB}/resourcegroups/rg1")]["body"]["tags"], json!({"team": "x", "env": "dev"}));
    a.srv.shutdown().await.unwrap();

    let (b, _arm, _all) = full_persistent(&data).await;
    let after = look(&b).await;
    assert_eq!(before, after, "everything ARM shows reads back as it was");
    // the data plane came back too: the queue made through ARM is in Service Bus
    let sb = format!("http://sbnsmoment01.servicebus.localhost:{}", b.srv.port());
    let r = b.dp_send("GET", &format!("{sb}/orders?api-version=2021-05"), &[], "").await;
    assert!(r.text().contains("<MaxDeliveryCount>3</MaxDeliveryCount>"), "{}", r.text());
    // and ARM still drives it: a change through ARM reaches the data plane, a delete too
    let r = b
        .put(&rid("rg1", "Microsoft.ServiceBus", "namespaces/sbnsmoment01/queues/orders"), json!({"properties": {"maxDeliveryCount": 7}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = b.dp_send("GET", &format!("{sb}/orders?api-version=2021-05"), &[], "").await;
    assert!(r.text().contains("<MaxDeliveryCount>7</MaxDeliveryCount>"), "{}", r.text());
    assert!(matches!(b.delete(&format!("/subscriptions/{SUB}/resourcegroups/rg1")).await.status, 200 | 202));
    let r = b.dp_send("GET", &format!("{sb}/orders?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<QueueDescription"));
    b.srv.shutdown().await.unwrap();
    // the delete is saved too
    let (c, _arm, _all) = full_persistent(&data).await;
    assert_eq!(c.get(&format!("/subscriptions/{SUB}/resourcegroups/rg1")).await.status, 404);
    assert_eq!(c.get(&format!("/subscriptions/{SUB}/resourcegroups/rg2")).await.status, 200);
    c.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn arm_and_the_data_plane_saved_at_different_moments_still_agree_on_the_rule() {
    // "ARM lists what ARM created; a GET of an ARM resource whose data-plane object is gone still answers"
    let data = dir("moments");
    let (a, arm, all) = full_persistent(&data).await;
    let sb = all.iter().find(|s| s.name() == "servicebus").cloned().expect("servicebus in the stack");
    a.group("rg").await;
    let r = a
        .put(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment"), json!({"location": "eastus", "sku": {"name": "Standard"}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = a.put(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment/queues/early"), json!({"properties": {}})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the data plane is saved here ...
    std::fs::create_dir_all(data.join("servicebus")).unwrap();
    std::fs::create_dir_all(data.join("arm")).unwrap();
    sb.save(&data.join("servicebus")).unwrap();
    // ... a queue is made through ARM after that, and one is made in the data plane only ...
    a.put(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment/queues/late"), json!({"properties": {}})).await;
    let sbu = format!("http://sbnsmoment.servicebus.localhost:{}", a.srv.port());
    let r = a.dp_send("PUT", &format!("{sbu}/data-only?api-version=2021-05"), &[("Content-Type", "application/atom+xml")], "<entry xmlns=\"http://www.w3.org/2005/Atom\"><content type=\"application/xml\"><QueueDescription xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\"/></content></entry>").await;
    assert!(r.status == 201 || r.status == 200, "{}", r.text());
    // ... and ARM is saved last
    arm.save(&data.join("arm")).unwrap();
    // a second life that reads both: ARM knows early and late; the data plane has early only (and no data-only: it came later)
    drop(a);
    let (b, _arm2, _all2) = full_persistent(&data).await;
    let listed = b.get(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment/queues")).await.json();
    let names: Vec<String> =
        listed["value"].as_array().unwrap_or_else(|| panic!("{listed}")).iter().map(|q| q["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(
        names,
        vec!["early".to_string(), "late".to_string()],
        "ARM lists what ARM created: not the queue that only the data plane had"
    );
    let late = b.get(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment/queues/late")).await;
    assert_eq!(late.status, 200, "a resource whose data-plane object is gone still answers: {}", late.text());
    let sbu = format!("http://sbnsmoment.servicebus.localhost:{}", b.srv.port());
    let r = b.dp_send("GET", &format!("{sbu}/late?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<QueueDescription"), "the data plane was saved before `late` was made: {}", r.text());
    let r = b.dp_send("GET", &format!("{sbu}/data-only?api-version=2021-05"), &[], "").await;
    assert!(!r.text().contains("<QueueDescription"), "{}", r.text());
    let r = b.dp_send("GET", &format!("{sbu}/early?api-version=2021-05"), &[], "").await;
    assert!(r.text().contains("<QueueDescription"), "{}", r.text());
    // putting it again through ARM makes it again
    let r = b
        .put(&rid("rg", "Microsoft.ServiceBus", "namespaces/sbnsmoment/queues/late"), json!({"properties": {"maxDeliveryCount": 4}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = b.dp_send("GET", &format!("{sbu}/late?api-version=2021-05"), &[], "").await;
    assert!(r.text().contains("<MaxDeliveryCount>4</MaxDeliveryCount>"), "{}", r.text());
    b.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_with_nothing_changed_writes_nothing_and_a_change_writes_only_its_scope() {
    let data = dir("quiet");
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let srv = TestServer::start_persistent(vec![svc.clone()], &data).await;
    let a = Arm { srv };
    for g in ["g1", "g2"] {
        a.group(g).await;
        a.put(
            &rid(g, "Microsoft.Storage", &format!("storageAccounts/acct{g}")),
            json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}),
        )
        .await;
    }
    a.srv.save_now().await;
    let x = files(&data);
    assert_eq!(x.len(), 3, "{x:?}: definitions and two groups");
    // reads are not changes
    a.get(&format!("/subscriptions/{SUB}/resourcegroups/g1/resources")).await;
    a.srv.save_now().await;
    a.srv.save_now().await;
    assert_eq!(files(&data), x, "nothing was written");
    a.put(
        &rid("g1", "Microsoft.Storage", "storageAccounts/second"),
        json!({"location": "eastus", "kind": "StorageV2", "sku": {"name": "Standard_LRS"}}),
    )
    .await;
    a.srv.save_now().await;
    let y = files(&data);
    let changed: Vec<_> = x.iter().zip(&y).filter(|(p, q)| p != q).map(|(p, _)| p.0.clone()).collect();
    assert_eq!(changed.len(), 1, "only the scope that changed is written again: {changed:?}");
    assert!(changed[0].starts_with("scope-"));
    // a deleted group loses its file
    assert!(matches!(a.delete(&format!("/subscriptions/{SUB}/resourcegroups/g2")).await.status, 200 | 202));
    a.srv.save_now().await;
    assert_eq!(files(&data).len(), 2);
    use std::os::unix::fs::PermissionsExt;
    for (name, _, _) in files(&data) {
        let mode = std::fs::metadata(data.join("arm").join(&name)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
    a.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_is_saved_too() {
    let data = dir("reset");
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    a.group("g").await;
    a.srv.save_now().await;
    a.srv.reset();
    a.srv.save_now().await;
    a.srv.shutdown().await.unwrap();
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let b = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    assert_eq!(b.get(&format!("/subscriptions/{SUB}/resourcegroups/g")).await.status, 404);
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("scope-")).count(), 0);
    b.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_of_one_subscription_is_saved_too() {
    let data = dir("reset-sub");
    let other = "99999999-8888-7777-6666-555555555555";
    let make = |data: PathBuf| async move {
        let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
        let a = Arm { srv: TestServer::start_persistent(vec![svc.clone()], &data).await };
        (a, svc)
    };
    let (a, svc) = make(data.clone()).await;
    for sub in [SUB, other] {
        let r = a.put(&format!("/subscriptions/{sub}/resourcegroups/g"), json!({"location": "eastus"})).await;
        assert_eq!(r.status, 201);
        a.put(&format!("/subscriptions/{sub}/resourcegroups/g/providers/Microsoft.Foo/things/t"), json!({"location": "eastus"})).await;
    }
    a.srv.save_now().await;
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("scope-")).count(), 2);
    assert!(azemu_core::Service::reset_account(svc.as_ref(), SUB));
    a.srv.save_now().await;
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("scope-")).count(), 1, "the removed subscription loses its file");
    a.srv.shutdown().await.unwrap();
    let (b, _) = make(data.clone()).await;
    assert_eq!(b.get(&format!("/subscriptions/{SUB}/resourcegroups/g")).await.status, 404);
    assert_eq!(b.get(&format!("/subscriptions/{other}/resourcegroups/g/providers/Microsoft.Foo/things/t")).await.status, 200);
    b.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_does_not_hold_the_lock_while_it_encodes_and_writes() {
    let data = dir("lock");
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = std::sync::Arc::new(Arm { srv: TestServer::start_persistent(vec![svc], &data).await });
    a.group("big").await;
    let filler = "x".repeat(4000);
    for chunk in 0..30 {
        let mut set = tokio::task::JoinSet::new();
        for i in 0..100 {
            let (a, f) = (a.clone(), filler.clone());
            set.spawn(async move {
                a.put(
                    &rid("big", "Microsoft.Foo", &format!("things/t{}", chunk * 100 + i)),
                    json!({"location": "eastus", "properties": {"f": f}}),
                )
                .await
                .status
            });
        }
        while let Some(s) = set.join_next().await {
            let s = s.unwrap();
            assert!(s == 200 || s == 201, "{s}");
        }
    }
    let saver = a.clone();
    let save = tokio::spawn(async move {
        let t0 = Instant::now();
        saver.srv.save_now().await;
        t0.elapsed()
    });
    let mut worst = Duration::ZERO;
    let mut n = 0;
    while !save.is_finished() {
        let t0 = Instant::now();
        let r = a.get(&format!("/subscriptions/{SUB}/resourcegroups/big")).await;
        assert_eq!(r.status, 200);
        worst = worst.max(t0.elapsed());
        n += 1;
    }
    let took = save.await.unwrap();
    assert!(n >= 3, "only {n} requests ran during a save of {took:?}");
    assert!(worst < took / 2 || worst < Duration::from_millis(80), "a request waited {worst:?} while the save took {took:?}");
    let a = std::sync::Arc::try_unwrap(a).ok().unwrap();
    a.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

// ---- files that are not to be trusted ----

async fn start_error(data: &Path) -> String {
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    match TestServer::try_start_persistent(vec![svc], data, |b| b).await {
        Ok(_) => panic!("the emulator started on a damaged directory"),
        Err(e) => format!("{e:#}"),
    }
}

async fn make_saved(name: &str) -> PathBuf {
    let data = dir(name);
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    a.group("g").await;
    a.put(&rid("g", "Microsoft.Storage", "storageAccounts/acct"), json!({"location": "eastus"})).await;
    a.srv.shutdown().await.unwrap();
    data
}

fn scope_file(data: &Path) -> PathBuf {
    std::fs::read_dir(data.join("arm"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("scope-"))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_files_stop_the_start_with_a_message_that_names_the_file() {
    let data = make_saved("trunc").await;
    let f = scope_file(&data);
    let bytes = std::fs::read(&f).unwrap();
    std::fs::write(&f, &bytes[..bytes.len() - 5]).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains(&f.file_name().unwrap().to_string_lossy().to_string()), "{e}");
    let _ = std::fs::remove_dir_all(&data);

    for which in ["arm.state", "scope"] {
        let data = make_saved("flip").await;
        let target = if which == "scope" { scope_file(&data) } else { data.join("arm").join(which) };
        let original = std::fs::read(&target).unwrap();
        let mut errors = 0;
        for i in 0..original.len() {
            let mut b = original.clone();
            b[i] ^= 0xff;
            std::fs::write(&target, &b).unwrap();
            let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
            match TestServer::try_start_persistent(vec![svc], &data, |b| b).await {
                Ok(s) => drop(s),
                Err(e) => {
                    errors += 1;
                    assert!(format!("{e:#}").contains(".state"), "{e:#}");
                }
            }
            std::fs::write(&target, &original).unwrap();
        }
        assert!(errors > 5, "{which}: {errors} of {} flipped bytes were noticed", original.len());
        let _ = std::fs::remove_dir_all(&data);
    }

    let data = make_saved("kind").await;
    let f = scope_file(&data);
    let mut b = std::fs::read(&f).unwrap();
    std::fs::copy(data.join("arm").join("arm.state"), &f).unwrap();
    let e = start_error(&data).await;
    assert!(
        e.contains("holds arm state, expected arm-resources") && e.contains(&f.file_name().unwrap().to_string_lossy().to_string()),
        "{e}"
    );
    let i = b.iter().position(|c| *c == b'\n').unwrap();
    let header = String::from_utf8(b[..i].to_vec()).unwrap().replace(" 1", " 99");
    b.splice(..i, header.into_bytes());
    std::fs::write(&f, &b).unwrap();
    let e = start_error(&data).await;
    assert!(e.contains("version 99") && e.contains("newer azemu"), "{e}");
    std::fs::write(data.join("arm").join("arm.state"), b"hello").unwrap();
    assert!(start_error(&data).await.contains("arm.state"));
    let _ = std::fs::remove_dir_all(&data);

    let data = make_saved("hostile").await;
    let defs = data.join("arm").join("arm.state");
    let mut b = b"AZEMU-STATE arm 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa6]);
    b.extend_from_slice(b"groups");
    b.extend(std::iter::repeat_n(0x91, 100_000));
    b.push(0xc0);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("arm.state"));
    let mut b = b"AZEMU-STATE arm 1\n".to_vec();
    b.extend_from_slice(&[0x81, 0xa6]);
    b.extend_from_slice(b"groups");
    b.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
    std::fs::write(&defs, b).unwrap();
    assert!(start_error(&data).await.contains("arm.state"));
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_and_extra_files_and_the_resources_of_a_deleted_group() {
    // a scope file is missing: the group is there and empty
    let data = make_saved("missing").await;
    std::fs::remove_file(scope_file(&data)).unwrap();
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/g")).await.status, 200);
    assert_eq!(a.get(&rid("g", "Microsoft.Storage", "storageAccounts/acct")).await.status, 404);
    a.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);

    // an extra file is ignored; the resources of a group that arm.state does not list are dropped, and their file goes
    // with the next save
    let data = make_saved("extra").await;
    std::fs::write(data.join("arm").join("notes.txt"), "mine").unwrap();
    let defs = data.join("arm").join("arm.state");
    let before_other = std::fs::read(&defs).unwrap();
    {
        let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
        let a = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
        a.group("other").await;
        a.put(&rid("other", "Microsoft.Storage", "storageAccounts/other1"), json!({"location": "eastus"})).await;
        a.srv.shutdown().await.unwrap();
    }
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("scope-")).count(), 2);
    std::fs::write(&defs, before_other).unwrap();
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    assert_eq!(a.get(&format!("/subscriptions/{SUB}/resourcegroups/other")).await.status, 404);
    assert_eq!(a.get(&rid("other", "Microsoft.Storage", "storageAccounts/other1")).await.status, 404);
    assert_eq!(a.get(&rid("g", "Microsoft.Storage", "storageAccounts/acct")).await.status, 200);
    a.group("later").await;
    a.put(&rid("later", "Microsoft.Storage", "storageAccounts/later1"), json!({"location": "eastus"})).await;
    a.srv.save_now().await;
    assert_eq!(files(&data).iter().filter(|f| f.0.starts_with("scope-")).count(), 2, "the file of the group that is gone was removed");
    assert!(data.join("arm").join("notes.txt").exists());
    a.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

/// Cost of a save after one small change next to about 100 MB of other state, and of a restart. Not part of the
/// normal run (it writes 100 MB): `AZEMU_PERSIST_BENCH_DIR=<dir not on /tmp> cargo test -p azemu-arm --test persist -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn cost_of_a_small_save_next_to_100_mb() {
    let data = PathBuf::from(std::env::var("AZEMU_PERSIST_BENCH_DIR").expect("AZEMU_PERSIST_BENCH_DIR")).join("arm-bench");
    let _ = std::fs::remove_dir_all(&data);
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let a = std::sync::Arc::new(Arm { srv: TestServer::start_persistent(vec![svc], &data).await });
    a.group("big").await;
    a.group("small").await;
    let filler = "x".repeat(6000);
    for chunk in 0..160 {
        let mut set = tokio::task::JoinSet::new();
        for i in 0..100 {
            let (a, f) = (a.clone(), filler.clone());
            set.spawn(async move {
                a.put(
                    &rid("big", "Microsoft.Foo", &format!("things/t{}", chunk * 100 + i)),
                    json!({"location": "eastus", "properties": {"f": f}}),
                )
                .await
                .status
            });
        }
        while let Some(s) = set.join_next().await {
            assert!(matches!(s.unwrap(), 200 | 201));
        }
    }
    let t0 = Instant::now();
    a.srv.save_now().await;
    eprintln!("BENCH first save of 16,000 resources of 6 KB: {:?}", t0.elapsed());
    let size: u64 = std::fs::read_dir(data.join("arm")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    eprintln!("BENCH bytes on disk: {size}");
    a.put(&rid("small", "Microsoft.Foo", "things/one"), json!({"location": "eastus"})).await;
    let t0 = Instant::now();
    a.srv.save_now().await;
    eprintln!("BENCH save after one small change: {:?}", t0.elapsed());
    let a = std::sync::Arc::try_unwrap(a).ok().unwrap();
    a.srv.shutdown().await.unwrap();
    let svc = std::sync::Arc::new(azemu_arm::ArmService::with_strict(azemu_arm::Backends::none(), false));
    let t0 = Instant::now();
    let b = Arm { srv: TestServer::start_persistent(vec![svc], &data).await };
    eprintln!("BENCH restart (load) of 16,000 resources: {:?}", t0.elapsed());
    b.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}
