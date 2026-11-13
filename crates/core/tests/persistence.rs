//! The `--data-dir` driver: load at start, timed saves, save at shutdown, `POST /_azemu/save`, the lock, unreadable data.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::resp::Resp;
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, persist, Mount, Request, Response, Service};
use parking_lot::Mutex;

/// A key-value service that keeps its map across restarts. `PUT /k` (body = value), `GET /k`.
struct Store {
    name: &'static str,
    map: Mutex<BTreeMap<String, String>>,
    generation: AtomicU64,
    saves: AtomicUsize,
    loads: AtomicUsize,
    fail_save: AtomicBool,
    fail_load: AtomicBool,
}

impl Store {
    fn new(name: &'static str) -> Arc<Store> {
        Arc::new(Store {
            name,
            map: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(1),
            saves: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            fail_save: AtomicBool::new(false),
            fail_load: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl Service for Store {
    fn name(&self) -> &'static str {
        self.name
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost(self.name)]
    }
    async fn handle(&self, req: Request) -> Response {
        let key = req.path.trim_start_matches('/').to_string();
        match req.method.as_str() {
            "PUT" => {
                if key == "slow" {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                self.map.lock().insert(key, String::from_utf8_lossy(&req.body).into_owned());
                self.generation.fetch_add(1, SeqCst);
                Resp::new(201u16).build()
            }
            _ => match self.map.lock().get(&key) {
                Some(v) => Resp::new(200u16).body(v.clone()).build(),
                None => Resp::new(404u16).build(),
            },
        }
    }
    fn reset(&self) {
        self.map.lock().clear();
        self.generation.fetch_add(1, SeqCst);
    }
    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(SeqCst))
    }
    fn save(&self, dir: &Path) -> std::io::Result<()> {
        if self.fail_save.load(SeqCst) {
            return Err(std::io::Error::other("disk full (test)"));
        }
        let copy = self.map.lock().clone();
        self.saves.fetch_add(1, SeqCst);
        persist::save_file(dir, "store.state", "store", 1, &copy)
    }
    fn load(&self, dir: &Path) -> std::io::Result<()> {
        self.loads.fetch_add(1, SeqCst);
        if let Some((_, m)) = persist::load_file::<BTreeMap<String, String>>(dir, "store.state", "store", 1)? {
            *self.map.lock() = m;
        }
        if self.fail_load.load(SeqCst) {
            return Err(std::io::Error::other("load failed (test)"));
        }
        Ok(())
    }
}

/// Keeps nothing (the defaults).
struct Plain;

#[async_trait]
impl Service for Plain {
    fn name(&self) -> &'static str {
        "plain"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("plain")]
    }
    async fn handle(&self, _req: Request) -> Response {
        Resp::new(200u16).build()
    }
    fn reset(&self) {}
}

fn dir(tag: &str) -> PathBuf {
    common::temp_dir(&format!("persist-{tag}"))
}

fn svcs(list: &[&Arc<Store>]) -> Vec<Arc<dyn Service>> {
    list.iter().map(|s| (*s).clone() as Arc<dyn Service>).collect()
}

async fn put(srv: &TestServer, svc: &str, key: &str, value: &str) {
    let r = srv.send("PUT", &format!("{}/{key}", srv.url(svc, "acct")), &[], value.to_string()).await;
    assert_eq!(r.status, 201);
}

async fn get(srv: &TestServer, svc: &str, key: &str) -> Option<String> {
    let r = srv.send("GET", &format!("{}/{key}", srv.url(svc, "acct")), &[], "").await;
    (r.status == 200).then(|| r.text())
}

async fn post(srv: &TestServer, path: &str) -> azemu_core::testing::TestResponse {
    srv.send("POST", &format!("{}{path}", srv.gateway_url()), &[], "").await
}

#[tokio::test]
async fn state_is_loaded_at_start_and_saved_at_shutdown() {
    let d = dir("shutdown");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "one", "1").await;
    put(&srv, "kv", "two", "2").await;
    assert_eq!(a.saves.load(SeqCst), 0, "nothing is written before it is asked for (interval 0)");
    srv.shutdown().await.unwrap();
    assert_eq!(a.saves.load(SeqCst), 1);

    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(b.loads.load(SeqCst), 1);
    assert_eq!(get(&srv, "kv", "one").await.as_deref(), Some("1"));
    assert_eq!(get(&srv, "kv", "two").await.as_deref(), Some("2"));
    // nothing changed: shutdown writes nothing
    srv.shutdown().await.unwrap();
    assert_eq!(b.saves.load(SeqCst), 0);
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn dropping_the_server_does_not_save_but_releases_the_directory() {
    let d = dir("drop");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "lost", "x").await;
    drop(srv);
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(get(&srv, "kv", "lost").await, None);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn timed_saves_happen_only_when_something_changed() {
    let d = dir("timed");
    let a = Store::new("kv");
    let srv = TestServer::try_start_persistent(svcs(&[&a]), &d, |b| b.save_interval(Duration::from_millis(100))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(a.saves.load(SeqCst), 0, "an unchanged service is never saved");
    put(&srv, "kv", "k", "v").await;
    let mut waited = 0;
    while a.saves.load(SeqCst) == 0 && waited < 50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    assert_eq!(a.saves.load(SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(a.saves.load(SeqCst), 1, "no change, no second save");
    put(&srv, "kv", "k", "w").await;
    let mut waited = 0;
    while a.saves.load(SeqCst) < 2 && waited < 50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    assert_eq!(a.saves.load(SeqCst), 2);
    srv.shutdown().await.unwrap();
    assert_eq!(a.saves.load(SeqCst), 2, "shutdown has nothing left to save");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn post_save_answers_when_it_is_on_disk_and_says_what_it_did() {
    let d = dir("save");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "k", "v").await;
    let r = post(&srv, "/_azemu/save").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["services"]["kv"]["result"], "saved");
    assert!(d.join("kv").join("store.state").exists(), "on disk when the answer arrives");
    let r = post(&srv, "/_azemu/save").await;
    assert_eq!(r.json()["services"]["kv"]["result"], "unchanged");
    assert_eq!(srv.send("GET", &format!("{}/_azemu/save", srv.gateway_url()), &[], "").await.status, 405);
    drop(srv); // like a crash
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(get(&srv, "kv", "k").await.as_deref(), Some("v"));
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn save_without_a_data_directory_is_a_conflict_and_health_has_no_persistence_object() {
    let a = Store::new("kv");
    let srv = TestServer::start(svcs(&[&a])).await;
    let r = post(&srv, "/_azemu/save").await;
    assert_eq!(r.status, 409);
    assert!(r.text().contains("NoDataDirectory"));
    let h = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await.json();
    assert!(h.get("persistence").is_none(), "{h}");
}

#[tokio::test]
async fn reset_empties_state_and_the_next_save_writes_the_empty_state() {
    let d = dir("reset");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "k", "v").await;
    post(&srv, "/_azemu/save").await;
    // reset, then a plain save
    assert_eq!(post(&srv, "/_azemu/reset").await.status, 200);
    let h = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await.json();
    assert_eq!(h["persistence"]["unsavedChanges"], true);
    assert_eq!(post(&srv, "/_azemu/save").await.json()["services"]["kv"]["result"], "saved");
    put(&srv, "kv", "k2", "v2").await;
    // reset with ?save=true
    let r = post(&srv, "/_azemu/reset?save=true").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["services"]["kv"]["result"], "saved");
    drop(srv);
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(get(&srv, "kv", "k").await, None);
    assert_eq!(get(&srv, "kv", "k2").await, None);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_failed_save_is_reported_kept_unsaved_and_retried() {
    let d = dir("failed");
    let a = Store::new("kv");
    let srv = TestServer::try_start_persistent(svcs(&[&a]), &d, |b| b.save_interval(Duration::from_millis(100))).await.unwrap();
    a.fail_save.store(true, SeqCst);
    put(&srv, "kv", "k", "v").await;
    let r = post(&srv, "/_azemu/save").await;
    assert_eq!(r.status, 500);
    assert_eq!(r.json()["services"]["kv"]["result"], "error");
    assert!(r.text().contains("disk full (test)"));
    let h = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await.json();
    let kv = &h["persistence"]["services"]["kv"];
    assert_eq!(kv["lastResult"], "error");
    assert_eq!(kv["unsavedChanges"], true);
    assert!(kv["error"].as_str().unwrap().contains("disk full"));
    assert_eq!(h["persistence"]["unsavedChanges"], true);
    // the disk recovers; the timed save retries without any new change
    a.fail_save.store(false, SeqCst);
    let mut waited = 0;
    while a.saves.load(SeqCst) == 0 && waited < 50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    assert_eq!(a.saves.load(SeqCst), 1);
    let h = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await.json();
    let kv = &h["persistence"]["services"]["kv"];
    assert_eq!(kv["lastResult"], "ok");
    assert_eq!(kv["unsavedChanges"], false);
    assert!(kv["lastSave"].is_string());
    // and a save that fails at shutdown is an error the caller can see
    a.fail_save.store(true, SeqCst);
    put(&srv, "kv", "k", "w").await;
    let e = srv.shutdown().await.unwrap_err().to_string();
    assert!(e.contains("kv") && e.contains("disk full"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_second_emulator_on_the_same_directory_is_refused_and_a_stale_lock_is_taken_over() {
    let d = dir("lock");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    let pid = std::process::id().to_string();
    assert_eq!(std::fs::read_to_string(d.join("LOCK")).unwrap().trim(), pid);
    let b = Store::new("kv");
    let err = TestServer::try_start_persistent(svcs(&[&b]), &d, |b| b).await.err().expect("must be refused").to_string();
    assert!(err.contains("in use by another running azemu") && err.contains(&pid), "{err}");
    assert_eq!(b.loads.load(SeqCst), 0, "a refused start does not read anything");
    srv.shutdown().await.unwrap();
    // the file stays behind after a clean stop; a later start takes it over
    assert!(d.join("LOCK").exists());
    std::fs::write(d.join("LOCK"), "2147483000\n").unwrap(); // a process that is long gone
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(std::fs::read_to_string(d.join("LOCK")).unwrap().trim(), pid);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_directory_that_cannot_be_written_stops_the_start_with_a_clear_message() {
    let d = dir("unwritable");
    let file = d.join("a-file");
    std::fs::write(&file, "x").unwrap();
    let a = Store::new("kv");
    let err = TestServer::try_start_persistent(svcs(&[&a]), &file.join("inside"), |b| b).await.err().expect("must fail").to_string();
    assert!(err.contains("cannot create the data directory") && err.contains("a-file"), "{err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn unreadable_state_stops_the_start_naming_the_service_the_file_and_the_way_out() {
    let d = dir("unreadable");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "k", "v").await;
    srv.shutdown().await.unwrap();
    let file = d.join("kv").join("store.state");
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.truncate(bytes.len() - 2);
    std::fs::write(&file, bytes).unwrap();

    let b = Store::new("kv");
    let err = TestServer::try_start_persistent(svcs(&[&b]), &d, |b| b).await.err().expect("must fail").to_string();
    assert!(err.contains("kv") && err.contains("store.state") && err.contains("--data-discard-unreadable"), "{err}");

    // with the flag: moved aside, the service starts empty (a half-loaded service is emptied too)
    let c = Store::new("kv");
    c.fail_load.store(true, SeqCst);
    let c2 = c.clone();
    let srv = TestServer::try_start_persistent(svcs(&[&c]), &d, |b| b.discard_unreadable_data(true)).await.unwrap();
    assert!(c2.map.lock().is_empty());
    assert_eq!(get(&srv, "kv", "k").await, None);
    let aside: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("kv.unreadable-"))
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert!(d.join(&aside[0]).join("store.state").exists(), "the unreadable files are kept, not deleted");
    let h = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await.json();
    assert!(h["persistence"]["services"]["kv"]["unreadableStateMovedTo"].as_str().unwrap().contains("kv.unreadable-"));
    // an empty, just started service is not written again; new state is
    put(&srv, "kv", "n", "1").await;
    srv.shutdown().await.unwrap();
    let e = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&e]), &d).await;
    assert_eq!(get(&srv, "kv", "n").await.as_deref(), Some("1"));
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn two_services_get_their_own_directories_and_a_service_without_state_gets_none() {
    let d = dir("two");
    let (a, b) = (Store::new("alpha"), Store::new("beta"));
    let mut list = svcs(&[&a, &b]);
    list.push(Arc::new(Plain));
    let srv = TestServer::start_persistent(list, &d).await;
    put(&srv, "alpha", "k", "from-alpha").await;
    let r = post(&srv, "/_azemu/save").await.json();
    assert_eq!(r["services"]["alpha"]["result"], "saved");
    assert_eq!(r["services"]["beta"]["result"], "unchanged");
    assert!(r["services"].get("plain").is_none());
    srv.shutdown().await.unwrap();
    assert!(d.join("alpha").join("store.state").exists());
    assert!(d.join("beta").is_dir() && !d.join("beta").join("store.state").exists());
    assert!(!d.join("plain").exists());
    assert_eq!((a.saves.load(SeqCst), b.saves.load(SeqCst)), (1, 0));

    let (a2, b2) = (Store::new("alpha"), Store::new("beta"));
    let srv = TestServer::start_persistent(svcs(&[&a2, &b2]), &d).await;
    assert_eq!(get(&srv, "alpha", "k").await.as_deref(), Some("from-alpha"));
    assert_eq!(get(&srv, "beta", "k").await, None);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn shutdown_lets_a_request_in_progress_finish_and_saves_its_result() {
    let d = dir("inflight");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    let client = srv.client().clone();
    let url = format!("{}/slow", srv.url("kv", "acct"));
    let port = srv.port();
    let req = tokio::spawn(async move {
        let _ = port;
        client.send("PUT", &url, &[], "late").await.status
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    srv.shutdown().await.unwrap();
    assert_eq!(req.await.unwrap(), 201);
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert_eq!(get(&srv, "kv", "slow").await.as_deref(), Some("late"));
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn manifest_names_the_writer_the_services_and_their_file_versions_and_the_directory_is_private() {
    let d = dir("manifest");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "k", "v").await;
    srv.shutdown().await.unwrap();
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("MANIFEST")).unwrap()).unwrap();
    assert_eq!(m["layout"], 1);
    assert_eq!(m["created_by"], env!("CARGO_PKG_VERSION"));
    assert_eq!(m["services"]["kv"]["files"]["store.state"], serde_json::json!({"kind": "store", "version": 1}));
    assert_eq!(m["services"]["kv"]["saved_by"], env!("CARGO_PKG_VERSION"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(d.join("kv")).unwrap().permissions().mode() & 0o777, 0o700);
    }
    // a directory written by a newer layout is refused
    let mut newer = m.clone();
    newer["layout"] = 2.into();
    std::fs::write(d.join("MANIFEST"), serde_json::to_vec(&newer).unwrap()).unwrap();
    let b = Store::new("kv");
    let err = TestServer::try_start_persistent(svcs(&[&b]), &d, |b| b).await.err().expect("must fail").to_string();
    assert!(err.contains("MANIFEST") && err.contains("newer azemu"), "{err}");
    // a damaged MANIFEST is replaced
    std::fs::write(d.join("MANIFEST"), "not json").unwrap();
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    srv.shutdown().await.unwrap();
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("MANIFEST")).unwrap()).unwrap();
    assert_eq!(m["layout"], 1);
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn stale_temporary_files_from_a_crashed_save_are_removed_at_start() {
    let d = dir("tmpfiles");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    srv.shutdown().await.unwrap();
    std::fs::write(d.join("kv").join(".store.state.tmp-99999"), "half").unwrap();
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert!(!d.join("kv").join(".store.state.tmp-99999").exists());
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn a_manifest_temporary_file_of_a_process_that_is_gone_is_removed_from_the_data_directory_itself() {
    let d = dir("roottmp");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    srv.shutdown().await.unwrap();
    // left by a kill in the middle of a save; the pid is one nobody has (the biggest a Linux pid can be is 4194304)
    std::fs::write(d.join(".MANIFEST.tmp-2147483000"), "half").unwrap();
    // a file that only looks alike stays: no dot first, or no pid after it
    std::fs::write(d.join("note.tmp-2147483000"), "mine").unwrap();
    std::fs::write(d.join(".MANIFEST.tmp-old"), "mine").unwrap();
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert!(!d.join(".MANIFEST.tmp-2147483000").exists());
    assert!(d.join("note.tmp-2147483000").exists() && d.join(".MANIFEST.tmp-old").exists());
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_temporary_file_of_a_process_that_still_runs_is_left_alone() {
    let d = dir("roottmp-live");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    srv.shutdown().await.unwrap();
    // pid 1 always runs
    let live = d.join(".MANIFEST.tmp-1");
    std::fs::write(&live, "being written").unwrap();
    let b = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&b]), &d).await;
    assert!(live.exists());
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn save_and_reset_with_save_go_through_the_cross_site_and_host_checks() {
    let d = dir("access");
    let a = Store::new("kv");
    let srv = TestServer::start_persistent(svcs(&[&a]), &d).await;
    put(&srv, "kv", "k", "v").await;
    let base = srv.gateway_url();
    for (method, path) in [("POST", "/_azemu/save"), ("POST", "/_azemu/reset?save=true")] {
        // a page on another site
        let r =
            srv.send(method, &format!("{base}{path}"), &[("Origin", "https://evil.example"), ("Sec-Fetch-Site", "cross-site")], "").await;
        assert_eq!(r.status, 403, "{path}: {}", r.text());
        assert!(r.text().contains("CrossSiteRequestRefused"));
        // a name the emulator does not serve (DNS rebinding)
        let r = srv.send(method, &format!("http://evil.example:{}{path}", srv.port()), &[], "").await;
        assert_ne!(r.status, 200, "{path}");
    }
    assert_eq!(a.saves.load(SeqCst), 0, "nothing was saved");
    assert_eq!(get(&srv, "kv", "k").await.as_deref(), Some("v"), "nothing was reset");
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
