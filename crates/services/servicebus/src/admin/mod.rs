//! HTTP management API (ATOM/XML), the API `azure.servicebus.management.ServiceBusAdministrationClient`
//! speaks. Wire format (element order, namespaces, headers, status codes and error texts) is copied
//! from the recordings of the real service.
//!
//! | Request | Module |
//! |---|---|
//! | `PUT/GET/DELETE /{queue or topic}`, `GET /$Resources/queues|topics` | `entities` |
//! | `PUT/GET/DELETE /{topic}/subscriptions/{sub}`, `GET .../subscriptions` | `subs` |
//! | `PUT/GET/DELETE .../rules/{rule}`, `GET .../rules` | `rule_api` |
//! | `GET /$namespaceinfo`, `GET /` | `misc` |
//!
//! Anything else answers 501 with a body that names it.

use azemu_core::{Request, Response};

mod entities;
mod error;
mod misc;
mod render;
mod rule_api;
mod rule_xml;
mod settings;
mod subs;
mod util;
mod xml;

use error::{ApiError, ApiResult, ErrKind};
use settings::Kind;

use crate::state::{Shared, DEFAULT_NAMESPACE};

/// What every handler needs to know about the request.
pub struct Cx<'a> {
    pub req: &'a Request,
    /// The namespace: first label of the host.
    pub account: String,
    pub now: i64,
    pub api: String,
    /// `scheme://host[:port]` the client used.
    pub base: String,
    pub authority: String,
}

impl<'a> Cx<'a> {
    fn new(req: &'a Request) -> Cx<'a> {
        Cx {
            req,
            account: if req.account.is_empty() { DEFAULT_NAMESPACE.to_string() } else { req.account.clone() },
            now: util::ms_of(req.ctx.time),
            api: req.query.get("api-version").map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| "2024-05".to_string()),
            base: req.ctx.endpoint.base_url(),
            authority: req.ctx.endpoint.authority.clone(),
        }
    }

    /// The request URL as the client sent it (path and query), with `%2F` inside a name written as `/` (the way Azure writes the `id`
    /// of the entry it answers: the request for `/txt%2F.-_123` gets the id `.../txt/.-_123`).
    pub fn url(&self) -> String {
        let q = self.req.query.raw();
        let path = self.req.path.replace("%2F", "/").replace("%2f", "/");
        if q.is_empty() {
            format!("{}{}", self.base, path)
        } else {
            format!("{}{}?{q}", self.base, path)
        }
    }

    /// `{base}/{name}?api-version=..`
    pub fn entity_url(&self, name: &str) -> String {
        format!("{}/{}", self.base, self.entity_rel(name))
    }

    /// The path word at segment `i` as the client wrote it (`Subscriptions` or `subscriptions`,
    /// `Rules` or `rules`). Service Bus repeats the casing of the request in the ids of the entries
    /// of a list, and the JavaScript SDK finds the names in them by matching `/Subscriptions/` and
    /// `/Rules/` exactly. `default` is used when the segment is something else.
    pub fn path_word(&self, i: usize, default: &str) -> String {
        match self.req.segments().get(i) {
            Some(w) if w.eq_ignore_ascii_case(default) => w.clone(),
            _ => default.to_string(),
        }
    }

    /// `{name}?api-version=..`
    pub fn entity_rel(&self, name: &str) -> String {
        format!("{}?api-version={}", util::enc_segment(name), util::enc_segment(&self.api))
    }

    /// A forwarding target as the API shows it.
    pub fn forward_url(&self, path: &str) -> String {
        format!("sb://{}/{path}", self.authority)
    }
}

/// Entry point, called by `ServiceBusService::handle`.
pub fn handle(shared: &Shared, req: Request) -> Response {
    let cx = Cx::new(&req);
    let segs = req.segments();
    let writes = !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
    let answer = match route(shared, &cx, &segs) {
        Ok(r) => r,
        Err(e) => e.response(&cx, &segs.join("/")),
    };
    if writes {
        // after the work, so that a save that began in between still finds the number moved at its next look
        crate::store::next_revision();
    }
    answer
}

fn verb(cx: &Cx) -> ApiError {
    ApiError::new(ErrKind::Verb(cx.req.method.to_string()))
}

fn route(shared: &Shared, cx: &Cx, segs: &[String]) -> ApiResult<Response> {
    if cx.req.header("authorization").map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return Err(ApiError::new(ErrKind::Unauthorized("MissingToken: The authorization header was not found.".into())));
    }
    let method = cx.req.method.as_str();
    let seg = |i: usize| segs[i].as_str();
    let is = |i: usize, w: &str| segs[i].eq_ignore_ascii_case(w);
    match segs.len() {
        0 => match method {
            "GET" => Ok(misc::root_feed(cx)),
            // The real service rejects an empty entity name with an ARM style 401.
            "PUT" => {
                let kind = if String::from_utf8_lossy(&cx.req.body).contains("TopicDescription") { "topics" } else { "queues" };
                Err(ApiError::new(ErrKind::Unauthorized(format!(
                    "Generic: Provided resource identifier '/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/azemu/providers/Microsoft.ServiceBus/namespaces/{}/{kind}/' is invalid.",
                    cx.account
                ))))
            }
            _ => Err(verb(cx)),
        },
        1 => {
            let name = seg(0);
            if is(0, "$namespaceinfo") {
                return match method {
                    "GET" => Ok(misc::namespace_info(cx)),
                    _ => Err(verb(cx)),
                };
            }
            if name.starts_with('$') {
                return Err(ApiError::not_implemented(format!("{method} /{name}")));
            }
            match method {
                "GET" => entities::get(shared, cx, name),
                "PUT" => entities::put(shared, cx, name),
                "DELETE" => entities::delete(shared, cx, name),
                _ => Err(verb(cx)),
            }
        }
        2 if is(0, "$Resources") => match (method, seg(1).to_ascii_lowercase().as_str()) {
            ("GET", "queues") => entities::list(shared, cx, Kind::Queue),
            ("GET", "topics") => entities::list(shared, cx, Kind::Topic),
            ("GET", other) => Err(ApiError::not_implemented(format!("GET /$Resources/{other}"))),
            _ => Err(verb(cx)),
        },
        2 if is(1, "subscriptions") => match method {
            "GET" => subs::list(shared, cx, seg(0)),
            _ => Err(verb(cx)),
        },
        3 if is(1, "subscriptions") => match method {
            "GET" => subs::get(shared, cx, seg(0), seg(2)),
            "PUT" => subs::put(shared, cx, seg(0), seg(2)),
            "DELETE" => subs::delete(shared, cx, seg(0), seg(2)),
            _ => Err(verb(cx)),
        },
        4 if is(1, "subscriptions") && is(3, "rules") => match method {
            "GET" => rule_api::list(shared, cx, seg(0), seg(2)),
            _ => Err(verb(cx)),
        },
        5 if is(1, "subscriptions") && is(3, "rules") => match method {
            "GET" => rule_api::get(shared, cx, seg(0), seg(2), seg(4)),
            "PUT" => rule_api::put(shared, cx, seg(0), seg(2), seg(4)),
            "DELETE" => rule_api::delete(shared, cx, seg(0), seg(2), seg(4)),
            _ => Err(verb(cx)),
        },
        _ => Err(ApiError::not_implemented(format!("{method} {}", cx.req.decoded_path()))),
    }
}
