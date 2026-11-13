//! One log line for a request that was refused for lack of a valid credential, saying why in plain words.
//!
//! A client built without a credential gets 401 or 403 (or, from Blob, 404 when the container does not exist) and the
//! answer alone does not say what the client forgot. The line names the service, the request and the reason. A test that
//! retries in a loop would write the same line again and again, so a given service and reason is logged once per
//! [`WINDOW`]; the next line then says how many refusals it stands for.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use http::{HeaderMap, StatusCode};

/// The same refusal (same service, same reason) is written once in this time.
const WINDOW: Duration = Duration::from_secs(5);
/// The table of what was logged lately never holds more than this many reasons.
const MAX_REASONS: usize = 256;

#[derive(Default)]
pub(crate) struct Refusals {
    seen: Mutex<HashMap<(String, String), Entry>>,
}

struct Entry {
    last_logged: Instant,
    /// Refusals since the line that was last written, that one not counted.
    skipped: u64,
}

/// What the request looked like, as far as the reason needs.
pub(crate) struct Refused<'a> {
    pub service: &'a str,
    pub account: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a HeaderMap,
    pub status: StatusCode,
    /// `x-ms-error-code` of the answer, when it has one.
    pub error_code: Option<&'a str>,
}

/// The services' names as a person says them.
fn display_name(service: &str) -> &str {
    match service {
        "blob" => "Blob",
        "dfs" => "Data Lake",
        "queue" => "Queue",
        "table" => "Table",
        "keyvault" => "Key Vault",
        "appconfig" => "App Configuration",
        "cosmos" => "Cosmos DB",
        "eventgrid" => "Event Grid",
        "arm" => "Resource Manager",
        "servicebus" => "Service Bus",
        "eventhubs" => "Event Hubs",
        other => other,
    }
}

fn query_has(query: &str, name: &str) -> bool {
    query.split('&').any(|p| p.split('=').next().is_some_and(|k| k.eq_ignore_ascii_case(name)))
}

fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
}

/// What credential the request carried: `None` when there was none.
fn credential_kind(r: &Refused<'_>) -> Option<String> {
    if let Some(a) = r.headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::trim).filter(|a| !a.is_empty()) {
        let scheme = a.split_whitespace().next().unwrap_or("");
        return Some(format!("an Authorization header ({scheme})"));
    }
    if query_has(r.query, "sig") {
        return Some("a SAS signature in the query".to_string());
    }
    if r.headers.contains_key("aeg-sas-key") || r.headers.contains_key("aeg-sas-token") {
        return Some("an Event Grid key or token".to_string());
    }
    None
}

/// Why this answer is a refusal for lack of a credential, as `(key, text)`: the key tells reasons apart for the rate limit.
/// `None` when the answer is no such refusal.
fn reason(r: &Refused<'_>) -> Option<(String, String)> {
    let credential = credential_kind(r);
    let anonymous_not_found = r.status == StatusCode::NOT_FOUND && r.error_code == Some("ResourceNotFound") && credential.is_none();
    if !(r.status == StatusCode::UNAUTHORIZED || r.status == StatusCode::FORBIDDEN || anonymous_not_found) {
        return None;
    }
    let name = display_name(r.service);
    match credential {
        None => {
            // A Key Vault client sends its first request without a token on purpose: the 401 tells it where to get one.
            if r.service == "keyvault" {
                return None;
            }
            let storage = matches!(r.service, "blob" | "dfs" | "queue" | "table");
            let text = if storage {
                format!("no Authorization header and no SAS in the query: the {name} SDK was probably built without a credential")
            } else {
                format!("no Authorization header: the {name} client was probably built without a credential")
            };
            let text = if anonymous_not_found {
                format!("{text}, and the container it names does not exist (an anonymous request to a container that does not exist is answered 404)")
            } else {
                text
            };
            Some(("no credential".to_string(), text))
        }
        Some(kind) => {
            let code = r.error_code.map(|c| format!(", error {c}")).unwrap_or_default();
            let text = format!(
                "the request carried {kind} and it was not accepted{code}: check the key, the SAS (its time window and its permissions) or the token, and that the client talks to the account it was set up for"
            );
            Some((format!("refused {kind}{code}"), text))
        }
    }
}

impl Refusals {
    /// `Some(n)` when a line is to be written now, standing for `n` refusals (this one and the ones skipped since the last
    /// line); `None` when this one is only counted.
    fn admit(&self, service: &str, key: &str, now: Instant) -> Option<u64> {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= MAX_REASONS {
            seen.clear();
        }
        match seen.get_mut(&(service.to_string(), key.to_string())) {
            Some(e) if now.duration_since(e.last_logged) < WINDOW => {
                e.skipped += 1;
                None
            }
            Some(e) => {
                let count = e.skipped + 1;
                e.last_logged = now;
                e.skipped = 0;
                Some(count)
            }
            None => {
                seen.insert((service.to_string(), key.to_string()), Entry { last_logged: now, skipped: 0 });
                Some(1)
            }
        }
    }

    /// Write the line when this is the first refusal of its kind in the window; otherwise count it for the next line.
    pub(crate) fn note(&self, r: &Refused<'_>) {
        let Some((key, text)) = reason(r) else { return };
        let Some(count) = self.admit(r.service, &key, Instant::now()) else { return };
        // Only the parts of the query that name the operation are shown, never its values in general (a SAS is in there).
        let mut operation = format!("{} {}", r.method, r.path);
        let shown: Vec<String> = ["restype", "comp"]
            .iter()
            .filter_map(|n| {
                query_value(r.query, n).filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).map(|v| format!("{n}={v}"))
            })
            .collect();
        if !shown.is_empty() {
            operation.push_str(&format!(" ({})", shown.join(" ")));
        }
        let more = if count > 1 { format!(" [{count} refusals like this since the last line of it]") } else { String::new() };
        tracing::info!(service = r.service, account = r.account, status = r.status.as_u16(), "refused {operation}: {text}{more}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused<'a>(service: &'a str, status: u16, headers: &'a HeaderMap, query: &'a str, code: Option<&'a str>) -> Refused<'a> {
        Refused {
            service,
            account: "acct",
            method: "PUT",
            path: "/c",
            query,
            headers,
            status: StatusCode::from_u16(status).unwrap(),
            error_code: code,
        }
    }

    #[test]
    fn a_request_with_no_credential_says_so() {
        let h = HeaderMap::new();
        let (key, text) = reason(&refused("blob", 401, &h, "restype=container", Some("NoAuthenticationInformation"))).unwrap();
        assert_eq!(key, "no credential");
        assert!(text.contains("no Authorization header and no SAS in the query") && text.contains("Blob SDK"), "{text}");
        let (_, text) = reason(&refused("cosmos", 401, &h, "", None)).unwrap();
        assert!(text.contains("no Authorization header") && !text.contains("SAS"), "{text}");
        let (_, text) = reason(&refused("blob", 404, &h, "", Some("ResourceNotFound"))).unwrap();
        assert!(text.contains("does not exist"), "{text}");
    }

    #[test]
    fn a_credential_that_was_refused_is_named_and_a_normal_answer_is_not_a_refusal() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "SharedKey acct:abc".parse().unwrap());
        let (_, text) = reason(&refused("table", 403, &h, "", Some("AuthenticationFailed"))).unwrap();
        assert!(text.contains("SharedKey") && text.contains("AuthenticationFailed"), "{text}");
        let none = HeaderMap::new();
        let (_, text) = reason(&refused("blob", 403, &none, "sv=2022&sig=SECRET", None)).unwrap();
        assert!(text.contains("SAS signature") && !text.contains("SECRET"), "{text}");
        assert!(reason(&refused("blob", 404, &none, "", Some("BlobNotFound"))).is_none());
        assert!(reason(&refused("blob", 200, &none, "", None)).is_none());
        // the Key Vault challenge is how its clients start, not a mistake
        assert!(reason(&refused("keyvault", 401, &none, "", None)).is_none());
    }

    #[test]
    fn the_same_refusal_is_written_once_per_window_with_a_count() {
        let log = Refusals::default();
        let t0 = Instant::now();
        assert_eq!(log.admit("blob", "no credential", t0), Some(1));
        for i in 1..=4 {
            assert_eq!(log.admit("blob", "no credential", t0 + Duration::from_millis(i * 100)), None, "inside the window");
        }
        // another service, or another reason, is its own refusal
        assert_eq!(log.admit("queue", "no credential", t0), Some(1));
        assert_eq!(log.admit("blob", "refused SharedKey", t0), Some(1));
        // after the window the line comes again and says how many it stands for
        assert_eq!(log.admit("blob", "no credential", t0 + WINDOW), Some(5));
        assert_eq!(log.admit("blob", "no credential", t0 + WINDOW + Duration::from_secs(1)), None);
        assert_eq!(log.admit("blob", "no credential", t0 + WINDOW * 2), Some(2));
    }
}
