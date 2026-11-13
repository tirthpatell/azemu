//! The signing key across restarts (`--data-dir`): tokens issued before a restart still verify against the key set
//! after it, and refresh tokens still work.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azemu_core::persist;
use azemu_core::testing::TestServer;
use azemu_core::Service;
use azemu_identity::IdentityService;
use common::*;

const STORAGE: &str = "https%3A%2F%2Fstorage.azure.com%2F.default";

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("azemu-identity-persist-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn open(d: &Path) -> TestServer {
    TestServer::start_persistent(vec![Arc::new(IdentityService::new())], d).await
}

async fn try_open(d: &Path) -> anyhow::Result<TestServer> {
    TestServer::try_start_persistent(vec![Arc::new(IdentityService::new())], d, |b| b).await
}

fn v2(srv: &TestServer) -> String {
    format!("{}/contoso/oauth2/v2.0/token", login(srv))
}

async fn token(srv: &TestServer) -> String {
    let form = format!("grant_type=client_credentials&client_id=app-1&client_secret=s&scope={STORAGE}");
    let r = post_form(srv, &v2(srv), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["access_token"].as_str().unwrap().to_string()
}

/// Wait until the key exists (it is made in the background).
async fn ready(srv: &TestServer) {
    let _ = token(srv).await;
}

#[tokio::test]
async fn tokens_issued_before_a_restart_verify_after_it_and_refresh_tokens_work() {
    let d = dir("key");
    let srv = open(&d).await;
    let access = token(&srv).await;
    let form = "grant_type=password&client_id=pub&username=alice%40example.com&password=pw&scope=https%3A%2F%2Fstorage.azure.com%2Fuser_impersonation+openid+offline_access";
    let b = post_form(&srv, &v2(&srv), form).await.json();
    let refresh = b["refresh_token"].as_str().unwrap().to_string();
    let jwks_before = jwks(&srv).await;
    srv.shutdown().await.unwrap();

    let srv = open(&d).await;
    let jwks_after = jwks(&srv).await;
    assert_eq!(jwks_before, jwks_after, "the same key set (kid, n, e)");
    let claims = verify(&access, &jwks_after);
    assert_eq!(claims["appid"], "app-1");
    // a token signed now is signed with the same key as before
    let again = token(&srv).await;
    assert_eq!(parts(&again).0["kid"], parts(&access).0["kid"]);
    let rt = percent_encoding::utf8_percent_encode(&refresh, percent_encoding::NON_ALPHANUMERIC);
    let r = post_form(&srv, &v2(&srv), &format!("grant_type=refresh_token&client_id=pub&refresh_token={rt}")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    verify(r.json()["access_token"].as_str().unwrap(), &jwks_after);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[tokio::test]
async fn without_a_data_directory_a_new_run_has_a_new_key() {
    let a = server().await;
    let b = server().await;
    assert_ne!(jwks(&a).await["keys"][0]["n"], jwks(&b).await["keys"][0]["n"]);
}

#[cfg(unix)]
#[tokio::test]
async fn the_key_file_is_private_and_unchanged_state_is_not_written_again() {
    use std::os::unix::fs::PermissionsExt;
    let d = dir("mode");
    let svc = Arc::new(IdentityService::new());
    let srv = TestServer::start_persistent(vec![svc.clone() as Arc<dyn Service>], &d).await;
    ready(&srv).await;
    let g = svc.persist_generation();
    assert_eq!(g, Some(1), "a key exists");
    svc.save(&d.join("identity")).unwrap();
    let file = d.join("identity").join("identity.state");
    let first = (std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap());
    std::thread::sleep(Duration::from_millis(30));
    svc.save(&d.join("identity")).unwrap();
    assert_eq!((std::fs::metadata(&file).unwrap().modified().unwrap(), std::fs::read(&file).unwrap()), first);
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(d.join("identity")).unwrap().permissions().mode() & 0o777, 0o700);
    // tokens and reset are not changes
    token(&srv).await;
    srv.reset();
    assert_eq!(svc.persist_generation(), g);
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

fn flip_byte(path: &Path, at: usize) {
    let mut b = std::fs::read(path).unwrap();
    let at = at.min(b.len() - 1);
    b[at] ^= 0xff;
    std::fs::write(path, b).unwrap();
}

#[tokio::test]
async fn damaged_state_files_are_refused_with_a_message_that_names_the_file() {
    let d = dir("damage");
    let srv = open(&d).await;
    ready(&srv).await;
    srv.shutdown().await.unwrap();
    let file = d.join("identity").join("identity.state");
    let good = std::fs::read(&file).unwrap();
    let refused = |d: PathBuf| async move {
        match try_open(&d).await {
            Ok(s) => {
                s.shutdown().await.ok();
                None
            }
            Err(e) => Some(e.to_string()),
        }
    };
    for cut in [0usize, 5, 21, good.len() / 2, good.len() - 1] {
        std::fs::write(&file, &good[..cut]).unwrap();
        let e = refused(d.clone()).await.unwrap_or_else(|| panic!("cut at {cut} was accepted"));
        assert!(e.contains("identity.state") && e.contains("--data-discard-unreadable"), "{e}");
    }
    let mut accepted = 0;
    let mut tried = 0;
    for at in (0..good.len()).step_by(7) {
        std::fs::write(&file, &good).unwrap();
        flip_byte(&file, at);
        tried += 1;
        if let Some(e) = refused(d.clone()).await {
            assert!(e.contains("identity.state"), "flip at {at}: {e}");
        } else {
            accepted += 1;
            std::fs::write(&file, &good).unwrap();
        }
    }
    assert!(accepted < tried / 2, "{accepted} of {tried} flips were accepted");
    std::fs::write(&file, persist::encode("queue", 1, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("holds queue state, expected identity"));
    std::fs::write(&file, persist::encode("identity", 99, &Vec::<u8>::new()).unwrap()).unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("version 99"));
    std::fs::write(&file, b"AZEMU-STATE identity 1\n\xc1\xc1").unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("identity.state"));
    // a key of the wrong size is not accepted either: a short modulus
    std::fs::write(&file, persist::encode("identity", 1, &serde_json::json!({"n": "AQ", "e": "AQ", "d": "AQ", "p": "", "q": ""})).unwrap())
        .unwrap();
    assert!(refused(d.clone()).await.unwrap().contains("identity.state"));
    std::fs::write(&file, &good).unwrap();
    std::fs::write(d.join("identity").join("notes.txt"), "mine").unwrap();
    let srv = try_open(&d).await.expect("extra files are ignored");
    ready(&srv).await;
    srv.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&d);
}
