//! Picks the handler for a request path.

use azemu_core::resp::Resp;
use azemu_core::{Request, Response};

use crate::cx::Cx;
use crate::error::{ArmError, ArmResult};
use crate::ids::parse_scope;
use crate::{basics, deployments, groups, metadata, resources, tags, util, Inner};

pub async fn route(inner: &Inner, req: &Request, stored_only: &std::sync::atomic::AtomicBool) -> ArmResult<Response> {
    let segs = req.segments();
    // The azurerm provider asks for `//metadata/endpoints` (a trailing slash on the base URL).
    if segs.len() == 2 && segs[0].eq_ignore_ascii_case("metadata") && segs[1].eq_ignore_ascii_case("endpoints") {
        return metadata::handle(req);
    }
    if req.header("authorization").map(|h| h.trim().is_empty()).unwrap_or(true) {
        let mut r = Resp::new(401u16)
            .json(&ArmError::new(401, "AuthenticationFailed", "Authentication failed. The 'Authorization' header is missing.").body())
            .build();
        if let Ok(v) = azemu_core::http::HeaderValue::from_str(
            "Bearer authorization_uri=\"https://login.windows.net/00000000-0000-0000-0000-000000000000\", error=\"invalid_token\", error_description=\"The authentication failed because of missing 'Authorization' header.\"",
        ) {
            r.headers_mut().insert("WWW-Authenticate", v);
        }
        return Ok(r);
    }
    if req.query.get("api-version").map(str::is_empty).unwrap_or(true) {
        return Err(ArmError::missing_api_version());
    }
    let cx = Cx { req, inner, now: req.ctx.time, stored_only };
    let method = req.method.as_str();
    let sc = parse_scope(&segs);
    if let Some(sub) = &sc.sub {
        inner.state.lock().touch_sub(sub);
    }

    // Tags of any scope: `<scope>/providers/Microsoft.Resources/tags/default`.
    let n = segs.len();
    if n >= 4
        && segs[n - 4].eq_ignore_ascii_case("providers")
        && segs[n - 3].eq_ignore_ascii_case("Microsoft.Resources")
        && segs[n - 2].eq_ignore_ascii_case("tags")
        && segs[n - 1].eq_ignore_ascii_case("default")
    {
        return tags::handle(&cx, &segs[..n - 4]);
    }

    let rest_lc: Vec<String> = sc.rest.iter().map(|s| s.to_ascii_lowercase()).collect();
    let rest: Vec<&str> = rest_lc.iter().map(String::as_str).collect();

    // Operation status URLs handed out in Location / Azure-AsyncOperation headers.
    // (Event Grid writes `operationsStatus` for its operation status URLs)
    if let Some(i) = rest.iter().position(|s| *s == "operationresults" || *s == "operationstatuses" || *s == "operationsstatus") {
        if method == "GET" && i + 1 < rest.len() {
            if rest[i] == "operationstatuses" && i >= 2 && rest[i - 2] == "deployments" {
                if let Some(sub) = &sc.sub {
                    return deployments::operation_status(&cx, sub, sc.rg.as_deref().unwrap_or(""), &sc.rest[i - 1]);
                }
            }
            if rest[i] == "operationsstatus" {
                // Event Grid's form: the id is the address of the status itself and the name is the id in lower case (recording
                // `test_domain.pyTestMgmtEventGridtest_domain.json`, entries 1, 4 and 8)
                let api = req.query.get("api-version").unwrap_or("");
                let id = format!("https://management.azure.com{}?api-version={api}", req.decoded_path());
                return Ok(util::json_ok(
                    200,
                    &serde_json::json!({ "id": id, "name": sc.rest[i + 1].to_ascii_lowercase(), "status": "Succeeded" }),
                ));
            }
            return Ok(operation_result(rest[i] != "operationresults", &sc.rest[i + 1]));
        }
    }

    // `HEAD /providers/Microsoft.DocumentDB/databaseAccountNames/{name}` (Cosmos DB "check name exists", what the azurerm
    // provider asks before it creates an account): 200 when an account has the name, 404 when it is free.
    if method == "HEAD" && sc.rg.is_none() {
        if let ["providers", ns, "databaseaccountnames", name] = rest.as_slice() {
            if ns.eq_ignore_ascii_case("microsoft.documentdb") {
                let taken = inner
                    .state
                    .lock()
                    .resources
                    .values()
                    .any(|r| r.ns == "Microsoft.DocumentDB" && r.types.len() == 1 && r.name().eq_ignore_ascii_case(name));
                return Ok(util::empty(if taken { 200 } else { 404 }));
            }
        }
    }

    match (&sc.sub, &sc.rg) {
        (None, _) => match (method, rest.as_slice()) {
            ("GET", ["subscriptions"]) => basics::list_subscriptions(&cx),
            ("GET", ["tenants"]) => basics::list_tenants(),
            ("GET", ["providers"]) => basics::list_providers(None),
            ("GET", ["providers", ns]) => basics::get_provider(None, &sc.rest[1], ns),
            ("POST", ["providers", ns, "register" | "unregister"]) => basics::get_provider(None, &sc.rest[1], ns),
            ("GET", ["providers", _, "operations"]) => Ok(util::list(vec![])),
            _ => Err(ArmError::not_implemented(format!("{method} {}", req.decoded_path()))),
        },
        (Some(sub), None) => match (method, rest.as_slice()) {
            ("GET", []) => basics::get_subscription(sub),
            ("GET", ["locations"]) => basics::list_locations(sub),
            ("GET", ["providers"]) => basics::list_providers(Some(sub)),
            ("GET", ["providers", ns]) => basics::get_provider(Some(sub), &sc.rest[1], ns),
            ("POST", ["providers", ns, "register" | "unregister"]) => basics::get_provider(Some(sub), &sc.rest[1], ns),
            (_, ["resourcegroups"]) | (_, ["resourcegroups", _]) => groups::handle(&cx, sub, None, &rest).await,
            ("GET", ["resources"]) => groups::list_resources(&cx, sub, None),
            (_, ["tagnames", ..]) => tags::tag_names(&cx, sub, &sc.rest),
            (_, ["providers", ns, ..]) => resources::handle(&cx, sub, None, ns, &sc.rest[2..]).await,
            _ => Err(ArmError::not_implemented(format!("{method} {}", req.decoded_path()))),
        },
        (Some(sub), Some(rg)) => match (method, rest.as_slice()) {
            (_, []) => groups::handle(&cx, sub, Some(rg), &rest).await,
            ("GET", ["resources"]) => groups::list_resources(&cx, sub, Some(rg)),
            // Older API versions address deployment operations without `providers/Microsoft.Resources`.
            (_, ["deployments", ..]) => deployments::handle(&cx, sub, Some(rg), &sc.rest[1..]).await,
            (_, ["providers", ns, ..]) => resources::handle(&cx, sub, Some(rg), ns, &sc.rest[2..]).await,
            _ => Err(ArmError::not_implemented(format!("{method} {}", req.decoded_path()))),
        },
    }
}

/// Every status URL answers "done" at once.
fn operation_result(status_form: bool, id: &str) -> Response {
    if status_form {
        util::json_ok(200, &serde_json::json!({ "id": id, "name": id, "status": "Succeeded" }))
    } else {
        util::empty(200)
    }
}
