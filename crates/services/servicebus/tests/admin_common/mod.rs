#![allow(dead_code)]
//! Helpers for the management API tests: a server, requests with an Authorization header,
//! entity bodies as the SDK writes them, and a few XML probes.

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_servicebus::state::Shared;

pub const AUTH: (&str, &str) = ("Authorization", "SharedAccessSignature sr=x&sig=y&se=1&skn=k");

pub struct Admin {
    pub srv: TestServer,
    pub shared: Arc<Shared>,
    pub base: String,
}

pub async fn admin() -> Admin {
    let shared = Shared::new(true, None);
    let (svc, _amqp) = azemu_servicebus::pair_with(shared.clone());
    let srv = TestServer::start(vec![svc]).await;
    let base = srv.url("servicebus", "default");
    Admin { srv, shared, base }
}

impl Admin {
    pub async fn req(&self, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> TestResponse {
        self.req_at(&self.base, method, path, body, headers).await
    }

    pub async fn req_at(&self, base: &str, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> TestResponse {
        let mut hs = vec![AUTH];
        hs.extend_from_slice(headers);
        self.srv.send(method, &format!("{base}{path}"), &hs, body.to_string()).await
    }

    pub async fn get(&self, path: &str) -> TestResponse {
        self.req("GET", path, "", &[]).await
    }

    pub async fn delete(&self, path: &str) -> TestResponse {
        self.req("DELETE", path, "", &[]).await
    }

    /// Create: PUT without If-Match.
    pub async fn create(&self, path: &str, body: &str) -> TestResponse {
        self.req("PUT", path, body, &[]).await
    }

    /// Update: PUT with `If-Match: *`.
    pub async fn update(&self, path: &str, body: &str) -> TestResponse {
        self.req("PUT", path, body, &[("If-Match", "*")]).await
    }

    pub async fn queue(&self, name: &str, inner: &str) -> TestResponse {
        self.create(&format!("/{name}?api-version=2021-05"), &entry("QueueDescription", inner)).await
    }

    pub async fn topic(&self, name: &str, inner: &str) -> TestResponse {
        self.create(&format!("/{name}?api-version=2021-05"), &entry("TopicDescription", inner)).await
    }

    pub async fn sub(&self, topic: &str, name: &str, inner: &str) -> TestResponse {
        self.create(&format!("/{topic}/subscriptions/{name}?api-version=2021-05"), &entry("SubscriptionDescription", inner)).await
    }

    pub async fn rule(&self, topic: &str, sub: &str, name: &str, inner: &str) -> TestResponse {
        self.create(&format!("/{topic}/subscriptions/{sub}/rules/{name}?api-version=2021-05"), &entry("RuleDescription", inner)).await
    }
}

/// The body the Python SDK sends.
pub fn entry(desc: &str, inner: &str) -> String {
    format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<ns0:entry xmlns:ns0=\"http://www.w3.org/2005/Atom\" xmlns:i=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\"><ns0:content type=\"application/xml\"><{desc}>{inner}</{desc}></ns0:content></ns0:entry>"
    )
}

/// Text of the first `<name>` element (any prefix), if any.
pub fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))?;
    Some(xml[start..start + end].to_string())
}

/// All `<name>` texts in document order.
pub fn tags(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let s = i + open.len();
        match rest[s..].find(&close) {
            Some(e) => {
                out.push(rest[s..s + e].to_string());
                rest = &rest[s + e + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// Assert the names appear in this order in `xml`.
pub fn assert_order(xml: &str, names: &[&str]) {
    let mut at = 0;
    for n in names {
        let found =
            xml[at..].find(&format!("<{n}>")).or_else(|| xml[at..].find(&format!("<{n} "))).or_else(|| xml[at..].find(&format!("<{n}/>")));
        match found {
            Some(i) => at += i + 1,
            None => panic!("<{n}> not found after offset {at} in {xml}"),
        }
    }
}

/// The `<title type="text">` texts in document order (the first one is the feed's).
pub fn titles(xml: &str) -> Vec<String> {
    xml.split("<title type=\"text\">").skip(1).filter_map(|p| p.split("</title>").next()).map(str::to_string).collect()
}
