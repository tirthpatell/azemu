//! Storage Analytics logging (`$logs`) and the `maxresults` rules of the listings. See `analytics.rs` and
//! `docs/services/blob.md`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use common::*;

const LOGGING: &str = "<StorageServiceProperties><Logging><Version>1.0</Version><Read>{r}</Read><Write>{w}</Write><Delete>{d}</Delete><RetentionPolicy><Enabled>true</Enabled><Days>7</Days></RetentionPolicy></Logging></StorageServiceProperties>";

fn logging_xml(r: bool, w: bool, d: bool) -> String {
    LOGGING.replace("{r}", &r.to_string()).replace("{w}", &w.to_string()).replace("{d}", &d.to_string())
}

async fn set_logging(e: &Env, r: bool, w: bool, d: bool) {
    let resp = e.send("PUT", "/?restype=service&comp=properties", &[], logging_xml(r, w, d)).await;
    assert_eq!(resp.status, 202, "{}", resp.text());
}

/// One log line split into its fields: `;` separates, a quoted field may hold a `;`.
fn fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (vec![], String::new(), false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// Names of the blobs in `$logs`.
async fn log_names(e: &Env) -> Vec<String> {
    let r = e.send("GET", "/$logs?restype=container&comp=list", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    tags(&r.text(), "Name")
}

/// Every line of every log blob, in name order.
async fn all_lines(e: &Env) -> Vec<Vec<String>> {
    let mut out = vec![];
    for n in log_names(e).await {
        let r = e.send("GET", &format!("/$logs/{n}"), &[], "").await;
        assert_eq!(r.status, 200);
        for l in r.text().lines() {
            out.push(fields(l));
        }
    }
    out
}

async fn start_svc() -> (Arc<BlobService>, Env) {
    let svc = Arc::new(BlobService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let base = srv.url("blob", "acct");
    (svc, Env { srv, base })
}

// --- maxresults ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn maxresults_above_5000_is_not_an_error_and_zero_or_less_is() {
    let e = start().await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "a", "x").await;
    // List Blobs, List Containers (Microsoft Learn: more than 5,000 returns up to 5,000; zero or less is 400)
    let r = e.send("GET", "/cnt?restype=container&comp=list&maxresults=10000", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(tags(&r.text(), "Name"), ["a"]);
    assert_eq!(next_marker(&r.text()), "");
    let r = e.send("GET", "/?comp=list&maxresults=2147483647", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    for bad in ["0", "-1", "abc"] {
        let r = e.send("GET", &format!("/cnt?restype=container&comp=list&maxresults={bad}"), &[], "").await;
        assert_eq!(r.status, 400, "maxresults={bad}");
        let r = e.send("GET", &format!("/?comp=list&maxresults={bad}"), &[], "").await;
        assert_eq!(r.status, 400, "maxresults={bad}");
    }
    // Find Blobs by Tags and Get Page Ranges share the rule
    let r = e.send("GET", "/?comp=blobs&where=%22k%22%3D%27v%27&maxresults=10000", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
}

// --- $logs ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn logs_container_appears_when_logging_is_turned_on_and_is_hidden_from_a_plain_listing() {
    let e = start().await;
    // before: no such container
    assert_eq!(e.send("GET", "/$logs?restype=container&comp=list", &[], "").await.status, 404);
    set_logging(&e, false, true, false).await;
    let r = e.send("GET", "/$logs?restype=container&comp=list", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(tags(&r.text(), "Name").is_empty());
    assert_eq!(e.send("HEAD", "/$logs?restype=container", &[], "").await.status, 200);
    // a plain List Containers does not return it; include=system does
    e.create_container("cnt").await;
    let plain = e.send("GET", "/?comp=list", &[], "").await.text();
    assert_eq!(tags(&plain, "Name"), ["cnt"]);
    let sys = e.send("GET", "/?comp=list&include=system", &[], "").await.text();
    assert_eq!(tags(&sys, "Name"), ["$logs", "cnt"]);
    let r = e.send("GET", "/?comp=list&include=metadata,system&prefix=%24", &[], "").await;
    assert_eq!(tags(&r.text(), "Name"), ["$logs"]);
}

#[tokio::test]
async fn logs_container_cannot_be_deleted_once_logging_was_enabled_but_its_blobs_can() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "a", "x").await;
    svc.flush_logs_for_tests();
    let names = log_names(&e).await;
    assert_eq!(names.len(), 1);
    // the blobs can be deleted
    assert_eq!(e.send("DELETE", &format!("/$logs/{}", names[0]), &[], "").await.status, 202);
    // the container cannot, also not after logging is switched off again
    let r = e.send("DELETE", "/$logs?restype=container", &[], "").await;
    assert_eq!(r.status, 400, "{}", r.text());
    set_logging(&e, false, false, false).await;
    assert_eq!(e.send("DELETE", "/$logs?restype=container", &[], "").await.status, 400);
    assert_eq!(e.send("GET", "/$logs?restype=container&comp=list", &[], "").await.status, 200);
}

#[tokio::test]
async fn an_update_of_cors_makes_logs_but_does_not_pin_it() {
    let e = start().await;
    let cors = "<StorageServiceProperties><Cors><CorsRule><AllowedOrigins>*</AllowedOrigins><AllowedMethods>GET</AllowedMethods><MaxAgeInSeconds>10</MaxAgeInSeconds><ExposedHeaders></ExposedHeaders><AllowedHeaders></AllowedHeaders></CorsRule></Cors></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], cors).await.status, 202);
    assert_eq!(e.send("GET", "/$logs?restype=container&comp=list", &[], "").await.status, 200);
    assert_eq!(e.send("DELETE", "/$logs?restype=container", &[], "").await.status, 202);
}

#[tokio::test]
async fn static_website_makes_web_which_is_listed_like_any_container() {
    let e = start().await;
    let web = "<StorageServiceProperties><StaticWebsite><Enabled>true</Enabled><IndexDocument>index.html</IndexDocument></StaticWebsite></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], web).await.status, 202);
    let plain = e.send("GET", "/?comp=list", &[], "").await.text();
    assert_eq!(tags(&plain, "Name"), ["$web"]);
}

#[tokio::test]
async fn data_lake_filesystem_list_does_not_return_logs() {
    let svc = Arc::new(BlobService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>, Arc::new(svc.datalake())]).await;
    let (b, d) = (srv.url("blob", "acct"), srv.url("dfs", "acct"));
    let auth = [AUTH];
    let r = srv.send("PUT", &format!("{b}/?restype=service&comp=properties"), &auth, logging_xml(false, true, false)).await;
    assert_eq!(r.status, 202);
    assert_eq!(srv.send("PUT", &format!("{b}/cnt?restype=container"), &auth, "").await.status, 201);
    let r = srv.send("GET", &format!("{d}/?resource=account"), &auth, "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("\"cnt\"") && !r.text().contains("$logs"), "{}", r.text());
}

// --- the log blobs ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_write_is_logged_in_a_named_blob_with_metadata_and_a_30_field_line() {
    let (svc, e) = start_svc().await;
    set_logging(&e, false, true, false).await;
    e.create_container("cnt").await;
    let r = e.put_blob("cnt", "dir/b1.txt", "hello").await;
    assert_eq!(r.status, 201);
    // reads are not logged when only writes are switched on
    assert_eq!(e.send("GET", "/cnt/dir/b1.txt", &[], "").await.status, 200);
    assert_eq!(e.send("GET", "/cnt?restype=container&comp=list", &[], "").await.status, 200);
    assert!(log_names(&e).await.is_empty(), "nothing is written before the delay is over");
    svc.flush_logs_for_tests();

    let now = azemu_core::time::OffsetDateTime::now_utc();
    let prefix = format!("blob/{:04}/{:02}/{:02}/{:02}00/", now.year(), now.month() as u8, now.day(), now.hour());
    let list = e
        .send("GET", &format!("/$logs?restype=container&comp=list&include=metadata&prefix={}", prefix.replace('/', "%2F")), &[], "")
        .await
        .text();
    let names = tags(&list, "Name");
    // (a flush that ran just after the hour changed would name the next hour: look at both)
    let names = if names.is_empty() { log_names(&e).await } else { names };
    assert_eq!(names.len(), 1, "{list}");
    assert!(names[0].ends_with("/000000.log"), "{}", names[0]);
    assert!(list.contains("<LogType>write</LogType>"), "{list}");
    assert!(list.contains("<LogVersion>1.0</LogVersion>"), "{list}");
    assert!(list.contains("<StartTime>") && list.contains("<EndTime>"), "{list}");

    let lines = all_lines(&e).await;
    // Create Container and Put Blob are writes; Set Blob Service Properties was sent before the first flush too
    let put: Vec<_> = lines.iter().filter(|f| f[2] == "PutBlob").collect();
    assert_eq!(put.len(), 1);
    let f = put[0];
    assert_eq!(f.len(), 30, "a version 1.0 line has 30 fields: {f:?}");
    assert_eq!(f[0], "1.0");
    assert!(f[1].ends_with('Z') && f[1].len() == 28, "{}", f[1]);
    assert_eq!(f[3], "Success");
    assert_eq!(f[4], "201");
    assert_eq!(f[7], "authenticated");
    assert_eq!(f[8], "acct");
    assert_eq!(f[9], "acct");
    assert_eq!(f[10], "blob");
    assert!(f[11].starts_with('"') && f[11].contains("/cnt/dir/b1.txt"), "{}", f[11]);
    assert_eq!(f[12], "\"/acct/cnt/dir/b1.txt\"");
    assert_eq!(f[14], "0");
    assert_eq!(f[21], "5", "request content length");
    assert!(f[24].starts_with("\"&quot;0x"), "etag is quoted and encoded: {}", f[24]);
    assert!(f[25].ends_with(" GMT\""), "{}", f[25]);
    assert!(!lines.iter().any(|f| f[2] == "GetBlob" || f[2] == "ListBlobs"), "reads were not asked for");
    assert!(lines.iter().any(|f| f[2] == "CreateContainer"));
    assert!(lines.iter().any(|f| f[2] == "SetBlobServiceProperties"));
}

#[tokio::test]
async fn log_type_lists_the_classes_in_the_blob_and_each_class_needs_its_switch() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, false, true).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    e.send("GET", "/cnt/b", &[], "").await;
    e.send("DELETE", "/cnt/b", &[], "").await;
    svc.flush_logs_for_tests();
    let list = e.send("GET", "/$logs?restype=container&comp=list&include=metadata", &[], "").await.text();
    assert!(list.contains("<LogType>read,delete</LogType>"), "{list}");
    let lines = all_lines(&e).await;
    let ops: Vec<&str> = lines.iter().map(|f| f[2].as_str()).collect();
    assert!(ops.contains(&"GetBlob") && ops.contains(&"DeleteBlob"), "{ops:?}");
    assert!(!ops.contains(&"PutBlob") && !ops.contains(&"CreateContainer"), "{ops:?}");
}

#[tokio::test]
async fn a_failed_authorized_request_is_logged_with_its_error_code() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    assert_eq!(e.send("GET", "/cnt/missing", &[], "").await.status, 404);
    svc.flush_logs_for_tests();
    let lines = all_lines(&e).await;
    let miss = lines.iter().find(|f| f[2] == "GetBlob").expect("the failed get");
    assert_eq!(miss[3], "BlobNotFound");
    assert_eq!(miss[4], "404");
}

#[tokio::test]
async fn copy_blob_logs_three_lines_with_one_request_id() {
    let (svc, e) = start_svc().await;
    set_logging(&e, false, true, false).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "src", "data").await;
    let src = format!("{}/cnt/src?sv=2020-10-02&sig=SECRETSIG", e.base);
    let r = e.send("PUT", "/cnt/dst", &[("x-ms-copy-source", &src)], "").await;
    assert_eq!(r.status, 202, "{}", r.text());
    svc.flush_logs_for_tests();
    let lines = all_lines(&e).await;
    let copy: Vec<_> = lines.iter().filter(|f| f[2].starts_with("CopyBlob")).collect();
    assert_eq!(copy.iter().map(|f| f[2].as_str()).collect::<Vec<_>>(), ["CopyBlob", "CopyBlobSource", "CopyBlobDestination"]);
    assert_eq!(copy.iter().map(|f| f[14].as_str()).collect::<Vec<_>>(), ["0", "1", "2"]);
    assert!(copy.iter().all(|f| f[13] == copy[0][13]));
    assert_eq!(copy[0][12], "\"/acct/cnt/dst\"");
    assert_eq!(copy[2][12], "\"/acct/cnt/dst\"");
    assert!(copy[1][12].contains("/cnt/src?sv=2020-10-02&amp;sig=XXXXX"), "{}", copy[1][12]);
}

#[tokio::test]
async fn credentials_never_reach_the_log() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    // a SAS signature and a signature-shaped parameter in the query, and a secret in a copy source
    let r = e.send("GET", "/cnt/b?se=2030-01-01T00%3A00%3A00Z&sp=r&sig=Zm9vYmFy%2Bx%3D&SIG=another", &[], "").await;
    let _ = r.status;
    svc.flush_logs_for_tests();
    let mut text = String::new();
    for n in log_names(&e).await {
        text.push_str(&e.send("GET", &format!("/$logs/{n}"), &[], "").await.text());
    }
    assert!(!text.contains("Zm9vYmFy") && !text.contains("another"), "{text}");
    assert!(text.contains("sig=XXXXX"), "{text}");
    assert!(!text.contains("SharedKey") && !text.contains("c2ln"), "the Authorization header is never written");
}

#[tokio::test]
async fn anonymous_requests_are_logged_only_when_azure_would() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "b", "x").await;
    assert_eq!(e.send("PUT", "/cnt?restype=container&comp=acl", &[("x-ms-blob-public-access", "blob")], "").await.status, 200);
    assert_eq!(e.anon("GET", "/cnt/b", &[], "").await.status, 200);
    assert_eq!(e.anon("GET", "/cnt/nothing", &[], "").await.status, 404);
    svc.flush_logs_for_tests();
    let lines = all_lines(&e).await;
    let anon: Vec<_> = lines.iter().filter(|f| f[7] == "anonymous").collect();
    assert_eq!(anon.len(), 1, "{anon:?}");
    assert_eq!(anon[0][3], "AnonymousSuccess");
    assert_eq!(anon[0][8], "", "the requester account is empty for anonymous requests");
}

#[tokio::test]
async fn reads_of_the_logs_are_not_logged_and_a_second_flush_makes_the_next_counter() {
    let (svc, e) = start_svc().await;
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    svc.flush_logs_for_tests();
    for _ in 0..3 {
        log_names(&e).await;
    }
    svc.flush_logs_for_tests();
    assert_eq!(svc.log_queue_for_tests(), (0, 0));
    assert_eq!(log_names(&e).await.len(), 1, "reading $logs queues nothing, so there is no second blob");
    e.put_blob("cnt", "b", "x").await;
    svc.flush_logs_for_tests();
    let names = log_names(&e).await;
    assert_eq!(names.len(), 2);
    let (a, b) = (names[0].rsplit('/').next().unwrap(), names[1].rsplit('/').next().unwrap());
    // the same hour except when the hour changed during the test
    if names[0].rsplit_once('/').unwrap().0 == names[1].rsplit_once('/').unwrap().0 {
        assert_eq!((a, b), ("000000.log", "000001.log"));
    }
}

#[tokio::test]
async fn version_2_lines_have_the_eight_extra_fields() {
    let (svc, e) = start_svc().await;
    let xml = "<StorageServiceProperties><Logging><Version>2.0</Version><Read>false</Read><Write>true</Write><Delete>false</Delete><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging></StorageServiceProperties>";
    assert_eq!(e.send("PUT", "/?restype=service&comp=properties", &[], xml).await.status, 202);
    e.create_container("cnt").await;
    svc.flush_logs_for_tests();
    let lines = all_lines(&e).await;
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|f| f.len() == 38 && f[0] == "2.0"), "{lines:?}");
    let list = e.send("GET", "/$logs?restype=container&comp=list&include=metadata", &[], "").await.text();
    assert!(list.contains("<LogVersion>2.0</LogVersion>"), "{list}");
}

#[tokio::test]
async fn the_log_is_written_after_the_delay_without_anyone_asking() {
    let svc = Arc::new(BlobService::with_log_settings(Duration::from_millis(150), 2000, 64 << 20));
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let e = Env { base: srv.url("blob", "acct"), srv };
    set_logging(&e, false, true, false).await;
    e.create_container("cnt").await;
    assert!(log_names(&e).await.is_empty());
    let mut seen = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        seen = log_names(&e).await.len();
        if seen > 0 {
            break;
        }
    }
    assert_eq!(seen, 1, "the timer should have written one log blob");
}

#[tokio::test]
async fn a_batch_over_the_blob_limit_keeps_the_newest_blobs() {
    // at most 2 log blobs per account: the oldest go first
    let svc = Arc::new(BlobService::with_log_settings(Duration::from_secs(3600), 2, 64 << 20));
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let e = Env { base: srv.url("blob", "acct"), srv };
    set_logging(&e, true, true, true).await;
    e.create_container("cnt").await;
    for i in 0..4 {
        e.put_blob("cnt", &format!("b{i}"), "x").await;
        svc.flush_logs_for_tests();
    }
    let names = log_names(&e).await;
    assert_eq!(names.len(), 2, "{names:?}");
    let lines = all_lines(&e).await;
    assert!(lines.iter().any(|f| f[12].contains("/cnt/b3")), "the newest line is kept");
    assert!(!lines.iter().any(|f| f[12].contains("/cnt/b0")), "the oldest line is gone");
}

#[tokio::test]
async fn nothing_is_logged_for_an_account_with_logging_off_and_a_reset_clears_the_queue() {
    let (svc, e) = start_svc().await;
    e.create_container("cnt").await;
    assert_eq!(svc.log_queue_for_tests(), (0, 0));
    set_logging(&e, true, true, true).await;
    e.put_blob("cnt", "b", "x").await;
    assert!(svc.log_queue_for_tests().1 > 0);
    svc.reset();
    assert_eq!(svc.log_queue_for_tests(), (0, 0));
    // after a reset logging is off again
    e.create_container("cnt").await;
    assert_eq!(svc.log_queue_for_tests(), (0, 0));
}

// --- restart ----------------------------------------------------------------------------------------------------------

/// `$logs` and the logging settings are ordinary state: they come back after a restart, a log line that was still
/// waiting for its delay at the stop is written first, and `$logs` is still protected.
#[tokio::test]
async fn logs_and_the_logging_settings_survive_a_restart() {
    let d = std::env::temp_dir().join(format!("azemu-blob-logs-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let open = |dir: std::path::PathBuf| async move {
        // a long delay: only the save at the stop can write the lines
        let svc = Arc::new(BlobService::with_log_settings(Duration::from_secs(3600), 2000, 64 << 20));
        let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &dir).await;
        let e = Env { base: srv.url("blob", "acct"), srv };
        (svc, e)
    };
    let (svc, e) = open(d.clone()).await;
    set_logging(&e, false, true, false).await;
    e.create_container("cnt").await;
    e.put_blob("cnt", "kept.txt", "x").await;
    svc.flush_logs_for_tests();
    e.put_blob("cnt", "waiting.txt", "y").await; // still queued at the stop
    assert_eq!(svc.log_queue_for_tests().1, 1);
    let before = all_lines(&e).await.len();
    e.srv.shutdown().await.unwrap();

    let (svc, e) = open(d.clone()).await;
    let props = e.send("GET", "/?restype=service&comp=properties", &[], "").await.text();
    assert!(props.contains("<Write>true</Write>") && props.contains("<Read>false</Read>"), "{props}");
    let lines = all_lines(&e).await;
    assert!(lines.len() > before, "the queued line was written at the stop: {before} before, {} after", lines.len());
    assert!(lines.iter().any(|f| f[12] == "\"/acct/cnt/kept.txt\""));
    assert!(lines.iter().any(|f| f[12] == "\"/acct/cnt/waiting.txt\""));
    let sys = e.send("GET", "/?comp=list&include=system", &[], "").await.text();
    assert_eq!(tags(&sys, "Name"), ["$logs", "cnt"]);
    assert_eq!(e.send("DELETE", "/$logs?restype=container", &[], "").await.status, 400, "still protected");
    // logging goes on after the restart (the switches are rebuilt from the saved properties)
    e.put_blob("cnt", "after.txt", "z").await;
    assert!(svc.log_queue_for_tests().1 > 0);
    e.srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
