//! Helpers shared by the integration tests: a server, request shortcuts and a tiny
//! webhook listener that records what it receives.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azemu_core::testing::{TestResponse, TestServer};
use azemu_eventgrid::{EventGridService, Options};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub const KEY: (&str, &str) = ("aeg-sas-key", "dGVzdA==");

pub fn fast_options() -> Options {
    // auto-creation is on here: most tests publish to topics they did not create; `autocreate: false` is tested on its own
    Options { autocreate: true, retry_base_ms: 20, retry_max_ms: 80, delivery_timeout_ms: 3000, ..Options::default() }
}

pub async fn server() -> TestServer {
    server_with(fast_options()).await
}

pub async fn server_with(opts: Options) -> TestServer {
    TestServer::start(vec![Arc::new(EventGridService::with_options(opts))]).await
}

pub struct Eg {
    pub srv: TestServer,
}

impl Eg {
    pub async fn new() -> Eg {
        Eg { srv: server().await }
    }

    pub async fn with(opts: Options) -> Eg {
        Eg { srv: server_with(opts).await }
    }

    pub fn host_url(&self, name: &str) -> String {
        self.srv.url("eventgrid", name)
    }

    pub fn admin(&self, path: &str) -> String {
        format!("{}/_azemu/eventgrid{path}", self.srv.gateway_url())
    }

    /// Classic publish with a key header.
    pub async fn publish(&self, topic: &str, content_type: &str, body: &Value) -> TestResponse {
        self.srv
            .request("POST", &format!("{}/api/events?api-version=2018-01-01", self.host_url(topic)))
            .header(KEY.0, KEY.1)
            .header("Content-Type", content_type)
            .body(body.to_string())
            .send()
            .await
    }

    pub async fn publish_eg(&self, topic: &str, events: Value) -> TestResponse {
        self.publish(topic, "application/json; charset=utf-8", &events).await
    }

    pub async fn publish_ce(&self, topic: &str, events: Value) -> TestResponse {
        self.publish(topic, "application/cloudevents-batch+json; charset=utf-8", &events).await
    }

    pub async fn ns_publish(&self, ns: &str, topic: &str, events: Value) -> TestResponse {
        self.srv
            .request("POST", &format!("{}/topics/{topic}:publish?api-version=2024-06-01", self.host_url(ns)))
            .header(KEY.0, KEY.1)
            .header("Content-Type", "application/cloudevents-batch+json; charset=utf-8")
            .body(events.to_string())
            .send()
            .await
    }

    /// `op` is receive / acknowledge / release / reject / renewLock.
    pub async fn ns_op(&self, ns: &str, topic: &str, sub: &str, op: &str, query: &str, body: Value) -> TestResponse {
        let q = if query.is_empty() { String::new() } else { format!("&{query}") };
        self.srv
            .request("POST", &format!("{}/topics/{topic}/eventsubscriptions/{sub}:{op}?api-version=2024-06-01{q}", self.host_url(ns)))
            .header(KEY.0, KEY.1)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
    }

    pub async fn receive(&self, ns: &str, topic: &str, sub: &str, query: &str) -> TestResponse {
        self.ns_op(ns, topic, sub, "receive", query, json!(null)).await
    }

    pub async fn put_json(&self, path: &str, body: &Value) -> TestResponse {
        self.srv.request("PUT", &self.admin(path)).header("Content-Type", "application/json").body(body.to_string()).send().await
    }

    pub async fn get_admin(&self, path: &str) -> TestResponse {
        self.srv.request("GET", &self.admin(path)).send().await
    }
}

pub fn ce(id: &str, ty: &str) -> Value {
    json!({"specversion":"1.0","id":id,"source":"/test","type":ty,"data":{"n":id}})
}

pub fn eg(id: &str, ty: &str, subject: &str) -> Value {
    json!({"id":id,"subject":subject,"eventType":ty,"eventTime":"2024-05-23T22:06:35.487928Z","dataVersion":"1.0","data":{"n":id}})
}

/// Poll until `f` returns Some, up to `secs`.
pub async fn eventually<T, F: FnMut() -> Option<T>>(secs: f64, mut f: F) -> Option<T> {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

// ---------------------------------------------------------------------------
// A webhook listener.

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub delay_ms: u64,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn status(status: u16) -> Reply {
        Reply { delay_ms: 0, status, headers: vec![], body: String::new() }
    }
}

type Handler = dyn Fn(&Recorded, usize) -> Reply + Send + Sync;

pub struct Hook {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Recorded>>>,
}

impl Hook {
    /// Listen on a free port; `handler(request, index)` decides every answer. The default
    /// handler (`Hook::ok`) echoes the validation code like a well-behaved webhook.
    pub async fn start(handler: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) -> Hook {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let seen = seen2.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Some(rec) = read_request(&mut sock).await else { return };
                    let idx = {
                        let mut s = seen.lock();
                        s.push(rec.clone());
                        s.len() - 1
                    };
                    let r = handler(&rec, idx);
                    if r.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(r.delay_ms)).await;
                    }
                    let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
                    for (k, v) in &r.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&r.body);
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Hook { url: format!("http://127.0.0.1:{port}/hook"), seen }
    }

    /// 200 to everything, completing both kinds of validation handshake.
    pub async fn ok() -> Hook {
        Hook::start(|rec, _| default_reply(rec)).await
    }

    pub fn count(&self) -> usize {
        self.seen.lock().len()
    }

    /// Requests that are not validation handshakes.
    pub fn notifications(&self) -> Vec<Recorded> {
        self.seen.lock().iter().filter(|r| r.method == "POST" && r.header("aeg-event-type") == Some("Notification")).cloned().collect()
    }
}

pub fn default_reply(rec: &Recorded) -> Reply {
    if rec.method == "OPTIONS" {
        return Reply {
            delay_ms: 0,
            status: 200,
            headers: vec![("WebHook-Allowed-Origin".into(), "eventgrid.azure.net".into()), ("WebHook-Allowed-Rate".into(), "*".into())],
            body: String::new(),
        };
    }
    if rec.header("aeg-event-type") == Some("SubscriptionValidation") {
        let code = rec.json()[0]["data"]["validationCode"].as_str().unwrap_or("").to_string();
        return Reply {
            delay_ms: 0,
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: json!({ "validationResponse": code }).to_string(),
        };
    }
    Reply::status(200)
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut parts = first.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(Recorded { method, path, headers, body: String::from_utf8_lossy(&body).to_string() })
}

/// Hook answering from a script of statuses (the last one repeats); handshakes always succeed.
pub async fn scripted(statuses: Vec<u16>) -> Hook {
    let q = Arc::new(Mutex::new(VecDeque::from(statuses)));
    Hook::start(move |rec, _| {
        if rec.method == "OPTIONS" || rec.header("aeg-event-type") == Some("SubscriptionValidation") {
            return default_reply(rec);
        }
        let mut q = q.lock();
        let s = if q.len() > 1 { q.pop_front().unwrap_or(200) } else { q.front().copied().unwrap_or(200) };
        Reply::status(s)
    })
    .await
}
