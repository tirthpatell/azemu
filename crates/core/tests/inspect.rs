//! The inspector route and the page, with a service that has a small tree of its own.

mod common;

use std::sync::Arc;

use azemu_core::inspect::{seg, InspectError, InspectRequest, InspectResult, Node};
use azemu_core::testing::TestServer;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use common::*;

/// Accounts "a1" and "a2"; a1 has 120 rows in `rows` and a name with a slash in it.
struct Tree;

#[async_trait]
impl Service for Tree {
    fn name(&self) -> &'static str {
        "tree"
    }
    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("tree")]
    }
    async fn handle(&self, _req: Request) -> Response {
        azemu_core::json_error::response(404, "NotFound", "nothing here")
    }
    fn reset(&self) {}
    fn inspect(&self, req: &InspectRequest) -> InspectResult {
        match req.segments.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] => Ok(Node::new("/tree", "service", "tree").prop("accounts", 2).with_items(
                vec![
                    serde_json::json!({"name": "a1", "path": req.path(&["a1"])}),
                    serde_json::json!({"name": "a2", "path": req.path(&["a2"])}),
                ],
                None,
            )),
            ["a1"] => Ok(Node::new(req.path(&["a1"]), "account", "a1").child("rows", "rows", Some(120), req.path(&["a1", "rows"]))),
            ["a1", "rows"] => {
                let (range, next) = req.page_range(120);
                let rows = range
                    .map(|i| serde_json::json!({"name": format!("row/{i}"), "path": req.path(&["a1", "rows", &format!("row/{i}")])}))
                    .collect();
                Ok(Node::new(req.path(&["a1", "rows"]), "rows", "rows").with_items(rows, next))
            }
            ["a1", "rows", name] => Ok(Node::new(req.path(&["a1", "rows", name]), "row", name).prop("secret", req.flag("reveal"))),
            ["a2"] => Ok(Node::new(req.path(&["a2"]), "account", "a2")),
            _ => Err(InspectError::not_found("no such node")),
        }
    }
}

async fn server() -> TestServer {
    let tree = Arc::new(Tree) as Arc<dyn Service>;
    let plain = Arc::new(Probe::new("plain", vec![Mount::AccountHost("plain")])) as Arc<dyn Service>;
    TestServer::start(vec![tree, plain]).await
}

#[tokio::test]
async fn the_root_lists_services_with_their_account_counts() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/_azemu/inspect", srv.gateway_url()), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let v = r.json();
    assert_eq!(v["kind"], "root");
    let names: Vec<&str> = v["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["_emulator", "tree"]);
    assert_eq!(v["children"][1]["count"], 2);
    assert_eq!(v["properties"]["servicesWithoutInspector"], serde_json::json!(["plain"]));
}

#[tokio::test]
async fn every_node_has_the_common_shape_and_paging_ends() {
    let srv = server().await;
    let base = srv.gateway_url();
    let r = srv.send("GET", &format!("{base}/_azemu/inspect/tree/a1/rows?limit=50"), &[], "").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    for k in ["path", "kind", "title", "properties", "children", "items", "next"] {
        assert!(v.get(k).is_some(), "missing {k}");
    }
    let mut seen = 0;
    let mut next = v["next"].as_str().map(str::to_string);
    seen += v["items"].as_array().unwrap().len();
    let mut pages = 1;
    while let Some(m) = next {
        let r = srv.send("GET", &format!("{base}/_azemu/inspect/tree/a1/rows?limit=50&marker={m}"), &[], "").await;
        assert_eq!(r.status, 200);
        let v = r.json();
        seen += v["items"].as_array().unwrap().len();
        next = v["next"].as_str().map(str::to_string);
        pages += 1;
        assert!(pages < 10);
    }
    assert_eq!((seen, pages), (120, 3));
}

#[tokio::test]
async fn a_name_with_a_slash_in_its_path_segment_is_one_segment() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/_azemu/inspect/tree/a1/rows/{}", srv.gateway_url(), seg("row/7")), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["title"], "row/7");
    assert_eq!(r.json()["path"], "/tree/a1/rows/row%2F7");
}

#[tokio::test]
async fn a_missing_service_account_or_node_is_a_404_and_a_bad_limit_a_400() {
    let srv = server().await;
    let base = srv.gateway_url();
    for (path, status) in [
        ("/_azemu/inspect/nothing", 404),
        ("/_azemu/inspect/tree/a3", 404),
        ("/_azemu/inspect/plain", 404),
        ("/_azemu/inspect/_emulator/nothing", 404),
        ("/_azemu/inspect/tree/a1/rows?limit=0", 400),
        ("/_azemu/inspect/tree/a1/rows?marker=!!", 400),
    ] {
        let r = srv.send("GET", &format!("{base}{path}"), &[], "").await;
        assert_eq!(r.status, status, "{path}");
        assert!(r.json()["error"]["code"].is_string(), "{path}");
    }
}

#[tokio::test]
async fn only_get_and_head_are_taken() {
    let srv = server().await;
    let base = srv.gateway_url();
    for m in ["POST", "PUT", "DELETE", "PATCH"] {
        let r = srv.send(m, &format!("{base}/_azemu/inspect/tree"), &[], "").await;
        assert_eq!(r.status, 405, "{m}");
        assert_eq!(r.header("allow"), Some("GET, HEAD"));
    }
    let r = srv.send("HEAD", &format!("{base}/_azemu/inspect/tree"), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn the_emulator_nodes_show_health_config_unsupported_and_faults() {
    let srv = server().await;
    let base = srv.gateway_url();
    let r = srv.send("GET", &format!("{base}/_azemu/inspect/_emulator"), &[], "").await;
    assert_eq!(r.status, 200);
    let names: Vec<String> = r.json()["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["health", "config", "unsupported", "faults"]);
    for n in names {
        let r = srv.send("GET", &format!("{base}/_azemu/inspect/_emulator/{n}"), &[], "").await;
        assert_eq!(r.status, 200, "{n}");
    }
    let r = srv.send("GET", &format!("{base}/_azemu/inspect/_emulator/health"), &[], "").await;
    assert_eq!(r.json()["properties"]["status"], "ok");
    // an unbuilt operation is listed
    srv.send("GET", &format!("http://x.plain.localhost:{}/unbuilt", srv.port()), &[], "").await;
    let r = srv.send("GET", &format!("{base}/_azemu/inspect/_emulator/unsupported"), &[], "").await;
    assert!(r.json()["items"].is_array());
}

#[tokio::test]
async fn a_cross_site_request_is_refused_also_with_reveal() {
    let srv = server().await;
    let base = srv.gateway_url();
    let path = "/_azemu/inspect/tree/a1/rows/x?reveal=1";
    for headers in [
        vec![("sec-fetch-site", "cross-site")],
        vec![("origin", "http://evil.example")],
        vec![("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "no-cors")],
    ] {
        let r = srv.send("GET", &format!("{base}{path}"), &headers, "").await;
        assert_eq!(r.status, 403, "{headers:?}");
        assert!(!r.text().contains("secret"));
        assert_eq!(r.json()["error"]["code"], "CrossSiteRequestRefused");
        assert!(r.header("access-control-allow-origin").is_none());
    }
    // the page and its files too
    for p in ["/_azemu/ui", "/_azemu/ui/app.js", "/_azemu/ui/app.css"] {
        let r = srv.send("GET", &format!("{base}{p}"), &[("sec-fetch-site", "cross-site")], "").await;
        assert_eq!(r.status, 403, "{p}");
    }
    // the same request from the page itself, or from a program, goes through
    let r = srv.send("GET", &format!("{base}{path}"), &[("sec-fetch-site", "same-origin")], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["properties"]["secret"], true);
    let r = srv.send("GET", &format!("{base}{path}"), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn the_page_is_three_files_with_the_right_types_and_a_strict_policy() {
    let srv = server().await;
    let base = srv.gateway_url();
    let policy = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    for (path, ct) in [
        ("/_azemu/ui", "text/html; charset=utf-8"),
        ("/_azemu/ui/", "text/html; charset=utf-8"),
        ("/_azemu/ui/app.js", "text/javascript; charset=utf-8"),
        ("/_azemu/ui/app.css", "text/css; charset=utf-8"),
    ] {
        let r = srv.send("GET", &format!("{base}{path}"), &[], "").await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.header("content-type"), Some(ct), "{path}");
        assert_eq!(r.header("content-security-policy"), Some(policy), "{path}");
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert!(!r.text().is_empty());
    }
    let html = srv.send("GET", &format!("{base}/_azemu/ui"), &[], "").await.text();
    assert!(html.contains("/_azemu/ui/app.js") && html.contains("/_azemu/ui/app.css"));
    // nothing is fetched from another host and nothing is inline
    assert!(!html.contains("http://") && !html.contains("https://"), "the page names another host");
    assert!(!html.contains("<script>") && !html.contains(" onclick=") && !html.contains("style="), "inline code or style in the page");
    let js = srv.send("GET", &format!("{base}/_azemu/ui/app.js"), &[], "").await.text();
    for banned in
        ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write", "eval(", "new Function", "setTimeout(\"", "javascript:"]
    {
        assert!(!js.contains(banned), "app.js uses {banned}");
    }
    assert!(!js.to_lowercase().contains("eval"), "app.js mentions eval");
    assert!(!js.contains("http://") && !js.contains("https://"), "app.js names another host");
    let r = srv.send("POST", &format!("{base}/_azemu/ui"), &[], "").await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn the_index_lists_the_inspector_and_the_page() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/_azemu/", srv.gateway_url()), &[], "").await;
    let text = r.text();
    assert!(text.contains("/_azemu/inspect/...") && text.contains("/_azemu/ui"));
}
