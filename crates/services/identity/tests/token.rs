mod common;

use common::*;

const STORAGE: &str = "https%3A%2F%2Fstorage.azure.com%2F.default";

fn v2(srv: &azemu_core::testing::TestServer, tenant: &str) -> String {
    format!("{}/{tenant}/oauth2/v2.0/token", login(srv))
}

#[tokio::test]
async fn client_secret_v2() {
    let srv = server().await;
    let form = format!("grant_type=client_credentials&client_id=app-1&client_secret=s3cret&scope={STORAGE}");
    let r = post_form(&srv, &v2(&srv, "contoso.onmicrosoft.com"), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.header("cache-control").unwrap().contains("no-store"));
    let b = r.json();
    assert_eq!(b["token_type"], "Bearer");
    assert_eq!(b["expires_in"], 3600);
    assert_eq!(b["ext_expires_in"], 3600);
    assert!(b.get("refresh_token").is_none() && b.get("id_token").is_none());
    let c = verify(b["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(c["aud"], "https://storage.azure.com");
    assert_eq!(c["appid"], "app-1");
    assert_eq!(c["appidacr"], "1");
    assert_eq!(c["idtyp"], "app");
    assert_eq!(c["ver"], "1.0");
    assert_eq!(c["oid"], c["sub"]);
    assert_ne!(c["tid"], "contoso.onmicrosoft.com");
    assert!(c["iss"].as_str().unwrap().ends_with(&format!("{}/", c["tid"].as_str().unwrap())));
}

#[tokio::test]
async fn oid_is_stable_per_client_and_tid_per_tenant() {
    let srv = server().await;
    let get = |client: &'static str, tenant: &'static str| {
        let srv = &srv;
        async move {
            let form = format!("grant_type=client_credentials&client_id={client}&client_secret=x&scope={STORAGE}");
            claims(post_form(srv, &v2(srv, tenant), &form).await.json()["access_token"].as_str().unwrap())
        }
    };
    let a1 = get("a", "t1").await;
    let a2 = get("a", "t1").await;
    let b = get("b", "t1").await;
    let a_other_tenant = get("a", "t2").await;
    assert_eq!(a1["oid"], a2["oid"]);
    assert_ne!(a1["oid"], b["oid"]);
    assert_eq!(a1["oid"], a_other_tenant["oid"]);
    assert_ne!(a1["tid"], a_other_tenant["tid"]);
    assert_eq!(get("a", "common").await["tid"], get("a", "organizations").await["tid"]);
    let guid = "11111111-2222-3333-4444-555555555555";
    assert_eq!(get("a", guid).await["tid"], guid);
}

#[tokio::test]
async fn client_assertion_certificate_and_federated() {
    let srv = server().await;
    let form = format!(
        "grant_type=client_credentials&client_id=app-1&scope={STORAGE}&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer&client_assertion=a.b.c"
    );
    let r = post_form(&srv, &v2(&srv, "t"), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(claims(r.json()["access_token"].as_str().unwrap())["appidacr"], "2");
    // wrong assertion type
    let r = post_form(&srv, &v2(&srv, "t"), &form.replace("jwt-bearer&", "saml&")).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn client_secret_basic_auth() {
    let srv = server().await;
    let form = format!("grant_type=client_credentials&scope={STORAGE}");
    let r = srv
        .send(
            "POST",
            &v2(&srv, "t"),
            &[("Authorization", &basic("app-9", "pw")), ("Content-Type", "application/x-www-form-urlencoded")],
            form,
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(claims(r.json()["access_token"].as_str().unwrap())["appid"], "app-9");
}

#[tokio::test]
async fn expires_in_on_the_token_url() {
    let srv = server().await;
    let form = format!("grant_type=client_credentials&client_id=a&client_secret=x&scope={STORAGE}");
    let r = post_form(&srv, &format!("{}?expires_in=90", v2(&srv, "t")), &form).await;
    assert_eq!(r.json()["expires_in"], 90);
}

#[tokio::test]
async fn error_bodies_have_entra_fields() {
    let srv = server().await;
    let err = |r: &azemu_core::testing::TestResponse| {
        let j = r.json();
        for k in ["error", "error_description", "error_codes", "timestamp", "trace_id", "correlation_id"] {
            assert!(!j[k].is_null(), "missing {k}: {j}");
        }
        assert!(j["error_description"].as_str().unwrap().starts_with(&format!("AADSTS{}", j["error_codes"][0])));
        j
    };
    // unknown grant
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=implicit&client_id=a").await;
    assert_eq!(r.status, 400);
    assert_eq!(err(&r)["error"], "unsupported_grant_type");
    // missing grant_type
    let r = post_form(&srv, &v2(&srv, "t"), "client_id=a").await;
    assert_eq!(r.status, 400);
    assert_eq!(err(&r)["error"], "invalid_request");
    // missing client_id
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=client_credentials&client_secret=x").await;
    assert_eq!(r.status, 400);
    err(&r);
    // missing secret
    let r = post_form(&srv, &v2(&srv, "t"), &format!("grant_type=client_credentials&client_id=a&scope={STORAGE}")).await;
    assert_eq!(r.status, 401);
    assert_eq!(err(&r)["error"], "invalid_client");
    assert_eq!(r.json()["error_codes"][0], 7000216);
    // scope without /.default
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        "grant_type=client_credentials&client_id=a&client_secret=x&scope=https%3A%2F%2Fgraph.microsoft.com%2FUser.Read",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(err(&r)["error"], "invalid_scope");
    // no scope
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=client_credentials&client_id=a&client_secret=x").await;
    assert_eq!(r.status, 400);
    // two resources
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        "grant_type=password&client_id=a&username=u&password=p&scope=https%3A%2F%2Fa%2F.default+https%3A%2F%2Fb%2F.default",
    )
    .await;
    assert_eq!(err(&r)["error"], "invalid_scope");
    // GET on the token endpoint
    let r = srv.send("GET", &v2(&srv, "t"), &[], "").await;
    assert_eq!(r.status, 405);
    // correlation id is echoed
    let r = srv
        .send("POST", &v2(&srv, "t"), &[("client-request-id", "6f1b2c3d-0000-4000-8000-000000000001")], "grant_type=x&client_id=a")
        .await;
    assert_eq!(r.json()["correlation_id"], "6f1b2c3d-0000-4000-8000-000000000001");
}

#[tokio::test]
async fn v1_token_endpoint_uses_resource_and_string_numbers() {
    let srv = server().await;
    let url = format!("{}/t/oauth2/token", login(&srv));
    let r =
        post_form(&srv, &url, "grant_type=client_credentials&client_id=a&client_secret=x&resource=https%3A%2F%2Fmanagement.azure.com%2F")
            .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    assert_eq!(b["resource"], "https://management.azure.com/");
    assert_eq!(b["expires_in"], "3600");
    assert!(b["expires_on"].is_string() && b["not_before"].is_string());
    assert_eq!(claims(b["access_token"].as_str().unwrap())["aud"], "https://management.azure.com/");
    let r = post_form(&srv, &url, "grant_type=client_credentials&client_id=a&client_secret=x").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn password_then_refresh() {
    let srv = server().await;
    let form = "grant_type=password&client_id=pub&username=alice%40example.com&password=pw&client_info=1&scope=https%3A%2F%2Fstorage.azure.com%2Fuser_impersonation+openid+profile+offline_access";
    let r = post_form(&srv, &v2(&srv, "t"), form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    let jwks = jwks(&srv).await;
    let at = verify(b["access_token"].as_str().unwrap(), &jwks);
    assert_eq!(at["upn"], "alice@example.com");
    assert_eq!(at["scp"], "user_impersonation");
    assert_eq!(at["appidacr"], "0");
    assert!(at.get("idtyp").is_none());
    assert_eq!(at["aud"], "https://storage.azure.com");
    assert_eq!(b["scope"], "https://storage.azure.com/user_impersonation openid profile offline_access");
    let id = verify(b["id_token"].as_str().unwrap(), &jwks);
    assert_eq!(id["aud"], "pub");
    assert_eq!(id["oid"], at["oid"]);
    // client_info is base64url JSON with uid and utid
    use base64::Engine;
    let ci: serde_json::Value =
        serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b["client_info"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(ci["uid"], at["oid"]);
    assert_eq!(ci["utid"], at["tid"]);

    let rt = b["refresh_token"].as_str().unwrap();
    let rt_enc = percent_encoding::utf8_percent_encode(rt, percent_encoding::NON_ALPHANUMERIC);
    // refresh with a different scope
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        &format!("grant_type=refresh_token&client_id=pub&refresh_token={rt_enc}&scope=https%3A%2F%2Fvault.azure.net%2F.default"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b2 = r.json();
    let at2 = verify(b2["access_token"].as_str().unwrap(), &jwks);
    assert_eq!(at2["aud"], "https://vault.azure.net");
    assert_eq!(at2["upn"], "alice@example.com");
    assert!(b2["refresh_token"].is_string());
    // refresh without scope reuses the stored one
    let r = post_form(&srv, &v2(&srv, "t"), &format!("grant_type=refresh_token&client_id=pub&refresh_token={rt_enc}")).await;
    assert_eq!(claims(r.json()["access_token"].as_str().unwrap())["aud"], "https://storage.azure.com");
    // garbage refresh token
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=refresh_token&client_id=pub&refresh_token=nope").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"], "invalid_grant");
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=refresh_token&client_id=pub").await;
    assert_eq!(r.status, 400);
    // password flow errors
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=password&client_id=pub&username=a&scope=openid").await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn no_refresh_token_without_offline_access() {
    let srv = server().await;
    let r = post_form(&srv, &v2(&srv, "t"), "grant_type=password&client_id=p&username=a&password=b&scope=https%3A%2F%2Fa%2F.default").await;
    let b = r.json();
    assert!(b.get("refresh_token").is_none() && b.get("id_token").is_none());
}

#[tokio::test]
async fn device_code_flow_is_approved_immediately() {
    let srv = server().await;
    let r = post_form(
        &srv,
        &format!("{}/t/oauth2/v2.0/devicecode", login(&srv)),
        &format!("client_id=pub&scope={STORAGE}+openid+offline_access"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let d = r.json();
    for k in ["user_code", "device_code", "verification_uri", "message"] {
        assert!(d[k].is_string(), "{k}");
    }
    assert_eq!(d["expires_in"], 900);
    assert_eq!(d["interval"], 5);
    assert!(d["message"].as_str().unwrap().contains(d["user_code"].as_str().unwrap()));
    assert!(d["verification_uri"].as_str().unwrap().starts_with(&login(&srv)));
    let code = percent_encoding::utf8_percent_encode(d["device_code"].as_str().unwrap(), percent_encoding::NON_ALPHANUMERIC);
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        &format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id=pub&device_code={code}"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    let at = verify(b["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(at["aud"], "https://storage.azure.com");
    assert!(at["upn"].as_str().unwrap().contains('@'));
    assert!(b["refresh_token"].is_string() && b["id_token"].is_string());

    let r =
        post_form(&srv, &v2(&srv, "t"), "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id=pub&device_code=zzz")
            .await;
    assert_eq!(r.status, 400);
    let r = post_form(&srv, &format!("{}/t/oauth2/v2.0/devicecode", login(&srv)), "client_id=pub").await;
    assert_eq!(r.status, 400);
    // v1 device code asks for resource
    let r = post_form(&srv, &format!("{}/t/oauth2/devicecode", login(&srv)), "client_id=pub&resource=https%3A%2F%2Fa").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn on_behalf_of() {
    let srv = server().await;
    let user = post_form(
        &srv,
        &v2(&srv, "t"),
        "grant_type=password&client_id=pub&username=carol%40example.com&password=p&scope=api%3A%2F%2Fmy-api%2F.default",
    )
    .await
    .json();
    let assertion = user["access_token"].as_str().unwrap();
    let form = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&client_id=api&client_secret=s&assertion={assertion}&requested_token_use=on_behalf_of&scope=https%3A%2F%2Fgraph.microsoft.com%2FUser.Read"
    );
    let r = post_form(&srv, &v2(&srv, "t"), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let c = verify(r.json()["access_token"].as_str().unwrap(), &jwks(&srv).await);
    assert_eq!(c["upn"], "carol@example.com");
    assert_eq!(c["aud"], "https://graph.microsoft.com");
    assert_eq!(c["scp"], "User.Read");
    assert_eq!(c["appid"], "api");
    // not a JWT
    let r = post_form(&srv, &v2(&srv, "t"), &form.replace(assertion, "garbage")).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"], "invalid_grant");
    // missing requested_token_use
    let r = post_form(&srv, &v2(&srv, "t"), &form.replace("requested_token_use=on_behalf_of", "x=1")).await;
    assert_eq!(r.status, 400);
    // public client cannot do OBO
    let r = post_form(&srv, &v2(&srv, "t"), &form.replace("client_secret=s", "x=1")).await;
    assert_eq!(r.status, 401);
}

#[tokio::test]
async fn token_endpoint_is_also_on_the_path_prefix() {
    let srv = server().await;
    let url = format!("{}/t/oauth2/v2.0/token", login_prefix(&srv));
    let r = post_form(&srv, &url, &format!("grant_type=client_credentials&client_id=a&client_secret=x&scope={STORAGE}")).await;
    assert_eq!(r.status, 200);
    // the id token issuer matches the discovery document's issuer on the same authority
    let r = post_form(&srv, &url, "grant_type=password&client_id=a&username=u&password=p&scope=openid").await;
    let id = claims(r.json()["id_token"].as_str().unwrap());
    let d = srv.send("GET", &format!("{}/t/v2.0/.well-known/openid-configuration", login_prefix(&srv)), &[], "").await.json();
    assert_eq!(id["iss"], d["issuer"]);
}

/// MSAL for Go compares the scopes it asked for (`{resource}/.default openid offline_access profile`)
/// with the `scope` of the answer and refuses an answer that leaves one out. So every grant that
/// answers with a `scope` has to carry what was asked for, `.default` included.
#[tokio::test]
async fn every_grant_answers_with_the_scopes_that_were_asked_for() {
    let srv = server().await;
    let asked = "https://storage.azure.com/.default openid offline_access profile";
    let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
    let check = |b: &serde_json::Value, what: &str| {
        let scope = b["scope"].as_str().unwrap_or_else(|| panic!("{what}: no scope in {b}"));
        let got: Vec<String> = scope.split(' ').map(str::to_ascii_lowercase).collect();
        for want in asked.split(' ') {
            assert!(got.contains(&want.to_ascii_lowercase()), "{what}: `{want}` missing from `{scope}`");
        }
        // what was granted is still there
        assert!(got.contains(&"https://storage.azure.com/user_impersonation".to_string()), "{what}: {scope}");
    };

    // password
    let r = post_form(&srv, &v2(&srv, "t"), &format!("grant_type=password&client_id=pub&username=a%40b.c&password=p&scope={}", enc(asked)))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let b = r.json();
    check(&b, "password");
    // refresh token, with and without a scope in the request
    let rt = enc(b["refresh_token"].as_str().unwrap());
    let r =
        post_form(&srv, &v2(&srv, "t"), &format!("grant_type=refresh_token&client_id=pub&refresh_token={rt}&scope={}", enc(asked))).await;
    check(&r.json(), "refresh_token");
    let r = post_form(&srv, &v2(&srv, "t"), &format!("grant_type=refresh_token&client_id=pub&refresh_token={rt}")).await;
    check(&r.json(), "refresh_token without scope");
    // on behalf of
    let assertion = b["access_token"].as_str().unwrap();
    let form = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&client_id=api&client_secret=s&assertion={assertion}&requested_token_use=on_behalf_of&scope={}",
        enc(asked)
    );
    let r = post_form(&srv, &v2(&srv, "t"), &form).await;
    assert_eq!(r.status, 200, "{}", r.text());
    check(&r.json(), "on_behalf_of");
    // device code: the URN and the short grant type name, both
    let r = post_form(&srv, &format!("{}/t/oauth2/v2.0/devicecode", login(&srv)), &format!("client_id=pub&scope={}", enc(asked))).await;
    let code = enc(r.json()["device_code"].as_str().unwrap());
    for grant in ["urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code", "device_code"] {
        let r = post_form(&srv, &v2(&srv, "t"), &format!("grant_type={grant}&client_id=pub&device_code={code}")).await;
        assert_eq!(r.status, 200, "{grant}: {}", r.text());
        check(&r.json(), grant);
    }
    // client credentials: no scope in the answer, as Entra does (MSAL then takes what it asked for)
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        "grant_type=client_credentials&client_id=a&client_secret=s&scope=https%3A%2F%2Fstorage.azure.com%2F.default",
    )
    .await;
    assert!(r.json().get("scope").is_none());
    // a resource written in the request in a shape that is not `{resource}/{permission}` still comes back as written
    let r = post_form(
        &srv,
        &v2(&srv, "t"),
        "grant_type=password&client_id=pub&username=a%40b.c&password=p&scope=api%3A%2F%2Fmy-api+offline_access",
    )
    .await;
    let scope = r.json()["scope"].as_str().unwrap().to_string();
    assert!(scope.split(' ').any(|s| s == "api://my-api"), "{scope}");
}
