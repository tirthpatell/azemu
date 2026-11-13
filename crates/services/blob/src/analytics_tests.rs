//! Unit tests of `analytics.rs`.

use super::*;

#[test]
fn sig_is_masked_and_the_rest_kept() {
    assert_eq!(mask_query("sv=2021&sig=abc%2Bdef&sp=r"), "sv=2021&sig=XXXXX&sp=r");
    assert_eq!(mask_query("SIG=zzz"), "SIG=XXXXX");
    assert_eq!(mask_url("https://a/b?sig=1&c=2"), "https://a/b?sig=XXXXX&c=2");
    assert_eq!(mask_url("https://a/b"), "https://a/b");
}

#[test]
fn html_encoding() {
    assert_eq!(html_encode("a&b\"c;d\ne"), "a&amp;b&quot;c;d&#10;e");
    assert_eq!(quoted(""), "");
    assert_eq!(quoted("x\"y"), "\"x&quot;y\"");
}

#[test]
fn last_modified_form() {
    let t = time::parse_rfc1123("Mon, 03 Jan 2022 20:34:54 GMT").unwrap();
    assert_eq!(lmt(t), "Monday, 03-Jan-22 20:34:54 GMT");
}

#[test]
fn hour_names() {
    let t = time::parse_rfc1123("Mon, 03 Jan 2022 20:34:54 GMT").unwrap();
    assert_eq!(hour_dir(t), "blob/2022/01/03/2000");
}

#[test]
fn classes() {
    assert_eq!(class_of(Op::GetBlob, "GET"), Class::Read);
    assert_eq!(class_of(Op::PutBlob, "PUT"), Class::Write);
    assert_eq!(class_of(Op::DeleteBlob, "DELETE"), Class::Delete);
    assert_eq!(class_of(Op::QueryBlob, "POST"), Class::Read);
    assert_eq!(class_of(Op::SetBlobTags, "PUT"), Class::Write);
}

#[test]
fn who_is_logged() {
    // authorized: everything; anonymous and SAS: successes, server errors, 304 on GET
    assert!(is_logged(Auth::Authenticated, 404, "GET"));
    assert!(is_logged(Auth::Sas, 201, "PUT"));
    assert!(!is_logged(Auth::Sas, 403, "GET"));
    assert!(!is_logged(Auth::Anonymous, 404, "GET"));
    assert!(is_logged(Auth::Anonymous, 304, "GET"));
    assert!(!is_logged(Auth::Anonymous, 304, "PUT"));
    assert!(is_logged(Auth::Anonymous, 500, "GET"));
    assert_eq!(status_text(Auth::Sas, 201, None), "SASSuccess");
    assert_eq!(status_text(Auth::Authenticated, 404, Some("BlobNotFound")), "BlobNotFound");
    assert_eq!(status_text(Auth::Anonymous, 304, None), "AnonymousClientOtherError");
}

#[test]
fn a_batch_is_split_by_account_hour_and_size() {
    let t = time::parse_rfc1123("Mon, 03 Jan 2022 20:34:54 GMT").unwrap();
    let mk = |a: &str, at: Time, n: usize| Pending { account: a.into(), end: at, start: at, class: Class::Write, line: "x".repeat(n) };
    let items = vec![mk("a", t, 10), mk("a", t + time::Duration::hours(1), 10), mk("b", t, 10), mk("a", t, MAX_LOG_BLOB_BYTES - 5)];
    let p = plan(items);
    // a/20h: two blobs (the big line does not fit beside the small one), a/21h, b/20h
    assert_eq!(p.len(), 4);
    assert!(p.iter().all(|x| x.text.len() <= MAX_LOG_BLOB_BYTES));
}

#[test]
fn prune_by_days_and_by_limits() {
    let now = time::parse_rfc1123("Mon, 10 Jan 2022 00:00:00 GMT").unwrap();
    let mut acct = Account::default();
    acct.props.logging.write = true;
    acct.props.logging.retention.enabled = true;
    acct.props.logging.retention.days = Some(3);
    let mk = |acct: &mut Account, name: &str, at: Time| {
        store_log(acct, name, "line\n".into(), &[Class::Write], at, at, "1.0", at);
    };
    mk(&mut acct, "blob/2022/01/01/0000/000000.log", now - time::Duration::days(9));
    mk(&mut acct, "blob/2022/01/08/0000/000000.log", now - time::Duration::days(2));
    mk(&mut acct, "blob/2022/01/09/0000/000000.log", now - time::Duration::days(1));
    // a blob of the user's under the same container is never touched
    let c = acct.containers.get_mut(LOGS).unwrap();
    c.blobs.insert(
        "mine.txt".into(),
        BlobEntry { current: Some(Blob::new(BlobKind::Block, now - time::Duration::days(30))), ..BlobEntry::default() },
    );
    let cfg = Config::default();
    assert_eq!(prune(&mut acct, now, &cfg), 1);
    assert!(!acct.containers[LOGS].blobs.contains_key("blob/2022/01/01/0000/000000.log"));
    assert!(acct.containers[LOGS].blobs.contains_key("mine.txt"));
    // hard limit on the number of blobs: the oldest go first, the newest stays
    let small = Config { log_max_blobs: 1, ..Config::default() };
    assert_eq!(prune(&mut acct, now, &small), 1);
    assert!(acct.containers[LOGS].blobs.contains_key("blob/2022/01/09/0000/000000.log"));
    let tiny = Config { log_max_bytes: 1, ..Config::default() };
    assert_eq!(prune(&mut acct, now, &tiny), 0, "the newest blob is kept even when it alone is over the limit");
}

#[test]
fn counters_continue_after_the_highest_name() {
    let now = time::parse_rfc1123("Mon, 10 Jan 2022 00:00:00 GMT").unwrap();
    let mut acct = Account::default();
    for n in [0, 1, 4] {
        store_log(&mut acct, &format!("blob/2022/01/10/0000/{n:06}.log"), "x\n".into(), &[Class::Read], now, now, "1.0", now);
    }
    let c = &acct.containers[LOGS];
    assert_eq!(next_counter(c, "blob/2022/01/10/0000"), 5);
    assert_eq!(next_counter(c, "blob/2022/01/10/0100"), 0);
}

#[test]
fn the_queue_is_bounded_in_lines_and_in_bytes_and_counts_what_it_drops() {
    let t = time::parse_rfc1123("Mon, 03 Jan 2022 20:34:54 GMT").unwrap();
    let mk = |n: usize| Pending { account: "a".into(), end: t, start: t, class: Class::Read, line: "x".repeat(n) };
    let l = Logger::default();
    l.push((0..MAX_QUEUED_LINES + 10).map(|_| mk(1)).collect());
    assert_eq!(l.queued(), MAX_QUEUED_LINES);
    assert_eq!(l.dropped.load(Ordering::Relaxed), 10);
    // taking the queue empties it and frees its bytes
    assert_eq!(l.take().len(), MAX_QUEUED_LINES);
    assert_eq!(l.queued(), 0);
    // a few big lines fill the byte limit before the line limit
    l.push((0..20).map(|_| mk(1024 * 1024)).collect());
    assert!(l.queued() <= MAX_QUEUED_BYTES / (1024 * 1024), "{}", l.queued());
    assert!(l.dropped.load(Ordering::Relaxed) > 10);
}

#[test]
fn a_header_that_is_too_long_is_cut_and_a_value_with_a_semicolon_stays_in_one_field() {
    let long = "é".repeat(MAX_FIELD);
    let q = quoted(&long);
    assert!(q.len() <= MAX_FIELD + 2, "cut at the limit, on a character boundary");
    assert_eq!(quoted("a;b").matches(';').count(), 1);
    assert!(quoted("a;b").starts_with('"') && quoted("a;b").ends_with('"'));
}
