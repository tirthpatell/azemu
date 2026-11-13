use azemu_kafka::api::{self, SERVED};
use azemu_kafka::header::{read_response, write_request, write_response};
use azemu_kafka::schema::{self, request_schema, response_schema};
use azemu_kafka::wire::{Reader, Writer};
use azemu_kafka::{read_request, Error, RequestHeader, Struct, Value};

fn hdr(key: i16, version: i16) -> RequestHeader {
    RequestHeader { api_key: key, version, correlation_id: 77, client_id: Some("test-client".into()) }
}

#[test]
fn every_served_api_has_schemas_for_every_served_version() {
    for spec in SERVED {
        let req = request_schema(spec.key).unwrap_or_else(|| panic!("no request schema for {}", spec.name));
        let resp = response_schema(spec.key).unwrap_or_else(|| panic!("no response schema for {}", spec.name));
        assert!(req.min <= spec.min && req.max >= spec.max, "{} request versions", spec.name);
        assert!(resp.min <= spec.min && resp.max >= spec.max, "{} response versions", spec.name);
    }
}

#[test]
fn empty_bodies_round_trip_for_every_served_version() {
    // an empty struct encodes with defaults and decodes again: layout of every version is self-consistent
    for spec in SERVED {
        for v in spec.min..=spec.max {
            let frame = write_request(&hdr(spec.key, v), &Struct::new());
            let req = read_request(&frame[4..]).unwrap_or_else(|e| panic!("{} v{v}: {e}", spec.name));
            assert_eq!(req.header.api_key, spec.key);
            assert_eq!(req.header.version, v);
            assert_eq!(req.header.correlation_id, 77);
            assert_eq!(req.header.client_id.as_deref(), Some("test-client"));
            let h = hdr(spec.key, v);
            let resp = write_response(&h, &Struct::new());
            let (corr, _) = read_response(&h, &resp[4..]).unwrap_or_else(|e| panic!("{} v{v} response: {e}", spec.name));
            assert_eq!(corr, 77);
        }
    }
}

/// ApiVersions v0 response, from the protocol guide: error 0, one api key (18, 0..3), nothing else in v0.
#[test]
fn api_versions_v0_response_bytes() {
    let body = Struct::new()
        .with("ErrorCode", 0)
        .with("ApiKeys", vec![Struct::new().with("ApiKey", 18).with("MinVersion", 0).with("MaxVersion", 3)]);
    let h = hdr(api::API_VERSIONS, 0);
    let frame = write_response(&h, &body);
    let expect: Vec<u8> = vec![
        0, 0, 0, 16, // length
        0, 0, 0, 77, // correlation id
        0, 0, // error
        0, 0, 0, 1, // one api key
        0, 18, 0, 0, 0, 3,
    ];
    assert_eq!(frame, expect);
}

/// ApiVersions v3 is flexible: the request header has tagged fields, the response header does not, and the body
/// uses compact strings and ends with a tag buffer.
#[test]
fn api_versions_v3_flexible_layout() {
    let mut w = Writer::new();
    w.i32(0);
    w.i16(18);
    w.i16(3);
    w.i32(5);
    w.string(Some("rdkafka")); // client id: plain string even in header v2
    w.uvarint(0); // header tagged fields
    w.compact_string(Some("librdkafka"));
    w.compact_string(Some("2.3.0"));
    w.uvarint(0); // body tagged fields
    let frame = w.into_vec();
    let req = read_request(&frame[4..]).unwrap();
    assert_eq!(req.header.client_id.as_deref(), Some("rdkafka"));
    assert_eq!(req.body.str("ClientSoftwareName"), Some("librdkafka"));
    assert_eq!(req.body.str("ClientSoftwareVersion"), Some("2.3.0"));

    let body = Struct::new().with("ErrorCode", 0).with("ApiKeys", Vec::<Struct>::new()).with("ThrottleTimeMs", 0);
    let resp = write_response(&req.header, &body);
    assert_eq!(
        resp,
        vec![
            0, 0, 0, 12, // length
            0, 0, 0, 5, // correlation id, short header (no tag buffer) for ApiVersions
            0, 0, // error
            1, // compact array, 0 elements
            0, 0, 0, 0, // throttle
            0, // tag buffer
        ]
    );
}

#[test]
fn unknown_tagged_fields_are_skipped() {
    let mut w = Writer::new();
    w.i16(18);
    w.i16(3);
    w.i32(1);
    w.string(None);
    w.uvarint(1); // one tagged field in the header
    w.uvarint(9);
    w.uvarint(2);
    w.raw(&[1, 2]);
    w.compact_string(Some("a"));
    w.compact_string(Some("b"));
    w.uvarint(1); // and one in the body
    w.uvarint(0);
    w.uvarint(3);
    w.raw(&[7, 7, 7]);
    let req = read_request(&w.buf).unwrap();
    assert_eq!(req.body.str("ClientSoftwareName"), Some("a"));
}

#[test]
fn metadata_request_versions() {
    // v1: topics array (null = all topics since v1); v4 adds AllowAutoTopicCreation; v9 is flexible
    for (v, expect_auto) in [(1, true), (4, false), (9, false)] {
        let body = Struct::new().with("Topics", vec![Struct::new().with("Name", "t1")]).with("AllowAutoTopicCreation", false);
        let frame = write_request(&hdr(api::METADATA, v), &body);
        let req = read_request(&frame[4..]).unwrap();
        let topics: Vec<_> = req.body.structs("Topics").collect();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].str("Name"), Some("t1"));
        // before v4 the flag is not on the wire: the default (true) applies
        assert_eq!(req.body.bool("AllowAutoTopicCreation"), expect_auto || v == 1, "v{v}");
    }
    let frame = write_request(&hdr(api::METADATA, 1), &Struct::new().with("Topics", Value::Null));
    let req = read_request(&frame[4..]).unwrap();
    assert!(req.body.get("Topics").is_null());
}

#[test]
fn produce_records_are_kept_as_bytes() {
    let body = Struct::new().with("Acks", -1).with("TimeoutMs", 1000).with(
        "TopicData",
        vec![Struct::new()
            .with("Name", "t")
            .with("PartitionData", vec![Struct::new().with("Index", 2).with("Records", vec![1u8, 2, 3, 4])])],
    );
    for v in 3..=9 {
        let frame = write_request(&hdr(api::PRODUCE, v), &body);
        let req = read_request(&frame[4..]).unwrap();
        let t = req.body.structs("TopicData").next().unwrap();
        let p = t.structs("PartitionData").next().unwrap();
        assert_eq!(p.i32("Index"), 2);
        assert_eq!(&p.bytes("Records").unwrap()[..], &[1, 2, 3, 4]);
        assert_eq!(req.body.i16("Acks"), -1);
    }
}

// ---- bounds ----------------------------------------------------------------------------------------------------

fn frame_for(key: i16, v: i16, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    w.i16(key);
    w.i16(v);
    w.i32(1);
    w.string(None);
    if request_schema(key).unwrap().flexible(v) {
        w.uvarint(0);
    }
    body(&mut w);
    w.into_vec()
}

#[test]
fn array_count_larger_than_the_bytes_is_refused() {
    // Metadata v1: topics array claims 2 billion entries and has none
    let f = frame_for(api::METADATA, 1, |w| w.i32(i32::MAX));
    assert!(matches!(read_request(&f), Err(Error::Invalid(_))));
    // flexible: compact array claiming 4 billion
    let f = frame_for(api::METADATA, 9, |w| {
        w.uvarint(u32::MAX);
    });
    assert!(matches!(read_request(&f), Err(Error::Invalid(_))));
}

#[test]
fn string_and_bytes_lengths_larger_than_the_bytes_are_refused() {
    let f = frame_for(api::FIND_COORDINATOR, 0, |w| {
        w.i16(i16::MAX);
        w.raw(b"abc");
    });
    assert_eq!(read_request(&f).unwrap_err(), Error::Truncated);
    let f = frame_for(api::SASL_AUTHENTICATE, 0, |w| {
        w.i32(i32::MAX);
    });
    assert_eq!(read_request(&f).unwrap_err(), Error::Truncated);
}

#[test]
fn many_small_elements_cannot_outgrow_the_request() {
    // an int32 array of n elements takes 4n bytes; a count that fits the bytes is fine, and the values it makes are
    // limited by the budget (one value per byte of the request)
    let n = 1000;
    let f = frame_for(api::OFFSET_FETCH, 1, |w| {
        w.string(Some("g"));
        w.i32(1);
        w.string(Some("t"));
        w.i32(n);
        for i in 0..n {
            w.i32(i);
        }
    });
    let req = read_request(&f).unwrap();
    let t = req.body.structs("Topics").next().unwrap();
    assert_eq!(t.items("PartitionIndexes").len(), n as usize);
}

#[test]
fn trailing_bytes_and_bad_versions_are_refused() {
    let mut f = frame_for(api::HEARTBEAT, 0, |w| {
        w.string(Some("g"));
        w.i32(1);
        w.string(Some("m"));
    });
    assert!(read_request(&f).is_ok());
    // bytes after the message are ignored (Kafka does, and librdkafka sends some, see below)
    f.push(0);
    assert!(read_request(&f).is_ok());
    let f = frame_for(api::HEARTBEAT, 0, |_| {});
    assert_eq!(read_request(&f).unwrap_err(), Error::Truncated);
    // a version the server does not serve, and an api key it does not know
    let mut w = Writer::new();
    w.i16(api::METADATA);
    w.i16(0);
    w.i32(1);
    w.string(None);
    w.i32(0);
    assert!(matches!(read_request(&w.buf), Err(Error::Unsupported { .. })));
    let mut w = Writer::new();
    w.i16(999);
    w.i16(0);
    w.i32(1);
    w.string(None);
    assert!(matches!(read_request(&w.buf), Err(Error::Unsupported { .. })));
    // garbage that looks like HTTP
    assert!(read_request(b"GET / HTTP/1.1\r\n\r\n").is_err());
}

#[test]
fn null_in_a_field_that_cannot_be_null_is_refused() {
    // Heartbeat GroupId is a plain string that cannot be null
    let f = frame_for(api::HEARTBEAT, 0, |w| {
        w.i16(-1);
        w.i32(1);
        w.string(Some("m"));
    });
    assert!(matches!(read_request(&f), Err(Error::Invalid(_))));
}

#[test]
fn schema_decode_checks_version_range() {
    let s = response_schema(api::PRODUCE).unwrap();
    let mut r = Reader::new(&[]);
    assert!(matches!(schema::decode(s, 99, &mut r), Err(Error::Unsupported { .. })));
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// Captured from librdkafka 2.16 (Python `AdminClient.list_topics`): the "all topics" Metadata request, v9. The client
/// leaves a 4-byte placeholder where the compact array null is one byte (its source: `rd_kafka_buf_write_arraycnt_pos`
/// writes an int32 that `finalize_arraycnt` is not called on for this case), so the body is longer than the fields.
/// Kafka's own reader ignores the extra bytes; so do we. Read the way Kafka reads it: Topics null, the booleans false.
#[test]
fn librdkafka_metadata_v9_all_topics() {
    let frame = unhex("00030009000000030005736d6f6b65000000000001000000");
    let req = read_request(&frame).unwrap();
    assert!(req.body.get("Topics").is_null());
    assert_eq!(req.header.client_id.as_deref(), Some("smoke"));
}

/// Captured from librdkafka 2.16: `ApiVersions` v3 and the brokers-only Metadata request (empty topic array).
#[test]
fn librdkafka_api_versions_and_metadata_brokers_only() {
    let frame = unhex(
        "00120003000000010005736d6f6b650017636f6e666c75656e742d6b61666b612d707974686f6e16322e31362e312d72646b61666b612d322e31362e3000",
    );
    let req = read_request(&frame).unwrap();
    assert_eq!(req.body.str("ClientSoftwareName"), Some("confluent-kafka-python"));
    assert_eq!(req.body.str("ClientSoftwareVersion"), Some("2.16.1-rdkafka-2.16.0"));
    let frame = unhex("00030009000000020005736d6f6b65000100000000");
    let req = read_request(&frame).unwrap();
    assert_eq!(req.body.get("Topics").items().len(), 0);
    assert!(!req.body.get("Topics").is_null());
    assert!(!req.body.bool("AllowAutoTopicCreation"));
}

// ---- frames captured from real clients ---------------------------------------------------------------------------

struct Frame {
    client: &'static str,
    api: &'static str,
    version: i16,
    bytes: Vec<u8>,
}

fn frames() -> Vec<Frame> {
    include_str!("frames.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut p = l.split(' ');
            let client = p.next().unwrap();
            let api = p.next().unwrap();
            let version: i16 = p.next().unwrap().trim_start_matches('v').parse().unwrap();
            Frame { client, api, version, bytes: unhex(p.next().unwrap()) }
        })
        .collect()
}

fn frame(client: &str, api: &str, version: i16) -> Frame {
    frames()
        .into_iter()
        .find(|f| f.client.starts_with(client) && f.api == api && f.version == version)
        .unwrap_or_else(|| panic!("no fixture {client} {api} v{version}"))
}

#[test]
fn every_captured_request_decodes_and_survives_a_round_trip() {
    let all = frames();
    assert!(all.len() >= 30);
    for f in &all {
        let req = read_request(&f.bytes).unwrap_or_else(|e| panic!("{} {} v{}: {e}", f.client, f.api, f.version));
        assert_eq!(req.header.version, f.version);
        let again = write_request(&req.header, &req.body);
        let req2 = read_request(&again[4..]).unwrap_or_else(|e| panic!("{} {} v{} (re-encoded): {e}", f.client, f.api, f.version));
        assert_eq!(req.body, req2.body, "{} {} v{}", f.client, f.api, f.version);
    }
}

#[test]
fn librdkafka_produce_has_a_gzip_idempotent_batch_with_headers() {
    use azemu_kafka::records::decode_batches;
    let req = read_request(&frame("librdkafka", "Produce", 9).bytes).unwrap();
    assert_eq!(req.header.client_id.as_deref(), Some("capture-rdkafka"));
    let t = req.body.structs("TopicData").next().unwrap();
    assert_eq!(t.str("Name"), Some("cap-topic"));
    let p = t.structs("PartitionData").next().unwrap();
    assert_eq!(p.i32("Index"), 1);
    let batches = decode_batches(p.bytes("Records").unwrap(), &mut azemu_kafka::records::Budget::bytes_only(1 << 20)).unwrap();
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    assert_eq!(b.compression(), 1, "gzip");
    assert!(b.producer_id >= 0 && b.base_sequence == 0, "idempotent: producer {} sequence {}", b.producer_id, b.base_sequence);
    assert_eq!(b.records.len(), 3);
    assert_eq!(b.records[1].key.as_deref(), Some(&b"k1"[..]));
    assert_eq!(b.records[1].value.as_deref(), Some(&b"value-1"[..]));
    assert_eq!(b.records[1].headers.len(), 2);
    assert_eq!(b.records[1].headers[0].key, "h1");
    assert_eq!(b.records[1].headers[0].value.as_deref(), Some(&b"v1"[..]));
    assert_eq!(b.records[1].headers[1].value, None);
    assert_eq!(b.records.iter().map(|r| r.offset_delta).collect::<Vec<_>>(), vec![0, 1, 2]);
}

#[test]
fn java_produce_batches_decode_or_are_refused_for_their_codec() {
    use azemu_kafka::records::{decode_batches, RecordsError};
    // the Java suite produced plain, gzip, snappy, lz4 and zstd batches: the first two read, the rest are refused by codec
    let (mut read, mut refused) = (0, 0);
    for f in frames().iter().filter(|f| f.client.starts_with("kafka-clients") && f.api == "Produce") {
        let req = read_request(&f.bytes).unwrap();
        let t = req.body.structs("TopicData").next().unwrap();
        let p = t.structs("PartitionData").next().unwrap();
        match decode_batches(p.bytes("Records").unwrap(), &mut azemu_kafka::records::Budget::bytes_only(1 << 20)) {
            Ok(batches) => {
                // the Java producer is idempotent by default
                assert!(batches[0].producer_id >= 0 && batches[0].producer_epoch >= 0 && !batches[0].records.is_empty());
                read += 1;
            }
            Err(RecordsError::UnsupportedCompression(c)) => {
                assert!((2..=4).contains(&c));
                refused += 1;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(read >= 1 && refused >= 1, "read {read}, refused {refused}");
}

#[test]
fn captured_group_requests_carry_what_the_coordinator_needs() {
    let j = read_request(&frame("kafka-clients", "JoinGroup", 7).bytes).unwrap().body;
    assert_eq!(j.str("ProtocolType"), Some("consumer"));
    assert!(j.structs("Protocols").next().unwrap().bytes("Metadata").unwrap().len() > 4);
    assert!(j.i32("SessionTimeoutMs") >= 6000);
    let s = read_request(&frame("kafka-clients", "SyncGroup", 5).bytes).unwrap().body;
    assert!(!s.str("GroupId").unwrap().is_empty());
    let r = read_request(&frame("librdkafka", "JoinGroup", 5).bytes).unwrap().body;
    assert_eq!(r.str("ProtocolType"), Some("consumer"));
    let f = read_request(&frame("librdkafka", "Fetch", 12).bytes).unwrap().body;
    assert!(f.structs("Topics").next().unwrap().structs("Partitions").next().is_some());
    assert_eq!(f.str("RackId"), Some(""));
}
