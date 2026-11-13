//! Showing what was not emulated.
//!
//! An operation that is not built answers 501. One line in a log of thousands is easy to miss, so core counts those
//! answers by service and operation, and `GET /_azemu/unsupported` returns the counts; a summary is logged when the
//! emulator stops. ARM adds the resource types it stores without emulating them (see [`note_stored_only`]).
//!
//! A service names the operation in its 501 answer with [`mark`]. Without that, core counts by the shape of the
//! request (`GET /secrets/{}`): good enough to see that something is missing, not good enough to read.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::Response;

/// The name of the operation a 501 answer is for. A response extension: nothing is sent to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedOperation(pub String);

/// Say which operation a 501 answer is for (`"GetKeyRotationPolicy"`, `"POST /certificates/{name}/import"`).
/// Call it where the 501 answer is made; the body and the headers stay as they are.
pub fn mark(mut resp: Response, operation: impl Into<String>) -> Response {
    resp.extensions_mut().insert(UnsupportedOperation(operation.into()));
    resp
}

/// The operation a response was marked with, if any.
pub fn marked(resp: &Response) -> Option<&str> {
    resp.extensions().get::<UnsupportedOperation>().map(|u| u.0.as_str())
}

/// Most distinct operations kept per service; later ones are counted as `(other)`. Keeps memory bounded when a
/// service does not name its operations and the paths carry names.
const MAX_OPERATIONS_PER_SERVICE: usize = 500;
/// Most distinct stored-only resource types kept.
const MAX_STORED_ONLY: usize = 500;
const OTHER: &str = "(other)";

/// (service, operation, count)
type UnsupportedRow = (String, String, u64);
/// (resource type, count)
type StoredOnlyRow = (String, u64);

#[derive(Default)]
struct Inner {
    /// (service, operation) -> count
    unsupported: BTreeMap<(String, String), u64>,
    /// resource type -> count
    stored_only: BTreeMap<String, u64>,
}

/// The counts of one emulator.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Count one 501 answer.
    pub(crate) fn count_unsupported(&self, service: &str, operation: &str) {
        let mut g = self.lock();
        let known = g.unsupported.contains_key(&(service.to_string(), operation.to_string()));
        let distinct = g.unsupported.keys().filter(|(s, _)| s == service).count();
        let op = if known || distinct < MAX_OPERATIONS_PER_SERVICE { operation } else { OTHER };
        *g.unsupported.entry((service.to_string(), op.to_string())).or_insert(0) += 1;
    }

    /// Count a resource type that was stored without being emulated; true the first time the type is seen.
    pub fn count_stored_only(&self, resource_type: &str) -> bool {
        let mut g = self.lock();
        let key = if g.stored_only.contains_key(resource_type) || g.stored_only.len() < MAX_STORED_ONLY { resource_type } else { OTHER };
        let c = g.stored_only.entry(key.to_string()).or_insert(0);
        *c += 1;
        *c == 1
    }

    fn snapshot(&self) -> (Vec<UnsupportedRow>, Vec<StoredOnlyRow>) {
        let g = self.lock();
        (
            g.unsupported.iter().map(|((s, o), n)| (s.clone(), o.clone(), *n)).collect(),
            g.stored_only.iter().map(|(t, n)| (t.clone(), *n)).collect(),
        )
    }

    pub(crate) fn clear(&self) {
        let mut g = self.lock();
        g.unsupported.clear();
        g.stored_only.clear();
    }

    /// The body of `GET /_azemu/unsupported`.
    pub(crate) fn to_json(&self, also: Option<&Registry>) -> serde_json::Value {
        let (mut ops, mut stored) = self.snapshot();
        if let Some(other) = also {
            let (o2, s2) = other.snapshot();
            for (s, o, n) in o2 {
                match ops.iter_mut().find(|(s1, o1, _)| *s1 == s && *o1 == o) {
                    Some(e) => e.2 += n,
                    None => ops.push((s, o, n)),
                }
            }
            for (t, n) in s2 {
                match stored.iter_mut().find(|(t1, _)| *t1 == t) {
                    Some(e) => e.1 += n,
                    None => stored.push((t, n)),
                }
            }
        }
        ops.sort();
        stored.sort();
        serde_json::json!({
            "notImplemented": ops.iter().map(|(s, o, n)| serde_json::json!({"service": s, "operation": o, "count": n})).collect::<Vec<_>>(),
            "storedOnly": stored.iter().map(|(t, n)| serde_json::json!({"resourceType": t, "count": n})).collect::<Vec<_>>(),
        })
    }

    /// The lines of the summary logged at shutdown.
    pub(crate) fn summary_lines(&self, also: Option<&Registry>) -> Vec<String> {
        let v = self.to_json(also);
        let mut lines = Vec::new();
        for e in v["notImplemented"].as_array().into_iter().flatten() {
            lines.push(format!(
                "{} x {} {}: answered 501 (not built)",
                e["count"],
                e["service"].as_str().unwrap_or(""),
                e["operation"].as_str().unwrap_or("")
            ));
        }
        for e in v["storedOnly"].as_array().into_iter().flatten() {
            lines.push(format!("{} x {}: stored without being emulated (ARM)", e["count"], e["resourceType"].as_str().unwrap_or("")));
        }
        lines
    }
}

tokio::task_local! {
    static CURRENT: Arc<Registry>;
}

/// Run `fut` with `registry` as the one [`note_stored_only`] counts into.
pub(crate) async fn scope<F: std::future::Future>(registry: Arc<Registry>, fut: F) -> F::Output {
    CURRENT.scope(registry, fut).await
}

/// Used when no request is being handled (a call from a unit test, or from a task the handler started).
pub(crate) fn fallback() -> &'static Arc<Registry> {
    static GLOBAL: OnceLock<Arc<Registry>> = OnceLock::new();
    GLOBAL.get_or_init(|| Arc::new(Registry::default()))
}

/// For ARM: a resource of this type (`Microsoft.Web/sites`) was stored although nothing emulates it. Core lists it in
/// `GET /_azemu/unsupported` and in the summary at shutdown. Returns true the first time a type is seen, so the
/// caller can log one warning per type. Call it from the request handler (a task the handler started itself counts
/// into a list that is shown too, but not into the count of one emulator when several run in one process).
pub fn note_stored_only(resource_type: &str) -> bool {
    CURRENT.try_with(|r| r.count_stored_only(resource_type)).unwrap_or_else(|_| fallback().count_stored_only(resource_type))
}

/// The shape of a request, for counting an unnamed 501: names, ids and anything with digits become `{}`.
pub(crate) fn request_shape(method: &http::Method, path: &str, query: &str) -> String {
    let mut out = format!("{method}");
    let mut any = false;
    for seg in path.split('/').filter(|s| !s.is_empty()).take(8) {
        any = true;
        let named = seg.len() > 24 || seg.chars().any(|c| c.is_ascii_digit() || matches!(c, '-' | '_' | '%' | '=' | '@' | '$'));
        out.push('/');
        out.push_str(if named { "{}" } else { seg });
    }
    if !any {
        out.push('/');
    }
    for key in ["comp", "restype", "operation"] {
        if let Some(v) = query.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v)) {
            if v.len() <= 32 && v.chars().all(|c| c.is_ascii_alphanumeric()) {
                out.push_str(&format!("{}{key}={v}", if out.contains('?') { "&" } else { "?" }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_hide_names() {
        let m = http::Method::GET;
        assert_eq!(request_shape(&m, "/secrets/my-secret/0123", ""), "GET/secrets/{}/{}");
        assert_eq!(request_shape(&http::Method::PUT, "/container/blob", "comp=tier&x=1"), "PUT/container/blob?comp=tier");
        assert_eq!(request_shape(&m, "/", ""), "GET/");
    }

    #[test]
    fn counts_are_bounded_and_merge() {
        let r = Registry::default();
        for i in 0..(MAX_OPERATIONS_PER_SERVICE + 10) {
            r.count_unsupported("x", &format!("op{i}"));
        }
        r.count_unsupported("x", "op0");
        let v = r.to_json(None);
        assert_eq!(v["notImplemented"].as_array().unwrap().len(), MAX_OPERATIONS_PER_SERVICE + 1);
        assert!(r.count_stored_only("Microsoft.Web/sites"));
        assert!(!r.count_stored_only("Microsoft.Web/sites"));
        let other = Registry::default();
        other.count_stored_only("Microsoft.Web/sites");
        let merged = r.to_json(Some(&other));
        assert_eq!(merged["storedOnly"][0]["count"], 3);
    }
}
