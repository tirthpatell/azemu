mod common;

use common::*;

#[tokio::test]
async fn the_app_id_lookup() {
    let t = start().await;
    let r = t.srv.send("GET", &format!("{}/api/profiles/{IKEY}/appId", t.ingest), &[], "").await;
    assert_eq!(r.status, 200);
    let id = r.text();
    assert!(uuid_like(&id), "{id}");
    // the same key has the same id, another key another one; the root form works too
    let again = t.srv.send("GET", &format!("{}/api/profiles/{IKEY}/appId", t.root), &[], "").await;
    assert_eq!(again.text(), id);
    let other = t.srv.send("GET", &format!("{}/api/profiles/{}/appId", t.ingest, "99999999-2222-3333-4444-555555555555"), &[], "").await;
    assert_ne!(other.text(), id);
    let r = t.srv.send("POST", &format!("{}/api/profiles/{IKEY}/appId", t.ingest), &[], "").await;
    assert_eq!((r.status, r.header("allow")), (405, Some("GET")));
    // what is not a key: Azure's 404 in plain text
    let r = t.srv.send("GET", &format!("{}/api/profiles/abc/appId", t.ingest), &[], "").await;
    assert_eq!((r.status, r.text()), (404, "Profile Not Found".to_string()));
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

#[tokio::test]
async fn live_metrics_ping_says_nobody_is_watching_in_both_forms() {
    let t = start().await;
    // the classic form (.NET and Java SDKs): the data is in x-ms-qps-* headers
    let classic = [("x-ms-qps-stream-id", "abc"), ("x-ms-qps-transmission-time", "1"), ("x-ms-qps-configuration-etag", "etag1")];
    // the REST form of the current exporters: the data is the JSON body
    let rest = [JSON];
    for (url, headers, body) in [
        (format!("{}/QuickPulseService.svc/ping?ikey={IKEY}", t.live), &classic[..], ""),
        (
            format!("{}/QuickPulseService.svc/ping?api-version=2024-04-01-preview&ikey={IKEY}", t.live),
            &rest[..],
            r#"{"InstrumentationKey":null}"#,
        ),
        (format!("{}/QuickPulseService.svc/ping?ikey={IKEY}", t.root), &classic[..], ""),
        // the case of the path does not matter on the real service (an IIS .svc path)
        (format!("{}/quickpulseservice.svc/PING?ikey={IKEY}", t.live), &classic[..], ""),
    ] {
        let r = t.srv.send("POST", &url, headers, body).await;
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.header("x-ms-qps-subscribed"), Some("false"), "{url}");
        assert_eq!(r.body.len(), 0);
    }
}

#[tokio::test]
async fn live_metrics_ping_that_azure_cannot_read_is_400() {
    let t = start().await;
    // no key at all
    let r = t.srv.send("POST", &format!("{}/QuickPulseService.svc/ping", t.live), &[("x-ms-qps-stream-id", "a")], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["Code"], "InvalidInput");
    assert_eq!(r.json()["Message"], "iKey is garbage or not in a valid GUID format");
    // no data in headers or body
    for method in ["POST", "GET"] {
        let r = t.srv.send(method, &format!("{}/QuickPulseService.svc/ping?ikey={IKEY}", t.live), &[], "").await;
        assert_eq!(r.status, 400, "{method}");
        assert_eq!(r.json()["Code"], "InvalidOperation");
        assert_eq!(r.json()["Message"], "Failed to parse monitoring data points from the ping request.");
    }
}

#[tokio::test]
async fn live_metrics_post_needs_json_and_says_not_subscribed() {
    let t = start().await;
    for url in [
        format!("{}/QuickPulseService.svc/post?ikey={IKEY}", t.live),
        format!("{}/QuickPulseService.svc/post?api-version=2024-04-01-preview&ikey={IKEY}", t.root),
    ] {
        let r = t.srv.send("POST", &url, &[JSON], "[]").await;
        assert_eq!(r.status, 200, "{url}");
        assert_eq!(r.header("x-ms-qps-subscribed"), Some("false"));
    }
    let r = t.srv.send("POST", &format!("{}/QuickPulseService.svc/post?ikey={IKEY}", t.live), &[], "zzz").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["Message"], "Failed to parse monitoring data points.");
}
