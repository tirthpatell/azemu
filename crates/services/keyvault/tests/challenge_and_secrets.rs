//! The 401 challenge, request checks and the secrets API.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn request_without_token_gets_the_challenge_even_with_a_body() {
    let kv = Kv::new().await;
    let url = format!("{}/secrets/s1?api-version=7.6", kv.base);
    let r = kv.srv.request("PUT", &url).json(&json!({"value": "x"})).send().await;
    assert_eq!(r.status, 401);
    assert_eq!(code(&r), "Unauthorized");
    // the wording is the service's current one for every api-version (recording `test_key_client.pyTestKeyClienttest_40x_handling[7.4_vault].json`, entry 0)
    assert_eq!(r.json()["error"]["message"], "AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token.");
    let www = r.header("www-authenticate").unwrap();
    assert!(www.starts_with("Bearer authorization=\"https://login.microsoftonline.com/"), "{www}");
    // the host is `<vault>.vault.localhost:<port>`: the resource is the same domain without the vault's name
    let (_, port) = kv.base.rsplit_once(':').unwrap();
    assert!(www.ends_with(&format!("resource=\"https://vault.localhost:{port}\"")), "{www}");
    // the headers real Key Vault sends on every response
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(r.header("pragma"), Some("no-cache"));
    assert_eq!(r.header("expires"), Some("-1"));
    assert!(r.header("x-ms-request-id").is_some());
    assert!(r.header("x-ms-keyvault-service-version").is_some());
    assert!(r.header("x-ms-keyvault-region").is_some());
    assert!(r.header("strict-transport-security").is_some());
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    // nothing was stored
    assert_eq!(kv.get("/secrets/s1").await.status, 404);
}

#[tokio::test]
async fn every_api_version_gets_the_same_challenge_message() {
    let kv = Kv::new().await;
    // the wording changed in the service over time, not with the api-version: the 2026 recordings of 7.4, 7.5, 7.6 and 2025-07-01 agree
    for v in ["2016-10-01", "7.0", "7.4", "7.5", "7.6", "2025-07-01", "2026-04-01"] {
        let url = format!("{}/secrets/s1?api-version={v}", kv.base);
        let r = kv.srv.request("GET", &url).send().await;
        assert_eq!(r.status, 401, "{v}");
        assert_eq!(r.json()["error"]["message"], "AKV10000: Request is missing a Bearer, PoP, or MTLS_POP token.", "{v}");
    }
}

#[tokio::test]
async fn a_non_bearer_scheme_is_challenged_and_any_bearer_token_is_accepted() {
    let kv = Kv::new().await;
    let url = format!("{}/secrets?api-version=7.6", kv.base);
    let r = kv.srv.request("GET", &url).header("Authorization", "Basic abc").send().await;
    assert_eq!(r.status, 401);
    let r = kv.srv.request("GET", &url).header("Authorization", "bearer whatever").send().await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn missing_or_bad_api_version_is_bad_parameter_and_every_known_version_works() {
    let kv = Kv::new().await;
    let url = format!("{}/secrets", kv.base);
    let r = kv.srv.request("GET", &url).header("Authorization", "Bearer t").send().await;
    assert_eq!(r.status, 400);
    assert_eq!(code(&r), "BadParameter");
    let r = kv.srv.request("GET", &format!("{url}?api-version=banana")).header("Authorization", "Bearer t").send().await;
    assert_eq!(r.status, 400);
    for v in ["7.0", "7.1", "7.5-preview.1", "7.6", "7.6-preview.2", "2016-10-01", "2025-07-01", "2026-04-01", "2026-01-01-preview"] {
        let r = kv.srv.request("GET", &format!("{url}?api-version={v}")).header("Authorization", "Bearer t").send().await;
        assert_eq!(r.status, 200, "api-version {v}");
    }
}

#[tokio::test]
async fn client_request_id_is_echoed() {
    let kv = Kv::new().await;
    let url = format!("{}/secrets?api-version=7.6", kv.base);
    let r = kv.srv.request("GET", &url).header("Authorization", "Bearer t").header("x-ms-client-request-id", "abc-123").send().await;
    assert_eq!(r.header("x-ms-client-request-id"), Some("abc-123"));
}

#[tokio::test]
async fn secret_set_get_update_and_versions() {
    let kv = Kv::new().await;
    let r = kv
        .call(
            "PUT",
            "/secrets/db-password",
            Some(json!({"value": "hunter2", "contentType": "text/plain", "tags": {"env": "dev"},
                        "attributes": {"nbf": 1422864000, "exp": 2527401600i64}})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let first = r.json();
    let id = first["id"].as_str().unwrap().to_string();
    assert!(id.starts_with(&format!("{}/secrets/db-password/", kv.base)), "{id}");
    let version = id.rsplit('/').next().unwrap().to_string();
    assert_eq!(version.len(), 32);
    assert!(version.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(first["value"], "hunter2");
    assert_eq!(first["contentType"], "text/plain");
    assert_eq!(first["tags"]["env"], "dev");
    let a = &first["attributes"];
    assert_eq!(a["enabled"], true);
    assert_eq!(a["nbf"], 1422864000);
    assert_eq!(a["exp"], 2527401600i64);
    assert_eq!(a["recoveryLevel"], "Recoverable+Purgeable");
    assert_eq!(a["recoverableDays"], 90);
    assert_eq!(a["created"], a["updated"]);

    // latest, with and without the trailing slash the SDKs send
    for p in ["/secrets/db-password", "/secrets/db-password/"] {
        let r = kv.get(p).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["id"], id);
    }
    assert_eq!(kv.get(&format!("/secrets/db-password/{version}")).await.json()["value"], "hunter2");

    // a second version becomes latest
    let second = kv.call("PUT", "/secrets/db-password", Some(json!({"value": "better"}))).await.json();
    assert_ne!(second["id"], id);
    assert_eq!(kv.get("/secrets/db-password").await.json()["value"], "better");
    assert_eq!(kv.get(&format!("/secrets/db-password/{version}")).await.json()["value"], "hunter2");

    // update changes properties of the chosen version only, no new version
    let r = kv
        .call(
            "PATCH",
            &format!("/secrets/db-password/{version}"),
            Some(json!({"contentType": "application/json", "tags": {"a": "b"}, "attributes": {"exp": 2524723200i64}})),
        )
        .await;
    assert_eq!(r.status, 200);
    let u = r.json();
    assert!(u.get("value").is_none(), "update does not return the value");
    assert_eq!(u["contentType"], "application/json");
    assert_eq!(u["attributes"]["nbf"], 1422864000, "untouched attributes stay");
    assert_eq!(u["attributes"]["exp"], 2524723200i64);
    assert_eq!(kv.get("/secrets/db-password/versions").await.json()["value"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn secret_errors_use_the_real_codes_and_messages() {
    let kv = Kv::new().await;
    let r = kv.get("/secrets/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(code(&r), "SecretNotFound");
    let msg = r.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(msg.starts_with("A secret with (name/id) nope was not found in this key vault."), "{msg}");

    kv.call("PUT", "/secrets/one", Some(json!({"value": "v"}))).await;
    assert_eq!(code(&kv.get(&format!("/secrets/one/{}", "0".repeat(32))).await), "SecretNotFound");

    for bad in ["under_score", "dot.ted", "sp%20ace", "a%40b"] {
        let r = kv.call("PUT", &format!("/secrets/{bad}"), Some(json!({"value": "v"}))).await;
        assert_eq!(r.status, 400, "{bad}");
        assert_eq!(code(&r), "BadParameter");
    }
    assert_eq!(kv.call("PUT", "/secrets/ok", Some(json!({}))).await.status, 400, "value is required");
    let big = "x".repeat(26 * 1024);
    assert_eq!(kv.call("PUT", "/secrets/big", Some(json!({"value": big}))).await.status, 400);
    let limit = "x".repeat(25 * 1024);
    assert_eq!(kv.call("PUT", "/secrets/limit", Some(json!({"value": limit}))).await.status, 200);

    // a plain secret cannot be returned as PEM
    let r = kv.get("/secrets/one?outContentType=application%2Fx-pem-file").await;
    assert_eq!(r.status, 400);
    assert_eq!(code(&r), "BadParameter");
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("PEM"));
}

#[tokio::test]
async fn disabled_secret_cannot_be_read_but_can_be_updated_and_deleted() {
    let kv = Kv::new().await;
    kv.call("PUT", "/secrets/off", Some(json!({"value": "v", "attributes": {"enabled": false}}))).await;
    let r = kv.get("/secrets/off").await;
    assert_eq!(r.status, 403);
    assert_eq!(code(&r), "Forbidden");
    assert_eq!(inner_code(&r), "SecretDisabled");
    assert_eq!(r.json()["error"]["message"], "Operation get is not allowed on a disabled secret.");
    // still listed
    assert_eq!(kv.get("/secrets").await.json()["value"].as_array().unwrap().len(), 1);
    let r = kv.call("PATCH", "/secrets/off/", Some(json!({"attributes": {"enabled": true}}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(kv.get("/secrets/off").await.status, 200);
}

#[tokio::test]
async fn secret_soft_delete_recover_purge() {
    let kv = Kv::new().await;
    kv.call("PUT", "/secrets/s", Some(json!({"value": "v1", "tags": {"k": "v"}}))).await;
    kv.call("PUT", "/secrets/s", Some(json!({"value": "v2"}))).await;

    let r = kv.call("DELETE", "/secrets/s", None).await;
    assert_eq!(r.status, 200);
    let d = r.json();
    assert_eq!(d["recoveryId"], format!("{}/deletedsecrets/s", kv.base));
    let (deleted, purge) = (d["deletedDate"].as_i64().unwrap(), d["scheduledPurgeDate"].as_i64().unwrap());
    assert_eq!(purge - deleted, 90 * 86400);

    assert_eq!(code(&kv.get("/secrets/s").await), "SecretNotFound");
    assert_eq!(kv.get("/secrets").await.json()["value"].as_array().unwrap().len(), 0);
    assert_eq!(kv.call("DELETE", "/secrets/s", None).await.status, 404);

    // the name is blocked while it is deleted
    let r = kv.call("PUT", "/secrets/s", Some(json!({"value": "again"}))).await;
    assert_eq!(r.status, 409);
    assert_eq!(code(&r), "Conflict");
    assert_eq!(inner_code(&r), "ObjectIsDeletedButRecoverable");

    let r = kv.get("/deletedsecrets/s").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["recoveryId"], format!("{}/deletedsecrets/s", kv.base));
    let list = kv.get("/deletedsecrets").await.json();
    assert_eq!(list["value"].as_array().unwrap().len(), 1);
    assert_eq!(list["value"][0]["id"], format!("{}/secrets/s", kv.base), "list items carry no version");

    let r = kv.call("POST", "/deletedsecrets/s/recover", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(kv.get("/secrets/s").await.json()["value"], "v2");
    assert_eq!(kv.get("/secrets/s/versions").await.json()["value"].as_array().unwrap().len(), 2);
    assert_eq!(kv.get("/deletedsecrets").await.json()["value"].as_array().unwrap().len(), 0);

    // purge only works on deleted secrets
    let r = kv.call("DELETE", "/deletedsecrets/s", None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["error"]["message"], "Deleted Secret not found: s");
    kv.call("DELETE", "/secrets/s", None).await;
    let r = kv.call("DELETE", "/deletedsecrets/s", None).await;
    assert_eq!(r.status, 204);
    assert!(r.text().is_empty());
    assert_eq!(kv.get("/deletedsecrets/s").await.status, 404);
    assert_eq!(kv.call("POST", "/deletedsecrets/s/recover", None).await.status, 404);
    // and the name is free again
    assert_eq!(kv.call("PUT", "/secrets/s", Some(json!({"value": "new"}))).await.status, 200);
}

#[tokio::test]
async fn listing_pages_with_next_link_and_skiptoken() {
    let kv = Kv::new().await;
    for i in 0..30 {
        kv.call("PUT", &format!("/secrets/item-{i:02}"), Some(json!({"value": "v"}))).await;
    }
    let page1 = kv.get("/secrets").await.json();
    assert_eq!(page1["value"].as_array().unwrap().len(), 25, "default page size is 25");
    let next = page1["nextLink"].as_str().unwrap().to_string();
    assert!(next.starts_with(&format!("{}/secrets?api-version=7.6&$skiptoken=", kv.base)), "{next}");
    // follow the link exactly as given
    let r = kv.srv.request("GET", &next).header("Authorization", "Bearer t").send().await;
    let page2 = r.json();
    assert_eq!(page2["value"].as_array().unwrap().len(), 5);
    assert!(page2["nextLink"].is_null());
    assert_eq!(page2["value"][0]["id"], format!("{}/secrets/item-25", kv.base));

    let small = kv.get("/secrets?maxresults=7").await.json();
    assert_eq!(small["value"].as_array().unwrap().len(), 7);
    assert!(small["nextLink"].as_str().unwrap().ends_with("&maxresults=7"));
    assert_eq!(kv.get("/secrets?maxresults=0").await.status, 400);

    // versions page the same way
    for i in 0..5 {
        kv.call("PUT", "/secrets/multi", Some(json!({"value": format!("v{i}")}))).await;
    }
    let v1 = kv.get("/secrets/multi/versions?maxresults=2").await.json();
    assert_eq!(v1["value"].as_array().unwrap().len(), 2);
    assert!(v1["nextLink"].as_str().unwrap().contains("/secrets/multi/versions?"));
    assert_eq!(kv.get("/secrets/never-set/versions").await.json()["value"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn secret_backup_and_restore_across_vaults() {
    let kv = Kv::new().await;
    let first = kv.call("PUT", "/secrets/b", Some(json!({"value": "one", "contentType": "x/y", "tags": {"t": "1"}}))).await.json();
    kv.call("PUT", "/secrets/b", Some(json!({"value": "two"}))).await;
    let blob = kv.call("POST", "/secrets/b/backup", None).await.json()["value"].as_str().unwrap().to_string();

    // restoring over the existing secret is a conflict with the real wording
    let r = kv.post("/secrets/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 409);
    assert_eq!(code(&r), "Conflict");
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("There was a conflict restoring the secret '"));

    kv.call("DELETE", "/secrets/b", None).await;
    kv.call("DELETE", "/deletedsecrets/b", None).await;
    let r = kv.post("/secrets/restore", json!({"value": blob})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(kv.get("/secrets/b").await.json()["value"], "two");
    let versions = kv.get("/secrets/b/versions").await.json();
    assert_eq!(versions["value"].as_array().unwrap().len(), 2);
    // the first version kept its id
    let again = kv.get(&format!("/secrets/b/{}", first["id"].as_str().unwrap().rsplit('/').next().unwrap())).await.json();
    assert_eq!(again["value"], "one");
    assert_eq!(again["tags"]["t"], "1");

    // a backup restores into another vault; ids point at that vault
    let other = kv.vault_base("othervault");
    let r = kv.call_at(&other, "POST", "/secrets/restore", Some(json!({"value": blob}))).await;
    assert_eq!(r.status, 200);
    assert!(r.json()["id"].as_str().unwrap().starts_with(&format!("{other}/secrets/b/")));

    assert_eq!(kv.post("/secrets/restore", json!({"value": "bm90IGEgYmFja3Vw"})).await.status, 400);
    assert_eq!(kv.call("POST", "/secrets/missing/backup", None).await.status, 404);
}

#[tokio::test]
async fn vaults_are_separate_and_reset_clears_them() {
    let kv = Kv::new().await;
    kv.call("PUT", "/secrets/shared-name", Some(json!({"value": "in-testvault"}))).await;
    let other = kv.vault_base("second");
    assert_eq!(kv.call_at(&other, "GET", "/secrets/shared-name", None).await.status, 404);
    kv.call_at(&other, "PUT", "/secrets/shared-name", Some(json!({"value": "in-second"}))).await;
    assert_eq!(kv.get("/secrets/shared-name").await.json()["value"], "in-testvault");
    let r = kv.call_at(&other, "GET", "/secrets/shared-name", None).await.json();
    assert_eq!(r["value"], "in-second");
    assert!(r["id"].as_str().unwrap().starts_with(&format!("{other}/secrets/")));
    kv.srv.reset();
    assert_eq!(kv.get("/secrets/shared-name").await.status, 404);
}

#[tokio::test]
async fn names_are_case_insensitive_and_unbuilt_routes_say_so() {
    let kv = Kv::new().await;
    kv.call("PUT", "/secrets/MixedCase", Some(json!({"value": "v"}))).await;
    assert_eq!(kv.get("/secrets/mixedcase").await.status, 200);
    let r = kv.get("/storage").await;
    assert_eq!(r.status, 501);
    assert_eq!(code(&r), "NotImplemented");
    assert!(r.json()["error"]["message"].as_str().unwrap().starts_with("keyvault: "));
    let r = kv.call("PATCH", "/secrets", None).await;
    assert_eq!(r.status, 501);
    assert_eq!(kv.get("/nothing-here").await.status, 404);
}
