//! monitor: a sink for Application Insights and Azure Monitor telemetry. It takes what the SDKs and the OpenTelemetry
//! exporters send (Application Insights ingestion, the calls the SDKs make besides it, the Logs Ingestion API), keeps it
//! in memory and lets a test read it back. It is not a query engine. See `docs/services/monitor.md`.
//!
//! Mounts: `AccountHost("in")` (`{region}.in.<anything>`, the ingestion endpoint), `AccountHost("livediagnostics")` (Live
//! Metrics), `AccountHost("ingest")` (`{name}.ingest.<anything>`, Logs Ingestion), `Host("api")` (the query APIs, not
//! built), `PathPrefix("/_azemu/monitor")` (the admin API), and the paths of the above at the root of a host that names no
//! service (`Service::claims_root_path`). Nothing is written to the data directory.

// Handlers return the finished error `Response` through `Result`; it is sent as is.
#![allow(clippy::result_large_err)]

mod admin;
mod body;
mod breeze;
mod inspect;
mod live;
mod logs;
mod store;
#[cfg(test)]
mod testutil;

use azemu_core::http::Method;
use azemu_core::{async_trait, json_error, settings, Mount, Request, Response, Service};
pub use live::app_id;
use store::{LogRow, Ring, Telemetry};

/// The prefix of the admin API.
const ADMIN: &str = "/_azemu/monitor";

/// The biggest request body of `track` as sent (before gzip is undone).
const TRACK_BODY_BYTES: usize = 32 << 20;

/// What is kept: the most items, the most log rows, and the most bytes of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_items: usize,
    pub max_log_rows: usize,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_items: 50_000, max_log_rows: 50_000, max_bytes: 128 << 20 }
    }
}

impl Limits {
    /// The defaults, changed by `AZEMU_MONITOR_MAX_ITEMS`, `AZEMU_MONITOR_MAX_LOG_ROWS` and `AZEMU_MONITOR_MAX_MB`.
    pub fn from_env() -> Limits {
        let d = Limits::default();
        settings::describe(
            "AZEMU_MONITOR_MAX_ITEMS",
            "50000",
            "most Application Insights telemetry items the monitor service keeps; the oldest are dropped (the admin API says how many)",
        );
        settings::describe(
            "AZEMU_MONITOR_MAX_LOG_ROWS",
            "50000",
            "most Logs Ingestion rows the monitor service keeps; the oldest are dropped",
        );
        settings::describe(
            "AZEMU_MONITOR_MAX_MB",
            "128",
            "megabytes of telemetry, and of log rows, the monitor service keeps (each list on its own); the oldest are dropped",
        );
        Limits {
            max_items: (settings::number("AZEMU_MONITOR_MAX_ITEMS", d.max_items as u64) as usize).max(1),
            max_log_rows: (settings::number("AZEMU_MONITOR_MAX_LOG_ROWS", d.max_log_rows as u64) as usize).max(1),
            max_bytes: (settings::number("AZEMU_MONITOR_MAX_MB", (d.max_bytes >> 20) as u64) as usize).clamp(1, 1 << 20) << 20,
        }
    }
}

pub struct MonitorService {
    pub(crate) telemetry: Ring<Telemetry>,
    pub(crate) logs: Ring<LogRow>,
    pub(crate) limits: Limits,
}

impl Default for MonitorService {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorService {
    /// The limits come from the settings.
    pub fn new() -> Self {
        Self::with_limits(Limits::from_env())
    }

    pub fn with_limits(limits: Limits) -> Self {
        MonitorService {
            telemetry: Ring::new(limits.max_items, limits.max_bytes),
            logs: Ring::new(limits.max_log_rows, limits.max_bytes),
            limits,
        }
    }
}

/// The first two segments of a path in lower case, for matching.
fn lower(segments: &[String]) -> Vec<String> {
    segments.iter().map(|s| s.to_ascii_lowercase()).collect()
}

fn is_track(l: &[&str]) -> bool {
    matches!(l, ["v2" | "v2.1", "track"])
}

fn is_app_id(l: &[&str]) -> bool {
    matches!(l, ["api", "profiles", _, "appid"])
}

fn is_quickpulse(l: &[&str]) -> bool {
    matches!(l, ["quickpulseservice.svc", "ping" | "post"])
}

fn is_logs(l: &[&str]) -> bool {
    matches!(l, ["datacollectionrules", _, "streams", _])
}

/// The query APIs, which are named so that the 501 says what was asked for.
fn is_query_api(l: &[&str]) -> bool {
    matches!(l, ["v1", "workspaces" | "apps", ..])
}

fn not_built(req: &Request, l: &[&str]) -> Response {
    // for `GET /_azemu/unsupported`: the fixed words of the path, not the ids
    const WORDS: &[&str] =
        &["v1", "workspaces", "apps", "query", "metrics", "events", "metadata", "search", "api", "profiles", "track", "validate"];
    let shape: Vec<&str> = l.iter().map(|s| if WORDS.contains(s) { *s } else { "{}" }).collect();
    let what = if is_query_api(l) { "the query APIs are not built (this service only takes telemetry in)" } else { "not built" };
    let resp = json_error::response(501, "NotImplemented", &format!("monitor: {} {}: {what}", req.method, req.decoded_path()));
    azemu_core::unsupported::mark(resp, format!("Monitor_Unknown ({} /{})", req.method, shape.join("/")))
}

/// `405` as Azure gives it: no body, and `Allow`.
fn wrong_method(allow: &str) -> Response {
    azemu_core::resp::Resp::new(405u16).header("allow", allow).header("content-length", "0").build()
}

#[async_trait]
impl Service for MonitorService {
    fn name(&self) -> &'static str {
        "monitor"
    }

    fn inspect(&self, req: &azemu_core::inspect::InspectRequest) -> azemu_core::inspect::InspectResult {
        self.inspect_node(req)
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![
            Mount::AccountHost("in"),
            Mount::AccountHost("livediagnostics"),
            Mount::AccountHost("ingest"),
            Mount::Host("api"),
            Mount::PathPrefix(ADMIN),
        ]
    }

    /// The paths the SDKs build from a host alone (`IngestionEndpoint=http://localhost:4577/`).
    fn claims_root_path(&self, path: &str) -> bool {
        let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase()).collect();
        let l: Vec<&str> = segs.iter().map(String::as_str).collect();
        is_track(&l) || is_app_id(&l) || is_quickpulse(&l) || is_logs(&l) || is_query_api(&l)
    }

    fn root_path_forms(&self) -> Vec<&'static str> {
        vec![
            "/v2/track",
            "/v2.1/track",
            "/api/profiles/{ikey}/appId",
            "/QuickPulseService.svc/ping",
            "/dataCollectionRules/{dcr}/streams/{stream}",
        ]
    }

    fn max_body_bytes(&self, _method: &Method, path: &str) -> Option<usize> {
        let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(|s| s.to_ascii_lowercase()).collect();
        let l: Vec<&str> = segs.iter().map(String::as_str).collect();
        if is_logs(&l) {
            Some(logs::MAX_CALL_BYTES)
        } else {
            Some(TRACK_BODY_BYTES)
        }
    }

    fn body_too_large(&self, limit: usize) -> Option<Response> {
        Some(if limit == logs::MAX_CALL_BYTES { logs::too_large() } else { breeze::too_large() })
    }

    async fn handle(&self, req: Request) -> Response {
        let segs = lower(&req.segments());
        let l: Vec<&str> = segs.iter().map(String::as_str).collect();
        if let ["_azemu", "monitor", rest @ ..] = l.as_slice() {
            return self.admin(&req, rest);
        }
        let post = req.method == Method::POST;
        if is_track(&l) {
            return match req.method {
                Method::POST => self.track(&req),
                Method::OPTIONS => breeze::preflight(),
                _ => wrong_method("POST"),
            };
        }
        if is_app_id(&l) {
            return if req.method == Method::GET { live::app_id_answer(&req.segments()[2]) } else { wrong_method("GET") };
        }
        if is_quickpulse(&l) {
            // Azure reads the same data whatever the method; what it cannot read is a 400
            return if l[1] == "ping" { live::ping(&req) } else { live::post(&req) };
        }
        if is_logs(&l) {
            let s = req.segments();
            return if post { self.upload_logs(&req, &s[1], &s[3]) } else { wrong_method("POST") };
        }
        not_built(&req, &l)
    }

    fn reset(&self) {
        self.telemetry.clear();
        self.logs.clear();
    }
}
