#![allow(dead_code)]

use std::sync::Arc;

use azemu_core::testing::{TestResponse, TestServer};
use azemu_identity::IdentityService;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rsa::{BoxedUint, PublicKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub async fn server() -> TestServer {
    TestServer::start(vec![Arc::new(IdentityService::new())]).await
}

/// `https://login.localhost:{port}`
pub fn login(srv: &TestServer) -> String {
    format!("https://login.localhost:{}", srv.port())
}

/// `https://localhost:{port}/_azemu/identity/login`
pub fn login_prefix(srv: &TestServer) -> String {
    format!("https://localhost:{}/_azemu/identity/login", srv.port())
}

pub fn gateway(srv: &TestServer) -> String {
    srv.gateway_url()
}

pub async fn post_form(srv: &TestServer, url: &str, form: &str) -> TestResponse {
    srv.send("POST", url, &[("Content-Type", "application/x-www-form-urlencoded")], form.to_string()).await
}

pub fn parts(token: &str) -> (Value, Value, Vec<u8>) {
    let p: Vec<&str> = token.split('.').collect();
    assert_eq!(p.len(), 3, "not a JWT: {token}");
    let h = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p[0]).unwrap()).unwrap();
    let c = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p[1]).unwrap()).unwrap();
    (h, c, URL_SAFE_NO_PAD.decode(p[2]).unwrap())
}

pub fn claims(token: &str) -> Value {
    parts(token).1
}

/// Verifies the RS256 signature with the key in `jwks` named by the header's `kid`.
pub fn verify(token: &str, jwks: &Value) -> Value {
    let (h, c, sig) = parts(token);
    assert_eq!(h["alg"], "RS256");
    assert_eq!(h["typ"], "JWT");
    let kid = h["kid"].as_str().expect("kid");
    let jwk = jwks["keys"].as_array().unwrap().iter().find(|k| k["kid"] == kid).expect("kid not in JWKS");
    assert_eq!(jwk["kty"], "RSA");
    let big = |s: &str| BoxedUint::from_be_slice_vartime(&URL_SAFE_NO_PAD.decode(s).unwrap());
    let key = PublicKey::new(big(jwk["n"].as_str().unwrap()), big(jwk["e"].as_str().unwrap())).unwrap();
    let signing_input = token.rsplit_once('.').unwrap().0;
    let digest = Sha256::digest(signing_input.as_bytes());
    key.pkcs1v15_verify_prehash::<Sha256>(&digest, &sig).expect("signature does not verify");
    c
}

pub async fn jwks(srv: &TestServer) -> Value {
    let r = srv.send("GET", &format!("{}/contoso/discovery/v2.0/keys", login(srv)), &[], "").await;
    assert_eq!(r.status, 200);
    r.json()
}

pub fn basic(id: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{id}:{secret}")))
}
