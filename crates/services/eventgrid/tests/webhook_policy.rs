//! Webhooks go only where the user said: a subscription whose target is a public address is
//! refused with a 400 that names the option; `WebhookPolicy` (`AZEMU_ALLOW_WEBHOOK_HOST`, `--allow-webhook-host`)
//! opens hosts. The same rule holds for the data-plane admin API, the config file and the public Rust API that ARM
//! uses (`put_source_subscription`). The check made when a connection is opened (a name that now points to a
//! public address) is tested in `src/http_client.rs` and `src/webhook_policy.rs`.
mod common;

use std::sync::Arc;

use azemu_eventgrid::{EventGridService, Options, SourceRef, SubscriptionConfig, WebhookPolicy};
use common::*;
use serde_json::{json, Value};

fn strict() -> Options {
    Options { webhooks: WebhookPolicy::default(), ..fast_options() }
}

fn with_hosts(allowed: &[&str]) -> Options {
    let allowed: Vec<String> = allowed.iter().map(|s| s.to_string()).collect();
    Options { webhooks: WebhookPolicy::new(&allowed, &[]), ..fast_options() }
}

async fn put_sub(t: &Eg, url: &str) -> (u16, Value) {
    let r = t.put_json("/topics/orders/subscriptions/s", &json!({"webhookUrl": url, "skipValidation": true})).await;
    (r.status, r.json())
}

async fn topic(opts: Options) -> Eg {
    let t = Eg::with(opts).await;
    assert_eq!(t.put_json("/topics/orders", &json!({})).await.status, 201);
    t
}

#[tokio::test]
async fn a_public_target_is_refused_by_default_and_the_message_names_the_option() {
    let t = topic(strict()).await;
    for url in ["https://example.com/hook", "http://8.8.8.8/hook", "https://hooks.example.org:8443/a?b=c", "http://169.254.169.254/latest"]
    {
        let (status, body) = put_sub(&t, url).await;
        assert_eq!(status, 400, "{url}: {body}");
        let text = body.to_string();
        assert!(text.contains("--allow-webhook-host") && text.contains("AZEMU_ALLOW_WEBHOOK_HOST"), "{url}: {text}");
    }
    // nothing was created
    assert_eq!(t.get_admin("/topics/orders/subscriptions/s").await.status, 404);
}

#[tokio::test]
async fn loopback_private_and_local_names_need_no_option() {
    let t = topic(strict()).await;
    for url in [
        "http://127.0.0.1:9000/hook",
        "http://localhost:7071/api/hook",
        "http://[::1]:9000/hook",
        "http://10.1.2.3/hook",
        "http://172.20.0.5:8080/hook",
        "http://192.168.1.10/hook",
        "http://my-function:7071/hook",
        "http://host.docker.internal:7071/hook",
        "http://app.localhost:3000/hook",
    ] {
        let (status, body) = put_sub(&t, url).await;
        assert!(status == 201 || status == 200, "{url}: {status} {body}");
    }
}

#[tokio::test]
async fn a_named_host_or_a_star_opens_the_target() {
    let t = topic(with_hosts(&["example.com"])).await;
    assert!(put_sub(&t, "https://hooks.example.com/a").await.0 < 300);
    assert_eq!(put_sub(&t, "https://hooks.example.org/a").await.0, 400);
    assert_eq!(put_sub(&t, "https://notexample.com/a").await.0, 400);

    let t = topic(with_hosts(&["203.0.113.7"])).await;
    assert!(put_sub(&t, "http://203.0.113.7/a").await.0 < 300);
    assert_eq!(put_sub(&t, "http://203.0.113.8/a").await.0, 400);

    let t = topic(with_hosts(&["*"])).await;
    assert!(put_sub(&t, "https://anything.example.net/a").await.0 < 300);
}

#[tokio::test]
async fn a_name_the_host_check_serves_is_allowed() {
    let t = topic(Options { webhooks: WebhookPolicy::new(&[], &["corp.example".to_string()]), ..fast_options() }).await;
    assert!(put_sub(&t, "https://hook.corp.example/a").await.0 < 300);
    assert_eq!(put_sub(&t, "https://hook.other.example/a").await.0, 400);
}

#[tokio::test]
async fn source_subscriptions_of_the_admin_api_follow_the_same_rule() {
    let t = Eg::with(strict()).await;
    let r =
        t.put_json("/sources/storage/acct/subscriptions/s", &json!({"webhookUrl": "https://example.com/x", "skipValidation": true})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains("--allow-webhook-host"));
    let r =
        t.put_json("/sources/storage/acct/subscriptions/s", &json!({"webhookUrl": "http://127.0.0.1:9/x", "skipValidation": true})).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn the_config_file_skips_a_public_target_and_keeps_the_rest() {
    let dir = std::env::temp_dir().join(format!("azemu-eg-webhook-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("eg.json");
    std::fs::write(
        &path,
        r#"{"topics":[{"name":"orders","subscriptions":[
            {"name":"public","webhookUrl":"https://example.com/hook"},
            {"name":"local","webhookUrl":"http://127.0.0.1:9/hook"}]}]}"#,
    )
    .unwrap();
    let t = Eg::with(Options { config_path: Some(path.clone()), ..strict() }).await;
    assert_eq!(t.get_admin("/topics/orders/subscriptions/public").await.status, 404);
    assert_eq!(t.get_admin("/topics/orders/subscriptions/local").await.status, 200);
    // with the host allowed, the same file loads both
    let t = Eg::with(Options { config_path: Some(path), ..with_hosts(&["example.com"]) }).await;
    assert_eq!(t.get_admin("/topics/orders/subscriptions/public").await.status, 200);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn the_rust_api_that_arm_uses_gives_the_same_400() {
    let service = Arc::new(EventGridService::with_options(strict()));
    let src = SourceRef::storage_account("acct");
    let cfg = |url: &str| SubscriptionConfig { webhook_url: Some(url.to_string()), skip_validation: true, ..Default::default() };
    let err = service.put_source_subscription(&src, "s", cfg("https://example.com/hook"), None).await.unwrap_err();
    assert_eq!(err.status, 400);
    assert!(err.message.contains("--allow-webhook-host"), "{}", err.message);
    let ok = service.put_source_subscription(&src, "s", cfg("http://127.0.0.1:9/hook"), None).await.unwrap();
    assert!(ok.created);
}
