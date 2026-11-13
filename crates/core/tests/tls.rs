mod common;

use std::process::Command;
use std::sync::Arc;

use azemu_core::testing::{Client, EchoService, TestServer};
use azemu_core::Emulator;
use common::*;

#[tokio::test]
async fn http_and_https_on_the_same_port_verified_against_ca() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let port = srv.port();

    let http = srv.send("GET", &format!("http://acct.echo.localhost:{port}/p"), &[], "").await;
    assert_eq!(http.json()["endpoint"]["scheme"], "http");

    // rustls client, certificate checked against ca.pem for the SNI name
    for name in ["acct.echo.localhost", "other-acct.echo.localhost", "UPPER.echo.localhost", "x.echo.core.localhost"] {
        let r = srv.send("GET", &format!("https://{name}:{port}/p?x=1"), &[], "").await;
        assert_eq!(r.status, 200, "{name}");
        let v = r.json();
        assert_eq!(v["endpoint"]["scheme"], "https");
        assert_eq!(v["endpoint"]["base_url"], format!("https://{name}:{port}"));
        assert_eq!(v["path"], "/p");
    }

    let r = srv.send("PUT", &format!("https://acct.echo.localhost:{port}/p"), &[], vec![7u8; 100_000]).await;
    assert_eq!(r.json()["body_len"], 100_000);

    // a client without the CA, or with a different CA, must reject the server
    let url = format!("https://acct.echo.localhost:{port}/p");
    assert!(Client::new().try_send("GET", &url, &[], "").await.is_err());
    let other = TestServer::start(vec![]).await;
    let wrong = Client::with_ca(other.ca_pem());
    assert!(wrong.try_send("GET", &url, &[], "").await.is_err());
}

// curl blocks the thread, so the server needs its own worker threads
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_with_curl_and_no_sni() {
    let srv = TestServer::start(vec![Arc::new(EchoService::new())]).await;
    let port = srv.port();
    let dir = temp_dir("curl");
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, srv.ca_pem()).unwrap();

    let curl = |args: &[String]| {
        let out = Command::new("curl")
            .args(["-sS", "--max-time", "10", "--cacert", ca.to_str().unwrap()])
            .args(args)
            .output()
            .expect("curl is installed");
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // SNI name, resolved by hand
    let (ok, out, err) =
        curl(&["--resolve".into(), format!("acct.echo.localhost:{port}:127.0.0.1"), format!("https://acct.echo.localhost:{port}/x")]);
    assert!(ok, "curl failed: {err}");
    assert!(out.contains("\"account\":\"acct\""), "{out}");
    assert!(out.contains("\"scheme\":\"https\""), "{out}");

    // IP literal: no SNI is sent, the localhost certificate with IP SANs is served
    let (ok, out, err) = curl(&[format!("https://127.0.0.1:{port}/_azemu/health")]);
    assert!(ok, "curl failed: {err}");
    assert!(out.contains("\"status\":\"ok\""), "{out}");

    // plain http on the same port
    let (ok, out, err) = curl(&[format!("http://127.0.0.1:{port}/_azemu/health")]);
    assert!(ok, "curl failed: {err}");
    assert!(out.contains("\"status\":\"ok\""), "{out}");

    // without the CA curl must refuse
    let out = Command::new("curl").args(["-sS", "--max-time", "10", &format!("https://127.0.0.1:{port}/_azemu/health")]).output().unwrap();
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(dir);
}

// curl blocks the thread, so the server needs its own worker threads
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ipv6_listener_if_available() {
    let dir = temp_dir("v6");
    let v6 = "[::1]:0".parse().unwrap();
    let emu = match Emulator::builder().service(Arc::new(EchoService::new())).gateway(v6).cert_dir(dir).start().await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: no IPv6 loopback ({e})");
            return;
        }
    };
    let addr = emu.gateway_addrs()[0];
    assert!(addr.is_ipv6());
    // the test client always dials 127.0.0.1, so use curl for ::1
    let out =
        Command::new("curl").args(["-sS", "--max-time", "10", &format!("http://[::1]:{}/_azemu/health", addr.port())]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"status\":\"ok\""));
    emu.shutdown().await;
}

#[tokio::test]
async fn ca_is_reused_across_restarts() {
    let dir = temp_dir("reuse");
    let start = || async {
        Emulator::builder().service(Arc::new(EchoService::new())).gateway(loopback0()).cert_dir(dir.clone()).start().await.unwrap()
    };
    let first = start().await;
    let pem1 = first.ca_pem().to_string();
    assert_eq!(std::fs::read_to_string(dir.join("ca.pem")).unwrap(), pem1);
    assert!(dir.join("ca.key").exists());
    first.shutdown().await;

    let second = start().await;
    assert_eq!(second.ca_pem(), pem1);
    // a leaf minted by the second run chains to the CA of the first run
    let port = gw_port(&second);
    let c = Client::with_ca(&pem1);
    let r = c.send("GET", &format!("https://acct.echo.localhost:{port}/x"), &[], "").await;
    assert_eq!(r.status, 200);
    second.shutdown().await;

    // a different dir gives a different CA
    let third = Emulator::builder().gateway(loopback0()).cert_dir(temp_dir("reuse2")).start().await.unwrap();
    assert_ne!(third.ca_pem(), pem1);
    third.shutdown().await;
}

/// The CA is limited to localhost, names under it and loopback addresses, plus what `tls_name` adds.
/// A name outside is refused with a TLS alert (no certificate is made), not answered with a
/// certificate for something else.
#[tokio::test]
async fn names_outside_the_ca_limits_are_refused_and_tls_name_widens_them() {
    let dir = temp_dir("limits");
    let start = |extra: &[&str]| {
        let dir = dir.clone();
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        async move {
            let mut b = Emulator::builder().service(Arc::new(EchoService::new())).gateway(loopback0()).cert_dir(dir);
            for e in extra {
                b = b.tls_name(e);
            }
            b.start().await.unwrap()
        }
    };
    let emu = start(&[]).await;
    let ca1 = emu.ca_pem().to_string();
    let port = gw_port(&emu);
    let c = Client::with_ca(&ca1);
    for name in ["acct.echo.localhost", "localhost"] {
        let r = c.try_send("GET", &format!("https://{name}:{port}/_azemu/health"), &[], "").await;
        assert!(r.is_ok(), "{name}: {:?}", r.err());
    }
    for name in ["acct.echo.core.windows.net", "azemu", "example.com"] {
        let err = c.try_send("GET", &format!("https://{name}:{port}/_azemu/health"), &[], "").await.unwrap_err();
        assert!(
            err.to_string().contains("alert")
                || err.to_string().to_lowercase().contains("tls")
                || err.to_string().to_lowercase().contains("handshake"),
            "{name}: {err}"
        );
    }
    emu.shutdown().await;

    // the same dir with names: a new CA that allows them (and still not others)
    let emu = start(&["core.windows.net", "azemu"]).await;
    assert_ne!(emu.ca_pem(), ca1);
    let ca2 = emu.ca_pem().to_string();
    let port = gw_port(&emu);
    let c = Client::with_ca(&ca2);
    for name in ["acct.echo.core.windows.net", "azemu", "acct.echo.azemu", "acct.echo.localhost"] {
        let r = c.try_send("GET", &format!("https://{name}:{port}/_azemu/health"), &[], "").await;
        assert!(r.is_ok(), "{name}: {:?}", r.err());
    }
    assert!(c.try_send("GET", &format!("https://example.com:{port}/_azemu/health"), &[], "").await.is_err());
    emu.shutdown().await;

    // same names again: same CA
    let emu = start(&["azemu", "core.windows.net"]).await;
    assert_eq!(emu.ca_pem(), ca2);
    emu.shutdown().await;

    // a bad name is an error at start
    let e = Emulator::builder().gateway(loopback0()).cert_dir(temp_dir("badname")).tls_name("not a name").start().await;
    assert!(e.is_err());
}
