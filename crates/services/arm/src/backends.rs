//! Handles on the services ARM drives. Every handle is optional: without one, ARM
//! stores the resources and nothing else happens.

use std::sync::Arc;

use azemu_core::http::{HeaderMap, HeaderName, HeaderValue, Method};
use azemu_core::time::OffsetDateTime;
use azemu_core::{Endpoint, Query, Request, RequestCtx, Response, Service};

#[derive(Default, Clone)]
pub struct Backends {
    pub blob: Option<Arc<azemu_blob::BlobService>>,
    pub queue: Option<Arc<dyn Service>>,
    pub table: Option<Arc<dyn Service>>,
    pub cosmos: Option<Arc<dyn Service>>,
    pub eventgrid: Option<Arc<azemu_eventgrid::EventGridService>>,
    pub keyvault: Option<Arc<azemu_keyvault::KeyVaultService>>,
    pub appconfig: Option<Arc<azemu_appconfig::AppConfigService>>,
    pub servicebus: Option<Arc<azemu_servicebus::Shared>>,
    pub eventhubs: Option<Arc<azemu_eventhubs::Shared>>,
}

impl Backends {
    pub fn none() -> Backends {
        Backends::default()
    }

    /// The Blob service as a plain `Service`, for requests to its HTTP API.
    pub fn blob_service(&self) -> Option<Arc<dyn Service>> {
        self.blob.clone().map(|b| b as Arc<dyn Service>)
    }

    /// The Event Grid service as a plain `Service`, for requests to its admin API.
    pub fn eventgrid_service(&self) -> Option<Arc<dyn Service>> {
        self.eventgrid.clone().map(|e| e as Arc<dyn Service>)
    }
}

/// A request to another service, built the way core would build it.
pub struct Call<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: Vec<u8>,
    /// Host label of the target service (`blob`, `queue`, `documents`).
    pub label: &'a str,
    pub account: &'a str,
    /// `:4577`, or empty.
    pub port_suffix: &'a str,
}

/// Send one request to a service in this process. Returns the response.
pub async fn call(svc: &Arc<dyn Service>, c: Call<'_>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer azemu-arm"));
    for (k, v) in c.headers {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            headers.insert(name, val);
        }
    }
    let req = Request {
        method: Method::from_bytes(c.method.as_bytes()).unwrap_or(Method::GET),
        path: c.path.to_string(),
        query: Query::parse(c.query),
        headers,
        body: c.body.into(),
        account: c.account.to_string(),
        ctx: RequestCtx {
            request_id: uuid::Uuid::new_v4().to_string(),
            time: OffsetDateTime::now_utc(),
            endpoint: Endpoint {
                scheme: "http",
                authority: if c.account.is_empty() {
                    format!("{}.localhost{}", c.label, c.port_suffix)
                } else {
                    format!("{}.{}.localhost{}", c.account, c.label, c.port_suffix)
                },
                path_style: false,
                account: c.account.to_string(),
            },
        },
    };
    svc.handle(req).await
}
