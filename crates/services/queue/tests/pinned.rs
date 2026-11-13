//! Azurite-style path addressing on a pinned listener, and the gateway host form.

use std::sync::Arc;

use azemu_core::testing::{Client, TestServer};
use azemu_core::Emulator;
use azemu_queue::QueueService;

mod common;
use common::{error_code, msg_body, tag, tags};

const KEY: &str = "Authorization";
const AUTH: &str = "SharedKey devstoreaccount1:c2ln";

#[tokio::test]
async fn path_style_account_on_a_pinned_port() {
    let dir = std::env::temp_dir().join(format!("azemu-queue-pinned-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(QueueService::new()))
        .pinned("127.0.0.1:0".parse().unwrap(), "queue")
        .cert_dir(dir.clone())
        .start()
        .await
        .expect("start");
    let addr = emu.pinned_addr("queue").expect("pinned address");
    let base = format!("http://{addr}/devstoreaccount1");
    let c = Client::new();
    let h = [(KEY, AUTH)];

    assert_eq!(c.send("PUT", &format!("{base}/pinq"), &h, "").await.status, 201);
    let r = c.send("POST", &format!("{base}/pinq/messages"), &h, msg_body("hello")).await;
    assert_eq!(r.status, 201);
    let r = c.send("GET", &format!("{base}/pinq/messages"), &h, "").await;
    assert_eq!(tag(&r.text(), "MessageText").as_deref(), Some("hello"));

    // the service URL in listings is the path-style URL the client used
    let r = c.send("GET", &format!("{base}?comp=list"), &h, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"http://{addr}/devstoreaccount1/\"")), "{}", r.text());
    assert_eq!(tags(&r.text(), "Name"), ["pinq"]);
    // the marker names the account the client used
    c.send("PUT", &format!("{base}/pinr"), &h, "").await;
    let r = c.send("GET", &format!("{base}?comp=list&maxresults=1"), &h, "").await;
    assert_eq!(tag(&r.text(), "NextMarker").as_deref(), Some("/devstoreaccount1/pinr"));

    // another account in the path is another account
    let other = format!("http://{addr}/otheraccount");
    let r = c.send("GET", &format!("{other}/pinq/messages"), &h, "").await;
    assert_eq!((r.status, error_code(&r).as_str()), (404, "QueueNotFound"));
    assert_eq!(c.send("GET", &format!("{other}?comp=list"), &h, "").await.text().matches("<Queue>").count(), 0);

    // a trailing slash after the account is the service root
    let r = c.send("GET", &format!("{base}/?comp=list"), &h, "").await;
    assert_eq!(r.status, 200);

    // SAS works the same on the pinned listener
    let sas = "sv=2021-08-06&sp=r&se=2099-01-01T00:00:00Z&sig=x";
    let r = c.send("GET", &format!("{base}/pinq/messages?peekonly=true&{sas}"), &[], "").await;
    assert_eq!(r.status, 200);

    emu.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn host_style_and_path_style_see_the_same_account() {
    let dir = std::env::temp_dir().join(format!("azemu-queue-pinned2-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(QueueService::new()))
        .gateway("127.0.0.1:0".parse().unwrap())
        .pinned("127.0.0.1:0".parse().unwrap(), "queue")
        .cert_dir(dir.clone())
        .start()
        .await
        .expect("start");
    let gw = emu.gateway_addrs()[0].port();
    let pin = emu.pinned_addr("queue").unwrap();
    let c = Client::new();
    let h = [(KEY, AUTH)];

    c.send("PUT", &format!("http://devstoreaccount1.queue.localhost:{gw}/shared"), &h, "").await;
    let r = c.send("GET", &format!("http://{pin}/devstoreaccount1?comp=list"), &h, "").await;
    assert_eq!(tags(&r.text(), "Name"), ["shared"]);
    emu.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn gateway_service_url_is_host_style() {
    let srv = TestServer::start(vec![Arc::new(QueueService::new())]).await;
    let base = srv.url("queue", "myacct");
    let h = [(KEY, "SharedKey myacct:x")];
    let r = srv.send("GET", &format!("{base}/?comp=list"), &h, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"{base}/\"")), "{}", r.text());
    // the same over TLS
    let tls = srv.https_url("queue", "myacct");
    let r = srv.send("PUT", &format!("{tls}/tlsq"), &h, "").await;
    assert_eq!(r.status, 201);
    let r = srv.send("GET", &format!("{tls}/?comp=list"), &h, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"{tls}/\"")));
}
