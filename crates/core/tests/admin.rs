mod common;

use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::{Mount, Service};
use common::*;

#[tokio::test]
async fn health_reset_and_ca() {
    let a = Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")]));
    let b = Arc::new(Probe::new("beta", vec![Mount::AccountHost("beta")]));
    let srv = TestServer::start(vec![a.clone() as Arc<dyn Service>, b.clone() as Arc<dyn Service>]).await;
    let base = srv.gateway_url();

    let r = srv.send("GET", &format!("{base}/_azemu/health"), &[], "").await;
    assert_eq!(r.status, 200);
    let health = r.json();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["services"], serde_json::json!(["alpha", "beta"]));
    // the build: the version of the crate, and the git commit or "unknown" (nothing is committed in some trees)
    assert_eq!(health["version"], azemu_core::VERSION);
    assert_eq!(health["commit"], azemu_core::GIT_COMMIT);
    assert!(!health["commit"].as_str().unwrap().is_empty());
    assert!(r.header("content-type").unwrap().starts_with("application/json"));

    // health also answers on a service host
    let r = srv.send("GET", &format!("http://anything.blob.localhost:{}/_azemu/health", srv.port()), &[], "").await;
    assert_eq!(r.status, 200);

    use std::sync::atomic::Ordering::SeqCst;
    assert_eq!(a.resets.load(SeqCst), 0);
    let r = srv.send("POST", &format!("{base}/_azemu/reset"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), serde_json::json!({}));
    assert_eq!(a.resets.load(SeqCst), 1);
    assert_eq!(b.resets.load(SeqCst), 1);
    srv.reset();
    assert_eq!(a.resets.load(SeqCst), 2);
    assert_eq!(b.resets.load(SeqCst), 2);

    // wrong method does not reset
    let r = srv.send("GET", &format!("{base}/_azemu/reset"), &[], "").await;
    assert_eq!(r.status, 405);
    assert_eq!(a.resets.load(SeqCst), 2);

    let r = srv.send("GET", &format!("{base}/_azemu/ca.pem"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/x-pem-file"));
    assert_eq!(r.text(), srv.ca_pem());
    assert!(r.text().starts_with("-----BEGIN CERTIFICATE-----"));
}

fn two() -> (Arc<Probe>, Arc<Probe>) {
    (
        Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")]).with_account_reset()),
        Arc::new(Probe::new("beta", vec![Mount::AccountHost("beta")])),
    )
}

#[tokio::test]
async fn reset_one_service() {
    let (a, b) = two();
    let srv = TestServer::start(vec![a.clone() as Arc<dyn Service>, b.clone() as Arc<dyn Service>]).await;
    let base = srv.gateway_url();
    use std::sync::atomic::Ordering::SeqCst;
    let r = srv.send("POST", &format!("{base}/_azemu/reset?service=beta"), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), serde_json::json!({"reset": ["beta"], "notSupported": []}));
    assert_eq!((a.resets.load(SeqCst), b.resets.load(SeqCst)), (0, 1));
    // an unknown service is a 400 that names the ones there are, and nothing is reset
    let r = srv.send("POST", &format!("{base}/_azemu/reset?service=gamma"), &[], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["code"], "UnknownService");
    assert!(r.text().contains("alpha, beta"), "{}", r.text());
    assert_eq!((a.resets.load(SeqCst), b.resets.load(SeqCst)), (0, 1));
    // an unknown parameter is a 400 too (a typing mistake must not empty everything), and so is an empty value
    for q in ["servise=beta", "service=beta&acount=x", "service=", "account=", "x"] {
        let r = srv.send("POST", &format!("{base}/_azemu/reset?{q}"), &[], "").await;
        assert_eq!(r.status, 400, "{q}: {}", r.text());
    }
    assert_eq!((a.resets.load(SeqCst), b.resets.load(SeqCst)), (0, 1));
    // no parameter at all: everything, and the answer is the old `{}`
    let r = srv.send("POST", &format!("{base}/_azemu/reset"), &[], "").await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!({})));
    assert_eq!((a.resets.load(SeqCst), b.resets.load(SeqCst)), (1, 2));
}

#[tokio::test]
async fn reset_one_account() {
    let (a, b) = two();
    let srv = TestServer::start(vec![a.clone() as Arc<dyn Service>, b.clone() as Arc<dyn Service>]).await;
    let base = srv.gateway_url();
    use std::sync::atomic::Ordering::SeqCst;
    // every service that can does it; the one that cannot is named, and keeps its state
    let r = srv.send("POST", &format!("{base}/_azemu/reset?account=acct1"), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), serde_json::json!({"reset": ["alpha"], "notSupported": ["beta"]}));
    assert_eq!(*a.account_resets.lock().unwrap(), vec!["acct1".to_string()]);
    assert_eq!((a.resets.load(SeqCst), b.resets.load(SeqCst)), (0, 0), "no whole-service reset");
    // service and account together
    let r = srv.send("POST", &format!("{base}/_azemu/reset?service=alpha&account=acct2"), &[], "").await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!({"reset": ["alpha"], "notSupported": []})));
    assert_eq!(*a.account_resets.lock().unwrap(), vec!["acct1".to_string(), "acct2".to_string()]);
    // only a service that cannot: nothing happens, and the answer says so instead of "ok"
    let r = srv.send("POST", &format!("{base}/_azemu/reset?service=beta&account=acct1"), &[], "").await;
    assert_eq!(r.status, 501, "{}", r.text());
    assert!(r.text().contains("beta") && r.text().contains("nothing was reset"), "{}", r.text());
    assert_eq!(b.resets.load(SeqCst), 0);
    // names are decoded
    let r = srv.send("POST", &format!("{base}/_azemu/reset?service=alpha&account=a%20b"), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(a.account_resets.lock().unwrap().last().unwrap(), "a b");
}

#[tokio::test]
async fn config_shows_what_is_running_and_goes_through_the_same_checks() {
    let (a, b) = two();
    let srv = TestServer::start(vec![a as Arc<dyn Service>, b as Arc<dyn Service>]).await;
    let base = srv.gateway_url();
    let r = srv.send("GET", &format!("{base}/_azemu/config"), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let c = r.json();
    assert_eq!(c["version"], azemu_core::VERSION);
    assert_eq!(c["commit"], azemu_core::GIT_COMMIT);
    assert_eq!(c["services"], serde_json::json!(["alpha", "beta"]));
    let listeners = c["listeners"].as_array().unwrap();
    assert!(
        listeners.iter().any(|l| l["kind"] == "gateway" && l["address"].as_str().unwrap().ends_with(&format!(":{}", srv.port()))),
        "{c}"
    );
    assert!(c["dataDirectory"].is_null(), "{c}");
    assert!(c["allowedHosts"]["builtIn"].as_array().unwrap().iter().any(|h| h == "*.localhost"), "{c}");
    assert!(c["allowedOrigins"].as_array().unwrap().is_empty());
    assert_eq!(c["crossSiteCheck"], "on");
    assert!(c["settings"].is_array(), "{c}");
    assert!(c["unreadVariables"].is_array(), "{c}");
    // not a write
    assert_eq!(srv.send("POST", &format!("{base}/_azemu/config"), &[], "").await.status, 405);
    // a host the emulator does not serve (DNS rebinding) and a request another site sent are refused, as on every admin path
    for path in ["/_azemu/config", "/_azemu/unsupported", "/_azemu/health"] {
        let (status, body) = raw_get(srv.port(), "evil.example.com", path, &[]).await;
        assert_eq!(status, 403, "{path}: {body}");
        assert!(!body.contains("\"version\""), "{path} leaked the configuration to a refused host");
        let (status, body) =
            raw_get(srv.port(), "localhost", path, &[("Sec-Fetch-Site", "cross-site"), ("Origin", "https://evil.example.com")]).await;
        assert_eq!(status, 403, "{path}: {body}");
        assert!(!body.contains("\"version\""), "{path} leaked the configuration to another site");
        // and the same path is served to an ordinary client
        let (status, _) = raw_get(srv.port(), "localhost", path, &[]).await;
        assert_eq!(status, 200, "{path}");
    }
}

#[tokio::test]
async fn answers_with_status_501_are_counted_by_service_and_operation() {
    let (a, b) = two();
    let srv = TestServer::start(vec![a as Arc<dyn Service>, b as Arc<dyn Service>]).await;
    let alpha = srv.url("alpha", "acct");
    let beta = srv.url("beta", "acct");
    for _ in 0..3 {
        assert_eq!(srv.send("GET", &format!("{alpha}/unbuilt"), &[], "").await.status, 501);
    }
    assert_eq!(srv.send("GET", &format!("{beta}/namedunbuilt"), &[], "").await.status, 501);
    assert_eq!(srv.send("GET", &format!("{beta}/namedunbuilt"), &[], "").await.status, 501);
    // the body of the 501 is the service's own, unchanged
    let r = srv.send("GET", &format!("{beta}/namedunbuilt"), &[], "").await;
    assert_eq!(r.json()["error"]["message"], "probe: GetThing for acct-123");
    // ARM's part: a type stored without being emulated; true the first time only
    let first = srv.send("GET", &format!("{alpha}/stored"), &[], "").await.text();
    let second = srv.send("GET", &format!("{alpha}/stored"), &[], "").await.text();
    assert_eq!((first.as_str(), second.as_str()), ("true", "false"));

    let r = srv.send("GET", &format!("{}/_azemu/unsupported", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    let rows: Vec<(String, String, u64)> = v["notImplemented"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["service"].as_str().unwrap().to_string(), e["operation"].as_str().unwrap().to_string(), e["count"].as_u64().unwrap()))
        .collect();
    assert!(
        rows.contains(&("alpha".into(), "GET/unbuilt".into(), 3)),
        "counted by the shape of the request when the service names nothing: {rows:?}"
    );
    assert!(rows.contains(&("beta".into(), "GetThing".into(), 3)), "counted by the name the service gave: {rows:?}");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(v["storedOnly"], serde_json::json!([{"resourceType": "Microsoft.Web/sites", "count": 2}]));
    // the list can be emptied between two test runs
    let r = srv.send("DELETE", &format!("{}/_azemu/unsupported", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200);
    let v = srv.send("GET", &format!("{}/_azemu/unsupported", srv.gateway_url()), &[], "").await.json();
    assert_eq!(v, serde_json::json!({"notImplemented": [], "storedOnly": []}));
}

#[tokio::test]
async fn the_index_lists_the_admin_endpoints_and_the_prefixes_of_the_mounted_services() {
    let svc = Arc::new(Probe::new("idp", vec![Mount::Host("login"), Mount::PathPrefix("/_azemu/idp"), Mount::PathPrefix("/metadata/idp")]));
    let other = Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")]));
    let srv = TestServer::start(vec![svc as Arc<dyn Service>, other as Arc<dyn Service>]).await;
    let port = srv.port();
    for url in [
        format!("http://localhost:{port}/_azemu/"),
        format!("http://localhost:{port}/_azemu"),
        format!("http://localhost:{port}//_azemu//"),
        // on a host that names a service, as health does
        format!("http://x.alpha.localhost:{port}/_azemu/"),
    ] {
        let r = srv.send("GET", &url, &[], "").await;
        assert_eq!(r.status, 200, "{url}: {}", r.text());
        assert!(r.header("content-type").unwrap().starts_with("application/json"));
        let list = r.json()["endpoints"].as_array().unwrap().clone();
        let path = |p: &str| list.iter().find(|e| e["path"] == p).unwrap_or_else(|| panic!("{p} missing in {list:?}"));
        for p in [
            "/_azemu/",
            "/_azemu/health",
            "/_azemu/config",
            "/_azemu/ca.pem",
            "/_azemu/reset",
            "/_azemu/save",
            "/_azemu/faults",
            "/_azemu/inspect/...",
            "/_azemu/ui",
            "/_azemu/unsupported",
        ] {
            let e = path(p);
            assert!(e["description"].as_str().is_some_and(|d| !d.is_empty()), "{p}");
            assert!(e["methods"].as_array().is_some_and(|m| !m.is_empty()), "{p}");
        }
        assert_eq!(path("/_azemu/reset")["methods"], serde_json::json!(["POST"]));
        // only the /_azemu/ prefixes of services, not their other mounts
        assert_eq!(path("/_azemu/idp/...")["service"], "idp");
        assert!(list.iter().all(|e| e["path"] != "/metadata/idp/..."), "{list:?}");
        assert_eq!(list.len(), 11);
    }
    let r = srv.send("HEAD", &format!("http://localhost:{port}/_azemu/"), &[], "").await;
    assert_eq!(r.status, 200);
    let r = srv.send("POST", &format!("http://localhost:{port}/_azemu/"), &[], "").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
}

#[tokio::test]
async fn an_unknown_admin_path_is_an_error_that_points_at_the_index() {
    let srv = TestServer::start(vec![Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")])) as Arc<dyn Service>]).await;
    let port = srv.port();
    for p in ["/_azemu/nope", "/_azemu/health/more", "/_azemu/oauth2/token"] {
        let r = srv.send("GET", &format!("http://localhost:{port}{p}"), &[], "").await;
        assert_eq!(r.status, 404, "{p}");
        assert_eq!(r.json()["error"]["code"], "NotFound", "{p}");
        assert!(r.json()["error"]["message"].as_str().unwrap().contains("GET /_azemu/"), "{p}");
    }
}

/// A web page can send a form or `text/plain` request to `localhost` without asking first (no preflight). `application/json` is not one of the
/// types a page may send that way, so an admin request with a body from a browser has to say it, and the answer to a page that does not is a 415.
/// A program that is not a browser (`curl -d` sends a form type) is not held to it.
#[tokio::test]
async fn an_admin_request_with_a_body_from_a_browser_must_say_it_is_json() {
    const BROWSER: &str =
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.6 Safari/605.1.15";
    let svc = Arc::new(Probe::new("idp", vec![Mount::PathPrefix("/_azemu/identity")]));
    let alpha = Arc::new(Probe::new("alpha", vec![Mount::AccountHost("alpha")]));
    let srv = TestServer::start(vec![svc as Arc<dyn Service>, alpha as Arc<dyn Service>]).await;
    let base = srv.gateway_url();
    let rule = r#"{"fault":{"kind":"drop"},"path":"/never/*"}"#;
    for (method, path) in
        [("POST", "/_azemu/faults"), ("PUT", "/_azemu/faults"), ("POST", "/_azemu/identity/anything"), ("PUT", "/_azemu/identity/x")]
    {
        for ct in
            ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/octet-stream", "text/json"]
        {
            let r = srv.send(method, &format!("{base}{path}"), &[("content-type", ct), ("user-agent", BROWSER)], rule).await;
            assert_eq!(r.status, 415, "{method} {path} as {ct}: {}", r.text());
            assert_eq!(r.json()["error"]["code"], "UnsupportedMediaType");
            assert!(r.text().contains("application/json"), "{}", r.text());
        }
        // no Content-Type at all, with a body
        let r = srv.send(method, &format!("{base}{path}"), &[("user-agent", BROWSER)], rule).await;
        assert_eq!(r.status, 415, "{method} {path} with no type: {}", r.text());
    }
    // nothing was added
    assert_eq!(srv.send("GET", &format!("{base}/_azemu/faults"), &[], "").await.json()["rules"], serde_json::json!([]));
    // JSON, with parameters and in any case, and the `+json` types, go through, from a browser too
    for ct in ["application/json", "Application/JSON; charset=utf-8", "application/merge-patch+json"] {
        let r = srv.send("POST", &format!("{base}/_azemu/faults"), &[("content-type", ct), ("user-agent", BROWSER)], rule).await;
        assert_eq!(r.status, 201, "{ct}: {}", r.text());
    }
    // a program that is not a browser is not held to the rule: `curl -d` sends a form type, and scripts written before the rule send none
    for headers in [
        vec![("content-type", "application/x-www-form-urlencoded"), ("user-agent", "curl/8.5.0")],
        vec![("content-type", "application/x-www-form-urlencoded")],
        vec![("user-agent", "python-requests/2.32.3")],
        vec![],
    ] {
        let r = srv.send("POST", &format!("{base}/_azemu/faults"), &headers, rule).await;
        assert_eq!(r.status, 201, "{headers:?}: {}", r.text());
    }
    let r = srv.send("POST", &format!("{base}/_azemu/identity/anything"), &[("content-type", "application/json")], "{}").await;
    assert_ne!(r.status, 415);
    // a request without a body needs no type: reset, save, a DELETE (many HTTP libraries send a form type with an empty body)
    assert_eq!(srv.send("POST", &format!("{base}/_azemu/reset"), &[], "").await.status, 200);
    assert_eq!(srv.send("DELETE", &format!("{base}/_azemu/faults"), &[], "").await.status, 200);
    let r = srv.send("POST", &format!("{base}/_azemu/reset"), &[("content-type", "application/x-www-form-urlencoded")], "").await;
    assert_eq!(r.status, 200, "{}", r.text());
    // the sign-in protocol of the identity service is a form, as OAuth 2.0 says (a doubled slash is the same path the server routes)
    for path in ["/_azemu/identity/login/t1/oauth2/v2.0/token", "/_azemu//identity/login/t1/oauth2/v2.0/devicecode"] {
        let r = srv
            .send(
                "POST",
                &format!("{base}{path}"),
                &[("content-type", "application/x-www-form-urlencoded")],
                "client_id=x&grant_type=client_credentials",
            )
            .await;
        assert_ne!(r.status, 415, "{path}: {}", r.text());
    }
    // but the rest of the identity service's admin paths are JSON
    let r =
        srv.send("POST", &format!("{base}/_azemu/identity/loginx"), &[("content-type", "text/plain"), ("user-agent", BROWSER)], "x").await;
    assert_eq!(r.status, 415, "{}", r.text());
    // the rule is for the admin API only: a service request keeps any type
    let r = srv.send("POST", &format!("http://thing.alpha.localhost:{}/x", srv.port()), &[("content-type", "text/plain")], "hello").await;
    assert_eq!(r.status, 200, "{}", r.text());
}
