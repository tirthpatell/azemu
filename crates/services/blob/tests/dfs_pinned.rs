//! The `dfs` service on a pinned listener (Azurite-style path addressing) and on the gateway
//! next to the blob service.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::Client;
use azemu_core::Emulator;

#[tokio::test]
async fn path_style_account_on_a_pinned_dfs_port() {
    let dir = std::env::temp_dir().join(format!("azemu-dfs-pinned-{}", std::process::id()));
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let emu = Emulator::builder()
        .service(Arc::new(blob))
        .service(Arc::new(dfs))
        .pinned("127.0.0.1:0".parse().unwrap(), "dfs")
        .pinned("127.0.0.1:0".parse().unwrap(), "blob")
        .cert_dir(dir)
        .start()
        .await
        .expect("start");
    let dfs_addr = emu.pinned_addr("dfs").expect("dfs address");
    let blob_addr = emu.pinned_addr("blob").expect("blob address");
    let d = format!("http://{dfs_addr}/pinacct");
    let b = format!("http://{blob_addr}/pinacct");
    let c = Client::new();
    let auth = [("Authorization", "SharedKey pinacct:c2ln")];

    assert_eq!(c.send("PUT", &format!("{d}/pfs?resource=filesystem"), &auth, "").await.status, 201);
    assert_eq!(c.send("PUT", &format!("{d}/pfs/dir/file.txt?resource=file"), &auth, "").await.status, 201);
    let r = c.send("PATCH", &format!("{d}/pfs/dir/file.txt?action=append&position=0"), &auth, "pinned").await;
    assert_eq!(r.status, 202, "{}", r.text());
    let r = c.send("PATCH", &format!("{d}/pfs/dir/file.txt?action=flush&position=6"), &auth, "").await;
    assert_eq!(r.status, 200);
    // the same data through the pinned blob port
    let r = c.send("GET", &format!("{b}/pfs/dir/file.txt"), &auth, "").await;
    assert_eq!((r.status, r.text().as_str()), (200, "pinned"));
    let r = c.send("GET", &format!("{d}/pfs?resource=filesystem&recursive=true"), &auth, "").await;
    let names: Vec<String> = r.json()["paths"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["dir", "dir/file.txt"]);
    // another account is another set of filesystems
    let r = c.send("GET", &format!("http://{dfs_addr}/other/pfs?resource=filesystem&recursive=true"), &auth, "").await;
    assert_eq!(r.status, 404);
    // the pinned port also answers on the account-less root (default account)
    // (the default account is the development account, whose signatures are compared: a bearer token is not)
    let bearer = [("Authorization", "Bearer token")];
    assert_eq!(c.send("PUT", &format!("http://{dfs_addr}/devstoreaccount1/dflt?resource=filesystem"), &bearer, "").await.status, 201);
    let r = c.send("GET", &format!("http://{dfs_addr}/?resource=account"), &bearer, "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["filesystems"][0]["name"], "dflt");
    // rename with a path-style source: the header names /filesystem/path
    let r = c.send("PUT", &format!("{d}/pfs/renamed?mode=legacy"), &[auth[0], ("x-ms-rename-source", "/pfs/dir")], "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(c.send("GET", &format!("{b}/pfs/renamed/file.txt"), &auth, "").await.text(), "pinned");
    // The Data Lake SDKs cannot work out a Blob address from a path-style one, so they send their container calls here:
    // create, properties and delete of a filesystem are taken (on a `dfs` host name they are refused, as on Azure).
    assert_eq!(c.send("PUT", &format!("{d}/sdkfs?restype=container"), &auth, "").await.status, 201);
    assert_eq!(c.send("HEAD", &format!("{d}/sdkfs?restype=container"), &auth, "").await.status, 200);
    assert_eq!(c.send("HEAD", &format!("{b}/sdkfs?restype=container"), &auth, "").await.status, 200);
    assert_eq!(c.send("DELETE", &format!("{d}/sdkfs?restype=container"), &auth, "").await.status, 202);
    assert_eq!(c.send("HEAD", &format!("{b}/sdkfs?restype=container"), &auth, "").await.status, 404);
    // URLs in blob listings are the path-style URLs
    let r = c.send("GET", &format!("{b}/pfs?restype=container&comp=list"), &auth, "").await;
    assert!(r.text().contains(&format!("ServiceEndpoint=\"http://{blob_addr}/pinacct/\"")), "{}", r.text());
}

#[tokio::test]
async fn gateway_serves_both_labels_and_names_dfs_in_the_404() {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let srv = azemu_core::testing::TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    let r = srv.send("GET", &format!("{}/_azemu/health", srv.gateway_url()), &[], "").await;
    assert!(r.text().contains("\"dfs\"") && r.text().contains("\"blob\""), "{}", r.text());
    let r = srv.send("GET", &format!("http://localhost:{}/", srv.port()), &[], "").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("dfs (http://{account}.dfs.core.localhost"), "{}", r.text());
}
