mod common;

use std::sync::Arc;

use azemu_core::testing::{Client, EchoService, TestServer};
use azemu_core::{Emulator, Mount, Service};
use common::*;

/// A server that also serves the real-Azure-looking names these tests route on (the host check refuses names
/// nobody listed: `tests/access.rs` covers that).
async fn start_open(services: Vec<Arc<dyn Service>>) -> TestServer {
    TestServer::start_configured(services, vec![], |b| {
        b.allow_host("core.windows.net")
            .allow_host("nip.io")
            .allow_host("microsoftonline.com")
            .allow_host("azure.com")
            .allow_host("example.com")
            .allow_host("example")
    })
    .await
}

fn echo() -> Arc<dyn Service> {
    Arc::new(EchoService::new())
}

#[tokio::test]
async fn gateway_host_style_forms() {
    let srv = start_open(vec![echo()]).await;
    let port = srv.port();
    for (host, account) in [
        ("acct.echo.localhost", "acct"),
        ("acct.echo.core.windows.net", "acct"),
        ("acct.echo.127.0.0.1.nip.io", "acct"),
        ("acct-secondary.echo.localhost", "acct"),
        ("ACCT.ECHO.LOCALHOST", "acct"),
        ("MixedCase.Echo.localhost", "mixedcase"),
    ] {
        let url = format!("http://{host}:{port}/c1/b%2Fx//y?a=1&b=x%20y");
        let r = srv.send("GET", &url, &[], "").await;
        assert_eq!(r.status, 200, "{host}: {}", r.text());
        let v = r.json();
        assert_eq!(v["account"], account, "{host}");
        // path passes through still percent-encoded, nothing collapsed
        assert_eq!(v["path"], "/c1/b%2Fx//y", "{host}");
        assert_eq!(v["raw_query"], "a=1&b=x%20y");
        assert_eq!(v["query"], serde_json::json!([["a", "1"], ["b", "x y"]]));
        assert_eq!(v["endpoint"]["scheme"], "http");
        assert_eq!(v["endpoint"]["authority"], format!("{host}:{port}"));
        assert_eq!(v["endpoint"]["path_style"], false);
        assert_eq!(v["endpoint"]["base_url"], format!("http://{host}:{port}"));
    }
}

#[tokio::test]
async fn gateway_request_fields() {
    let srv = start_open(vec![echo()]).await;
    let url = format!("{}/p", srv.url("echo", "acct"));
    let r = srv.send("PUT", &url, &[], "hello").await;
    let v = r.json();
    assert_eq!(v["method"], "PUT");
    assert_eq!(v["body_len"], 5);
    assert_eq!(v["service"], "echo");
    assert_eq!(v["request_id"].as_str().unwrap().len(), 36);
}

#[tokio::test]
async fn unknown_host_is_404_no_service_matched() {
    let svc: Arc<dyn Service> = Arc::new(Probe::new("idp", vec![Mount::Host("login"), Mount::PathPrefix("/metadata/identity")]));
    let srv = start_open(vec![echo(), svc]).await;
    let port = srv.port();
    for url in [
        format!("http://127.0.0.1:{port}/anything"),
        format!("http://nothing.example.com:{port}/x"),
        format!("http://acct.unknown.localhost:{port}/x"),
        format!("http://echo.localhost:{port}/x"), // label but no account
    ] {
        let r = srv.send("GET", &url, &[], "").await;
        assert_eq!(r.status, 404, "{url}");
        assert!(r.header("content-type").unwrap().starts_with("application/json"));
        let v = r.json();
        assert_eq!(v["error"]["code"], "NoServiceMatched");
        let m = v["error"]["message"].as_str().unwrap();
        assert!(m.contains("echo"), "{m}");
        assert!(m.contains(&format!("http://{{account}}.echo.localhost:{port}")), "{m}");
        assert!(m.contains("idp"), "{m}");
        assert!(m.contains(&format!("http://login.localhost:{port}")), "{m}");
        assert!(m.contains(&format!("http://localhost:{port}/metadata/identity")), "{m}");
    }
}

#[tokio::test]
async fn the_no_service_answer_shows_the_core_label_and_the_standard_ports() {
    let blob: Arc<dyn Service> = Arc::new(Probe::new("blob", vec![Mount::AccountHost("blob")]));
    let vault: Arc<dyn Service> = Arc::new(Probe::new("keyvault", vec![Mount::AccountHost("vault")]));
    let srv = start_open(vec![blob, vault]).await;
    let port = srv.port();
    let r = srv.send("GET", &format!("http://127.0.0.1:{port}/x"), &[], "").await;
    assert_eq!(r.status, 404);
    let m = r.json()["error"]["message"].as_str().unwrap().to_string();
    // the storage SDKs read a host with `localhost` and without `.core.` as an Azurite address
    assert!(m.contains(&format!("http://{{account}}.blob.core.localhost:{port}")), "{m}");
    assert!(!m.contains("{account}.blob.localhost"), "{m}");
    assert!(m.contains(&format!("http://{{account}}.vault.localhost:{port}")), "{m}");
    assert!(m.contains("--standard-ports") && m.contains("blob 10000") && m.contains("keyvault 10010"), "{m}");
    assert!(!m.contains("queue 10001"), "only mounted services are listed: {m}");
}

#[tokio::test]
async fn mount_host_and_path_prefix() {
    let idp: Arc<dyn Service> = Arc::new(Probe::new(
        "idp",
        vec![Mount::Host("login"), Mount::PathPrefix("/metadata/identity"), Mount::PathPrefix("/_azemu/identity")],
    ));
    let srv = start_open(vec![echo(), idp]).await;
    let port = srv.port();

    // Host("login"): account is "" and the path is untouched
    let r = srv.send("POST", &format!("http://login.localhost:{port}/tenant/oauth2/v2.0/token"), &[], "").await;
    let v = r.json();
    assert_eq!(v["service"], "idp");
    assert_eq!(v["account"], "");
    assert_eq!(v["path"], "/tenant/oauth2/v2.0/token");
    let r = srv.send("GET", &format!("http://LOGIN.microsoftonline.com:{port}/x"), &[], "").await;
    assert_eq!(r.json()["service"], "idp");

    // PathPrefix: not stripped; used when the host names no service
    for host in ["localhost", "127.0.0.1", "[::1]", "azemu", "nothing.example.com", "169.254.169.254"] {
        let r = srv.send("GET", &format!("http://{host}:{port}/metadata/identity/oauth2/token?resource=x"), &[], "").await;
        let v = r.json();
        assert_eq!(v["service"], "idp", "{host}");
        assert_eq!(v["path"], "/metadata/identity/oauth2/token");
        assert_eq!(v["account"], "");
        assert_eq!(v["raw_query"], "resource=x");
    }

    // /_azemu/ prefix mount reaches the service; admin paths still belong to core
    let r = srv.send("GET", &format!("http://localhost:{port}/_azemu/identity/msi/token"), &[], "").await;
    assert_eq!(r.json()["service"], "idp");
    let r = srv.send("GET", &format!("http://localhost:{port}/_azemu/health"), &[], "").await;
    assert_eq!(r.json()["status"], "ok");
    // unknown /_azemu/ path without a mount: 404
    let r = srv.send("GET", &format!("http://localhost:{port}/_azemu/nope"), &[], "").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["code"], "NotFound");
}

#[tokio::test]
async fn longest_path_prefix_wins() {
    let a: Arc<dyn Service> = Arc::new(Probe::new("a", vec![Mount::PathPrefix("/x")]));
    let b: Arc<dyn Service> = Arc::new(Probe::new("b", vec![Mount::PathPrefix("/x/y")]));
    let srv = start_open(vec![a, b]).await;
    let p = srv.port();
    let r = srv.send("GET", &format!("http://localhost:{p}/x/y/z"), &[], "").await;
    assert_eq!(r.json()["service"], "b");
    let r = srv.send("GET", &format!("http://localhost:{p}/x/q"), &[], "").await;
    assert_eq!(r.json()["service"], "a");
}

async fn pinned_setup(svc: Arc<dyn Service>) -> (azemu_core::RunningEmulator, Client, std::path::PathBuf) {
    let dir = temp_dir("pin");
    let emu =
        Emulator::builder().service(svc).gateway(loopback0()).pinned(loopback0(), "echo").cert_dir(dir.clone()).start().await.unwrap();
    (emu, Client::new(), dir)
}

#[tokio::test]
async fn pinned_host_style_path_style_and_default() {
    let (emu, c, _d) = pinned_setup(echo()).await;
    let pin = emu.pinned_addr("echo").unwrap();
    assert_ne!(pin.port(), 0);
    assert_ne!(pin.port(), gw_port(&emu));
    assert!(emu.pinned_addr("nope").is_none());
    let p = pin.port();

    // host style on a pinned port
    let r = c.send("GET", &format!("http://acct.echo.localhost:{p}/c1?x=1"), &[], "").await;
    let v = r.json();
    assert_eq!(v["account"], "acct");
    assert_eq!(v["path"], "/c1");
    assert_eq!(v["endpoint"]["path_style"], false);
    assert_eq!(v["endpoint"]["base_url"], format!("http://acct.echo.localhost:{p}"));

    // path style: the account segment is stripped from Request::path
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/devstoreaccount1/c1/b%2Fx?x=1"), &[], "").await;
    let v = r.json();
    assert_eq!(v["account"], "devstoreaccount1");
    assert_eq!(v["path"], "/c1/b%2Fx");
    assert_eq!(v["endpoint"]["path_style"], true);
    assert_eq!(v["endpoint"]["account"], "devstoreaccount1");
    assert_eq!(v["endpoint"]["base_url"], format!("http://127.0.0.1:{p}/devstoreaccount1"));
    assert_eq!(v["endpoint"]["authority"], format!("127.0.0.1:{p}"));

    // path style, account only
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/devstoreaccount1?comp=list"), &[], "").await;
    let v = r.json();
    assert_eq!(v["account"], "devstoreaccount1");
    assert_eq!(v["path"], "/");
    assert_eq!(v["raw_query"], "comp=list");
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/devstoreaccount1/"), &[], "").await;
    assert_eq!(r.json()["path"], "/");

    // nothing in the path: default account
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/"), &[], "").await;
    let v = r.json();
    assert_eq!(v["account"], "devaccount");
    assert_eq!(v["path"], "/");
    assert_eq!(v["endpoint"]["path_style"], false);
    assert_eq!(v["endpoint"]["base_url"], format!("http://127.0.0.1:{p}"));

    // admin API works on pinned listeners too
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/_azemu/health"), &[], "").await;
    assert_eq!(r.status, 200);

    // gateway still routes by host
    let g = gw_port(&emu);
    let r = c.send("GET", &format!("http://a.echo.localhost:{g}/x"), &[], "").await;
    assert_eq!(r.json()["account"], "a");
    emu.shutdown().await;
}

#[tokio::test]
async fn pinned_service_without_path_style_uses_default_account() {
    let svc: Arc<dyn Service> = Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]));
    let (emu, c, _d) = pinned_setup(svc).await;
    let p = emu.pinned_addr("echo").unwrap().port();
    let r = c.send("GET", &format!("http://127.0.0.1:{p}/acct/c1"), &[], "").await;
    let v = r.json();
    assert_eq!(v["account"], "default");
    assert_eq!(v["path"], "/acct/c1");
    assert_eq!(v["endpoint"]["path_style"], false);
    emu.shutdown().await;
}

#[tokio::test]
async fn on_a_pinned_port_a_bare_host_name_names_the_account() {
    // a service that takes its account from the host (Key Vault, Cosmos, Service Bus ...)
    let svc: Arc<dyn Service> = Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]));
    let (emu, _c, _d) = pinned_setup(svc).await;
    let p = emu.pinned_addr("echo").unwrap().port();
    let account = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap()["account"].as_str().unwrap().to_string();
    // a Compose service name or network alias is the account (and so the AMQP namespace, which is the first label too)
    assert_eq!(account(raw_get(p, "azemu", "/x", &[]).await.1), "azemu");
    assert_eq!(account(raw_get(p, "Vault2", "/x", &[]).await.1), "vault2");
    // `localhost`, addresses and dotted names that name no account use the service's default
    for host in ["localhost", "127.0.0.1", "[::1]", "host.docker.internal"] {
        assert_eq!(account(raw_get(p, host, "/x", &[]).await.1), "default", "{host}");
    }
    // a name with the label wins, as before
    assert_eq!(account(raw_get(p, "acct.echo.localhost", "/x", &[]).await.1), "acct");
    emu.shutdown().await;

    // a path style service keeps taking the account from the path, and its default when the path has none
    let (emu, _c, _d) = pinned_setup(echo()).await;
    let p = emu.pinned_addr("echo").unwrap().port();
    assert_eq!(account(raw_get(p, "azemu", "/devstoreaccount1/c1", &[]).await.1), "devstoreaccount1");
    assert_eq!(account(raw_get(p, "azemu", "/", &[]).await.1), "devaccount");
    emu.shutdown().await;
}

#[tokio::test]
async fn builder_errors() {
    let dir = temp_dir("err");
    // no listeners
    assert!(Emulator::builder().cert_dir(dir.clone()).start().await.is_err());
    // pin to a service that is not mounted
    let e = Emulator::builder().gateway(loopback0()).pinned(loopback0(), "blob").cert_dir(dir.clone()).start().await;
    assert!(e.is_err());
    // duplicate names
    let e = Emulator::builder().service(echo()).service(echo()).gateway(loopback0()).cert_dir(dir.clone()).start().await;
    assert!(e.is_err());
    // port already taken
    let (emu, _d) = start_gateway(vec![echo()]).await;
    let taken = emu.gateway_addrs()[0];
    let e = Emulator::builder().gateway(taken).cert_dir(dir).start().await;
    assert!(e.is_err());
    // an optional address that cannot bind is skipped
    let ok = Emulator::builder().gateway(loopback0()).gateway_optional(taken).cert_dir(temp_dir("opt")).start().await.unwrap();
    assert_eq!(ok.gateway_addrs().len(), 1);
    ok.shutdown().await;
    emu.shutdown().await;
}

fn idp_arm_echo() -> Vec<Arc<dyn Service>> {
    let idp: Arc<dyn Service> = Arc::new(Probe::new(
        "idp",
        vec![Mount::Host("login"), Mount::PathPrefix("/metadata/identity"), Mount::PathPrefix("/_azemu/identity")],
    ));
    let arm: Arc<dyn Service> = Arc::new(Probe::new("arm", vec![Mount::Host("management"), Mount::PathPrefix("/metadata/endpoints")]));
    vec![echo(), idp, arm]
}

async fn who(srv: &TestServer, method: &str, url: &str) -> (String, String, String) {
    let r = srv.send(method, url, &[], "").await;
    assert_eq!(r.status, 200, "{url}: {}", r.text());
    let v = r.json();
    (
        v["service"].as_str().unwrap_or("?").to_string(),
        v["account"].as_str().unwrap_or("?").to_string(),
        v["path"].as_str().unwrap_or("?").to_string(),
    )
}

#[tokio::test]
async fn host_that_names_a_service_beats_path_prefix() {
    let srv = start_open(idp_arm_echo()).await;
    let port = srv.port();
    // a blob-like path that starts like the identity or ARM mounts stays with the host's service
    for path in ["/metadata/identity/x", "/metadata/endpoints", "//metadata/identity/x"] {
        let (svc, account, got) = who(&srv, "GET", &format!("http://acct.echo.localhost:{port}{path}")).await;
        assert_eq!((svc.as_str(), account.as_str(), got.as_str()), ("echo", "acct", path), "{path}");
    }
    // the same for the `{label}.` form: `login.` is identity whatever the path
    let (svc, _, got) = who(&srv, "GET", &format!("http://management.localhost:{port}/metadata/identity/x")).await;
    assert_eq!((svc.as_str(), got.as_str()), ("arm", "/metadata/identity/x"));
    let (svc, _, got) = who(&srv, "GET", &format!("http://login.localhost:{port}/metadata/endpoints")).await;
    assert_eq!((svc.as_str(), got.as_str()), ("idp", "/metadata/endpoints"));
}

#[tokio::test]
async fn account_named_like_a_host_mount_is_still_an_account() {
    let srv = start_open(idp_arm_echo()).await;
    let port = srv.port();
    for account in ["login", "management"] {
        let (svc, acct, path) = who(&srv, "GET", &format!("http://{account}.echo.localhost:{port}/c1")).await;
        assert_eq!((svc.as_str(), acct.as_str(), path.as_str()), ("echo", account, "/c1"));
    }
    // and the plain host-mount forms keep working
    let (svc, acct, _) = who(&srv, "GET", &format!("http://login.localhost:{port}/t/oauth2/v2.0/token")).await;
    assert_eq!((svc.as_str(), acct.as_str()), ("idp", ""));
    let (svc, acct, _) = who(&srv, "GET", &format!("http://management.azure.com:{port}/subscriptions")).await;
    assert_eq!((svc.as_str(), acct.as_str()), ("arm", ""));
}

#[tokio::test]
async fn path_prefix_used_when_host_names_no_service() {
    let srv = start_open(idp_arm_echo()).await;
    let port = srv.port();
    for host in ["localhost", "127.0.0.1", "[::1]", "azemu", "host.docker.internal", "169.254.169.254"] {
        let (svc, acct, path) = who(&srv, "GET", &format!("http://{host}:{port}/metadata/endpoints")).await;
        assert_eq!((svc.as_str(), acct.as_str(), path.as_str()), ("arm", "", "/metadata/endpoints"), "{host}");
        let (svc, _, _) = who(&srv, "GET", &format!("http://{host}:{port}/metadata/identity/oauth2/token")).await;
        assert_eq!(svc, "idp", "{host}");
    }
}

#[tokio::test]
async fn prefix_matching_ignores_empty_segments_but_path_is_unchanged() {
    let srv = start_open(idp_arm_echo()).await;
    let port = srv.port();
    for path in ["//metadata/endpoints", "///metadata//endpoints/x"] {
        let (svc, _, got) = who(&srv, "GET", &format!("http://localhost:{port}{path}")).await;
        assert_eq!((svc.as_str(), got.as_str()), ("arm", path), "{path}");
    }
    let (svc, _, got) = who(&srv, "GET", &format!("http://localhost:{port}//_azemu/identity/msi/token")).await;
    assert_eq!((svc.as_str(), got.as_str()), ("idp", "//_azemu/identity/msi/token"));
    // admin paths too
    let r = srv.send("GET", &format!("http://localhost:{port}//_azemu/health"), &[], "").await;
    assert_eq!(r.json()["status"], "ok");
}

#[tokio::test]
async fn admin_paths_work_on_any_host() {
    let srv = start_open(idp_arm_echo()).await;
    let port = srv.port();
    for host in ["localhost", "127.0.0.1", "acct.echo.localhost", "login.localhost", "management.localhost", "x.unknown.example"] {
        let r = srv.send("GET", &format!("http://{host}:{port}/_azemu/health"), &[], "").await;
        assert_eq!(r.status, 200, "{host}");
        assert_eq!(r.json()["status"], "ok", "{host}");
        let r = srv.send("GET", &format!("http://{host}:{port}/_azemu/ca.pem"), &[], "").await;
        assert_eq!(r.status, 200, "{host}");
        // a service's admin API, mounted as /_azemu/<name>
        let (svc, acct, path) = who(&srv, "GET", &format!("http://{host}:{port}/_azemu/identity/msi/token")).await;
        assert_eq!((svc.as_str(), acct.as_str(), path.as_str()), ("idp", "", "/_azemu/identity/msi/token"), "{host}");
        let r = srv.send("POST", &format!("http://{host}:{port}/_azemu/reset"), &[], "").await;
        assert_eq!(r.status, 200, "{host}");
    }
}

#[tokio::test]
async fn pinned_port_picks_the_service_whatever_the_path() {
    let svc: Arc<dyn Service> = Arc::new(Probe::new("echo", vec![Mount::AccountHost("echo")]).path_style("devstoreaccount1"));
    let idp: Arc<dyn Service> = Arc::new(Probe::new("idp", vec![Mount::Host("login"), Mount::PathPrefix("/metadata/identity")]));
    let dir = temp_dir("pin2");
    let emu =
        Emulator::builder().service(svc).service(idp).gateway(loopback0()).pinned(loopback0(), "echo").cert_dir(dir).start().await.unwrap();
    let c = Client::new();
    let p = emu.pinned_addr("echo").unwrap().port();
    // Azurite path style: first segment is the account, even if it looks like a mount
    for (url, acct, path) in [
        (format!("http://127.0.0.1:{p}/devstoreaccount1/metadata/identity/x"), "devstoreaccount1", "/metadata/identity/x"),
        (format!("http://127.0.0.1:{p}/login/c1"), "login", "/c1"),
        (format!("http://127.0.0.1:{p}/metadata/identity/x"), "metadata", "/identity/x"),
    ] {
        let v = c.send("GET", &url, &[], "").await.json();
        assert_eq!(v["service"], "echo", "{url}");
        assert_eq!(v["account"], acct, "{url}");
        assert_eq!(v["path"], path, "{url}");
    }
    // a host that names another service does not matter on a pinned port
    let v = c.send("GET", &format!("http://login.localhost:{p}/c1"), &[], "").await.json();
    assert_eq!(v["service"], "echo");
    emu.shutdown().await;
}
