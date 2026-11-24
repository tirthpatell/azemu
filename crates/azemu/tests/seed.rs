//! The seed file (`--seed`, `docs/seed.md`) against the real services: what exists at start, after a reset and a partial
//! reset, what happens to things that already exist, and what the refusals say. The last tests run the program itself.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use azemu_core::serde_json;
use azemu_core::testing::{TestResponse, TestServer};
use azemu_core::{async_trait, Emulator, Mount, Request, Response, Service};

const SEED: &str = r#"{
  "blob": {"acct": {"containers": ["uploads", {"name": "public-files", "publicAccess": "blob", "metadata": {"team": "red"}}]}},
  "queue": {"acct": {"queues": ["jobs"]}},
  "table": {"acct": {"tables": ["Orders"]}},
  "cosmos": {"acct": {"databases": [{"id": "app", "containers": [
    {"id": "items", "partitionKey": "/tenantId", "defaultTtl": -1, "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/email"]}]}},
    {"id": "events", "partitionKey": ["/tenantId", "/userId"]}]}]}},
  "keyvault": {"vault1": {"secrets": {"db-password": "first"}}},
  "appconfig": {"store1": {"keyValues": [
    {"key": "App:Title", "value": "Hello"},
    {"key": "App:Title", "label": "dev", "value": "Hello dev", "contentType": "text/plain"}]}}
}"#;

fn scratch(what: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("azemu-seed-test-{what}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn stack() -> azemu::Stack {
    azemu::full_stack()
}

async fn start(seed: &Path) -> TestServer {
    let azemu::Stack { services, raw } = stack();
    let seed = seed.to_path_buf();
    TestServer::start_configured(services, raw, move |b| b.seed_file(seed)).await
}

/// One request with a bearer token (every service accepts any).
async fn call(srv: &TestServer, method: &str, label: &str, account: &str, path: &str, headers: &[(&str, &str)]) -> TestResponse {
    let mut r = srv.request(method, &format!("{}{path}", srv.url(label, account))).header("authorization", "Bearer test");
    for (k, v) in headers {
        r = r.header(k, v);
    }
    r.send().await
}

async fn blob_container(srv: &TestServer, name: &str) -> TestResponse {
    call(srv, "GET", "blob", "acct", &format!("/{name}?restype=container"), &[]).await
}

async fn queue(srv: &TestServer, name: &str) -> u16 {
    call(srv, "GET", "queue", "acct", &format!("/{name}?comp=metadata"), &[]).await.status
}

async fn table_names(srv: &TestServer) -> Vec<String> {
    let r = call(srv, "GET", "table", "acct", "/Tables", &[("accept", "application/json;odata=nometadata")]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["value"].as_array().unwrap().iter().map(|t| t["TableName"].as_str().unwrap().to_string()).collect()
}

async fn cosmos(srv: &TestServer, path: &str) -> TestResponse {
    call(srv, "GET", "documents", "acct", path, &[("x-ms-version", "2018-12-31"), ("authorization", "type=master&ver=1.0&sig=x")]).await
}

async fn secret_versions(srv: &TestServer, name: &str) -> usize {
    let r = call(srv, "GET", "vault", "vault1", &format!("/secrets/{name}/versions?api-version=7.5"), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["value"].as_array().unwrap().len()
}

async fn secret_value(srv: &TestServer, name: &str) -> Option<String> {
    let r = call(srv, "GET", "vault", "vault1", &format!("/secrets/{name}?api-version=7.5"), &[]).await;
    (r.status == 200).then(|| r.json()["value"].as_str().unwrap().to_string())
}

async fn key_value(srv: &TestServer, key: &str, label: Option<&str>) -> Option<serde_json::Value> {
    let l = label.map(|l| format!("&label={l}")).unwrap_or_default();
    let r = call(srv, "GET", "azconfig", "store1", &format!("/kv/{key}?api-version=2023-11-01{l}"), &[]).await;
    (r.status == 200).then(|| r.json())
}

/// Everything in `SEED` is there.
async fn assert_all_there(srv: &TestServer) {
    let r = blob_container(srv, "uploads").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-ms-blob-public-access"), None);
    let r = blob_container(srv, "public-files").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-blob-public-access"), Some("blob"));
    assert_eq!(r.header("x-ms-meta-team"), Some("red"));
    assert_eq!(queue(srv, "jobs").await, 200);
    assert_eq!(table_names(srv).await, ["Orders"]);
    let r = cosmos(srv, "/dbs/app/colls/items").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let c = r.json();
    assert_eq!(c["partitionKey"]["paths"], serde_json::json!(["/tenantId"]));
    assert_eq!(c["defaultTtl"], -1);
    assert_eq!(c["uniqueKeyPolicy"]["uniqueKeys"][0]["paths"][0], "/email");
    let c = cosmos(srv, "/dbs/app/colls/events").await.json();
    assert_eq!((c["partitionKey"]["kind"].as_str(), c["partitionKey"]["paths"].as_array().map(Vec::len)), (Some("MultiHash"), Some(2)));
    assert_eq!(secret_value(srv, "db-password").await.as_deref(), Some("first"));
    assert_eq!(key_value(srv, "App:Title", None).await.unwrap()["value"], "Hello");
    let dev = key_value(srv, "App:Title", Some("dev")).await.unwrap();
    assert_eq!((dev["value"].as_str(), dev["content_type"].as_str()), (Some("Hello dev"), Some("text/plain")));
}

async fn reset(srv: &TestServer, query: &str) -> serde_json::Value {
    let r = srv.request("POST", &format!("{}/_azemu/reset{query}", srv.gateway_url())).send().await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

#[tokio::test]
async fn everything_in_the_file_exists_at_start_and_a_client_can_use_it() {
    let dir = scratch("start");
    let srv = start(&write(&dir, "seed.json", SEED)).await;
    assert_all_there(&srv).await;
    // the container is a real container: a client can put a blob in it and read it back
    let r = call(&srv, "PUT", "blob", "acct", "/uploads/a.txt", &[("x-ms-blob-type", "BlockBlob")]).await;
    assert_eq!(r.status, 201, "{}", r.text());
    // and a client that creates one of the same names is told it exists, as in Azure
    let r = call(&srv, "PUT", "blob", "acct", "/uploads?restype=container", &[]).await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (409, Some("ContainerAlreadyExists")));
    // the config shows the file in force, as an absolute path
    let cfg = srv.request("GET", &format!("{}/_azemu/config", srv.gateway_url())).send().await.json();
    let shown = cfg["seedFile"].as_str().unwrap();
    assert!(Path::new(shown).is_absolute() && shown.ends_with("seed.json"), "{shown}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The seed file is the emulator's own call into its services: strict mode does not stand in its way, at the start and after a reset.
#[tokio::test]
async fn the_seed_file_works_in_strict_mode() {
    let dir = scratch("strict");
    let seed = write(&dir, "seed.json", SEED);
    let azemu::Stack { services, raw } = azemu::full_stack_with(azemu_core::auth::Auth::strict());
    let srv = TestServer::start_configured(services, raw, move |b| b.seed_file(seed)).await;
    let listed = |what: &'static str, path: &'static str| {
        let srv = &srv;
        async move {
            let r = srv.request("GET", &format!("{}/_azemu/inspect/{path}", srv.gateway_url())).send().await;
            assert_eq!(r.status, 200, "{what}: {}", r.text());
            assert!(r.text().contains(what), "{what} is not in {path}: {}", r.text());
        }
    };
    for (what, path) in [("uploads", "blob/acct/containers"), ("jobs", "queue/acct/queues"), ("Orders", "table/acct/tables")] {
        listed(what, path).await;
    }
    // a client with a bearer token of the same text is not the seed file
    let r = call(
        &srv,
        "GET",
        "blob",
        "acct",
        "/uploads?restype=container",
        &[("authorization", "Bearer azemu-seed"), ("x-ms-version", "2024-11-04")],
    )
    .await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(reset(&srv, "").await, serde_json::json!({}));
    for (what, path) in [("uploads", "blob/acct/containers"), ("jobs", "queue/acct/queues"), ("Orders", "table/acct/tables")] {
        listed(what, path).await;
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_reset_puts_everything_back_and_removes_what_a_test_added() {
    let dir = scratch("reset");
    let srv = start(&write(&dir, "seed.json", SEED)).await;
    assert_eq!(call(&srv, "PUT", "blob", "acct", "/extra?restype=container", &[]).await.status, 201);
    assert_eq!(call(&srv, "DELETE", "queue", "acct", "/jobs", &[]).await.status, 204);
    assert_eq!(reset(&srv, "").await, serde_json::json!({}));
    assert_all_there(&srv).await;
    assert_eq!(blob_container(&srv, "extra").await.status, 404);
    // a secret set twice has two versions, and after a reset it has the one the seed made
    let set = srv
        .request("PUT", &format!("{}/secrets/db-password?api-version=7.5", srv.url("vault", "vault1")))
        .header("authorization", "Bearer t")
        .json(&serde_json::json!({"value": "second"}))
        .send()
        .await;
    assert_eq!(set.status, 200);
    assert_eq!(secret_versions(&srv, "db-password").await, 2);
    reset(&srv, "").await;
    assert_eq!(secret_versions(&srv, "db-password").await, 1);
    // the embedding call does the same as the admin call
    assert_eq!(call(&srv, "DELETE", "blob", "acct", "/uploads?restype=container", &[]).await.status, 202);
    srv.reset();
    assert_eq!(blob_container(&srv, "uploads").await.status, 200);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_partial_reset_puts_back_only_what_it_removed() {
    let dir = scratch("partial");
    let srv = start(&write(&dir, "seed.json", SEED)).await;
    let lose_blob_and_queue = || async {
        assert_eq!(call(&srv, "DELETE", "blob", "acct", "/uploads?restype=container", &[]).await.status, 202);
        assert_eq!(call(&srv, "DELETE", "queue", "acct", "/jobs", &[]).await.status, 204);
    };
    // one service: its things come back, the other service's stay missing
    lose_blob_and_queue().await;
    assert_eq!(reset(&srv, "?service=queue").await["reset"], serde_json::json!(["queue"]));
    assert_eq!((queue(&srv, "jobs").await, blob_container(&srv, "uploads").await.status), (200, 404));
    // the Data Lake service shares the blob state, so its reset empties the containers and the seed fills them again
    reset(&srv, "?service=dfs").await;
    assert_eq!(blob_container(&srv, "uploads").await.status, 200);
    // one account: only that account's things; another account's containers made by a test are untouched
    assert_eq!(call(&srv, "PUT", "blob", "other", "/mine?restype=container", &[]).await.status, 201);
    lose_blob_and_queue().await;
    reset(&srv, "?account=acct&service=blob").await;
    assert_eq!((queue(&srv, "jobs").await, blob_container(&srv, "uploads").await.status), (404, 200));
    assert_eq!(call(&srv, "GET", "blob", "other", "/mine?restype=container", &[]).await.status, 200);
    reset(&srv, "?account=other&service=blob").await;
    assert_eq!(call(&srv, "GET", "blob", "other", "/mine?restype=container", &[]).await.status, 404);
    assert_eq!(blob_container(&srv, "public-files").await.status, 200, "the account that was not reset keeps what it has");
    // a vault and a store are accounts too
    assert_eq!(call(&srv, "DELETE", "vault", "vault1", "/secrets/db-password?api-version=7.5", &[]).await.status, 200);
    reset(&srv, "?account=vault1").await;
    assert_eq!(secret_value(&srv, "db-password").await.as_deref(), Some("first"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn what_already_exists_is_left_as_it_is() {
    let dir = scratch("exists");
    let data = dir.join("data");
    // `uploads` is named twice, the second time with other settings: the first stays, no error
    let seed = write(
        &dir,
        "seed.json",
        &SEED.replace(r#""containers": ["uploads","#, r#""containers": ["uploads", {"name": "uploads", "publicAccess": "container"},"#),
    );
    let services = stack().services;
    let first = {
        let s = seed.clone();
        TestServer::try_start_persistent(services, &data, move |b| b.seed_file(s)).await.unwrap()
    };
    assert_all_there(&first).await;
    assert_eq!(blob_container(&first, "uploads").await.header("x-ms-blob-public-access"), None);
    // change what the seed made: a new secret version, other metadata on a container, another value for a key
    let r = first
        .request("PUT", &format!("{}/secrets/db-password?api-version=7.5", first.url("vault", "vault1")))
        .header("authorization", "Bearer t")
        .json(&serde_json::json!({"value": "changed"}))
        .send()
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        call(&first, "PUT", "blob", "acct", "/public-files?restype=container&comp=metadata", &[("x-ms-meta-team", "blue")]).await.status,
        200
    );
    let r = first
        .request("PUT", &format!("{}/kv/App:Title?api-version=2023-11-01", first.url("azconfig", "store1")))
        .header("authorization", "Bearer t")
        .json(&serde_json::json!({"value": "Changed"}))
        .send()
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    first.shutdown().await.unwrap();

    // the second start reads the saved state, then the seed: nothing is made twice or put back
    let services = stack().services;
    let second = {
        let s = seed.clone();
        TestServer::try_start_persistent(services, &data, move |b| b.seed_file(s)).await.unwrap()
    };
    assert_eq!(secret_versions(&second, "db-password").await, 2);
    assert_eq!(secret_value(&second, "db-password").await.as_deref(), Some("changed"));
    assert_eq!(blob_container(&second, "public-files").await.header("x-ms-meta-team"), Some("blue"));
    assert_eq!(key_value(&second, "App:Title", None).await.unwrap()["value"], "Changed");
    assert_eq!(table_names(&second).await, ["Orders"]);
    // a reset still goes back to the file
    reset(&second, "").await;
    assert_all_there(&second).await;
    second.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// The message the start fails with, for a file with this text.
async fn start_error(file_text: &str) -> String {
    let dir = scratch("refused");
    let seed = write(&dir, "seed.json", file_text);
    let azemu::Stack { services, raw } = stack();
    let mut b = Emulator::builder().gateway("127.0.0.1:0".parse().unwrap()).cert_dir(dir.join("ca")).seed_file(&seed);
    for s in services {
        b = b.service(s);
    }
    for r in raw {
        b = b.raw_protocol(r);
    }
    let e = b.start().await.err().expect("the start must fail");
    let _ = std::fs::remove_dir_all(dir);
    format!("{e:#}").replace(&seed.display().to_string(), "<seed>")
}

#[tokio::test]
async fn a_file_that_is_wrong_stops_the_start_and_says_where() {
    // the service's own rules for names
    let m = start_error(r#"{"blob": {"acct": {"containers": ["fine", "Not_Valid"]}}}"#).await;
    assert!(m.starts_with("seed file <seed>: blob.acct.containers[1]: the blob service refused it: 400 InvalidResourceName"), "{m}");
    let m = start_error(r#"{"queue": {"acct": {"queues": ["jobs", "x"]}}}"#).await;
    assert!(m.starts_with("seed file <seed>: queue.acct.queues[1]: the queue service refused it: 400"), "{m}");
    let m = start_error(r#"{"table": {"acct": {"tables": ["1abc"]}}}"#).await;
    assert!(m.contains("table.acct.tables[0]: the table service refused it: 400 InvalidResourceName"), "{m}");
    let m = start_error(r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": "/p", "indexingPolicy": {"indexingMode": "odd"}}]}]}}}"#).await;
    assert!(m.contains("cosmos.a.databases[0].containers[0]: the cosmos service refused it: 400") && m.contains("indexingMode odd"), "{m}");
    let m = start_error(r#"{"keyvault": {"v": {"secrets": {"ok": "1", "bad name!": "2"}}}}"#).await;
    assert!(m.contains("keyvault.v.secrets.bad name!: the keyvault service refused it: 400 BadParameter"), "{m}");
    // a mistake in the file itself is found before anything is created
    let m = start_error(r#"{"blob": {"acct": {"containers": ["fine"]}}, "queue": {"acct": {"quues": []}}}"#).await;
    assert!(m.starts_with("seed file <seed>: queue.acct.quues: unknown property (known here: queues)"), "{m}");
    let m = start_error(r#"{"blob": "#).await;
    assert!(m.starts_with("seed file <seed>: not valid JSON: "), "{m}");
    let m = start_error(r#"{"eventgrid": {}}"#).await;
    assert!(m.contains("eventgrid") && m.contains("AZEMU_EVENTGRID_CONFIG"), "{m}");
}

#[tokio::test]
async fn a_program_without_the_service_says_so() {
    let dir = scratch("mounted");
    let seed = write(&dir, "seed.json", r#"{"queue": {"acct": {"queues": ["jobs"]}}}"#);
    let blob = Arc::new(azemu_blob::BlobService::new());
    let e = Emulator::builder()
        .gateway("127.0.0.1:0".parse().unwrap())
        .cert_dir(dir.join("ca"))
        .seed_file(&seed)
        .service(blob)
        .start()
        .await
        .err()
        .expect("the start must fail");
    let m = format!("{e:#}");
    assert!(m.contains("queue.acct.queues[0]") && m.contains("no queue service") && m.contains("it has: blob"), "{m}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The blob service, until told to refuse every creation.
struct Refusing {
    inner: Arc<azemu_blob::BlobService>,
    refuse: Arc<AtomicBool>,
}

#[async_trait]
impl Service for Refusing {
    fn name(&self) -> &'static str {
        "blob"
    }
    fn mounts(&self) -> Vec<Mount> {
        self.inner.mounts()
    }
    async fn handle(&self, req: Request) -> Response {
        if self.refuse.load(Ordering::SeqCst) && req.method == azemu_core::http::Method::PUT {
            return azemu_core::json_error::response(azemu_core::http::StatusCode::SERVICE_UNAVAILABLE, "ServerBusy", "Come back later.");
        }
        self.inner.handle(req).await
    }
    fn reset(&self) {
        self.inner.reset();
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// This file installs the process-wide logger in this one test.
#[tokio::test]
async fn a_failure_after_a_reset_is_logged_as_an_error_and_the_reset_still_happens() {
    let sink = Sink::default();
    let writer = sink.clone();
    assert!(azemu_core::init_logging_with(tracing::Level::INFO, move || writer.clone(), false), "another test installed a logger");
    let dir = scratch("afterreset");
    let seed = write(&dir, "seed.json", r#"{"blob": {"acct": {"containers": ["uploads"]}}}"#);
    let refuse = Arc::new(AtomicBool::new(false));
    let svc = Arc::new(Refusing { inner: Arc::new(azemu_blob::BlobService::new()), refuse: refuse.clone() });
    let srv = {
        let svc = svc.clone();
        let seed = seed.clone();
        TestServer::start_configured(vec![svc as Arc<dyn Service>], vec![], move |b| b.seed_file(seed)).await
    };
    assert_eq!(blob_container(&srv, "uploads").await.status, 200);
    refuse.store(true, Ordering::SeqCst);
    // the reset answers as always; the seed could not be put back, and the log says so
    assert_eq!(reset(&srv, "").await, serde_json::json!({}));
    refuse.store(false, Ordering::SeqCst);
    assert_eq!(blob_container(&srv, "uploads").await.status, 404, "the reset emptied the service");
    let log = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    let line = log.lines().find(|l| l.contains("ERROR")).unwrap_or_else(|| panic!("no ERROR line in:\n{log}"));
    for want in ["seed file", "after a reset", "blob.acct.containers[0]", "503 ServerBusy", "Come back later"] {
        assert!(line.contains(want), "{want}: {line}");
    }
    // a partial reset does the same
    assert_eq!(log.matches("ERROR").count(), 1);
    refuse.store(true, Ordering::SeqCst);
    reset(&srv, "?service=blob").await;
    let log = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    assert_eq!(log.matches("ERROR").count(), 2, "{log}");
    let _ = std::fs::remove_dir_all(dir);
}

// -- the program itself ----------------------------------------------------------------------------------------------

const BIN: &str = env!("CARGO_BIN_EXE_azemu");

struct Program {
    child: Child,
    gateway: String,
    cert_dir: PathBuf,
}

impl Program {
    fn start(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Program {
        let cert_dir = scratch("bin-ca");
        let mut cmd = Command::new(BIN);
        cmd.current_dir(cwd).args(["--port", "0", "--cert-dir"]).arg(&cert_dir).args(args);
        for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT") {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("start azemu");
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(out).read_line(&mut line);
            let _ = tx.send(line);
        });
        let ready = rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
        assert!(ready.starts_with("azemu ready"), "no ready line");
        let gateway = ready.split_whitespace().find_map(|w| w.strip_prefix("gateway=")).unwrap().to_string();
        Program { child, gateway, cert_dir }
    }

    /// The program is expected to refuse to start: its exit status and what it says on standard error.
    fn refuses(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (Option<i32>, String) {
        let cert_dir = scratch("bin-ca");
        let mut cmd = Command::new(BIN);
        cmd.current_dir(cwd).args(["--port", "0", "--cert-dir"]).arg(&cert_dir).args(args);
        for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_") && k != "AZEMU_GIT_COMMIT") {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.stdin(Stdio::null()).output().expect("run azemu");
        let _ = std::fs::remove_dir_all(cert_dir);
        let mut err = String::new();
        let _ = out.stderr.as_slice().read_to_string(&mut err);
        (out.status.code(), err)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.cert_dir);
    }
}

#[tokio::test]
async fn the_program_takes_a_relative_path_and_the_variable_and_shows_the_file_in_its_config() {
    let dir = scratch("bin");
    write(&dir, "seed.json", SEED);
    let client = azemu_core::testing::Client::new();
    for (args, env) in [(vec!["--seed", "seed.json"], vec![]), (vec![], vec![("AZEMU_SEED", "./seed.json")])] {
        let p = Program::start(&dir, &args, &env);
        let cfg = client.request("GET", &format!("{}/_azemu/config", p.gateway)).send().await.json();
        let shown = cfg["seedFile"].as_str().expect("seedFile");
        assert!(Path::new(shown).is_absolute() && shown.ends_with("seed.json"), "{shown}");
        // the variable is a setting that is known (no "typing mistake" warning), and the queue the file lists is there
        let url = p.gateway.replace("127.0.0.1", "acct.queue.localhost");
        let r = client.request("GET", &format!("{url}/jobs?comp=metadata")).header("authorization", "Bearer t").send().await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(cfg["unreadVariables"].as_array().is_none_or(|v| !v.iter().any(|x| x == "AZEMU_SEED")), "{cfg}");
    }
    // without the option the config says there is none
    let p = Program::start(&dir, &[], &[]);
    let cfg = client.request("GET", &format!("{}/_azemu/config", p.gateway)).send().await.json();
    assert!(cfg["seedFile"].is_null(), "{cfg}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_program_does_not_start_when_the_file_is_wrong_and_names_it() {
    let dir = scratch("bin-bad");
    write(&dir, "bad.json", r#"{"blob": {"acct": {"containers": ["Not_Valid"]}}}"#);
    let (code, err) = Program::refuses(&dir, &["--seed", "bad.json"], &[]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("seed file")
            && err.contains("bad.json")
            && err.contains("blob.acct.containers[0]")
            && err.contains("InvalidResourceName"),
        "{err}"
    );
    let (code, err) = Program::refuses(&dir, &[], &[("AZEMU_SEED", "missing.json")]);
    assert_eq!(code, Some(1));
    assert!(err.contains("seed file missing.json: cannot read it"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}
