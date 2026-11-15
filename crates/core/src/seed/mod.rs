//! The seed file (`--seed`, `AZEMU_SEED`): a JSON list of things that exist from the start (blob containers, queues, tables,
//! Cosmos databases and containers, Key Vault secrets, App Configuration key-values), created again after every reset.
//! The format is in `docs/seed.md`.
//!
//! Each item is created by sending the service the request a client would send (see `send.rs`), so a seed cannot make
//! what a client could not, and the rules for names stay in the services. An item that is already there (state read from
//! `--data-dir`, or named twice) is left as it is.

mod parse;
mod send;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Service;
use parse::{Entry, Item};

/// A seed file that has been read and checked.
#[derive(Debug)]
pub(crate) struct Seed {
    /// Absolute when the file could be resolved.
    path: PathBuf,
    entries: Vec<Entry>,
}

/// What a reset removed, so that only that is created again.
#[derive(Debug, Default, Clone)]
pub(crate) struct Scope {
    /// Only these services (`None`: all of them).
    pub services: Option<Vec<String>>,
    /// Only this account, vault or store (`None`: all of them).
    pub account: Option<String>,
}

impl Scope {
    fn wants(&self, item: &Item) -> bool {
        let service = item.service();
        let service_ok = match &self.services {
            None => true,
            // the Data Lake service shares the state of Blob: its reset empties the containers too
            Some(list) => list.iter().any(|s| s == service || (service == "blob" && s == "dfs")),
        };
        service_ok && self.account.as_ref().is_none_or(|a| a.eq_ignore_ascii_case(item.account()))
    }
}

/// What `apply` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub created: usize,
    pub existing: usize,
}

impl Seed {
    /// Read and check the file. The message names the file, the place in it and the reason.
    pub(crate) fn load(path: &Path) -> Result<Seed, String> {
        let shown = path.display();
        let bytes = std::fs::read(path).map_err(|e| {
            let why = match std::fs::metadata(path) {
                // a bind mount of a file that did not exist on the host makes a directory in its place
                Ok(m) if m.is_dir() => "it is a directory, not a file (a container bind mount of a file that does not exist on the host makes an empty directory)".to_string(),
                _ => e.to_string(),
            };
            format!("seed file {shown}: cannot read it: {why}")
        })?;
        // from here on the messages and `GET /_azemu/config` show the absolute path, whatever directory the program was started in
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let entries = parse::parse(&bytes).map_err(|e| format!("seed file {}: {e}", path.display()))?;
        Ok(Seed { path, entries })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Every service the file names must be part of this program (the one-service binaries leave most out).
    pub(crate) fn check_mounted(&self, services: &[Arc<dyn Service>]) -> Result<(), String> {
        for e in &self.entries {
            let name = e.item.service();
            if !services.iter().any(|s| s.name() == name) {
                let mounted: Vec<&str> = services.iter().map(|s| s.name()).collect();
                return Err(format!(
                    "seed file {}: {}: this program has no {name} service to create it in (it has: {})",
                    self.path.display(),
                    e.place,
                    mounted.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// Create what `scope` covers, in the order of the file. The first item a service refuses stops it.
    pub(crate) async fn apply(&self, services: &[Arc<dyn Service>], scope: &Scope) -> Result<Applied, String> {
        let mut done = Applied::default();
        for e in self.entries.iter().filter(|e| scope.wants(&e.item)) {
            let fail = |why: String| format!("seed file {}: {}: {why}", self.path.display(), e.place);
            let svc = services
                .iter()
                .find(|s| s.name() == e.item.service())
                .ok_or_else(|| fail(format!("no {} service in this program", e.item.service())))?;
            let plan = send::plan(&e.item);
            let mut exists = false;
            for probe in &plan.probes {
                if svc.handle(probe.request()).await.status() == http::StatusCode::OK {
                    exists = true;
                    break;
                }
            }
            if !exists {
                let r = svc.handle(plan.create.request()).await;
                if r.status().is_success() {
                    done.created += 1;
                    continue;
                }
                if !(plan.already)(&r) {
                    return Err(fail(format!("the {} service refused it: {}", e.item.service(), send::reason(&r))));
                }
            }
            done.existing += 1;
        }
        Ok(done)
    }

    /// [`apply`](Self::apply) for code that is not async (`RunningEmulator::reset`). The services answer these requests
    /// without waiting for anything else, so the future is polled on the calling thread until it is done.
    pub(crate) fn apply_blocking(&self, services: &[Arc<dyn Service>], scope: &Scope) -> Result<Applied, String> {
        block_on(self.apply(services, scope))
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = Box::pin(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Say what the seed did, at start or after a reset.
pub(crate) fn log_applied(seed: &Seed, what: &str, a: &Applied) {
    tracing::info!(file = %seed.path().display(), created = a.created, already_there = a.existing, "{what}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resp::Resp;
    use crate::{Mount, Request, Response};
    use async_trait::async_trait;
    use http::StatusCode;
    use parking_lot::Mutex;

    /// A service that records what it is asked and answers from a function.
    struct Fake {
        name: &'static str,
        seen: Mutex<Vec<String>>,
        answer: Box<dyn Fn(&Request) -> Response + Send + Sync>,
    }

    impl Fake {
        fn new(name: &'static str, answer: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Arc<Fake> {
            Arc::new(Fake { name, seen: Mutex::new(Vec::new()), answer: Box::new(answer) })
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().clone()
        }
    }

    #[async_trait]
    impl Service for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn mounts(&self) -> Vec<Mount> {
            vec![Mount::AccountHost("fake")]
        }
        async fn handle(&self, req: Request) -> Response {
            self.seen.lock().push(format!(
                "{} {}{} @{}",
                req.method,
                req.path,
                if req.query.raw().is_empty() { String::new() } else { format!("?{}", req.query.raw()) },
                req.account
            ));
            (self.answer)(&req)
        }
        fn reset(&self) {}
    }

    fn status(code: u16) -> Response {
        Resp::new(StatusCode::from_u16(code).unwrap()).build()
    }

    fn seed(json: &str) -> Seed {
        Seed { path: PathBuf::from("seed.json"), entries: parse::parse(json.as_bytes()).unwrap() }
    }

    fn one(f: &Arc<Fake>) -> Vec<Arc<dyn Service>> {
        vec![f.clone() as Arc<dyn Service>]
    }

    fn as_services(s: &[Arc<Fake>]) -> Vec<Arc<dyn Service>> {
        s.iter().map(|f| f.clone() as Arc<dyn Service>).collect()
    }

    const TWO_BLOBS: &str = r#"{"blob": {"A": {"containers": ["one", "two"]}, "b": {"containers": ["three"]}}}"#;

    #[tokio::test]
    async fn items_are_sent_as_the_requests_a_client_would_send() {
        let blob = Fake::new("blob", |_| status(201));
        let n = seed(TWO_BLOBS).apply(&one(&blob), &Scope::default()).await.unwrap();
        assert_eq!(n, Applied { created: 3, existing: 0 });
        assert_eq!(blob.seen(), ["PUT /one?restype=container @A", "PUT /two?restype=container @A", "PUT /three?restype=container @b"]);
    }

    #[tokio::test]
    async fn an_item_that_exists_is_left_alone_and_is_not_an_error() {
        let blob = Fake::new("blob", |_| Resp::new(StatusCode::CONFLICT).header("x-ms-error-code", "ContainerAlreadyExists").build());
        let n = seed(TWO_BLOBS).apply(&one(&blob), &Scope::default()).await.unwrap();
        assert_eq!(n, Applied { created: 0, existing: 3 });
        // a conflict that is not "already exists" is a refusal
        let blob = Fake::new("blob", |_| Resp::new(StatusCode::CONFLICT).header("x-ms-error-code", "PublicAccessNotPermitted").build());
        let e = seed(TWO_BLOBS).apply(&one(&blob), &Scope::default()).await.unwrap_err();
        assert!(e.contains("blob.A.containers[0]") && e.contains("PublicAccessNotPermitted") && e.contains("409"), "{e}");
    }

    #[tokio::test]
    async fn a_secret_or_value_that_exists_is_not_set_again() {
        let kv =
            Fake::new("keyvault", |r| status(if r.method == http::Method::GET && r.path.starts_with("/secrets/") { 200 } else { 500 }));
        let n = seed(r#"{"keyvault": {"v": {"secrets": {"a": "1"}}}}"#).apply(&one(&kv), &Scope::default()).await.unwrap();
        assert_eq!(n, Applied { created: 0, existing: 1 });
        assert_eq!(kv.seen(), ["GET /secrets/a?api-version=7.5 @v"], "no PUT after a GET that found it");
        // a secret that was deleted but can be recovered counts as existing as well
        let kv = Fake::new("keyvault", |r| status(if r.path.starts_with("/deletedsecrets/") { 200 } else { 404 }));
        let n = seed(r#"{"keyvault": {"v": {"secrets": {"a": "1"}}}}"#).apply(&one(&kv), &Scope::default()).await.unwrap();
        assert_eq!(n.existing, 1);
        assert_eq!(kv.seen().len(), 2);
        // not there: GET, GET, then PUT
        let kv = Fake::new("keyvault", |r| status(if r.method == http::Method::PUT { 200 } else { 404 }));
        let n = seed(r#"{"keyvault": {"v": {"secrets": {"a": "1"}}}}"#).apply(&one(&kv), &Scope::default()).await.unwrap();
        assert_eq!(n.created, 1);
        assert_eq!(kv.seen().last().unwrap(), "PUT /secrets/a?api-version=7.5 @v");
        // key-values: the label is part of what is looked for and of what is set
        let ac = Fake::new("appconfig", |r| status(if r.method == http::Method::PUT { 200 } else { 404 }));
        seed(r#"{"appconfig": {"s": {"keyValues": [{"key": "a/b:c", "label": "dev x", "value": "1"}]}}}"#)
            .apply(&one(&ac), &Scope::default())
            .await
            .unwrap();
        assert_eq!(
            ac.seen(),
            ["GET /kv/a%2Fb%3Ac?api-version=2023-11-01&label=dev%20x @s", "PUT /kv/a%2Fb%3Ac?api-version=2023-11-01&label=dev%20x @s"]
        );
    }

    #[tokio::test]
    async fn the_first_refusal_stops_and_names_the_file_the_place_and_the_reason() {
        let blob = Fake::new("blob", |r| {
            if r.path == "/two" {
                Resp::new(StatusCode::BAD_REQUEST)
                    .header("x-ms-error-code", "InvalidResourceName")
                    .xml(bytes::Bytes::from_static(
                        b"<Error><Code>InvalidResourceName</Code><Message>The name is bad.\nRequestId:x</Message></Error>",
                    ))
                    .build()
            } else {
                status(201)
            }
        });
        let e = seed(TWO_BLOBS).apply(&one(&blob), &Scope::default()).await.unwrap_err();
        assert_eq!(e, "seed file seed.json: blob.A.containers[1]: the blob service refused it: 400 InvalidResourceName: The name is bad.");
        assert_eq!(blob.seen().len(), 2, "nothing after the refusal");
    }

    #[tokio::test]
    async fn a_scope_covers_only_what_a_reset_removed() {
        let blob = Fake::new("blob", |_| status(201));
        let queue = Fake::new("queue", |_| status(201));
        let both = as_services(&[blob.clone(), queue.clone()]);
        let s = seed(r#"{"blob": {"A": {"containers": ["one"]}, "b": {"containers": ["two"]}}, "queue": {"a": {"queues": ["q"]}}}"#);
        let only = |services: &[&str], account: Option<&str>| Scope {
            services: Some(services.iter().map(|s| s.to_string()).collect()),
            account: account.map(str::to_string),
        };
        s.apply(&both, &only(&["queue"], None)).await.unwrap();
        assert_eq!((blob.seen().len(), queue.seen().len()), (0, 1));
        // the account is compared without regard to case, and the Data Lake service stands for Blob
        s.apply(&both, &only(&["dfs"], Some("a"))).await.unwrap();
        assert_eq!(blob.seen(), ["PUT /one?restype=container @A"]);
        // nothing named: nothing created
        let n = s.apply(&both, &only(&[], None)).await.unwrap();
        assert_eq!(n, Applied::default());
        // an account alone covers every service
        s.apply(&both, &Scope { services: None, account: Some("b".into()) }).await.unwrap();
        assert_eq!(blob.seen().len(), 2);
        assert_eq!(queue.seen().len(), 1, "the queue account is a, not b");
    }

    #[test]
    fn a_service_the_program_does_not_have_is_named() {
        let blob = Fake::new("blob", |_| status(201));
        let s = seed(r#"{"blob": {"a": {"containers": ["c"]}}, "queue": {"a": {"queues": ["qqq"]}}}"#);
        let e = s.check_mounted(&one(&blob)).unwrap_err();
        assert!(
            e.contains("seed.json") && e.contains("queue.a.queues[0]") && e.contains("no queue service") && e.contains("it has: blob"),
            "{e}"
        );
    }

    #[test]
    fn the_blocking_form_works_inside_a_runtime_and_outside_one() {
        let blob = Fake::new("blob", |_| status(201));
        let s = seed(TWO_BLOBS);
        assert_eq!(s.apply_blocking(&one(&blob), &Scope::default()).unwrap().created, 3);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async { assert_eq!(s.apply_blocking(&one(&blob), &Scope::default()).unwrap().created, 3) });
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named_with_the_reason() {
        let dir = std::env::temp_dir().join(format!("azemu-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let e = Seed::load(&dir.join("missing.json")).unwrap_err();
        assert!(
            e.starts_with("seed file ") && e.contains("missing.json") && e.contains("cannot read it") && e.contains("No such file"),
            "{e}"
        );
        let e = Seed::load(&dir).unwrap_err();
        assert!(e.contains("is a directory") && e.contains("bind mount"), "{e}");
        std::fs::write(dir.join("bad.json"), "{\"blob\": 1}").unwrap();
        let e = Seed::load(&dir.join("bad.json")).unwrap_err();
        assert!(e.contains("bad.json") && e.contains("blob: must be an object"), "{e}");
        std::fs::write(dir.join("good.json"), "{}").unwrap();
        let s = Seed::load(&dir.join("good.json")).unwrap();
        assert!(s.path().is_absolute());
        let _ = std::fs::remove_dir_all(dir);
    }
}
