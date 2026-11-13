mod common;

use common::*;

const GUID_RE: fn(&str) -> bool = |s| s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4;

#[tokio::test]
async fn app_service_token_is_a_verifiable_jwt() {
    let srv = server().await;
    let url = format!("{}/_azemu/identity/msi/token?resource=https%3A%2F%2Fstorage.azure.com&api-version=2019-08-01", gateway(&srv));
    let r = srv.send("GET", &url, &[("X-IDENTITY-HEADER", "azemu")], "").await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().starts_with("application/json"));
    let body = r.json();
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["resource"], "https://storage.azure.com");
    // expires_on is a string of Unix seconds
    let exp: i64 = body["expires_on"].as_str().expect("expires_on is a string").parse().unwrap();
    assert!(GUID_RE(body["client_id"].as_str().unwrap()));
    let c = verify(body["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(c["aud"], "https://storage.azure.com");
    assert_eq!(c["exp"].as_i64().unwrap(), exp);
    assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 3600);
    assert_eq!(c["nbf"], c["iat"]);
    assert_eq!(c["idtyp"], "app");
    assert_eq!(c["ver"], "1.0");
    assert_eq!(c["appid"], body["client_id"]);
    assert_eq!(c["oid"], c["sub"]);
    assert!(c["iss"].as_str().unwrap().starts_with("https://sts.windows.net/"));
    assert!(c["iss"].as_str().unwrap().contains(c["tid"].as_str().unwrap()));
    assert!(c.get("roles").is_none());
}

#[tokio::test]
async fn app_service_works_over_plain_http_on_the_login_host_too() {
    let srv = server().await;
    let url = format!("http://login.localhost:{}/_azemu/identity/msi/token?resource=r&api-version=2019-08-01", srv.port());
    let r = srv.send("GET", &url, &[("x-identity-header", "x")], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn app_service_errors() {
    let srv = server().await;
    let base = format!("{}/_azemu/identity/msi/token", gateway(&srv));
    let h = [("X-IDENTITY-HEADER", "azemu")];
    let r = srv.send("GET", &format!("{base}?resource=r&api-version=2019-08-01"), &[], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["statusCode"], 400);
    assert!(r.json()["message"].as_str().unwrap().contains("X-IDENTITY-HEADER"));
    let r = srv.send("GET", &format!("{base}?api-version=2019-08-01"), &h, "").await;
    assert_eq!(r.status, 400);
    assert!(r.json()["message"].as_str().unwrap().contains("resource"));
    let r = srv.send("GET", &format!("{base}?resource=r"), &h, "").await;
    assert_eq!(r.status, 400);
    let r = srv.send("GET", &format!("{base}?resource=r&api-version=1999-01-01"), &h, "").await;
    assert_eq!(r.status, 400);
    let r = srv.send("GET", &format!("{base}?resource=r&api-version=2019-08-01&client_id=a&object_id=b"), &h, "").await;
    assert_eq!(r.status, 400);
    let r = srv.send("POST", &format!("{base}?resource=r&api-version=2019-08-01"), &h, "").await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn app_service_identities_are_stable_and_distinct() {
    let srv = server().await;
    let base = format!("{}/_azemu/identity/msi/token?resource=r&api-version=2019-08-01", gateway(&srv));
    let h = [("X-IDENTITY-HEADER", "azemu")];
    let get = |extra: &'static str| {
        let url = format!("{base}{extra}");
        let srv = &srv;
        async move { srv.send("GET", &url, &h, "").await.json() }
    };
    let sys1 = get("").await;
    let sys2 = get("").await;
    assert_eq!(sys1["client_id"], sys2["client_id"]);
    assert_eq!(claims(sys1["access_token"].as_str().unwrap())["oid"], claims(sys2["access_token"].as_str().unwrap())["oid"]);

    let user = get("&client_id=11111111-2222-3333-4444-555555555555").await;
    assert_eq!(user["client_id"], "11111111-2222-3333-4444-555555555555");
    assert_ne!(user["client_id"], sys1["client_id"]);
    let uc = claims(user["access_token"].as_str().unwrap());
    assert_eq!(uc["appid"], "11111111-2222-3333-4444-555555555555");
    assert_ne!(uc["oid"], claims(sys1["access_token"].as_str().unwrap())["oid"]);

    let by_obj = get("&object_id=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").await;
    assert_eq!(claims(by_obj["access_token"].as_str().unwrap())["oid"], "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let by_principal = get("&principal_id=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").await;
    assert_eq!(by_principal["client_id"], by_obj["client_id"]);
    let by_res = get("&mi_res_id=%2Fsubscriptions%2Fs%2Fid").await;
    assert!(GUID_RE(by_res["client_id"].as_str().unwrap()));
    assert_ne!(by_res["client_id"], sys1["client_id"]);
}

#[tokio::test]
async fn expires_in_query_changes_the_lifetime() {
    let srv = server().await;
    let url = format!("{}/_azemu/identity/msi/token?resource=r&api-version=2019-08-01&expires_in=120", gateway(&srv));
    let body = srv.send("GET", &url, &[("X-IDENTITY-HEADER", "a")], "").await.json();
    let c = claims(body["access_token"].as_str().unwrap());
    assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 120);
}

#[tokio::test]
async fn imds_response_shape() {
    let srv = server().await;
    let url = format!("{}/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fvault.azure.net", gateway(&srv));
    let r = srv.send("GET", &url, &[("Metadata", "true")], "").await;
    assert_eq!(r.status, 200);
    let b = r.json();
    for k in ["access_token", "client_id", "expires_in", "expires_on", "ext_expires_in", "not_before", "resource", "token_type"] {
        assert!(b[k].is_string(), "{k} must be a string: {b}");
    }
    assert_eq!(b["expires_in"], "3600");
    assert_eq!(b["token_type"], "Bearer");
    assert_eq!(b["resource"], "https://vault.azure.net");
    let c = verify(b["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(c["aud"], "https://vault.azure.net");
    assert_eq!(c["exp"].to_string(), b["expires_on"].as_str().unwrap());
    assert_eq!(c["nbf"].to_string(), b["not_before"].as_str().unwrap());

    let r = srv.send("GET", &format!("{url}&client_id=cafe"), &[("Metadata", "true")], "").await;
    assert_eq!(r.json()["client_id"], "cafe");
}

#[tokio::test]
async fn imds_probe_without_metadata_header_is_400_like_imds() {
    let srv = server().await;
    let url = format!("{}/metadata/identity/oauth2/token?api-version=2018-02-01&resource=r", gateway(&srv));
    for headers in [&[][..], &[("Metadata", "false")][..], &[("Metadata", "True")][..]] {
        let r = srv.send("GET", &url, headers, "").await;
        assert_eq!(r.status, 400);
        assert_eq!(r.json()["error"], "bad_request_102");
        assert!(r.json()["error_description"].as_str().unwrap().contains("metadata header"));
    }
    let r = srv
        .send("GET", &format!("{}/metadata/identity/oauth2/token?api-version=2018-02-01", gateway(&srv)), &[("Metadata", "true")], "")
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"], "invalid_request");
}

#[tokio::test]
async fn helper_endpoint() {
    let srv = server().await;
    let base = format!("{}/_azemu/identity/token", gateway(&srv));
    let r = srv.send("GET", &format!("{base}?resource=https%3A%2F%2Fstorage.azure.com%2F"), &[], "").await;
    assert_eq!(r.status, 200);
    let b = r.json();
    let c = verify(b["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(c["aud"], "https://storage.azure.com/");
    // scope= works and drops /.default
    let r =
        srv.send("GET", &format!("{base}?scope=https%3A%2F%2Fvault.azure.net%2F.default&tenant=contoso.com&expires_in=30"), &[], "").await;
    let c = claims(r.json()["access_token"].as_str().unwrap());
    assert_eq!(c["aud"], "https://vault.azure.net");
    assert_eq!(c["exp"].as_i64().unwrap() - c["iat"].as_i64().unwrap(), 30);
    assert_eq!(srv.send("GET", &base, &[], "").await.status, 400);
}

#[tokio::test]
async fn unbuilt_operations_are_501_and_named() {
    let srv = server().await;
    let r = srv.send("GET", &format!("{}/metadata/identity/info", gateway(&srv)), &[], "").await;
    assert_eq!(r.status, 501);
    assert_eq!(r.json()["error"]["code"], "NotImplemented");
    assert_eq!(r.json()["error"]["message"], "identity: GET /metadata/identity/info");
    let r = srv.send("GET", &format!("{}/_azemu/identity/nothing", gateway(&srv)), &[], "").await;
    assert_eq!(r.status, 501);
    let r = srv.send("GET", &format!("{}/contoso/oauth2/v2.0/logout", login(&srv)), &[], "").await;
    assert_eq!(r.status, 501);
    assert!(r.json()["error"]["message"].as_str().unwrap().contains("/contoso/oauth2/v2.0/logout"));
}

#[tokio::test]
async fn signing_key_survives_reset() {
    let srv = server().await;
    let url = format!("{}/_azemu/identity/token?resource=r", gateway(&srv));
    let t = srv.send("GET", &url, &[], "").await.json()["access_token"].as_str().unwrap().to_string();
    srv.reset();
    verify(&t, &jwks(&srv).await);
}
