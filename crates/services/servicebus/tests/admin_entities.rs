mod admin_common;

use admin_common::*;

const ATOM_ENTRY: &str = "application/atom+xml; type=entry; charset=utf-8";
const ATOM_FEED: &str = "application/atom+xml; type=feed; charset=utf-8";

#[tokio::test]
async fn create_queue_with_defaults_matches_the_recording() {
    let a = admin().await;
    let r = a.queue("queue_testaddf", "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    assert_eq!(r.header("server"), Some("Microsoft-HTTPAPI/2.0"));
    assert_eq!(r.header("etag"), None, "the create answer has no ETag");
    let b = r.text();
    assert_order(
        &b,
        &[
            "LockDuration",
            "MaxSizeInMegabytes",
            "RequiresDuplicateDetection",
            "RequiresSession",
            "DefaultMessageTimeToLive",
            "DeadLetteringOnMessageExpiration",
            "DuplicateDetectionHistoryTimeWindow",
            "MaxDeliveryCount",
            "EnableBatchedOperations",
            "SizeInBytes",
            "MessageCount",
            "IsAnonymousAccessible",
            "AuthorizationRules",
            "Status",
            "CreatedAt",
            "UpdatedAt",
            "SupportOrdering",
            "AutoDeleteOnIdle",
            "EnablePartitioning",
            "EntityAvailabilityStatus",
            "EnableExpress",
            "MaxMessageSizeInKilobytes",
        ],
    );
    assert!(!b.contains("CountDetails") && !b.contains("AccessedAt"));
    assert_eq!(tag(&b, "LockDuration").unwrap(), "PT1M");
    assert_eq!(tag(&b, "MaxSizeInMegabytes").unwrap(), "1024");
    assert_eq!(tag(&b, "DefaultMessageTimeToLive").unwrap(), "P10675199DT2H48M5.4775807S");
    assert_eq!(tag(&b, "AutoDeleteOnIdle").unwrap(), "P10675199DT2H48M5.4775807S");
    assert_eq!(tag(&b, "DuplicateDetectionHistoryTimeWindow").unwrap(), "PT10M");
    assert_eq!(tag(&b, "MaxDeliveryCount").unwrap(), "10");
    assert_eq!(tag(&b, "Status").unwrap(), "Active");
    assert_eq!(tag(&b, "SupportOrdering").unwrap(), "true");
    assert_eq!(tag(&b, "EntityAvailabilityStatus").unwrap(), "Available");
    assert_eq!(tag(&b, "MaxMessageSizeInKilobytes").unwrap(), "256");
    assert!(b.contains("<AuthorizationRules></AuthorizationRules>"));
    assert!(b.contains("<title type=\"text\">queue_testaddf</title>"));
    assert!(b.contains("xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\" xmlns:i=\"http://www.w3.org/2001/XMLSchema-instance\""));
}

#[tokio::test]
async fn get_queue_has_runtime_data_and_an_etag() {
    let a = admin().await;
    a.queue("q", "").await;
    let r = a.get("/q?enrich=false&api-version=2021-05").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    let etag = r.header("etag").unwrap().to_string();
    assert!(etag.chars().all(|c| c.is_ascii_digit()) && etag.len() >= 18, "{etag}");
    let b = r.text();
    assert_order(&b, &["UpdatedAt", "AccessedAt", "SupportOrdering", "CountDetails", "AutoDeleteOnIdle"]);
    assert_eq!(tag(&b, "AccessedAt").unwrap(), "0001-01-01T00:00:00Z");
    assert!(b.contains("<CountDetails xmlns:d2p1=\"http://schemas.microsoft.com/netservices/2011/06/servicebus\"><d2p1:ActiveMessageCount>0</d2p1:ActiveMessageCount>"));
    assert!(b.contains("<d2p1:TransferDeadLetterMessageCount>0</d2p1:TransferDeadLetterMessageCount>"));
    // the entry id is the request URL
    assert!(b.contains("/q?enrich=false&amp;api-version=2021-05</id>"), "{b}");
}

#[tokio::test]
async fn get_of_an_unknown_entity_is_the_service_document() {
    let a = admin().await;
    for path in ["/non_existing_queue?enrich=false", "/?enrich=false"] {
        let r = a.get(path).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.header("content-type"), Some(ATOM_FEED));
        let b = r.text();
        assert!(
            b.contains("<title type=\"text\">Publicly Listed Services</title>") && b.contains("<generator>Service Bus 1.1</generator>"),
            "{b}"
        );
        assert!(!b.contains("<entry"));
    }
}

#[tokio::test]
async fn duplicate_create_is_409_with_the_recorded_text() {
    let a = admin().await;
    a.queue("rtofdk", "").await;
    let r = a.queue("rtofdk", "").await;
    assert_eq!(r.status, 409);
    assert_eq!(r.header("content-type"), Some("application/xml; charset=utf-8"));
    assert!(r.header("etag").is_some());
    let b = r.text();
    assert!(b.starts_with("<Error><Code>409</Code><Detail>SubCode=40900. Conflict. You're requesting an operation that isn't allowed in the resource's current state. To know more visit https://aka.ms/sbResourceMgrExceptions. . TrackingId:"), "{b}");
    assert!(b.contains("SystemTracker:default.servicebus.windows.net:rtofdk, Timestamp:"));
    // a topic with the same name conflicts too
    assert_eq!(a.topic("rtofdk", "").await.status, 409);
}

#[tokio::test]
async fn names_are_case_insensitive_and_keep_their_casing() {
    let a = admin().await;
    assert_eq!(a.queue("MixedCaseQueue", "").await.status, 201);
    assert_eq!(a.queue("mixedcasequeue", "").await.status, 409);
    let r = a.get("/MIXEDCASEQUEUE").await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("<title type=\"text\">MixedCaseQueue</title>"));
    assert_eq!(a.delete("/mixedCASEqueue").await.status, 200);
    assert_eq!(a.get("/MixedCaseQueue").await.header("content-type"), Some(ATOM_FEED));
}

#[tokio::test]
async fn queue_name_with_an_encoded_slash() {
    let a = admin().await;
    assert_eq!(a.queue("txt%2F.-_123", "").await.status, 201);
    let r = a.get("/$Resources/queues?$skip=0&$top=100&api-version=2021-05").await;
    assert_eq!(r.status, 200);
    assert!(r.text().contains("<title type=\"text\">txt/.-_123</title>"), "{}", r.text());
    assert_eq!(a.get("/txt%2F.-_123").await.status, 200);
    assert_eq!(a.delete("/txt%2F.-_123").await.status, 200);
    assert!(!a.get("/$Resources/queues").await.text().contains("<entry"));
}

#[tokio::test]
async fn settings_round_trip() {
    let a = admin().await;
    let inner = "<LockDuration>PT13S</LockDuration><MaxSizeInMegabytes>3072</MaxSizeInMegabytes><RequiresSession>true</RequiresSession>\
        <DefaultMessageTimeToLive>PT11M2S</DefaultMessageTimeToLive><DeadLetteringOnMessageExpiration>true</DeadLetteringOnMessageExpiration>\
        <DuplicateDetectionHistoryTimeWindow>PT12M3S</DuplicateDetectionHistoryTimeWindow><MaxDeliveryCount>14</MaxDeliveryCount>\
        <EnableBatchedOperations>false</EnableBatchedOperations><Status>SendDisabled</Status><AutoDeleteOnIdle>PT10M1S</AutoDeleteOnIdle>\
        <EnableExpress>true</EnableExpress><UserMetadata>a &amp; b &lt;c&gt;</UserMetadata>";
    let r = a.queue("q", inner).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let b = a.get("/q").await.text();
    for (n, v) in [
        ("LockDuration", "PT13S"),
        ("MaxSizeInMegabytes", "3072"),
        ("RequiresSession", "true"),
        ("DefaultMessageTimeToLive", "PT11M2S"),
        ("DeadLetteringOnMessageExpiration", "true"),
        ("DuplicateDetectionHistoryTimeWindow", "PT12M3S"),
        ("MaxDeliveryCount", "14"),
        ("EnableBatchedOperations", "false"),
        ("Status", "SendDisabled"),
        ("AutoDeleteOnIdle", "PT10M1S"),
        ("EnableExpress", "true"),
        ("UserMetadata", "a &amp; b &lt;c&gt;"),
    ] {
        assert_eq!(tag(&b, n).as_deref(), Some(v), "{n}");
    }
    // the engine sees the same settings
    let s = a.shared.state.lock().ns("default").queue("q").unwrap().core.settings.clone();
    assert_eq!((s.lock_duration_ms, s.max_delivery_count, s.requires_session, s.dead_letter_on_expiry), (13_000, 14, true, true));
    assert_eq!(s.default_ttl_ms, Some(662_000));
    assert_eq!(s.user_metadata.as_deref(), Some("a & b <c>"));
}

#[tokio::test]
async fn partitioned_entities_report_sixteen_times_the_size_and_no_ordering() {
    let a = admin().await;
    let r = a.queue("p", "<MaxSizeInMegabytes>3072</MaxSizeInMegabytes><EnablePartitioning>true</EnablePartitioning>").await;
    let b = r.text();
    assert_eq!(tag(&b, "MaxSizeInMegabytes").unwrap(), "49152");
    assert_eq!(tag(&b, "SupportOrdering").unwrap(), "false");
    let r = a.topic("pt", "<EnablePartitioning>true</EnablePartitioning>").await;
    assert_eq!(tag(&r.text(), "MaxSizeInMegabytes").unwrap(), "16384");
    // what the SDK sends back on update is the total and must not grow
    let upd = a
        .update(
            "/p",
            &entry("QueueDescription", "<MaxSizeInMegabytes>49152</MaxSizeInMegabytes><EnablePartitioning>true</EnablePartitioning>"),
        )
        .await;
    assert_eq!(upd.status, 200, "{}", upd.text());
    assert_eq!(tag(&upd.text(), "MaxSizeInMegabytes").unwrap(), "49152");
}

#[tokio::test]
async fn forwarding_targets_are_stored_as_paths_and_shown_as_urls() {
    let a = admin().await;
    a.queue("target", "").await;
    let host = format!("default.servicebus.localhost:{}", a.srv.port());
    let r = a
        .queue(
            "src",
            &format!("<ForwardTo>sb://other.example/target</ForwardTo><ForwardDeadLetteredMessagesTo>sb://{host}/target</ForwardDeadLetteredMessagesTo>"),
        )
        .await;
    assert_eq!(r.status, 201);
    let b = a.get("/src").await.text();
    assert_eq!(tag(&b, "ForwardTo").unwrap(), format!("sb://{host}/target"));
    assert_eq!(tag(&b, "ForwardDeadLetteredMessagesTo").unwrap(), format!("sb://{host}/target"));
    assert_order(&b, &["EnableExpress", "ForwardTo", "ForwardDeadLetteredMessagesTo", "MaxMessageSizeInKilobytes"]);
    {
        let mut st = a.shared.state.lock();
        let q = st.ns("default").queue("src").unwrap();
        assert_eq!(q.core.settings.forward_to.as_deref(), Some("target"));
    }
    // an update without them clears them
    let u = a.update("/src", &entry("QueueDescription", "")).await;
    assert_eq!(u.status, 200);
    assert!(!a.get("/src").await.text().contains("ForwardTo"));
}

#[tokio::test]
async fn authorization_rules_are_echoed() {
    let a = admin().await;
    let rule = "<AuthorizationRules><AuthorizationRule i:type=\"SharedAccessAuthorizationRule\"><ClaimType>SharedAccessKey</ClaimType><ClaimValue>None</ClaimValue>\
        <Rights><AccessRights>Manage</AccessRights><AccessRights>Listen</AccessRights><AccessRights>Send</AccessRights></Rights><KeyName>test_key</KeyName>\
        <PrimaryKey>cHJpbWFyeQ==</PrimaryKey><SecondaryKey>c2Vjb25kYXJ5</SecondaryKey></AuthorizationRule></AuthorizationRules>";
    let r = a.queue("q", rule).await;
    let b = r.text();
    assert!(b.contains("<AuthorizationRule i:type=\"SharedAccessAuthorizationRule\"><ClaimType>SharedAccessKey</ClaimType><ClaimValue>None</ClaimValue><Rights><AccessRights>Manage</AccessRights><AccessRights>Listen</AccessRights><AccessRights>Send</AccessRights></Rights><KeyName>test_key</KeyName><PrimaryKey>cHJpbWFyeQ==</PrimaryKey><SecondaryKey>c2Vjb25kYXJ5</SecondaryKey></AuthorizationRule>"), "{b}");
    assert_eq!(a.get("/q").await.text().matches("<KeyName>test_key</KeyName>").count(), 1);
    // keys are made up when the request has none
    let r = a.topic("t", "<AuthorizationRules><AuthorizationRule><KeyName>k</KeyName><Rights><AccessRights>Send</AccessRights></Rights></AuthorizationRule></AuthorizationRules>").await;
    let b = r.text();
    assert!(tag(&b, "PrimaryKey").unwrap().len() >= 40 && tag(&b, "SecondaryKey").unwrap().len() >= 40, "{b}");
}

#[tokio::test]
async fn update_needs_if_match_and_an_existing_entity() {
    let a = admin().await;
    a.queue("q", "").await;
    let before = a.get("/q").await.header("etag").unwrap().to_string();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let r = a.update("/q", &entry("QueueDescription", "<LockDuration>PT2M</LockDuration><MaxDeliveryCount>21</MaxDeliveryCount>")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    assert!(r.header("etag").is_some());
    assert!(!r.text().contains("<published>"), "update answers carry no published");
    let b = a.get("/q").await;
    assert_eq!(tag(&b.text(), "LockDuration").unwrap(), "PT2M");
    assert_eq!(tag(&b.text(), "MaxDeliveryCount").unwrap(), "21");
    assert_ne!(b.header("etag").unwrap(), before, "the ETag follows the update time");
    // missing
    let r = a.update("/iewdm", &entry("QueueDescription", "")).await;
    assert_eq!(r.status, 404);
    assert!(
        r.text().contains("<Code>404</Code><Detail>SubCode=40400. Not Found. The Operation doesn't exist. To know more visit"),
        "{}",
        r.text()
    );
    // a topic under a queue's name is not an update of it
    assert_eq!(a.update("/q", &entry("TopicDescription", "")).await.status, 404);
}

#[tokio::test]
async fn update_keeps_the_messages() {
    use azemu_amqp::message::Message;
    use azemu_servicebus::core::Incoming;
    let a = admin().await;
    a.queue("q", "").await;
    {
        let mut st = a.shared.state.lock();
        let q = st.ns("default").queue_mut("q").unwrap();
        q.core.enqueue(Incoming::new(Message::data(b"x".to_vec()), 10), azemu_servicebus::state::now_ms()).unwrap();
    }
    a.update("/q", &entry("QueueDescription", "<LockDuration>PT3M</LockDuration>")).await;
    let b = a.get("/q").await.text();
    assert_eq!(tag(&b, "MessageCount").unwrap(), "1");
    assert_eq!(a.shared.state.lock().ns("default").queue("q").unwrap().core.settings.lock_duration_ms, 180_000);
}

#[tokio::test]
async fn updates_that_change_fixed_properties_are_400() {
    let a = admin().await;
    a.queue("q", "").await;
    let r = a.update("/q", &entry("QueueDescription", "<RequiresSession>true</RequiresSession>")).await;
    assert_eq!(r.status, 400);
    assert!(r.header("etag").is_some());
    assert!(
        r.text().contains("SubCode=40000. The value for the RequiresSession property of an existing Queue cannot be changed."),
        "{}",
        r.text()
    );
    let r = a.update("/q", &entry("QueueDescription", "<EnablePartitioning>true</EnablePartitioning>")).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The value for the EnablePartitioning property of an existing Queue cannot be changed."));
    let r = a.update("/q", &entry("QueueDescription", "<RequiresDuplicateDetection>true</RequiresDuplicateDetection>")).await;
    assert_eq!(r.status, 400);
    a.topic("t", "").await;
    let r = a.update("/t", &entry("TopicDescription", "<EnablePartitioning>true</EnablePartitioning>")).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("existing Topic cannot be changed"));
}

#[tokio::test]
async fn validation_errors_are_400_with_the_recorded_texts() {
    let a = admin().await;
    // lock duration over five minutes (create and update), recorded text including the CR/LF layout
    let r = a.queue("q", "<LockDuration>P25D</LockDuration>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains(
        "SubCode=40000. The supplied lock time exceeds the allowed maximum of '5' minutes. To know more visit https://aka.ms/sbResourceMgrExceptions. &#xD;\nParameter name: LockDuration&#xD;\nActual value was 25.00:00:00. TrackingId:"
    ), "{}", r.text());
    a.queue("q", "").await;
    let r = a.update("/q", &entry("QueueDescription", "<LockDuration>P25D</LockDuration>")).await;
    assert_eq!(r.status, 400);
    assert!(r.header("etag").is_some());
    assert_eq!(a.queue("q2", "<LockDuration>PT5M</LockDuration>").await.status, 201);
    assert_eq!(a.queue("q3", "<LockDuration>PT5M1S</LockDuration>").await.status, 400);
    // duplicate detection window
    let r = a.topic("t", "<DuplicateDetectionHistoryTimeWindow>P25D</DuplicateDetectionHistoryTimeWindow>").await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("SubCode=40000. The value supplied must be between 00:00:20 and 7.00:00:00.&#xD;\nParameter name: DuplicateDetectionHistoryTimeWindow&#xD;\nActual value was 25.00:00:00. TrackingId:"), "{}", r.text());
    assert_eq!(a.queue("q4", "<DuplicateDetectionHistoryTimeWindow>PT19S</DuplicateDetectionHistoryTimeWindow>").await.status, 400);
    // 256 KB is the ceiling on Standard
    let r = a.queue("q5", "<MaxMessageSizeInKilobytes>1024</MaxMessageSizeInKilobytes>").await;
    assert_eq!(r.status, 400);
    assert!(
        r.text().contains("SubCode=40000. Bad Request. To know more visit https://aka.ms/sbResourceMgrExceptions. . TrackingId:"),
        "{}",
        r.text()
    );
    assert_eq!(a.queue("q6", "<MaxMessageSizeInKilobytes>128</MaxMessageSizeInKilobytes>").await.status, 201);
    // the rest
    for (inner, why) in [
        ("<MaxDeliveryCount>0</MaxDeliveryCount>", "max delivery count"),
        ("<Status>Bogus</Status>", "status"),
        ("<AutoDeleteOnIdle>PT1M</AutoDeleteOnIdle>", "auto delete"),
        ("<RequiresSession>maybe</RequiresSession>", "bool"),
        ("<LockDuration>1 minute</LockDuration>", "duration"),
        ("<MaxSizeInMegabytes>0</MaxSizeInMegabytes>", "size"),
    ] {
        assert_eq!(a.queue("qx", inner).await.status, 400, "{why}");
    }
    assert!(a.get("/qx").await.header("content-type") == Some(ATOM_FEED), "a refused create creates nothing");
}

#[tokio::test]
async fn api_version_2017_04_has_no_max_message_size() {
    let a = admin().await;
    let r = a.create("/q?api-version=2017-04", &entry("QueueDescription", "")).await;
    assert_eq!(r.status, 201);
    assert!(!r.text().contains("MaxMessageSizeInKilobytes"));
    let r = a
        .create("/q2?api-version=2017-04", &entry("QueueDescription", "<MaxMessageSizeInKilobytes>1024</MaxMessageSizeInKilobytes>"))
        .await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The entity description of type 'QueueDescription' has unsupported properties for api-version '2017-04' in the request. Try including the correct api-version query string in the request."), "{}", r.text());
    assert!(a
        .get("/q?enrich=false&api-version=2021-05")
        .await
        .text()
        .contains("<MaxMessageSizeInKilobytes>256</MaxMessageSizeInKilobytes>"));
}

#[tokio::test]
async fn delete_answers_with_the_etag_and_no_body() {
    let a = admin().await;
    a.queue("q", "").await;
    let etag = a.get("/q").await.header("etag").unwrap().to_string();
    let r = a.delete("/q?api-version=2021-05").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("etag"), Some(etag.as_str()));
    assert!(r.body.is_empty());
    let r = a.delete("/q").await;
    assert_eq!(r.status, 404);
    assert!(r.text().contains("<Code>404</Code><Detail>No service is hosted at the specified address. TrackingId:"), "{}", r.text());
    assert_eq!(a.delete("/non_existing_queue").await.status, 404);
}

#[tokio::test]
async fn queue_can_be_recreated_after_delete_and_wakes_nobody_stale() {
    let a = admin().await;
    a.queue("q", "<MaxDeliveryCount>3</MaxDeliveryCount>").await;
    a.delete("/q").await;
    let r = a.queue("q", "").await;
    assert_eq!(r.status, 201);
    assert_eq!(tag(&r.text(), "MaxDeliveryCount").unwrap(), "10");
}

#[tokio::test]
async fn topics_have_their_own_shape() {
    let a = admin().await;
    let r = a.topic("topic_testaddf", "").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let b = r.text();
    assert_order(
        &b,
        &[
            "DefaultMessageTimeToLive",
            "MaxSizeInMegabytes",
            "RequiresDuplicateDetection",
            "DuplicateDetectionHistoryTimeWindow",
            "EnableBatchedOperations",
            "SizeInBytes",
            "FilteringMessagesBeforePublishing",
            "IsAnonymousAccessible",
            "AuthorizationRules",
            "Status",
            "CreatedAt",
            "UpdatedAt",
            "SupportOrdering",
            "AutoDeleteOnIdle",
            "EnablePartitioning",
            "EntityAvailabilityStatus",
            "EnableSubscriptionPartitioning",
            "EnableExpress",
            "MaxMessageSizeInKilobytes",
        ],
    );
    assert!(b.contains("<TopicDescription xmlns="));
    assert_eq!(tag(&b, "SupportOrdering").unwrap(), "true");
    let g = a.get("/topic_testaddf?enrich=false").await;
    let b = g.text();
    assert_order(&b, &["AccessedAt", "SupportOrdering", "CountDetails", "SubscriptionCount", "AutoDeleteOnIdle"]);
    assert_eq!(tag(&b, "SubscriptionCount").unwrap(), "0");
    // settable properties
    let r = a.topic("t2", "<SupportOrdering>false</SupportOrdering><FilteringMessagesBeforePublishing>true</FilteringMessagesBeforePublishing><Status>SendDisabled</Status><UserMetadata>m</UserMetadata><EnableExpress>true</EnableExpress>").await;
    let b = r.text();
    assert_eq!(tag(&b, "SupportOrdering").unwrap(), "false");
    assert_eq!(tag(&b, "FilteringMessagesBeforePublishing").unwrap(), "true");
    assert_eq!(tag(&b, "Status").unwrap(), "SendDisabled");
    assert_eq!(tag(&b, "UserMetadata").unwrap(), "m");
    // update
    let u = a.update("/t2", &entry("TopicDescription", "<DefaultMessageTimeToLive>PT2M</DefaultMessageTimeToLive>")).await;
    assert_eq!(u.status, 200, "{}", u.text());
    assert_eq!(tag(&a.get("/t2").await.text(), "DefaultMessageTimeToLive").unwrap(), "PT2M");
    assert_eq!(a.update("/nope", &entry("TopicDescription", "")).await.status, 404);
    // a subscription description is not an entity description
    assert_eq!(a.create("/t3", &entry("SubscriptionDescription", "")).await.status, 400);
}

#[tokio::test]
async fn deleting_a_topic_deletes_its_subscriptions() {
    let a = admin().await;
    a.topic("t", "").await;
    a.sub("t", "s1", "").await;
    assert_eq!(a.delete("/t").await.status, 200);
    assert_eq!(a.get("/t").await.header("content-type"), Some(ATOM_FEED));
    a.topic("t", "").await;
    assert!(!a.get("/t/subscriptions").await.text().contains("<entry"));
}

#[tokio::test]
async fn lists_page_with_skip_and_top() {
    let a = admin().await;
    let empty = a.get("/$Resources/queues?$skip=0&$top=100&api-version=2021-05").await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.header("content-type"), Some(ATOM_FEED));
    let b = empty.text();
    assert!(b.starts_with("<feed xmlns=\"http://www.w3.org/2005/Atom\"><title type=\"text\">Queues</title><id>"), "{b}");
    assert!(b.contains("<link rel=\"self\" href=") && !b.contains("rel=\"next\"") && !b.contains("<entry"));
    for i in 0..105 {
        assert_eq!(a.queue(&format!("pg-{i:03}"), "").await.status, 201);
    }
    let p1 = a.get("/$Resources/queues?$skip=0&$top=100&api-version=2021-05").await.text();
    assert_eq!(p1.matches("<entry ").count(), 100);
    assert!(p1.contains("<link rel=\"next\" href=\"") && p1.contains("$skip=100&amp;$top=100"), "next link missing");
    assert_eq!(titles(&p1)[1], "pg-000");
    let p2 = a.get("/$Resources/queues?$skip=100&$top=100&api-version=2021-05").await.text();
    assert_eq!(p2.matches("<entry ").count(), 5);
    assert!(!p2.contains("rel=\"next\""));
    let small = a.get("/$Resources/queues?$skip=3&$top=2").await.text();
    assert_eq!(titles(&small)[1..].to_vec(), vec!["pg-003", "pg-004"]);
    assert_eq!(a.get("/$Resources/queues?$top=0").await.text().matches("<entry ").count(), 0);
    assert_eq!(a.get("/$Resources/queues?$skip=-1").await.status, 400);
    assert_eq!(a.get("/$Resources/queues?$top=abc").await.status, 400);
    // feed entries are the full description with xml:base and relative self links
    assert!(
        p1.contains("<entry xml:base=\"") && p1.contains("<link rel=\"self\" href=\"../pg-000?api-version=2021-05\"/>"),
        "{}",
        &p1[..1500]
    );
    // topics are listed separately
    assert!(!a.get("/$Resources/topics").await.text().contains("<entry"));
}

#[tokio::test]
async fn requests_without_authorization_are_401() {
    let a = admin().await;
    a.queue("q", "").await;
    for (m, p) in [("GET", "/q"), ("GET", "/$Resources/queues"), ("PUT", "/q2"), ("DELETE", "/q"), ("GET", "/$namespaceinfo")] {
        let r = a.srv.send(m, &format!("{}{p}", a.base), &[], entry("QueueDescription", "")).await;
        assert_eq!(r.status, 401, "{m} {p}");
        assert!(r.text().starts_with("<Error><Code>401</Code><Detail>"), "{}", r.text());
    }
    assert!(a.get("/q").await.status == 200, "the queue is untouched");
    // any value is accepted
    for v in ["Bearer abc", "SharedAccessSignature sr=x", "anything"] {
        let r = a.srv.send("GET", &format!("{}/q", a.base), &[("Authorization", v)], "").await;
        assert_eq!(r.status, 200, "{v}");
    }
}

#[tokio::test]
async fn an_empty_name_is_refused_as_recorded() {
    let a = admin().await;
    let r = a.create("/?api-version=2021-05", &entry("QueueDescription", "")).await;
    assert_eq!(r.status, 401);
    assert!(
        r.text().contains("Generic: Provided resource identifier '/subscriptions/") && r.text().contains("/queues/' is invalid."),
        "{}",
        r.text()
    );
    let r = a.create("/t/subscriptions/?api-version=2021-05", &entry("SubscriptionDescription", "")).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The specified HTTP verb (PUT) is not valid."), "{}", r.text());
}

#[tokio::test]
async fn bad_bodies_are_400() {
    let a = admin().await;
    for body in ["", "not xml", "<a><b></a>", "<entry/>", "<entry><content><Bogus/></content></entry>", "<!DOCTYPE x><entry/>"] {
        let r = a.create("/q", body).await;
        assert_eq!(r.status, 400, "{body}");
        assert!(r.text().starts_with("<Error><Code>400</Code>"));
    }
}

#[tokio::test]
async fn unbuilt_operations_are_501_and_name_themselves() {
    let a = admin().await;
    a.queue("q", "").await;
    for (m, p, what) in [
        ("GET", "/q/messages", "GET /q/messages"),
        ("POST", "/q/messages", "POST /q/messages"),
        ("GET", "/$Resources/eventhubs", "GET /$Resources/eventhubs"),
        ("GET", "/$Resources/Queues/extra/more", "/$Resources/Queues/extra/more"),
    ] {
        let r = a.req(m, p, "", &[]).await;
        assert_eq!(r.status, 501, "{m} {p}");
        assert!(r.text().contains(what), "{}", r.text());
        assert!(r.text().starts_with("<Error><Code>501</Code>"));
    }
}

#[tokio::test]
async fn namespace_info_and_service_document() {
    let a = admin().await;
    let r = a.get("/$namespaceinfo?api-version=2021-05").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some(ATOM_ENTRY));
    let b = r.text();
    assert_eq!(tag(&b, "MessagingSKU").unwrap(), "Standard");
    assert_eq!(tag(&b, "NamespaceType").unwrap(), "Messaging");
    assert_eq!(tag(&b, "Name").unwrap(), "default");
    assert_order(&b, &["CreatedTime", "MessagingSKU", "ModifiedTime", "Name", "NamespaceType"]);
    assert!(b.contains("<NamespaceInfo xmlns=\"http://schemas.microsoft.com/netservices/2010/10/servicebus/connect\" xmlns:i="));
    let r = a.get("/").await;
    assert!(r.text().contains("Publicly Listed Services"));
}

#[tokio::test]
async fn namespaces_are_selected_by_host() {
    let a = admin().await;
    a.queue("only-default", "").await;
    let other = a.srv.url("servicebus", "other");
    let r = a.req_at(&other, "GET", "/$Resources/queues", "", &[]).await;
    assert!(!r.text().contains("only-default"));
    let r = a.req_at(&other, "PUT", "/only-default", &entry("QueueDescription", ""), &[]).await;
    assert_eq!(r.status, 201, "same name, other namespace");
    assert!(a.req_at(&other, "GET", "/$namespaceinfo", "", &[]).await.text().contains("<Name>other</Name>"));
    assert_eq!(a.get("/only-default").await.status, 200);
}

#[tokio::test]
async fn wrong_verbs_are_400() {
    let a = admin().await;
    let r = a.req("POST", "/q", "", &[]).await;
    assert_eq!(r.status, 400);
    assert!(r.text().contains("The specified HTTP verb (POST) is not valid."));
    assert_eq!(a.req("PUT", "/$Resources/queues", "", &[]).await.status, 400);
}
