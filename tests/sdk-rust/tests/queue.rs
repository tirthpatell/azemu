//! Queue Storage through `azure_storage_queue`. HTTPS with a bearer token.

use std::collections::HashMap;
use std::time::Duration;

use azemu_sdk_rust_tests::{cred, emu, uniq};
use azure_core::http::{RequestContent, StatusCode, Url};
use azure_core::Error;
use azure_storage_queue::models::*;
use azure_storage_queue::{QueueClient, QueueServiceClient};
use futures::TryStreamExt;

fn service() -> QueueServiceClient {
    let url = Url::parse(&emu().https("queue", &uniq("acct"))).unwrap();
    QueueServiceClient::new(url, Some(cred()), None).unwrap()
}

async fn queue() -> (QueueServiceClient, QueueClient) {
    let s = service();
    let q = s.queue_client(&uniq("q")).unwrap();
    q.create(None).await.unwrap();
    (s, q)
}

fn status(e: &Error) -> Option<StatusCode> {
    e.http_status()
}
fn code(e: &Error) -> Option<String> {
    match e.kind() {
        azure_core::error::ErrorKind::HttpResponse { error_code, .. } => error_code.clone(),
        _ => None,
    }
}

async fn send(q: &QueueClient, text: &str) -> SentMessage {
    let m = QueueMessage { message_text: Some(text.into()) };
    q.send_message(m.try_into().unwrap(), None).await.unwrap().into_model().unwrap().items.unwrap().remove(0)
}

async fn receive(q: &QueueClient, n: i32, vis: i32) -> Vec<ReceivedMessage> {
    let o = QueueClientReceiveMessagesOptions { number_of_messages: Some(n), visibility_timeout: Some(vis), ..Default::default() };
    q.receive_messages(Some(o)).await.unwrap().into_model().unwrap().items.unwrap_or_default()
}

#[tokio::test]
async fn create_queue_then_it_exists_and_delete_removes_it() {
    let s = service();
    let q = s.queue_client(&uniq("q")).unwrap();
    assert!(!q.exists().await.unwrap());
    q.create(None).await.unwrap();
    assert!(q.exists().await.unwrap());
    q.delete(None).await.unwrap();
    let e = q.get_properties(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("QueueNotFound"));
}

#[tokio::test]
async fn create_queue_again_with_same_metadata_is_accepted() {
    let (_s, q) = queue().await;
    q.create(None).await.expect("Azure answers 204 for an identical create");
}

#[tokio::test]
async fn create_queue_again_with_different_metadata_is_409_queue_already_exists() {
    let s = service();
    let q = s.queue_client(&uniq("q")).unwrap();
    q.create(Some(QueueClientCreateOptions { metadata: Some(HashMap::from([("a".into(), "1".into())])), ..Default::default() }))
        .await
        .unwrap();
    let e = q
        .create(Some(QueueClientCreateOptions { metadata: Some(HashMap::from([("a".into(), "2".into())])), ..Default::default() }))
        .await
        .unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::Conflict));
    assert_eq!(code(&e).as_deref(), Some("QueueAlreadyExists"));
}

#[tokio::test]
async fn queue_name_with_uppercase_letters_is_400_invalid_resource_name() {
    let s = service();
    let q = s.queue_client("BadName").unwrap();
    let e = q.create(None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
    assert_eq!(code(&e).as_deref(), Some("InvalidResourceName"));
}

#[tokio::test]
async fn queue_metadata_roundtrip_and_message_count() {
    let (_s, q) = queue().await;
    let md = HashMap::from([("owner".to_string(), "rust".to_string())]);
    q.set_metadata(&md, None).await.unwrap();
    send(&q, "a").await;
    send(&q, "b").await;
    let p = q.get_properties(None).await.unwrap();
    assert_eq!(p.metadata().unwrap(), md);
    assert_eq!(p.approximate_messages_count().unwrap(), Some(2));
}

#[tokio::test]
async fn list_queues_by_prefix_with_paging() {
    let s = service();
    let prefix = uniq("lq");
    for i in 0..5 {
        s.queue_client(&format!("{prefix}-{i}")).unwrap().create(None).await.unwrap();
    }
    s.queue_client(&uniq("other")).unwrap().create(None).await.unwrap();
    let o = QueueServiceClientListQueuesOptions { prefix: Some(prefix.clone()), maxresults: Some(2), ..Default::default() };
    let mut pages = s.list_queues(Some(o)).unwrap().into_pages();
    let mut names = vec![];
    let mut sizes = vec![];
    while let Some(p) = pages.try_next().await.unwrap() {
        let m = p.into_model().unwrap();
        sizes.push(m.queue_items.len());
        names.extend(m.queue_items.into_iter().map(|q| q.name.unwrap()));
    }
    assert_eq!(sizes, vec![2, 2, 1]);
    names.sort();
    assert_eq!(names, (0..5).map(|i| format!("{prefix}-{i}")).collect::<Vec<_>>());
}

#[tokio::test]
async fn send_returns_id_pop_receipt_and_times() {
    let (_s, q) = queue().await;
    let m = send(&q, "hello").await;
    assert!(m.message_id.is_some());
    assert!(m.pop_receipt.is_some());
    let ins = m.insertion_time.unwrap();
    let exp = m.expiration_time.unwrap();
    assert_eq!((exp - ins).whole_days(), 7, "default time to live is 7 days");
}

#[tokio::test]
async fn message_text_with_xml_characters_and_unicode_roundtrips() {
    let (_s, q) = queue().await;
    let text = "<a href=\"x\">&amp; 'quote' é 日本 \u{1F600}</a>";
    send(&q, text).await;
    let got = receive(&q, 1, 30).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].message_text.as_deref(), Some(text));
    assert_eq!(got[0].dequeue_count, Some(1));
}

#[tokio::test]
async fn receive_hides_the_message_until_the_visibility_timeout_passes() {
    let (_s, q) = queue().await;
    send(&q, "once").await;
    let first = receive(&q, 1, 30).await;
    assert_eq!(first.len(), 1);
    assert!(receive(&q, 1, 30).await.is_empty(), "a received message is invisible");
    // Peek does not see it either.
    let peeked = q.peek_messages(None).await.unwrap().into_model().unwrap().items.unwrap_or_default();
    assert!(peeked.is_empty());
}

#[tokio::test]
async fn message_comes_back_after_visibility_timeout_with_dequeue_count_two() {
    let (_s, q) = queue().await;
    send(&q, "again").await;
    let first = receive(&q, 1, 1).await;
    assert_eq!(first[0].dequeue_count, Some(1));
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let second = receive(&q, 1, 30).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].dequeue_count, Some(2));
    assert_eq!(second[0].message_id, first[0].message_id);
    assert_ne!(second[0].pop_receipt, first[0].pop_receipt);
}

#[tokio::test]
async fn messages_are_received_in_the_order_they_were_sent() {
    let (_s, q) = queue().await;
    for i in 0..5 {
        send(&q, &format!("m{i}")).await;
    }
    let got = receive(&q, 5, 30).await;
    let texts: Vec<_> = got.iter().map(|m| m.message_text.clone().unwrap()).collect();
    assert_eq!(texts, vec!["m0", "m1", "m2", "m3", "m4"]);
}

#[tokio::test]
async fn peek_returns_messages_without_hiding_them_and_without_pop_receipt() {
    let (_s, q) = queue().await;
    send(&q, "p").await;
    let o = QueueClientPeekMessagesOptions { number_of_messages: Some(5), ..Default::default() };
    let a = q.peek_messages(Some(o.clone())).await.unwrap().into_model().unwrap().items.unwrap();
    let b = q.peek_messages(Some(o)).await.unwrap().into_model().unwrap().items.unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0].message_text.as_deref(), Some("p"));
    assert_eq!(a[0].dequeue_count, Some(0), "peek does not count as a dequeue");
}

#[tokio::test]
async fn delete_message_with_its_pop_receipt_removes_it() {
    let (_s, q) = queue().await;
    send(&q, "d").await;
    let m = receive(&q, 1, 1).await.remove(0);
    q.delete_message(m.message_id.as_deref().unwrap(), m.pop_receipt.as_deref().unwrap(), None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(receive(&q, 1, 30).await.is_empty());
    assert_eq!(q.get_properties(None).await.unwrap().approximate_messages_count().unwrap(), Some(0));
}

#[tokio::test]
async fn delete_message_with_stale_pop_receipt_is_400_pop_receipt_mismatch() {
    let (_s, q) = queue().await;
    send(&q, "d").await;
    let first = receive(&q, 1, 1).await.remove(0);
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let _second = receive(&q, 1, 30).await.remove(0);
    let e = q.delete_message(first.message_id.as_deref().unwrap(), first.pop_receipt.as_deref().unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
    assert_eq!(code(&e).as_deref(), Some("PopReceiptMismatch"));
}

#[tokio::test]
async fn delete_message_that_does_not_exist_is_404_message_not_found() {
    let (_s, q) = queue().await;
    let m = send(&q, "x").await;
    let got = receive(&q, 1, 30).await.remove(0);
    q.delete_message(got.message_id.as_deref().unwrap(), got.pop_receipt.as_deref().unwrap(), None).await.unwrap();
    let e = q.delete_message(m.message_id.as_deref().unwrap(), got.pop_receipt.as_deref().unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("MessageNotFound"));
}

#[tokio::test]
async fn send_with_visibility_timeout_hides_the_message_at_first() {
    let (_s, q) = queue().await;
    let o = QueueClientSendMessageOptions { visibility_timeout: Some(2), ..Default::default() };
    q.send_message(QueueMessage { message_text: Some("later".into()) }.try_into().unwrap(), Some(o)).await.unwrap();
    assert!(receive(&q, 1, 30).await.is_empty());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(receive(&q, 1, 30).await.len(), 1);
}

#[tokio::test]
async fn send_with_time_to_live_sets_the_expiration_time() {
    let (_s, q) = queue().await;
    let o = QueueClientSendMessageOptions { message_time_to_live: Some(120), ..Default::default() };
    let m = q
        .send_message(QueueMessage { message_text: Some("ttl".into()) }.try_into().unwrap(), Some(o))
        .await
        .unwrap()
        .into_model()
        .unwrap()
        .items
        .unwrap()
        .remove(0);
    assert_eq!((m.expiration_time.unwrap() - m.insertion_time.unwrap()).whole_seconds(), 120);
}

#[tokio::test]
async fn update_message_changes_text_and_visibility_and_returns_a_new_pop_receipt() {
    let (_s, q) = queue().await;
    send(&q, "old").await;
    let m = receive(&q, 1, 30).await.remove(0);
    let o = QueueClientUpdateMessageOptions {
        queue_message: Some(QueueMessage { message_text: Some("new".into()) }.try_into().unwrap()),
        ..Default::default()
    };
    let r = q.update_message(m.message_id.as_deref().unwrap(), m.pop_receipt.as_deref().unwrap(), 1, Some(o)).await.unwrap();
    let new_receipt =
        r.headers().get_optional_str(&azure_core::http::headers::HeaderName::from_static("x-ms-popreceipt")).map(String::from);
    assert!(new_receipt.is_some());
    assert_ne!(new_receipt.as_deref(), m.pop_receipt.as_deref());
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let again = receive(&q, 1, 30).await;
    assert_eq!(again[0].message_text.as_deref(), Some("new"));
}

#[tokio::test]
async fn receive_of_more_than_32_messages_is_400_out_of_range_query_parameter_value() {
    let (_s, q) = queue().await;
    let o = QueueClientReceiveMessagesOptions { number_of_messages: Some(33), ..Default::default() };
    let e = q.receive_messages(Some(o)).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::BadRequest));
    assert_eq!(code(&e).as_deref(), Some("OutOfRangeQueryParameterValue"));
}

#[tokio::test]
async fn clear_removes_all_messages() {
    let (_s, q) = queue().await;
    for i in 0..4 {
        send(&q, &format!("{i}")).await;
    }
    q.clear(None).await.unwrap();
    assert!(receive(&q, 32, 30).await.is_empty());
}

#[tokio::test]
async fn message_over_64_kib_is_rejected() {
    // Azure: the text may be up to 64 KiB (65536 bytes) once encoded; larger is 400.
    let (_s, q) = queue().await;
    let big = "x".repeat(70 * 1024);
    let e = q.send_message(QueueMessage { message_text: Some(big) }.try_into().unwrap(), None).await.unwrap_err();
    assert!(matches!(status(&e), Some(StatusCode::BadRequest) | Some(StatusCode::PayloadTooLarge)), "{e}");
}

#[tokio::test]
async fn message_of_exactly_64_kib_is_accepted() {
    let (_s, q) = queue().await;
    let text = "y".repeat(64 * 1024);
    send(&q, &text).await;
    assert_eq!(receive(&q, 1, 30).await[0].message_text.as_deref(), Some(text.as_str()));
}

#[tokio::test]
async fn send_to_a_missing_queue_is_404_queue_not_found() {
    let s = service();
    let q = s.queue_client(&uniq("nosuch")).unwrap();
    let e = q.send_message(QueueMessage { message_text: Some("x".into()) }.try_into().unwrap(), None).await.unwrap_err();
    assert_eq!(status(&e), Some(StatusCode::NotFound));
    assert_eq!(code(&e).as_deref(), Some("QueueNotFound"));
}

#[tokio::test]
async fn stored_access_policy_roundtrip() {
    let (_s, q) = queue().await;
    let start = azure_core::time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let expiry = azure_core::time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    let ids = SignedIdentifiers {
        items: Some(vec![SignedIdentifier {
            id: Some("p1".into()),
            access_policy: Some(AccessPolicy { start: Some(start), expiry: Some(expiry), permission: Some("rp".into()) }),
        }]),
    };
    q.set_access_policy(RequestContent::try_from(ids).unwrap(), None).await.unwrap();
    let got = q.get_access_policy(None).await.unwrap().into_model().unwrap().items.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id.as_deref(), Some("p1"));
    let ap = got[0].access_policy.as_ref().unwrap();
    assert_eq!(ap.permission.as_deref(), Some("rp"));
    assert_eq!(ap.expiry, Some(expiry));
}

#[tokio::test]
async fn service_properties_get_returns_logging_and_metrics() {
    let s = service();
    let p = s.get_properties(None).await.unwrap().into_model().unwrap();
    assert!(p.logging.is_some() || p.hour_metrics.is_some());
}
