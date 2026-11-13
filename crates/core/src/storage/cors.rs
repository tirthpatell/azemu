//! CORS for the storage services (Microsoft Learn, "Cross-Origin Resource Sharing (CORS) support
//! for Azure Storage"). The rules come from Set Service Properties; the services keep them and pass
//! them in as [`CorsRule`]s. Blob, Queue and Table can use the same two functions:
//!
//! - [`preflight`] answers an `OPTIONS` request. It needs no credentials and is judged against the
//!   service, not the resource. Missing `Origin` or `Access-Control-Request-Method`: 400
//!   `MissingRequiredHeader`. No rule matches (or there are no rules): 403 `CorsPreflightFailure`.
//! - [`apply`] adds the `Access-Control-*` headers to the answer of an actual request that carries
//!   `Origin`.
//!
//! Rules are tried in order. A rule is picked when the origin and the method match; from then on
//! only that rule counts, and if the request headers of a preflight are not all allowed by it, the
//! preflight fails. Origins match case-sensitively; `*` stands for any origin, and a `*` inside an
//! entry (`http://*.contoso.com`) for any run of characters. Allowed headers match by name without
//! case, with a trailing `*` as a prefix.

use http::{HeaderValue, Method};

use crate::resp::Resp;
use crate::{storage, Request, Response};

/// The header lists of a rule (`AllowedHeaders`, `ExposedHeaders`) as Azure answers Get Service Properties: the names
/// without a trailing `*` first, then the prefixes that end in `*`, each group in the order it was set. Set with
/// `x-ms-meta-data*,x-ms-meta-target*,x-ms-meta-xyz,x-ms-meta-foo`, Azure answers
/// `x-ms-meta-xyz,x-ms-meta-foo,x-ms-meta-data*,x-ms-meta-target*` (recording
/// `test_table_service_properties.pyTestTableServicePropertiestest_set_cors.json`, entry 1; the same in the Blob, Queue
/// and Data Lake recordings). The origins and methods keep the order they were set in.
pub fn header_list_in_azure_order(list: &str) -> String {
    let items: Vec<&str> = list.split(',').collect();
    if items.len() < 2 {
        return list.to_string();
    }
    let (exact, prefixes): (Vec<&str>, Vec<&str>) = items.into_iter().partition(|i| !i.trim_end().ends_with('*'));
    exact.into_iter().chain(prefixes).collect::<Vec<_>>().join(",")
}

/// One `CorsRule` of the service properties. The lists are comma separated, as in the XML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsRule {
    pub allowed_origins: String,
    pub allowed_methods: String,
    pub allowed_headers: String,
    pub exposed_headers: String,
    pub max_age: u32,
}

fn items(list: &str) -> impl Iterator<Item = &str> {
    list.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// `*` matches any run of characters (also none).
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn origin_allowed(rule: &CorsRule, origin: &str) -> bool {
    items(&rule.allowed_origins).any(|o| glob(o, origin))
}

/// The rule lists `*` itself as an origin (not a pattern such as `http://*.contoso.com`).
fn allows_any_origin(rule: &CorsRule) -> bool {
    items(&rule.allowed_origins).any(|o| o == "*")
}

fn method_allowed(rule: &CorsRule, method: &str) -> bool {
    items(&rule.allowed_methods).any(|m| m.eq_ignore_ascii_case(method))
}

fn header_allowed(rule: &CorsRule, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    items(&rule.allowed_headers).any(|a| {
        let a = a.to_ascii_lowercase();
        match a.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => a == name,
        }
    })
}

/// The first rule that matches origin and method.
fn pick<'a>(rules: &'a [CorsRule], origin: &str, method: &str) -> Option<&'a CorsRule> {
    rules.iter().find(|r| origin_allowed(r, origin) && method_allowed(r, method))
}

fn set(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

/// Answer a preflight request (`OPTIONS` with `Origin` and `Access-Control-Request-Method`).
/// The caller adds the common storage headers (`storage::finish`).
pub fn preflight(req: &Request, rules: &[CorsRule]) -> Response {
    let ctx = &req.ctx;
    let Some(origin) = req.header("origin") else {
        return missing(req, "Origin");
    };
    let Some(method) = req.header("access-control-request-method") else {
        return missing(req, "Access-Control-Request-Method");
    };
    let asked: Vec<&str> = req.header("access-control-request-headers").map(|h| items(h).collect()).unwrap_or_default();
    let failed = || storage::error(ctx, 403u16, "CorsPreflightFailure", "CORS not enabled or no matching rule found for this request.");
    let Some(rule) = pick(rules, origin.trim(), method.trim()) else {
        return failed();
    };
    if !asked.iter().all(|h| header_allowed(rule, h)) {
        return failed();
    }
    let any = allows_any_origin(rule);
    let mut resp = Resp::new(200u16).body("").build();
    set(&mut resp, "access-control-allow-origin", if any { "*" } else { origin.trim() });
    set(&mut resp, "access-control-allow-methods", method.trim());
    if !asked.is_empty() {
        set(&mut resp, "access-control-allow-headers", &asked.join(","));
    }
    set(&mut resp, "access-control-max-age", &rule.max_age.to_string());
    if !any {
        // an echoed origin may carry credentials; `*` may not (browsers refuse the pair)
        set(&mut resp, "access-control-allow-credentials", "true");
        set(&mut resp, "vary", "Origin");
    }
    resp
}

fn missing(req: &Request, header_name: &str) -> Response {
    storage::error_with_details(
        &req.ctx,
        400u16,
        "MissingRequiredHeader",
        "An HTTP header that's mandatory for this request is not specified.",
        &[("HeaderName", header_name)],
    )
}

/// Add the CORS headers to the answer of an actual request. Does nothing without `Origin` or without
/// rules. With a matching rule: `Access-Control-Allow-Origin` (`*` or the origin), the exposed headers
/// and, for an exact origin, `Access-Control-Allow-Credentials` and (GET/HEAD) `Vary: Origin`. Without a
/// matching rule only `Vary: Origin` is added to a GET/HEAD answer, as Azure does.
pub fn apply(resp: &mut Response, req: &Request, rules: &[CorsRule]) {
    if rules.is_empty() || req.method == Method::OPTIONS {
        return;
    }
    let Some(origin) = req.header("origin") else { return };
    let cacheable = req.method == Method::GET || req.method == Method::HEAD;
    match pick(rules, origin.trim(), req.method.as_str()) {
        Some(rule) => {
            let any = allows_any_origin(rule);
            set(resp, "access-control-allow-origin", if any { "*" } else { origin.trim() });
            if !rule.exposed_headers.trim().is_empty() {
                set(resp, "access-control-expose-headers", rule.exposed_headers.trim());
            }
            if !any {
                set(resp, "access-control-allow-credentials", "true");
                if cacheable {
                    set(resp, "vary", "Origin");
                }
            }
        }
        None if cacheable => set(resp, "vary", "Origin"),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Endpoint, Query, RequestCtx};
    use crate::time::OffsetDateTime;

    fn rule(origins: &str, methods: &str, headers: &str, exposed: &str, age: u32) -> CorsRule {
        CorsRule {
            allowed_origins: origins.into(),
            allowed_methods: methods.into(),
            allowed_headers: headers.into(),
            exposed_headers: exposed.into(),
            max_age: age,
        }
    }

    fn req(method: &str, headers: &[(&str, &str)]) -> Request {
        let mut h = http::HeaderMap::new();
        for (k, v) in headers {
            h.append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        Request {
            method: method.parse().unwrap(),
            path: "/".into(),
            query: Query::parse(""),
            headers: h,
            body: bytes::Bytes::new(),
            account: "a".into(),
            ctx: RequestCtx {
                request_id: "r".into(),
                time: OffsetDateTime::UNIX_EPOCH,
                endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style: false, account: "a".into() },
            },
        }
    }

    fn hv<'a>(r: &'a Response, n: &str) -> Option<&'a str> {
        r.headers().get(n).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn globs() {
        assert!(glob("http://www.fabrikam.com", "http://www.fabrikam.com"));
        assert!(!glob("http://www.fabrikam.com", "http://WWW.fabrikam.com"));
        assert!(glob("http://*.contoso.com", "http://a.b.contoso.com"));
        assert!(!glob("http://*.contoso.com", "http://contoso.com"));
        assert!(!glob("http://*.contoso.com", "https://a.contoso.com"));
        assert!(glob("*", "anything"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b*c", "aXcYb"));
    }

    // the example of the documentation: three rules, evaluated in order
    fn docs_rules() -> Vec<CorsRule> {
        vec![
            rule("http://www.contoso.com", "PUT,HEAD", "x-ms-blob-content-type, x-ms-blob-content-disposition", "x-ms-*", 5),
            rule("*", "PUT,GET", "x-ms-blob-content-type, x-ms-blob-content-disposition", "x-ms-*", 5),
            rule("http://www.contoso.com", "GET", "x-ms-client-request-id", "x-ms-*", 5),
        ]
    }

    fn pre(method: &str, hdrs: &str) -> Request {
        let mut v = vec![("origin", "http://www.contoso.com"), ("access-control-request-method", method)];
        if !hdrs.is_empty() {
            v.push(("access-control-request-headers", hdrs));
        }
        req("OPTIONS", &v)
    }

    #[test]
    fn documentation_table_of_rule_evaluation() {
        let rules = docs_rules();
        assert_eq!(preflight(&pre("PUT", "x-ms-blob-content-type"), &rules).status(), 200);
        assert_eq!(preflight(&pre("GET", "x-ms-blob-content-type"), &rules).status(), 200);
        let r = preflight(&pre("GET", "x-ms-client-request-id"), &rules);
        assert_eq!(r.status(), 403);
        assert_eq!(hv(&r, "x-ms-error-code"), Some("CorsPreflightFailure"));
    }

    #[test]
    fn preflight_headers() {
        let rules = vec![rule("http://a.example", "GET,PUT", "x-ms-meta-*,content-type", "", 120)];
        let r = preflight(&pre_from("http://a.example", "PUT", "X-MS-Meta-One, Content-Type"), &rules);
        assert_eq!(r.status(), 200);
        assert_eq!(hv(&r, "access-control-allow-origin"), Some("http://a.example"));
        assert_eq!(hv(&r, "access-control-allow-methods"), Some("PUT"));
        assert_eq!(hv(&r, "access-control-allow-headers"), Some("X-MS-Meta-One,Content-Type"));
        assert_eq!(hv(&r, "access-control-max-age"), Some("120"));
        assert_eq!(hv(&r, "access-control-allow-credentials"), Some("true"));
        assert_eq!(hv(&r, "vary"), Some("Origin"));
        // another origin, another method
        assert_eq!(preflight(&pre_from("http://b.example", "PUT", ""), &rules).status(), 403);
        assert_eq!(preflight(&pre_from("http://a.example", "DELETE", ""), &rules).status(), 403);
        // a header the rule does not list
        assert_eq!(preflight(&pre_from("http://a.example", "PUT", "authorization"), &rules).status(), 403);
        // no rules at all
        assert_eq!(preflight(&pre_from("http://a.example", "PUT", ""), &[]).status(), 403);
    }

    fn pre_from(origin: &str, method: &str, hdrs: &str) -> Request {
        let mut v = vec![("origin", origin), ("access-control-request-method", method)];
        if !hdrs.is_empty() {
            v.push(("access-control-request-headers", hdrs));
        }
        req("OPTIONS", &v)
    }

    #[test]
    fn star_origin_has_no_credentials_and_no_vary() {
        let rules = vec![rule("*", "GET", "*", "x-ms-*", 0)];
        let r = preflight(&pre_from("http://any.example", "GET", "x-anything"), &rules);
        assert_eq!(hv(&r, "access-control-allow-origin"), Some("*"));
        assert!(hv(&r, "access-control-allow-credentials").is_none());
        assert!(hv(&r, "vary").is_none());
    }

    #[test]
    fn missing_headers_are_400() {
        let rules = docs_rules();
        let r = preflight(&req("OPTIONS", &[("access-control-request-method", "GET")]), &rules);
        assert_eq!(r.status(), 400);
        assert_eq!(hv(&r, "x-ms-error-code"), Some("MissingRequiredHeader"));
        assert!(String::from_utf8_lossy(r.body()).contains("<HeaderName>Origin</HeaderName>"));
        let r = preflight(&req("OPTIONS", &[("origin", "http://x")]), &rules);
        assert_eq!(r.status(), 400);
        assert!(String::from_utf8_lossy(r.body()).contains("<HeaderName>Access-Control-Request-Method</HeaderName>"));
    }

    #[test]
    fn actual_requests() {
        let rules = vec![rule("http://a.example", "GET,PUT", "", "x-ms-meta-*", 0), rule("*", "GET", "", "", 0)];
        let mut ok = Resp::new(200u16).build();
        apply(&mut ok, &req("GET", &[("origin", "http://a.example")]), &rules);
        assert_eq!(hv(&ok, "access-control-allow-origin"), Some("http://a.example"));
        assert_eq!(hv(&ok, "access-control-expose-headers"), Some("x-ms-meta-*"));
        assert_eq!(hv(&ok, "vary"), Some("Origin"));
        // PUT is not cached by browsers: no Vary
        let mut put = Resp::new(200u16).build();
        apply(&mut put, &req("PUT", &[("origin", "http://a.example")]), &rules);
        assert_eq!(hv(&put, "access-control-allow-origin"), Some("http://a.example"));
        assert!(hv(&put, "vary").is_none());
        // the wildcard rule for another origin
        let mut star = Resp::new(200u16).build();
        apply(&mut star, &req("GET", &[("origin", "http://z.example")]), &rules);
        assert_eq!(hv(&star, "access-control-allow-origin"), Some("*"));
        assert!(hv(&star, "vary").is_none());
        // no matching rule: nothing but Vary on GET
        let mut none = Resp::new(200u16).build();
        apply(&mut none, &req("DELETE", &[("origin", "http://z.example")]), &rules);
        assert!(hv(&none, "access-control-allow-origin").is_none() && hv(&none, "vary").is_none());
        let mut none = Resp::new(200u16).build();
        apply(&mut none, &req("GET", &[("origin", "http://z.example")]), &rules[..1]);
        assert!(hv(&none, "access-control-allow-origin").is_none());
        assert_eq!(hv(&none, "vary"), Some("Origin"));
        // no Origin, or no rules: untouched
        let mut plain = Resp::new(200u16).build();
        apply(&mut plain, &req("GET", &[]), &rules);
        apply(&mut plain, &req("GET", &[("origin", "http://a.example")]), &[]);
        assert!(plain.headers().get("access-control-allow-origin").is_none() && plain.headers().get("vary").is_none());
    }
}

#[cfg(test)]
mod order_tests {
    use super::header_list_in_azure_order as order;

    #[test]
    fn names_come_before_prefixes_and_each_group_keeps_its_order() {
        // recording `test_table_service_properties.pyTestTableServicePropertiestest_set_cors.json`, entry 1 (set) and entry 2 (get)
        assert_eq!(
            order("x-ms-meta-data*,x-ms-meta-target*,x-ms-meta-xyz,x-ms-meta-foo"),
            "x-ms-meta-xyz,x-ms-meta-foo,x-ms-meta-data*,x-ms-meta-target*"
        );
        assert_eq!(
            order("x-ms-meta-data*,x-ms-meta-source*,x-ms-meta-abc,x-ms-meta-bcd"),
            "x-ms-meta-abc,x-ms-meta-bcd,x-ms-meta-data*,x-ms-meta-source*"
        );
        assert_eq!(order(""), "");
        assert_eq!(order("x-ms-*"), "x-ms-*");
        assert_eq!(order("b,a"), "b,a");
    }
}
