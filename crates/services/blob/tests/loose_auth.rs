//! `AZEMU_LOOSE_AUTH=1`: no signature is looked at and the date of a Shared Key request is not checked. The setting is
//! read once per process, so this is a test file of its own.

use std::sync::Arc;

use azemu_blob::BlobService;
use azemu_core::testing::TestServer;

#[tokio::test]
async fn loose_auth_accepts_placeholders_wrong_signatures_and_old_dates() {
    std::env::set_var("AZEMU_LOOSE_AUTH", "1");
    let srv = TestServer::start(vec![Arc::new(BlobService::new())]).await;
    let base = srv.url("blob", "devstoreaccount1");
    let old = "Fri, 27 Mar 2020 01:00:51 GMT";
    for sig in ["replay", "Sanitized", "", "c2ln", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] {
        let auth = format!("SharedKey devstoreaccount1:{sig}");
        let r = srv.send("GET", &format!("{base}/?comp=list"), &[("Authorization", &auth), ("x-ms-date", old)], "").await;
        assert_eq!(r.status, 200, "{sig}");
    }
    for sig in ["Sanitized", "", "AAAA"] {
        let r = srv
            .send("GET", &format!("{base}/?comp=list&sv=2021-08-06&ss=b&srt=sco&sp=rl&se=2099-01-01T00%3A00%3A00Z&sig={sig}"), &[], "")
            .await;
        assert_eq!(r.status, 200, "{sig}");
    }
}
