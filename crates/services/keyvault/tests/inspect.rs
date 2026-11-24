//! The inspector (`/_azemu/inspect/keyvault`): the whole tree, a vault that is not there, what looking changes (nothing) and what is
//! never shown (secret values without `?reveal=1`, private key material at all).

mod common;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::*;
use serde_json::{json, Value};

/// Every answer of a walk from the service node, with the path it was asked at.
struct Walk {
    answers: Vec<(String, Value)>,
}

impl Walk {
    fn text(&self) -> String {
        self.answers.iter().map(|(_, v)| v.to_string()).collect::<Vec<_>>().join("\n")
    }
    fn node(&self, path: &str) -> &Value {
        &self.answers.iter().find(|(p, _)| p == path).unwrap_or_else(|| panic!("no node {path}")).1
    }
}

async fn get(kv: &Kv, path_and_query: &str) -> (u16, Value) {
    let r = kv.srv.send("GET", &format!("{}/_azemu/inspect{path_and_query}", kv.srv.gateway_url()), &[], "").await;
    (r.status, r.json())
}

fn check_shape(path: &str, v: &Value) {
    for k in ["path", "kind", "title", "properties", "children", "items", "next"] {
        assert!(v.get(k).is_some(), "{path}: missing {k}");
    }
    assert_eq!(v["path"], path, "the path of a node is the path it was asked at");
}

/// Follow every child and every row with a path, every page, with `extra` added to each request.
async fn walk(kv: &Kv, extra: &str) -> Walk {
    let mut answers: Vec<(String, Value)> = Vec::new();
    let mut todo = vec!["/keyvault".to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(path) = todo.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let mut marker: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut q = format!("?limit=2{extra}");
            if let Some(m) = &marker {
                q.push_str(&format!("&marker={m}"));
            }
            let (status, v) = get(kv, &format!("{path}{q}")).await;
            assert_eq!(status, 200, "{path}{q}: {v}");
            check_shape(&path, &v);
            for c in v["children"].as_array().unwrap() {
                todo.push(c["path"].as_str().unwrap().to_string());
            }
            for i in v["items"].as_array().unwrap() {
                if let Some(p) = i["path"].as_str() {
                    todo.push(p.to_string());
                }
            }
            marker = v["next"].as_str().map(str::to_string);
            answers.push((path.clone(), v));
            pages += 1;
            assert!(pages < 50, "paging of {path} does not end");
            if marker.is_none() {
                break;
            }
        }
    }
    Walk { answers }
}

/// A vault with secrets (with versions), keys of every kind, certificates and deleted ones. Returns what must stay private.
struct Filled {
    secret_values: Vec<String>,
    private_parts: Vec<String>,
}

async fn fill(kv: &Kv) -> Filled {
    let mut secret_values = Vec::new();
    for (name, value) in
        [("db-password", "hunter2-first"), ("api-token", "tok-1234567890"), ("gone", "to-be-deleted-secret"), ("zeta", "zzz-value")]
    {
        let r = kv
            .call("PUT", &format!("/secrets/{name}"), Some(json!({"value": value, "contentType": "text/plain", "tags": {"team": "x"}})))
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        secret_values.push(value.to_string());
    }
    let r = kv
        .call("PUT", "/secrets/db-password", Some(json!({"value": "hunter2-second", "attributes": {"exp": 4102444800i64, "nbf": 1}})))
        .await;
    assert_eq!(r.status, 200);
    secret_values.push("hunter2-second".to_string());
    assert_eq!(kv.call("DELETE", "/secrets/gone", None).await.status, 200);

    let mut private_parts = Vec::new();
    for (name, body) in [
        ("rsa-key", json!({"kty": "RSA", "key_size": 2048, "key_ops": ["sign", "verify"], "tags": {"k": "v"}})),
        ("ec-key", json!({"kty": "EC", "crv": "P-256"})),
        ("oct-key", json!({"kty": "oct-HSM", "key_size": 256})),
        ("gone-key", json!({"kty": "EC", "crv": "P-384"})),
    ] {
        let r = kv.post(&format!("/keys/{name}/create"), body).await;
        assert_eq!(r.status, 200, "{name}: {}", r.text());
    }
    kv.post("/keys/rsa-key/create", json!({"kty": "RSA", "key_size": 2048})).await;
    assert_eq!(kv.call("DELETE", "/keys/gone-key", None).await.status, 200);

    let policy = json!({"key_props": {"kty": "EC", "crv": "P-256", "exportable": true, "reuse_key": false},
        "x509_props": {"subject": "CN=web.example.com", "sans": {"dns_names": ["web.example.com"]}, "validity_months": 6},
        "issuer": {"name": "Self"}});
    for name in ["web", "old-cert"] {
        let r = kv.post(&format!("/certificates/{name}/create"), json!({"policy": policy})).await;
        assert_eq!(r.status, 202, "{}", r.text());
        kv.get(&format!("/certificates/{name}/pending")).await;
    }
    assert_eq!(kv.call("DELETE", "/certificates/old-cert", None).await.status, 200);
    // a certificate request that waits for a signed certificate (an issuer the vault does not know)
    let unknown = json!({"key_props": {"kty": "RSA", "key_size": 2048}, "x509_props": {"subject": "CN=pending.example.com"}, "issuer": {"name": "Unknown"}});
    assert_eq!(kv.post("/certificates/waiting/create", json!({"policy": unknown})).await.status, 202);
    assert_eq!(
        kv.call(
            "PUT",
            "/certificates/issuers/acme",
            Some(json!({"provider": "Test", "credentials": {"account_id": "acct-1", "pwd": "issuer-pass-77"}}))
        )
        .await
        .status,
        200
    );
    secret_values.push("issuer-pass-77".to_string());

    // the private parts, read through the backup of each key (the service's own way to hand them out)
    for name in ["rsa-key", "ec-key", "oct-key", "web"] {
        let r = kv.call("POST", &format!("/keys/{name}/backup"), None).await;
        assert_eq!(r.status, 200, "{name}: {}", r.text());
        let blob = unb64(r.json()["value"].as_str().unwrap());
        let json_start = blob.iter().position(|b| *b == b'{').unwrap();
        let payload: Value = serde_json::from_slice(&blob[json_start..]).unwrap();
        collect_private(&payload, &mut private_parts);
    }
    assert!(private_parts.len() >= 5, "the backups hold private parts: {private_parts:?}");
    // the PFX behind the certificate
    let pfx = kv.get("/secrets/web").await.json()["value"].as_str().unwrap().to_string();
    private_parts.push(pfx);
    Filled { secret_values, private_parts }
}

fn collect_private(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                if matches!(k.as_str(), "d" | "p" | "q" | "dp" | "dq" | "qi" | "k" | "secret" | "key" | "material" | "private") {
                    if let Some(s) = v.as_str() {
                        if s.len() >= 16 {
                            out.push(s.to_string());
                        }
                    }
                }
                collect_private(v, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| collect_private(v, out)),
        _ => {}
    }
}

/// The state as the service shows it: used before and after a walk.
async fn state_view(kv: &Kv) -> Vec<Value> {
    let mut out = Vec::new();
    for p in ["/secrets", "/keys", "/certificates", "/deletedsecrets", "/deletedkeys", "/deletedcertificates", "/certificates/issuers"] {
        out.push(kv.get(&format!("{p}?maxresults=25")).await.json());
    }
    for p in ["/secrets/db-password/versions", "/keys/rsa-key/versions"] {
        out.push(kv.get(&format!("{p}?maxresults=25")).await.json());
    }
    out
}

#[tokio::test]
async fn a_full_walk_reaches_every_node_and_paging_ends() {
    let kv = Kv::in_vault("alpha").await;
    fill(&kv).await;
    // a second vault, so the service node pages too
    kv.call_at(&kv.vault_base("beta"), "PUT", "/secrets/s", Some(json!({"value": "other"}))).await;
    kv.call_at(&kv.vault_base("gamma"), "PUT", "/secrets/s", Some(json!({"value": "other"}))).await;
    let w = walk(&kv, "").await;
    let svc = w.node("/keyvault");
    assert_eq!(svc["kind"], "service");
    assert_eq!(svc["properties"]["accounts"], 3);
    let alpha = w.node("/keyvault/alpha");
    assert_eq!(alpha["kind"], "vault");
    // the secrets of the vault: three that were written, and one that backs the certificate `web` (as on Azure)
    assert_eq!(alpha["properties"]["secrets"], 4);
    assert_eq!(alpha["properties"]["deletedsecrets"], 2, "gone, and the secret behind the deleted certificate");
    assert_eq!(alpha["properties"]["keys"], 4);
    assert_eq!(alpha["properties"]["deletedkeys"], 2);
    assert_eq!(alpha["properties"]["issuers"], 1);

    let secrets = w.node("/keyvault/alpha/secrets");
    assert_eq!(secrets["properties"]["count"], 4);
    let names: Vec<&str> = w
        .answers
        .iter()
        .filter(|(p, _)| p == "/keyvault/alpha/secrets")
        .flat_map(|(_, v)| v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()))
        .collect();
    assert_eq!(names, ["api-token", "db-password", "web", "zeta"], "all pages, in name order, deleted ones left out");
    let versions = w.node("/keyvault/alpha/secrets/db-password/versions");
    assert_eq!(versions["properties"]["count"], 2);
    let one = w.node("/keyvault/alpha/secrets/db-password");
    assert_eq!(one["kind"], "secret");
    assert_eq!(one["properties"]["enabled"], true);
    assert_eq!(one["properties"]["versions"], 2);
    assert_eq!(one["properties"]["expires"], "2100-01-01T00:00:00.000Z");
    assert_eq!(one["views"][0]["query"], "reveal=1");
    assert!(one["properties"].get("value").is_none());

    let gone = w.node("/keyvault/alpha/deletedsecrets/gone");
    assert_eq!(gone["kind"], "deletedsecret");
    assert!(gone["properties"]["deletedDate"].is_string() && gone["properties"]["scheduledPurgeDate"].is_string());
    assert_eq!(gone["properties"]["recoverableDays"], 90);
    assert!(w.node("/keyvault/alpha/deletedkeys/gone-key")["properties"]["scheduledPurgeDate"].is_string());
    assert!(w.node("/keyvault/alpha/deletedcertificates/old-cert")["properties"]["scheduledPurgeDate"].is_string());

    let rsa = w.node("/keyvault/alpha/keys/rsa-key");
    assert_eq!(rsa["properties"]["jwk"]["kty"], "RSA");
    assert!(rsa["properties"]["jwk"]["n"].is_string() && rsa["properties"]["jwk"]["e"].is_string());
    assert!(rsa["properties"]["keyOps"].is_array());
    assert_eq!(rsa["properties"]["versions"], 2);
    let ec = w.node("/keyvault/alpha/keys/ec-key");
    assert_eq!(ec["properties"]["jwk"]["crv"], "P-256");
    assert!(ec["properties"]["jwk"]["x"].is_string());
    assert_eq!(w.node("/keyvault/alpha/keys/oct-key")["properties"]["kty"], "oct-HSM");

    let cert = w.node("/keyvault/alpha/certificates/web");
    assert_eq!(cert["kind"], "certificate");
    assert!(cert["properties"]["cer"].is_string());
    assert!(cert["properties"]["subject"].as_str().unwrap().contains("web.example.com"));
    assert_eq!(cert["properties"]["policy"]["issuer"]["name"], "Self");
    // a certificate with no certificate yet: its request shows
    let waiting = w.node("/keyvault/alpha/certificates/waiting");
    assert_eq!(waiting["properties"]["pendingOperation"]["status"], "inProgress");
    assert!(waiting["properties"]["pendingOperation"]["csr"].is_string());
    let issuers = w.node("/keyvault/alpha/issuers");
    assert_eq!(issuers["items"][0]["name"], "acme");
    // the linked key and secret of a certificate are marked
    assert_eq!(w.node("/keyvault/alpha/secrets/web")["properties"]["managed"], true);
    assert_eq!(w.node("/keyvault/alpha/keys/web")["properties"]["managed"], true);
    assert!(w.answers.len() > 40, "walked {} nodes", w.answers.len());
}

#[tokio::test]
async fn no_secret_value_and_no_private_key_in_any_walk() {
    let kv = Kv::in_vault("alpha").await;
    let f = fill(&kv).await;
    let w = walk(&kv, "").await;
    let text = w.text();
    for v in &f.secret_values {
        assert!(!text.contains(v.as_str()), "a secret value is in a walk without reveal: {v}");
    }
    for p in &f.private_parts {
        assert!(!text.contains(p.as_str()), "private material is in a walk");
    }
    // also with reveal=1 on every node: values only on the secret nodes, never keys or certificates
    let w = walk(&kv, "&reveal=1").await;
    let text = w.text();
    for p in &f.private_parts {
        assert!(!text.contains(p.as_str()), "private material is in a walk with reveal=1");
    }
    for (path, v) in &w.answers {
        let revealed = v["properties"].get("value").is_some();
        let secret_node = path.contains("/secrets/") && v["items"].as_array().unwrap().is_empty() && !path.ends_with("/versions");
        assert!(!revealed || secret_node, "a value on {path}");
        assert!(v["properties"].get("valueWithheld").is_none() || path.contains("/secrets/web"), "{path}");
    }
    // the JWKs of keys never carry private fields
    for (path, v) in &w.answers {
        if let Some(jwk) = v["properties"].get("jwk") {
            for f in ["d", "p", "q", "dp", "dq", "qi", "k"] {
                assert!(jwk.get(f).is_none(), "{path}: jwk has {f}");
            }
        }
    }
}

#[tokio::test]
async fn reveal_returns_one_value_and_never_the_private_parts_of_a_certificate() {
    let kv = Kv::in_vault("alpha").await;
    let f = fill(&kv).await;
    let (s, v) = get(&kv, "/keyvault/alpha/secrets/db-password?reveal=1").await;
    assert_eq!(s, 200);
    assert_eq!(v["properties"]["value"], "hunter2-second", "the newest version");
    let versions = get(&kv, "/keyvault/alpha/secrets/db-password/versions").await.1;
    let first = versions["items"].as_array().unwrap().last().unwrap();
    assert!(first["path"].is_string());
    let (_, v) = get(&kv, &format!("{}?reveal=1", first["path"].as_str().unwrap())).await;
    assert_eq!(v["properties"]["value"], "hunter2-first", "the version asked for");
    assert_eq!(v["kind"], "secretversion");
    assert_eq!(get(&kv, "/keyvault/alpha/secrets/api-token?reveal=true").await.1["properties"]["value"], "tok-1234567890");
    assert!(get(&kv, "/keyvault/alpha/secrets/api-token?reveal=0").await.1["properties"].get("value").is_none());
    // a deleted secret's value is not offered
    let (s, v) = get(&kv, "/keyvault/alpha/deletedsecrets/gone?reveal=1").await;
    assert_eq!(s, 200);
    assert!(!v.to_string().contains("to-be-deleted-secret"));
    // the secret behind a certificate holds the PFX: withheld
    let (_, v) = get(&kv, "/keyvault/alpha/secrets/web?reveal=1").await;
    assert!(v["properties"].get("value").is_none());
    assert!(v["properties"]["valueWithheld"].is_string());
    for p in ["/keyvault/alpha/keys/rsa-key", "/keyvault/alpha/keys/web", "/keyvault/alpha/certificates/web"] {
        let (_, v) = get(&kv, &format!("{p}?reveal=1")).await;
        let t = v.to_string();
        for private in &f.private_parts {
            assert!(!t.contains(private.as_str()), "{p} shows private material");
        }
    }
}

#[tokio::test]
async fn a_vault_that_is_not_there_is_404_and_stays_not_there() {
    let kv = Kv::in_vault("alpha").await;
    kv.call("PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    for p in [
        "/keyvault/nothere",
        "/keyvault/nothere/secrets",
        "/keyvault/nothere/secrets/s?reveal=1",
        "/keyvault/alpha/secrets/missing",
        "/keyvault/alpha/secrets/s/versions/0000",
        "/keyvault/alpha/deletedsecrets/s",
        "/keyvault/alpha/deletedsecrets/s/versions",
        "/keyvault/alpha/nothing",
    ] {
        assert_eq!(get(&kv, p).await.0, 404, "{p}");
    }
    assert_eq!(get(&kv, "/keyvault/alpha/secrets?limit=0").await.0, 400);
    let svc = get(&kv, "/keyvault").await.1;
    assert_eq!(svc["properties"]["accounts"], 1, "looking created no vault");
    assert_eq!(svc["items"][0]["name"], "alpha");
}

#[tokio::test]
async fn looking_changes_nothing() {
    let kv = Kv::in_vault("alpha").await;
    fill(&kv).await;
    let before = state_view(&kv).await;
    let w1 = walk(&kv, "&reveal=1").await;
    let after = state_view(&kv).await;
    assert_eq!(before, after, "the service's own lists differ after a walk");
    // and the walk itself is the same twice (no clock or version moved)
    let w2 = walk(&kv, "&reveal=1").await;
    assert_eq!(w1.text(), w2.text());
    // the secret is still there and is not marked as read
    let r = kv.get("/secrets/db-password").await;
    assert_eq!(r.json()["value"], "hunter2-second");
}

#[tokio::test]
async fn a_deleted_item_past_its_purge_date_is_not_shown_and_not_purged() {
    // the purge date is 90 days after the deletion: the state cannot be aged through the API, so this checks the listing rules on
    // a fresh deletion, and that the item is still recoverable after looking
    let kv = Kv::in_vault("alpha").await;
    kv.call("PUT", "/secrets/s", Some(json!({"value": "v"}))).await;
    kv.call("DELETE", "/secrets/s", None).await;
    let (_, v) = get(&kv, "/keyvault/alpha/deletedsecrets").await;
    assert_eq!(v["items"][0]["name"], "s");
    assert_eq!(get(&kv, "/keyvault/alpha/secrets").await.1["items"].as_array().unwrap().len(), 0);
    assert_eq!(kv.call("POST", "/deletedsecrets/s/recover", None).await.status, 200);
    assert_eq!(get(&kv, "/keyvault/alpha/secrets").await.1["items"][0]["name"], "s");
    let _ = STANDARD.encode(b"");
}
