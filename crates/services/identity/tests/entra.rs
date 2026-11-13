mod common;

use common::*;
use serde_json::Value;

#[tokio::test]
async fn openid_configuration_v2_points_back_at_the_emulator() {
    let srv = server().await;
    for (base, expect) in [(login(&srv), login(&srv)), (login_prefix(&srv), login_prefix(&srv))] {
        let r = srv.send("GET", &format!("{base}/contoso.onmicrosoft.com/v2.0/.well-known/openid-configuration"), &[], "").await;
        assert_eq!(r.status, 200);
        let d = r.json();
        let t = format!("{expect}/contoso.onmicrosoft.com");
        assert_eq!(d["token_endpoint"], format!("{t}/oauth2/v2.0/token"));
        assert_eq!(d["authorization_endpoint"], format!("{t}/oauth2/v2.0/authorize"));
        assert_eq!(d["device_authorization_endpoint"], format!("{t}/oauth2/v2.0/devicecode"));
        assert_eq!(d["jwks_uri"], format!("{t}/discovery/v2.0/keys"));
        let iss = d["issuer"].as_str().unwrap();
        assert!(iss.starts_with(&expect) && iss.ends_with("/v2.0"), "{iss}");
        assert!(d["id_token_signing_alg_values_supported"].as_array().unwrap().iter().any(|a| a == "RS256"));
    }
}

#[tokio::test]
async fn openid_configuration_v1() {
    let srv = server().await;
    let r = srv
        .send("GET", &format!("{}/{}/.well-known/openid-configuration", login(&srv), "11111111-2222-3333-4444-555555555555"), &[], "")
        .await;
    assert_eq!(r.status, 200);
    let d = r.json();
    assert_eq!(d["issuer"], "https://sts.windows.net/11111111-2222-3333-4444-555555555555/");
    assert!(d["token_endpoint"].as_str().unwrap().ends_with("/oauth2/token"));
    assert!(d["jwks_uri"].as_str().unwrap().ends_with("/discovery/keys"));
}

#[tokio::test]
async fn http_authority_gives_http_urls() {
    let srv = server().await;
    let r = srv.send("GET", &format!("http://login.localhost:{}/t/v2.0/.well-known/openid-configuration", srv.port()), &[], "").await;
    assert!(r.json()["token_endpoint"].as_str().unwrap().starts_with(&format!("http://login.localhost:{}/t/", srv.port())));
}

#[tokio::test]
async fn bad_tenant_is_an_entra_error() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/a%20b/v2.0/.well-known/openid-configuration", login(&srv)), &[], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error_codes"][0], 90002);
}

#[tokio::test]
async fn instance_discovery_lists_the_emulator_as_its_own_network() {
    let srv = server().await;
    let url = format!(
        "{}/common/discovery/instance?api-version=1.1&authorization_endpoint=https%3A%2F%2Flogin.localhost%3A1234%2Fcontoso%2Foauth2%2Fv2.0%2Fauthorize",
        login(&srv)
    );
    let r = srv.send("GET", &url, &[], "").await;
    assert_eq!(r.status, 200);
    let d = r.json();
    assert_eq!(d["tenant_discovery_endpoint"], format!("{}/contoso/v2.0/.well-known/openid-configuration", login(&srv)));
    assert_eq!(d["api-version"], "1.1");
    let m = &d["metadata"][0];
    assert_eq!(m["preferred_network"], "login.localhost");
    assert_eq!(m["preferred_cache"], "login.localhost");
    assert_eq!(m["aliases"][0], "login.localhost");

    // No authorization_endpoint: falls back to the common tenant.
    let r = srv.send("GET", &format!("{}/common/discovery/instance?api-version=1.1", login(&srv)), &[], "").await;
    assert!(r.json()["tenant_discovery_endpoint"].as_str().unwrap().contains("/common/v2.0/"));
}

#[tokio::test]
async fn jwks_at_every_documented_path() {
    let srv = server().await;
    let mut first: Option<Value> = None;
    for base in [login(&srv), login_prefix(&srv)] {
        for p in ["/contoso/discovery/v2.0/keys", "/contoso/discovery/keys", "/common/discovery/keys", "/common/discovery/v2.0/keys"] {
            let r = srv.send("GET", &format!("{base}{p}"), &[], "").await;
            assert_eq!(r.status, 200, "{p}");
            let k = r.json();
            assert_eq!(k["keys"].as_array().unwrap().len(), 1);
            assert_eq!(k["keys"][0]["kty"], "RSA");
            assert_eq!(k["keys"][0]["use"], "sig");
            assert_eq!(k["keys"][0]["e"], "AQAB");
            if let Some(f) = &first {
                assert_eq!(f, &k);
            }
            first = Some(k);
        }
    }
}

#[tokio::test]
async fn userrealm_and_device_page() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/common/userrealm/me@example.com?api-version=1.0", login(&srv)), &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["account_type"], "Managed");
    assert_eq!(r.json()["domain_name"], "example.com");
    let r = srv.send("GET", &format!("{}/t/oauth2/deviceauth", login(&srv)), &[], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn authorize_redirects_with_a_code_that_redeems() {
    let srv = server().await;
    let auth = format!(
        "{}/contoso/oauth2/v2.0/authorize?client_id=app1&response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A9%2Fcb&scope=https%3A%2F%2Fstorage.azure.com%2F.default+openid+offline_access&state=xyz&nonce=n1&login_hint=bob%40example.com",
        login(&srv)
    );
    let r = srv.send("GET", &auth, &[], "").await;
    assert_eq!(r.status, 302);
    let loc = r.header("location").unwrap().to_string();
    assert!(loc.starts_with("http://localhost:9/cb?code="), "{loc}");
    assert!(loc.ends_with("&state=xyz"), "{loc}");
    let code = percent_encoding::percent_decode_str(loc.split("code=").nth(1).unwrap().split('&').next().unwrap())
        .decode_utf8()
        .unwrap()
        .into_owned();

    let form = format!(
        "grant_type=authorization_code&client_id=app1&code={}&redirect_uri=http%3A%2F%2Flocalhost%3A9%2Fcb&scope=https%3A%2F%2Fstorage.azure.com%2F.default+openid+offline_access&client_info=1",
        percent_encoding::utf8_percent_encode(&code, percent_encoding::NON_ALPHANUMERIC)
    );
    let r = post_form(&srv, &format!("{}/contoso/oauth2/v2.0/token", login(&srv)), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    // MSAL for Go refuses an answer whose scope leaves out one it asked for
    let scope = b["scope"].as_str().unwrap().to_ascii_lowercase();
    for want in ["https://storage.azure.com/.default", "openid", "offline_access"] {
        assert!(scope.split(' ').any(|s| s == want), "{want} missing from {scope}");
    }
    let jwks = jwks(&srv).await;
    let at = verify(b["access_token"].as_str().unwrap(), &jwks);
    assert_eq!(at["upn"], "bob@example.com");
    assert_eq!(at["aud"], "https://storage.azure.com");
    let id = verify(b["id_token"].as_str().unwrap(), &jwks);
    assert_eq!(id["aud"], "app1");
    assert_eq!(id["nonce"], "n1");
    assert_eq!(id["preferred_username"], "bob@example.com");
    assert_eq!(id["ver"], "2.0");
    assert!(b["refresh_token"].is_string());

    // Bad pieces
    let r = post_form(
        &srv,
        &format!("{}/contoso/oauth2/v2.0/token", login(&srv)),
        "grant_type=authorization_code&client_id=app1&code=garbage&scope=openid",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"], "invalid_grant");
    let r = srv.send("GET", &auth.replace("response_type=code", "response_type=token"), &[], "").await;
    assert_eq!(r.status, 302);
    assert!(r.header("location").unwrap().contains("error=unsupported_response_type"));
    let r = srv.send("GET", &format!("{}/contoso/oauth2/v2.0/authorize?client_id=a", login(&srv)), &[], "").await;
    assert_eq!(r.status, 400);
}
