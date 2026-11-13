//! Azurite-style path addressing on a pinned listener.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::Client;
use azemu_core::Emulator;

#[tokio::test]
async fn path_style_account_on_a_pinned_port() {
    let dir = std::env::temp_dir().join(format!("azemu-blob-pinned-{}", std::process::id()));
    let emu = Emulator::builder()
        .service(Arc::new(BlobService::new()))
        .pinned("127.0.0.1:0".parse().unwrap(), "blob")
        .cert_dir(dir)
        .start()
        .await
        .expect("start");
    let addr = emu.pinned_addr("blob").expect("pinned address");
    let base = format!("http://{addr}/devstoreaccount1");
    let c = Client::new();
    let auth = [("Authorization", "SharedKey devstoreaccount1:c2ln")];

    assert_eq!(c.send("PUT", &format!("{base}/pinc?restype=container"), &auth, "").await.status, 201);
    let r = c.send("PUT", &format!("{base}/pinc/dir/blob.txt"), &[auth[0], ("x-ms-blob-type", "BlockBlob")], "pinned").await;
    assert_eq!(r.status, 201);
    let r = c.send("GET", &format!("{base}/pinc/dir/blob.txt"), &auth, "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "pinned"));

    // URLs in listings are the path-style URLs the client used
    let r = c.send("GET", &format!("{base}/pinc?restype=container&comp=list"), &auth, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"http://{addr}/devstoreaccount1/\"")), "{}", r.text());
    assert!(r.text().contains("<Name>dir/blob.txt</Name>"));
    let r = c.send("GET", &format!("{base}?comp=list"), &auth, "").await;
    assert!(r.text().contains("<Name>pinc</Name>"));

    // another account in the path is another account
    let other = format!("http://{addr}/otheraccount");
    let r = c.send("GET", &format!("{other}/pinc/dir/blob.txt"), &auth, "").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (404, Some("ContainerNotFound")));

    // a copy source given in path style resolves inside the emulator
    let src = format!("{base}/pinc/dir/blob.txt");
    let r = c.send("PUT", &format!("{base}/pinc/copy"), &[auth[0], ("x-ms-copy-source", &src)], "").await;
    assert_eq!(r.status, 202);
    assert_eq!(c.send("GET", &format!("{base}/pinc/copy"), &auth, "").await.text(), "pinned");

    // a batch sub-request path carries the account in path style
    let boundary = "batch_pin";
    let body = format!("--{boundary}\r\nContent-Type: application/http\r\nContent-ID: 0\r\n\r\nDELETE /devstoreaccount1/pinc/copy HTTP/1.1\r\nAuthorization: SharedKey devstoreaccount1:c2ln\r\nContent-Length: 0\r\n\r\n\r\n--{boundary}--\r\n");
    let ct = format!("multipart/mixed; boundary={boundary}");
    let r = c.send("POST", &format!("{base}/pinc?restype=container&comp=batch"), &[auth[0], ("Content-Type", &ct)], body).await;
    assert_eq!(r.status, 202);
    assert!(r.text().contains("HTTP/1.1 202 Accepted"), "{}", r.text());
    assert_eq!(c.send("GET", &format!("{base}/pinc/copy"), &auth, "").await.status, 404);

    // SAS works the same on the pinned listener
    let sas = "sv=2021-08-06&sr=b&sp=r&se=2099-01-01T00:00:00Z&sig=x";
    let r = c.send("GET", &format!("{base}/pinc/dir/blob.txt?{sas}"), &[], "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "pinned"));

    // no credentials at all
    let r = c.send("GET", &format!("{base}/pinc/dir/blob.txt"), &[], "").await;
    assert_eq!((r.status, r.header("x-ms-error-code")), (401, Some("NoAuthenticationInformation")));

    emu.shutdown().await;
}

#[tokio::test]
async fn reset_drops_everything() {
    let srv = azemu_core::testing::TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let base = srv.url("blob", "acct");
    let auth = [("Authorization", "SharedKey acct:c2ln")];
    srv.send("PUT", &format!("{base}/cnt?restype=container"), &auth, "").await;
    assert_eq!(srv.send("GET", &format!("{base}/cnt?restype=container"), &auth, "").await.status, 200);
    srv.reset();
    assert_eq!(srv.send("GET", &format!("{base}/cnt?restype=container"), &auth, "").await.status, 404);
}
