//! State files of version 1 must keep loading.
//!
//! `tests/state-v1/identity/identity.state` (`AZEMU-STATE identity 1`) is what `Service::save` wrote after the service made its
//! signing key; `jwks.json` next to it is the public key set the service served with that key. The files are checked in and
//! are never rewritten by a test run. `version_1_files_still_load` copies them to a scratch directory, loads the state into a
//! new service and checks that it serves the same key set and that tokens it issues verify against it.
//!
//! **The state file holds a private key.** It is a throwaway one, made by the service when the fixture was written, and
//! used for nothing else.
//!
//! If that test fails, a change broke the reading of files that people have on disk. Do not edit the checked-in files and do
//! not weaken the test: raise `VERSION` in `src/persist.rs`, keep a reader for version 1 (loading the same key), and add a
//! fixture directory `state-v2` next to this one with a test of its own.
//! To make the fixture again (only when the state itself changes, which also means a new version):
//! `cargo test -p azemu-identity --test state_v1 -- --ignored write_the_fixture`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_identity::IdentityService;
use common::*;

const STORAGE: &str = "https%3A%2F%2Fstorage.azure.com%2F.default";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("state-v1").join("identity")
}

async fn token(srv: &TestServer) -> String {
    let form = format!("grant_type=client_credentials&client_id=app-1&client_secret=s&scope={STORAGE}");
    let r = post_form(srv, &format!("{}/contoso/oauth2/v2.0/token", login(srv)), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["access_token"].as_str().unwrap().to_string()
}

#[tokio::test]
#[ignore = "writes the checked-in fixture; see the module text"]
async fn write_the_fixture() {
    let svc = Arc::new(IdentityService::new());
    let srv = TestServer::start(vec![svc.clone() as Arc<dyn Service>]).await;
    let _ = token(&srv).await; // the key is made in the background; this waits for it
    let dir = fixture_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    svc.save(&dir).unwrap();
    std::fs::write(dir.join("jwks.json"), serde_json::to_string_pretty(&jwks(&srv).await).unwrap()).unwrap();
}

#[tokio::test]
async fn version_1_files_still_load() {
    let dir = std::env::temp_dir().join(format!("azemu-identity-state-v1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture_dir().join("identity.state"), dir.join("identity.state"))
        .expect("tests/state-v1/identity/identity.state is checked in");
    let expected: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(fixture_dir().join("jwks.json")).unwrap()).unwrap();

    let svc = Arc::new(IdentityService::new());
    if let Err(e) = svc.load(&dir) {
        panic!(
            "Version 1 identity state files no longer load: {e}\nA change broke the reading of files people have on disk. Raise VERSION in \
             src/persist.rs, keep a reader for version 1 and add a fixture for the new version. Do not edit tests/state-v1."
        );
    }
    let srv = TestServer::start(vec![svc as Arc<dyn Service>]).await;
    let keys = jwks(&srv).await;
    assert_eq!(keys, expected, "the key set after loading is the key set the fixture was written with");
    let access = token(&srv).await;
    let claims = verify(&access, &keys);
    assert!(claims["appid"].as_str().or(claims["azp"].as_str()).is_some(), "{claims}");
    let _ = std::fs::remove_dir_all(&dir);
}
