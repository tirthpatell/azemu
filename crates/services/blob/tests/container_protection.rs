//! A container cannot be deleted while a blob in it is protected by a legal hold or by an immutability policy that has
//! not run out (docs/services/blob.md, "Deleting a container that holds protected blobs").

mod common;

use azemu_blob::BlobService;
use azemu_core::Service;
use common::*;

const FAR: &str = "Thu, 04 Apr 2075 00:00:00 GMT";

async fn delete(e: &Env, container: &str) -> (u16, String) {
    let r = e.send("DELETE", &format!("/{container}?restype=container"), &[], "").await;
    (r.status, code(&r))
}

async fn hold(e: &Env, path: &str, on: bool) {
    let r = e.send("PUT", &format!("{path}?comp=legalhold"), &[("x-ms-legal-hold", if on { "true" } else { "false" })], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
}

async fn policy(e: &Env, path: &str, until: &str, mode: &str) {
    let r = e
        .send(
            "PUT",
            &format!("{path}?comp=immutabilityPolicies"),
            &[("x-ms-immutability-policy-until-date", until), ("x-ms-immutability-policy-mode", mode)],
            "",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn a_legal_hold_stops_the_delete_until_it_is_cleared() {
    let e = start().await;
    e.create_container("held").await;
    e.put_blob("held", "a", "x").await;
    hold(&e, "/held/a", true).await;
    assert_eq!(delete(&e, "held").await, (409, "ContainerHasLegalHold".into()));
    // nothing was deleted
    assert_eq!(e.send("GET", "/held/a", &[], "").await.text(), "x");
    hold(&e, "/held/a", false).await;
    assert_eq!(delete(&e, "held").await.0, 202);
    assert_eq!(e.send("GET", "/held/a", &[], "").await.status, 404);
}

#[tokio::test]
async fn a_locked_policy_stops_the_delete_and_an_unlocked_one_too() {
    let e = start().await;
    e.create_container("locked").await;
    e.put_blob("locked", "a", "x").await;
    policy(&e, "/locked/a", FAR, "locked").await;
    assert_eq!(delete(&e, "locked").await, (409, "ContainerImmutabilityPolicyLocked".into()));

    e.create_container("unlocked").await;
    e.put_blob("unlocked", "a", "x").await;
    policy(&e, "/unlocked/a", FAR, "unlocked").await;
    assert_eq!(delete(&e, "unlocked").await, (409, "BlobImmutableDueToPolicy".into()));
    // an unlocked policy can be removed, then the container goes
    let r = e.send("DELETE", "/unlocked/a?comp=immutabilityPolicies", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(delete(&e, "unlocked").await.0, 202);
}

#[tokio::test]
async fn a_policy_that_has_run_out_does_not_protect() {
    let e = start().await;
    e.create_container("old").await;
    e.put_blob("old", "a", "x").await;
    // a date in the past is accepted by the service; the protection is over at once
    policy(&e, "/old/a", "Thu, 04 Apr 2019 00:00:00 GMT", "unlocked").await;
    assert_eq!(delete(&e, "old").await.0, 202);
}

#[tokio::test]
async fn a_hold_on_a_snapshot_or_a_version_counts_and_other_containers_are_not_affected() {
    let e = start_versioned().await;
    e.create_container("snap").await;
    e.put_blob("snap", "a", "x").await;
    let r = e.send("PUT", "/snap/a?comp=snapshot", &[], "").await;
    let snapshot = r.header("x-ms-snapshot").unwrap().to_string();
    e.send("PUT", &format!("/snap/a?comp=legalhold&snapshot={snapshot}"), &[("x-ms-legal-hold", "true")], "").await;
    // the base blob has no hold, the snapshot has
    assert_eq!(delete(&e, "snap").await, (409, "ContainerHasLegalHold".into()));
    // a second container without any hold is deleted normally
    e.create_container("free").await;
    e.put_blob("free", "b", "y").await;
    assert_eq!(delete(&e, "free").await.0, 202);
}

#[tokio::test]
async fn a_blob_that_is_already_deleted_does_not_count() {
    let e = start_with(BlobService::new()).await;
    e.create_container("gone").await;
    e.put_blob("gone", "a", "x").await;
    assert_eq!(e.send("DELETE", "/gone/a", &[], "").await.status, 202);
    assert_eq!(delete(&e, "gone").await.0, 202);
}

#[tokio::test]
async fn the_data_lake_file_system_delete_is_refused_the_same_way() {
    let svc = std::sync::Arc::new(BlobService::new());
    let lake = svc.datalake();
    let srv = azemu_core::testing::TestServer::start(vec![svc.clone() as std::sync::Arc<dyn Service>, std::sync::Arc::new(lake)]).await;
    let (blob, dfs) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    let a = [AUTH];
    assert_eq!(srv.send("PUT", &format!("{dfs}/lake?resource=filesystem"), &a, "").await.status, 201);
    assert_eq!(srv.send("PUT", &format!("{dfs}/lake/f?resource=file"), &a, "").await.status, 201);
    let r = srv.send("PUT", &format!("{blob}/lake/f?comp=legalhold"), &[AUTH, ("x-ms-legal-hold", "true")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = srv.send("DELETE", &format!("{dfs}/lake?resource=filesystem"), &a, "").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (409, Some("ContainerHasLegalHold")), "{}", r.text());
    srv.send("PUT", &format!("{blob}/lake/f?comp=legalhold"), &[AUTH, ("x-ms-legal-hold", "false")], "").await;
    let r = srv.send("DELETE", &format!("{dfs}/lake?resource=filesystem"), &a, "").await;
    assert_eq!(r.status, 202, "{}", r.text());
}
