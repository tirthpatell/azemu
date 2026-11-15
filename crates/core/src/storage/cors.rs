//! CORS for the storage services (Microsoft Learn, "Cross-Origin Resource Sharing (CORS) support
//! for Azure Storage"). The rules come from Set Service Properties; the services keep them and pass
//! them in as [`CorsRule`]s. Blob, Queue and Table can use the same two functions:
//!
//! - [`preflight`] (Table: [`preflight_table`]) answers an `OPTIONS` request. It needs no credentials and is judged
//!   against the service, not the resource. Missing `Origin` or `Access-Control-Request-Method`: 400
//!   `InvalidHeaderValue`. No rule matches (or there are no rules): 403 `CorsPreflightFailure`.
//! - [`apply`] adds the `Access-Control-*` headers to the answer of an actual request.
//!
//! Rules are tried in order. A rule is picked when the origin and the method match; from then on
//! only that rule counts, and if the request headers of a preflight are not all allowed by it, the
//! preflight fails. Origins match case-sensitively; `*` stands for any origin, and a `*` inside an
//! entry (`http://*.contoso.com`) for any run of characters. Methods match case-sensitively. Allowed headers match by
//! name without case, with a trailing `*` as a prefix.
//!
//! All of this, and the answers below, was checked against real Azure in November 2026:
//!
//! - A preflight that matches answers the origin it was sent (also for a rule with `*`), with
//!   `Access-Control-Allow-Credentials: true`, the method and the request headers as sent, and no `Vary`.
//! - An actual request with a matching rule gets `Access-Control-Allow-Origin` (`*` when the rule lists `*`, else the
//!   origin, with `Access-Control-Allow-Credentials: true` and, on GET and HEAD, `Vary: Origin`) and
//!   `Access-Control-Expose-Headers`: the names of the rule, where a name with a trailing `*` stands for the headers
//!   of this answer that start with it. A request with no `Origin` is matched by a rule with `*` as origin; on an
//!   account with CORS rules a GET or HEAD that no rule matches still gets `Vary: Origin`.
//! - The error bodies have the form of the service: Blob and Queue have `MessageDetails`; Table answers in
//!   its own XML (`m:error`).

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
    items(&rule.allowed_methods).any(|m| m == method)
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

/// How the errors of a preflight are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Blob and Queue: `<Error>` with a `MessageDetails` element.
    Storage,
    /// Table: the OData `m:error` document.
    Table,
}

/// Answer a preflight request (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) of Blob or Queue.
/// The caller adds the common storage headers (`storage::finish`).
pub fn preflight(req: &Request, rules: &[CorsRule]) -> Response {
    preflight_as(req, rules, Style::Storage)
}

/// The same for Table, whose errors have another form.
pub fn preflight_table(req: &Request, rules: &[CorsRule]) -> Response {
    preflight_as(req, rules, Style::Table)
}

fn preflight_as(req: &Request, rules: &[CorsRule], style: Style) -> Response {
    let Some(origin) = req.header("origin") else {
        return missing(req, "Origin", style);
    };
    let Some(method) = req.header("access-control-request-method") else {
        return missing(req, "Access-Control-Request-Method", style);
    };
    let asked_raw = req.header("access-control-request-headers");
    let asked: Vec<&str> = asked_raw.map(|h| items(h).collect()).unwrap_or_default();
    let failed = || {
        error(
            req,
            style,
            403,
            "CorsPreflightFailure",
            "CORS not enabled or no matching rule found for this request.",
            "No CORS rules matches this request",
        )
    };
    let Some(rule) = pick(rules, origin.trim(), method.trim()) else {
        return failed();
    };
    if !asked.iter().all(|h| header_allowed(rule, h)) {
        return failed();
    }
    let mut resp = Resp::new(200u16).body("").build();
    // the origin is echoed even for a rule with `*`, and credentials are allowed
    set(&mut resp, "access-control-allow-origin", origin.trim());
    set(&mut resp, "access-control-allow-methods", method.trim());
    if let (Some(raw), false) = (asked_raw, asked.is_empty()) {
        set(&mut resp, "access-control-allow-headers", raw.trim());
    }
    set(&mut resp, "access-control-max-age", &rule.max_age.to_string());
    set(&mut resp, "access-control-allow-credentials", "true");
    resp
}

fn missing(req: &Request, header_name: &str, style: Style) -> Response {
    error(
        req,
        style,
        400,
        "InvalidHeaderValue",
        "A required CORS header is not present.",
        &format!("Missing required CORS header {header_name}"),
    )
}

fn error(req: &Request, style: Style, status: u16, code: &str, message: &str, details: &str) -> Response {
    match style {
        Style::Storage => storage::error_with_details(&req.ctx, status, code, message, &[("MessageDetails", details)]),
        Style::Table => {
            let ctx = &req.ctx;
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><m:error xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\"><m:code>{}</m:code><m:message xml:lang=\"en-US\">{}\nRequestId:{}\nTime:{}</m:message></m:error>",
                storage::xml_escape(code),
                storage::xml_escape(message),
                storage::xml_escape(&ctx.request_id),
                crate::time::iso8601_7(ctx.time)
            );
            Resp::new(status).xml(body).header("x-ms-error-code", code).build()
        }
    }
}

/// The names of `ExposedHeaders` as Azure sends them: a name is sent as written, a name with a trailing `*` stands for the
/// headers of the answer that start with it (`x-ms-request-id` and `x-ms-version` first, then the others by name).
fn exposed_names(rule: &CorsRule, resp: &Response) -> String {
    let rank = |n: &str| match n {
        "x-ms-request-id" => 0,
        "x-ms-version" => 1,
        _ => 2,
    };
    let mut out: Vec<String> = Vec::new();
    for item in items(&rule.exposed_headers) {
        match item.strip_suffix('*') {
            Some(prefix) => {
                let prefix = prefix.to_ascii_lowercase();
                let mut found: Vec<String> =
                    resp.headers().keys().map(|k| k.as_str().to_string()).filter(|n| n.starts_with(&prefix) && !out.contains(n)).collect();
                found.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
                out.extend(found);
            }
            None if !out.iter().any(|n| n.eq_ignore_ascii_case(item)) => out.push(item.to_string()),
            None => {}
        }
    }
    out.join(",")
}

/// Add the CORS headers to the answer of an actual request (call it when the common headers are already on the answer:
/// the exposed names are matched against them). Does nothing without rules. With a matching rule:
/// `Access-Control-Allow-Origin` (`*` when the rule lists `*`, else the origin), the exposed headers and, for an exact
/// origin, `Access-Control-Allow-Credentials` and (GET/HEAD) `Vary: Origin`. A request with no `Origin` is matched by a rule
/// that lists `*`. A GET or HEAD that no rule matches gets only `Vary: Origin`.
pub fn apply(resp: &mut Response, req: &Request, rules: &[CorsRule]) {
    if rules.is_empty() || req.method == Method::OPTIONS {
        return;
    }
    let cacheable = req.method == Method::GET || req.method == Method::HEAD;
    let method = req.method.as_str();
    let origin = req.header("origin").map(str::trim);
    let matched = match origin {
        Some(o) => pick(rules, o, method),
        None => rules.iter().find(|r| allows_any_origin(r) && method_allowed(r, method)),
    };
    match matched {
        Some(rule) => {
            let any = allows_any_origin(rule);
            set(resp, "access-control-allow-origin", if any { "*" } else { origin.unwrap_or("*") });
            let exposed = exposed_names(rule, resp);
            if !exposed.is_empty() {
                set(resp, "access-control-expose-headers", &exposed);
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
        // the request headers are sent back as they came (real Azure keeps the space)
        assert_eq!(hv(&r, "access-control-allow-headers"), Some("X-MS-Meta-One, Content-Type"));
        assert_eq!(hv(&r, "access-control-max-age"), Some("120"));
        assert_eq!(hv(&r, "access-control-allow-credentials"), Some("true"));
        // real Azure sends no Vary on a preflight
        assert!(hv(&r, "vary").is_none());
        // another origin, another method
        assert_eq!(preflight(&pre_from("http://b.example", "PUT", ""), &rules).status(), 403);
        assert_eq!(preflight(&pre_from("http://a.example", "DELETE", ""), &rules).status(), 403);
        // the method is matched as written: `put` is not `PUT`
        assert_eq!(preflight(&pre_from("http://a.example", "put", ""), &rules).status(), 403);
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
    fn a_preflight_for_a_rule_with_star_answers_the_origin_with_credentials() {
        let rules = vec![rule("*", "GET", "*", "x-ms-*", 0)];
        let r = preflight(&pre_from("http://any.example", "GET", "x-anything"), &rules);
        assert_eq!(hv(&r, "access-control-allow-origin"), Some("http://any.example"));
        assert_eq!(hv(&r, "access-control-allow-credentials"), Some("true"));
        assert!(hv(&r, "vary").is_none());
    }

    #[test]
    fn errors_are_those_of_azure() {
        let rules = docs_rules();
        let body = |r: &Response| String::from_utf8_lossy(r.body()).into_owned();
        // blob and queue: InvalidHeaderValue with MessageDetails
        let r = preflight(&req("OPTIONS", &[("access-control-request-method", "GET")]), &rules);
        assert_eq!(r.status(), 400);
        assert_eq!(hv(&r, "x-ms-error-code"), Some("InvalidHeaderValue"));
        assert!(body(&r).contains("<Message>A required CORS header is not present."), "{}", body(&r));
        assert!(body(&r).contains("<MessageDetails>Missing required CORS header Origin</MessageDetails>"));
        let r = preflight(&req("OPTIONS", &[("origin", "http://x")]), &rules);
        assert_eq!(r.status(), 400);
        assert!(body(&r).contains("<MessageDetails>Missing required CORS header Access-Control-Request-Method</MessageDetails>"));
        let r = preflight(&pre_from("http://nope.example", "DELETE", ""), &rules);
        assert!(body(&r).contains("<MessageDetails>No CORS rules matches this request</MessageDetails>"), "{}", body(&r));
        // table: the OData document, no byte order mark
        let r = preflight_table(&req("OPTIONS", &[("access-control-request-method", "GET")]), &rules);
        assert_eq!(
            (r.status().as_u16(), hv(&r, "x-ms-error-code"), hv(&r, "content-type")),
            (400, Some("InvalidHeaderValue"), Some("application/xml"))
        );
        let b = body(&r);
        assert!(b.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?><m:error xmlns:m=\"http://schemas.microsoft.com/ado/2007/08/dataservices/metadata\"><m:code>InvalidHeaderValue</m:code><m:message xml:lang=\"en-US\">A required CORS header is not present.\nRequestId:r\nTime:"), "{b}");
        assert!(b.ends_with("</m:message></m:error>") && !b.contains("MessageDetails"));
        let r = preflight_table(&pre_from("http://nope.example", "DELETE", ""), &rules);
        assert!(body(&r).contains("<m:code>CorsPreflightFailure</m:code>"), "{}", body(&r));
    }

    #[test]
    fn actual_requests() {
        let rules = vec![rule("http://a.example", "GET,PUT", "", "x-ms-meta-*,x-lit", 0), rule("*", "GET", "", "x-star", 0)];
        let answer = || Resp::new(200u16).header("x-ms-request-id", "i").header("x-ms-version", "v").header("x-ms-meta-b", "1").build();
        let mut ok = answer();
        apply(&mut ok, &req("GET", &[("origin", "http://a.example")]), &rules);
        assert_eq!(hv(&ok, "access-control-allow-origin"), Some("http://a.example"));
        assert_eq!(hv(&ok, "access-control-allow-credentials"), Some("true"));
        // a name with `*` stands for the headers this answer has; a plain name is listed as written
        assert_eq!(hv(&ok, "access-control-expose-headers"), Some("x-ms-meta-b,x-lit"));
        assert_eq!(hv(&ok, "vary"), Some("Origin"));
        // PUT is not cached by browsers: no Vary
        let mut put = answer();
        apply(&mut put, &req("PUT", &[("origin", "http://a.example")]), &rules);
        assert_eq!(hv(&put, "access-control-allow-origin"), Some("http://a.example"));
        assert!(hv(&put, "vary").is_none());
        // the rule with `*`: `*` back, no credentials, no Vary
        let mut star = answer();
        apply(&mut star, &req("GET", &[("origin", "http://z.example")]), &rules);
        assert_eq!(hv(&star, "access-control-allow-origin"), Some("*"));
        assert_eq!(hv(&star, "access-control-expose-headers"), Some("x-star"));
        assert!(hv(&star, "access-control-allow-credentials").is_none() && hv(&star, "vary").is_none());
        // no Origin: the rule with `*` still applies; with only exact origins a GET gets Vary
        let mut none = answer();
        apply(&mut none, &req("GET", &[]), &rules);
        assert_eq!(hv(&none, "access-control-allow-origin"), Some("*"));
        let mut exact = answer();
        apply(&mut exact, &req("GET", &[]), &rules[..1]);
        assert!(hv(&exact, "access-control-allow-origin").is_none());
        assert_eq!(hv(&exact, "vary"), Some("Origin"));
        // no matching rule: nothing but Vary on GET
        let mut none = answer();
        apply(&mut none, &req("DELETE", &[("origin", "http://z.example")]), &rules);
        assert!(hv(&none, "access-control-allow-origin").is_none() && hv(&none, "vary").is_none());
        let mut none = answer();
        apply(&mut none, &req("GET", &[("origin", "http://z.example")]), &rules[..1]);
        assert!(hv(&none, "access-control-allow-origin").is_none());
        assert_eq!(hv(&none, "vary"), Some("Origin"));
        // no rules: untouched
        let mut plain = answer();
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
