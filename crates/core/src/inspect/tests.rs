use std::collections::BTreeMap;

use super::*;

#[test]
fn a_segment_escapes_what_would_break_a_path() {
    assert_eq!(seg("plain-name_1.txt"), "plain-name_1.txt");
    assert_eq!(seg("a/b c"), "a%2Fb%20c");
    assert_eq!(seg("100%?#"), "100%25%3F%23");
    let req = InspectRequest::for_test("blob", &[], "");
    assert_eq!(req.path(&["acct", "containers", "a/b"]), "/blob/acct/containers/a%2Fb");
}

#[test]
fn limit_defaults_is_capped_and_refuses_nonsense() {
    assert_eq!(InspectRequest::for_test("x", &[], "").limit(), DEFAULT_LIMIT);
    assert_eq!(InspectRequest::for_test("x", &[], "limit=7").limit(), 7);
    assert_eq!(InspectRequest::for_test("x", &[], "limit=999999").limit(), MAX_LIMIT);
    for bad in ["limit=0", "limit=-1", "limit=abc"] {
        let e = InspectRequest::new("x", vec![], Query::parse(bad)).unwrap_err();
        assert!(matches!(e, InspectError::BadRequest(_)), "{bad}");
    }
    let e = InspectRequest::new("x", vec![], Query::parse("marker=***")).unwrap_err();
    assert!(matches!(e, InspectError::BadRequest(_)));
}

#[test]
fn paging_a_map_walks_every_key_once_and_ends() {
    let map: BTreeMap<String, u32> = (0..23).map(|i| (format!("k{i:02}"), i)).collect();
    let mut seen = Vec::new();
    let mut marker: Option<String> = None;
    let mut pages = 0;
    loop {
        let q = match &marker {
            Some(m) => format!("limit=5&marker={m}"),
            None => "limit=5".to_string(),
        };
        let req = InspectRequest::for_test("x", &[], &q);
        let (rows, next) = req.page_map(&map);
        assert!(rows.len() <= 5);
        seen.extend(rows.iter().map(|(k, _)| k.to_string()));
        pages += 1;
        assert!(pages < 20, "paging does not end");
        match next {
            Some(n) => marker = Some(n),
            None => break,
        }
    }
    let all: Vec<String> = map.keys().cloned().collect();
    assert_eq!(seen, all);
    assert_eq!(pages, 5);
}

#[test]
fn an_exact_multiple_of_the_page_size_has_no_empty_last_page() {
    let map: BTreeMap<String, u32> = (0..10).map(|i| (format!("k{i}"), i)).collect();
    let req = InspectRequest::for_test("x", &[], "limit=5");
    let (_, next) = req.page_map(&map);
    let req = InspectRequest::for_test("x", &[], &format!("limit=5&marker={}", next.unwrap()));
    let (rows, next) = req.page_map(&map);
    assert_eq!(rows.len(), 5);
    assert!(next.is_none());
}

#[test]
fn paging_by_position() {
    let req = InspectRequest::for_test("x", &[], "limit=4");
    let (range, next) = req.page_range(10);
    assert_eq!(range, 0..4);
    let req = InspectRequest::for_test("x", &[], &format!("limit=4&marker={}", next.unwrap()));
    let (range, next) = req.page_range(10);
    assert_eq!(range, 4..8);
    let req = InspectRequest::for_test("x", &[], &format!("limit=4&marker={}", next.unwrap()));
    let (range, next) = req.page_range(10);
    assert_eq!(range, 8..10);
    assert!(next.is_none());
    // a marker past the end (the list shrank) is an empty page, not a panic
    let req = InspectRequest::for_test("x", &[], &format!("limit=4&marker={}", marker_for("99")));
    assert_eq!(req.page_range(10).0, 10..10);
}

#[test]
fn a_body_is_text_when_it_is_text_and_base64_when_it_is_not() {
    let v = body_view(b"hello\nworld", Some("text/plain"));
    assert_eq!(v["text"], "hello\nworld");
    assert_eq!(v["size"], 11);
    assert_eq!(v["truncated"], false);
    assert_eq!(v["contentType"], "text/plain");
    let v = body_view(&[0u8, 1, 2, 255], None);
    assert!(v.get("text").is_none());
    assert_eq!(v["base64"], "AAEC/w==");
    // a cut in the middle of a character stays text
    let s = "\u{e9}".repeat(10);
    let v = body_view_limited(s.as_bytes(), None, 5);
    assert_eq!(v["text"], "\u{e9}\u{e9}");
    assert_eq!(v["truncated"], true);
    assert_eq!(v["size"], 20);
    assert_eq!(v["shown"], 4);
}

#[test]
fn a_queue_text_that_is_base64_of_text_also_shows_the_decoded_text() {
    let v = text_with_decoded_base64("aGVsbG8gd29ybGQ=");
    assert_eq!(v["text"], "aGVsbG8gd29ybGQ=");
    assert_eq!(v["decoded"], "hello world");
    assert!(text_with_decoded_base64("hello world").get("decoded").is_none());
    // base64 of bytes that are not UTF-8 text is not decoded
    assert!(text_with_decoded_base64("AAEC/w==").get("decoded").is_none());
}

#[test]
fn times_are_rfc3339_in_utc_with_milliseconds() {
    let t = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    assert_eq!(rfc3339(t), "2023-11-14T22:13:20.000Z");
    assert_eq!(unix_time_value(1_700_000_000), json!("2023-11-14T22:13:20.000Z"));
}

// ---- a row and a page have a size ----

#[test]
fn a_row_with_a_value_too_big_shows_that_it_was_cut_and_how_long_it_was() {
    let small = json!({"name": "a", "body": "x".repeat(1000)});
    assert_eq!(cap_row(small.clone()), small);
    // the largest value goes first, and the small ones stay as they are
    let row = json!({"name": "a", "small": "y".repeat(100), "huge": "z".repeat(3_000_000), "big": "w".repeat(ROW_LIMIT)});
    let capped = cap_row(row);
    assert_eq!(capped["name"], "a");
    assert_eq!(capped["small"], "y".repeat(100));
    assert_eq!(capped["huge"]["cut"], true);
    assert_eq!(capped["huge"]["bytes"], 3_000_002);
    assert!(capped["huge"]["preview"].as_str().unwrap().len() <= 256);
    assert!(json_size(&capped, usize::MAX / 4) <= ROW_LIMIT + 1024, "{}", json_size(&capped, usize::MAX / 4));
    // a row that is not an object is cut whole; text with multi-byte characters is cut on a character
    assert_eq!(cap_row(Value::String("é".repeat(ROW_LIMIT)))["cut"], true);
    // a body view of 64 KiB with a few properties around it fits as it is
    let body = json!({"size": BODY_LIMIT, "shown": BODY_LIMIT, "truncated": false, "text": "b".repeat(BODY_LIMIT)});
    assert_eq!(cap_row(json!({"sequence": 1, "body": body.clone()}))["body"], body);
}

#[test]
fn a_page_of_long_rows_ends_early_and_the_next_marker_goes_on_from_there() {
    // by position
    let rows: Vec<Value> = (0..100).map(|i| json!({"name": i, "data": "d".repeat(100 * 1024)})).collect();
    let req = InspectRequest::for_test("x", &[], "limit=100");
    let (range, next) = req.page_range(rows.len());
    assert!(next.is_none());
    let node = Node::new("/x", "list", "x").with_items(rows[range].to_vec(), next);
    let node = req.limit_answer(node);
    let n = node.items.len();
    assert!((1..100).contains(&n), "{n}");
    assert!(node.items.iter().map(|r| json_size(r, usize::MAX / 4)).sum::<usize>() <= PAGE_LIMIT);
    let next = node.next.expect("a next marker");
    let again = InspectRequest::for_test("x", &[], &format!("limit=100&marker={next}"));
    assert_eq!(again.start_index(), n, "the next page starts at the first row left out");
    // by key
    let map: BTreeMap<String, Value> = (0..100).map(|i| (format!("k{i:03}"), json!({"name": i, "data": "d".repeat(100 * 1024)}))).collect();
    let req = InspectRequest::for_test("x", &[], "limit=100");
    let (page, next) = req.page_map(&map);
    assert!(next.is_none());
    let node = Node::new("/x", "list", "x").with_items(page.into_iter().map(|(_, v)| v.clone()).collect(), next);
    let node = req.limit_answer(node);
    let n = node.items.len();
    assert!((1..100).contains(&n), "{n}");
    let next = node.next.expect("a next marker");
    let again = InspectRequest::for_test("x", &[], &format!("limit=100&marker={next}"));
    let (rest, _) = again.page_map(&map);
    assert_eq!(rest[0].0, &format!("k{n:03}"), "the next page starts at the first row left out");
    // rows that fit are left alone, and so is a page that was not paged by one of the two ways
    let small: Vec<Value> = (0..10).map(|i| json!({"name": i})).collect();
    let req = InspectRequest::for_test("x", &[], "");
    let (range, next) = req.page_range(small.len());
    let node = req.limit_answer(Node::new("/x", "list", "x").with_items(small[range].to_vec(), next));
    assert_eq!((node.items.len(), node.next.clone()), (10, None));
}
