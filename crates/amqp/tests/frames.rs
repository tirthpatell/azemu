//! Frames, performatives and messages: byte vectors produced by the Python SDK's encoder
//! (`azure.servicebus._pyamqp._encode`, 7.15.0) and by hand from the specification.

use azemu_amqp::codec::{decode, encode_to_vec};
use azemu_amqp::error::Error;
use azemu_amqp::frame::*;
use azemu_amqp::message::*;
use azemu_amqp::performatives::*;
use azemu_amqp::sasl::Sasl;
use azemu_amqp::terminus::*;
use azemu_amqp::types::Value;
use bytes::{Bytes, BytesMut};

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The Python encoder returns a 6 byte header (size, doff, type); its transport inserts the
/// channel afterwards. Put it back (channel 0) and fix the size.
fn py(s: &str) -> Vec<u8> {
    let b = hex(s);
    let mut out = b[..6].to_vec();
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&b[6..]);
    let size = out.len() as u32;
    out[..4].copy_from_slice(&size.to_be_bytes());
    out
}

fn parse(bytes: &[u8]) -> Frame {
    let mut b = BytesMut::from(bytes);
    let f = decode_frame(&mut b, u32::MAX).unwrap().expect("a whole frame");
    assert!(b.is_empty());
    f
}

fn perf(f: Frame) -> (u16, Performative, Bytes) {
    match f.body {
        FrameBody::Amqp(p, payload) => (f.channel, p, payload),
        other => panic!("not an amqp frame: {other:?}"),
    }
}

// Vectors from the Python encoder.
const PY_OPEN: &str = "000000500200005310c0430aa103636964a11b6d796e732e736572766963656275732e77696e646f77732e6e6574700001000060ffff700003a98040404040c10d02a30770726f64756374a10178";
const PY_BEGIN: &str = "000000220200005311c0150840437000001388700001000070ffffffff404040";
const PY_ATTACH: &str = "000000750200005312c0680ea1056c696e6b315203560150005000005328c0140ba1076d79717565756540404040404040404040005329c00d07a104246362734040404040404040408000000000001000004040c11d02a315636f6d2e6d6963726f736f66743a74696d656f75747100007530";
const PY_FLOW: &str = "000000200200005313c0130b52015264520252c843520552324056004040";
const PY_TRANSFER: &str = "000000260200005314c0120b435207a0020102435600560040404040405041594c4f4144";
const PY_DISPOSITION: &str = "000000180200005315c00b0656015201520356014040";
const PY_DETACH: &str = "0000002f0200005316c022035202560100531dc01803a30e616d71703a6e6f742d666f756e64a1046e6f706540";
const PY_END: &str = "0000000f0200005317c0020140";
const PY_CLOSE: &str = "000000300200005318c0230100531dc01d03a313616d71703a696e7465726e616c2d6572726f72a104626f6f6d40";
const PY_EMPTY: &str = "000000080200";
const PY_MESSAGE: &str = "005370c00d05560150047000007530405202005372c11a02a313782d6f70742d706172746974696f6e2d6b6579a102706b005373c0350da1056d69642d3140a103746f31a1047375626aa1027274a103636964a30a746578742f706c61696e404040a10573657373314040005374c10e04a101615401a10162a10374776f005375a00568656c6c6f";

#[test]
fn python_open() {
    let (ch, p, _) = perf(parse(&py(PY_OPEN)));
    assert_eq!(ch, 0);
    let Performative::Open(o) = p else { panic!() };
    assert_eq!(o.container_id, "cid");
    assert_eq!(o.hostname.as_deref(), Some("myns.servicebus.windows.net"));
    assert_eq!(o.max_frame_size, 65536);
    assert_eq!(o.channel_max, 65535);
    assert_eq!(o.idle_timeout, Some(240000));
    assert_eq!(o.properties.unwrap().map_get("product"), Some(&Value::str("x")));
}

#[test]
fn python_begin() {
    let (_, p, _) = perf(parse(&py(PY_BEGIN)));
    let Performative::Begin(b) = p else { panic!() };
    assert_eq!(b.remote_channel, None);
    assert_eq!((b.next_outgoing_id, b.incoming_window, b.outgoing_window, b.handle_max), (0, 5000, 65536, u32::MAX));
}

#[test]
fn python_attach() {
    let (_, p, _) = perf(parse(&py(PY_ATTACH)));
    let Performative::Attach(a) = p else { panic!() };
    assert_eq!(a.name, "link1");
    assert_eq!(a.handle, 3);
    assert_eq!(a.role, Role::Receiver);
    assert_eq!(a.snd_settle_mode, SndSettleMode::Unsettled);
    assert_eq!(a.rcv_settle_mode, RcvSettleMode::First);
    assert_eq!(a.source.unwrap().address.as_deref(), Some("myqueue"));
    assert_eq!(a.target.unwrap().address(), Some("$cbs"));
    assert_eq!(a.max_message_size, Some(1048576));
    assert_eq!(a.properties.unwrap().map_get("com.microsoft:timeout"), Some(&Value::Int(30000)));
}

#[test]
fn python_flow_transfer_disposition() {
    let (_, p, _) = perf(parse(&py(PY_FLOW)));
    let Performative::Flow(f) = p else { panic!() };
    assert_eq!(f.next_incoming_id, Some(1));
    assert_eq!((f.incoming_window, f.next_outgoing_id, f.outgoing_window), (100, 2, 200));
    assert_eq!((f.handle, f.delivery_count, f.link_credit), (Some(0), Some(5), Some(50)));
    assert!(!f.drain && !f.echo);

    let (_, p, payload) = perf(parse(&py(PY_TRANSFER)));
    let Performative::Transfer(t) = p else { panic!() };
    assert_eq!(t.handle, 0);
    assert_eq!(t.delivery_id, Some(7));
    assert_eq!(t.delivery_tag, Some(vec![1, 2]));
    assert_eq!(t.message_format, Some(0));
    assert_eq!(t.settled, Some(false));
    assert!(!t.more);
    assert_eq!(&payload[..], b"PAYLOAD");

    let (_, p, _) = perf(parse(&py(PY_DISPOSITION)));
    let Performative::Disposition(d) = p else { panic!() };
    assert_eq!((d.role, d.first, d.last, d.settled), (Role::Receiver, 1, Some(3), true));
    assert_eq!(d.state, None);
}

#[test]
fn python_detach_end_close_empty() {
    let (_, p, _) = perf(parse(&py(PY_DETACH)));
    let Performative::Detach(d) = p else { panic!() };
    assert_eq!((d.handle, d.closed), (2, true));
    assert_eq!(d.error, Some(Error::new("amqp:not-found", "nope")));

    let (_, p, _) = perf(parse(&py(PY_END)));
    assert_eq!(p, Performative::End(End { error: None }));

    let (_, p, _) = perf(parse(&py(PY_CLOSE)));
    assert_eq!(p, Performative::Close(Close { error: Some(Error::new("amqp:internal-error", "boom")) }));

    assert_eq!(parse(&py(PY_EMPTY)).body, FrameBody::Empty);
}

#[test]
fn python_message_sections() {
    let m = Message::decode(&hex(PY_MESSAGE)).unwrap();
    let h = m.header.as_ref().unwrap();
    assert_eq!((h.durable, h.priority, h.ttl, h.delivery_count), (Some(true), Some(4), Some(30000), Some(2)));
    assert_eq!(m.annotation("x-opt-partition-key"), Some(&Value::str("pk")));
    let p = m.properties.as_ref().unwrap();
    assert_eq!(p.message_id, Some(Value::str("mid-1")));
    assert_eq!(p.to.as_deref(), Some("to1"));
    assert_eq!(p.subject.as_deref(), Some("subj"));
    assert_eq!(p.reply_to.as_deref(), Some("rt"));
    assert_eq!(p.correlation_id, Some(Value::str("cid")));
    assert_eq!(p.content_type.as_deref(), Some("text/plain"));
    assert_eq!(p.group_id.as_deref(), Some("sess1"));
    assert_eq!(m.app_property("a"), Some(&Value::Int(1)));
    assert_eq!(m.app_property("b"), Some(&Value::str("two")));
    assert_eq!(m.body, Body::Data(vec![b"hello".to_vec()]));
    // our encoder produces something the decoder reads back identically
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
}

#[test]
fn python_amqp_value_uuid_body() {
    let m = Message::decode(&hex("0053779812345678123456781234567812345678")).unwrap();
    assert_eq!(m.body, Body::Value(Value::Uuid(hex("12345678123456781234567812345678").try_into().unwrap())));
}

#[test]
fn every_body_section_type_is_kept_as_it_was_sent() {
    // JavaScript Event Hubs SDK, body null: a data section (0x75) holding null (0x40)
    let m = Message::decode(&hex("00537540")).unwrap();
    assert_eq!(m.body, Body::NullData);
    assert_eq!(m.encode(), hex("00537540"), "sent back as it came");
    // an amqp-value section holding null, and no body at all
    let m = Message::decode(&hex("00537740")).unwrap();
    assert_eq!(m.body, Body::Value(Value::Null));
    assert_eq!(m.encode(), hex("00537740"));
    assert_eq!(Message::decode(&[]).unwrap().body, Body::Empty);
    // amqp-sequence sections: one list each
    let m = Message::decode(&hex("00537645" /* [] */)).unwrap();
    assert_eq!(m.body, Body::Sequence(vec![vec![]]));
    let m = Message::decode(&hex("005376c00301540a")).unwrap();
    assert_eq!(m.body, Body::Sequence(vec![vec![Value::Int(10)]]));
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    // amqp-value with a string, data sections with binary, several of them
    let m = Message::decode(&hex("005377a10161")).unwrap();
    assert_eq!(m.body, Body::Value(Value::str("a")));
    let m = Message::decode(&hex("005375a0016100537540005375a00162")).unwrap();
    assert_eq!(m.body, Body::Data(vec![b"a".to_vec(), vec![], b"b".to_vec()]), "a null section among binary ones is an empty one");
    // a data section that is neither binary nor null is still refused
    assert!(Message::decode(&hex("005375a10161")).is_err());
}

#[test]
fn our_encoding_of_open_matches_spec_example() {
    // open with only container-id: the trailing nulls are omitted (spec 1.6.x allows it)
    let o = Open { container_id: "c".into(), ..Default::default() };
    assert_eq!(encode_to_vec(&o.to_value()), hex("0053 10 c0 04 01 a1 01 63"));
    // hand-derived: described(0x10) list8{size 4, count 1, str8 "c"}
}

fn rt_perf(p: Performative) {
    let f = Frame { channel: 7, body: FrameBody::Amqp(p.clone(), Bytes::new()) };
    let (ch, back, _) = perf(parse(&frame_bytes(&f)));
    assert_eq!(ch, 7);
    assert_eq!(back, p);
}

#[test]
fn performative_round_trips() {
    rt_perf(Performative::Open(Open {
        container_id: "c".into(),
        hostname: Some("h".into()),
        max_frame_size: 4096,
        channel_max: 10,
        idle_timeout: Some(1000),
        outgoing_locales: vec!["en-US".into()],
        incoming_locales: vec!["en-US".into(), "de".into()],
        offered_capabilities: vec!["ANONYMOUS-RELAY".into()],
        desired_capabilities: vec!["x".into()],
        properties: Some(Value::Map(vec![(Value::sym("p"), Value::Long(-3))])),
    }));
    rt_perf(Performative::Begin(Begin {
        remote_channel: Some(3),
        next_outgoing_id: 9,
        incoming_window: 1,
        outgoing_window: 2,
        handle_max: 7,
        ..Default::default()
    }));
    let mut a = Attach::new("n", 4, Role::Sender);
    a.source = Some(Source {
        address: Some("q".into()),
        durable: 2,
        expiry_policy: Some("session-end".into()),
        timeout: 5,
        dynamic: true,
        dynamic_node_properties: Some(Value::Map(vec![(Value::sym("a"), Value::Null)])),
        distribution_mode: Some("move".into()),
        filter: Some(Value::Map(vec![(
            Value::sym("com.microsoft:session-filter"),
            Value::described(Value::sym("com.microsoft:session-filter"), Value::str("s1")),
        )])),
        default_outcome: Some(DeliveryState::Released.to_value()),
        outcomes: vec!["amqp:accepted:list".into(), "amqp:rejected:list".into()],
        capabilities: vec!["queue".into()],
    });
    a.target = Some(TargetKind::Target(Target::with_address("t")));
    a.snd_settle_mode = SndSettleMode::Settled;
    a.rcv_settle_mode = RcvSettleMode::Second;
    a.initial_delivery_count = Some(12);
    a.max_message_size = Some(u64::MAX);
    a.offered_capabilities = vec!["o".into()];
    a.desired_capabilities = vec!["d".into()];
    a.properties = Some(Value::Map(vec![(Value::sym("com.microsoft:tracking-id"), Value::str("x"))]));
    rt_perf(Performative::Attach(a));
    let mut c = Attach::new("coord", 1, Role::Sender);
    c.target = Some(TargetKind::Coordinator(Coordinator { capabilities: vec!["amqp:local-transactions".into()] }));
    rt_perf(Performative::Attach(c));
    rt_perf(Performative::Flow(Flow {
        next_incoming_id: Some(1),
        incoming_window: 2,
        next_outgoing_id: 3,
        outgoing_window: 4,
        handle: Some(5),
        delivery_count: Some(6),
        link_credit: Some(7),
        available: Some(8),
        drain: true,
        echo: true,
        properties: Some(Value::Map(vec![(Value::sym("k"), Value::Bool(true))])),
    }));
    rt_perf(Performative::Flow(Flow { incoming_window: 2, next_outgoing_id: 3, outgoing_window: 4, ..Default::default() }));
    rt_perf(Performative::Transfer(Transfer {
        handle: 1,
        delivery_id: Some(2),
        delivery_tag: Some(vec![1, 2, 3]),
        message_format: Some(0x80013700),
        settled: Some(true),
        more: true,
        rcv_settle_mode: Some(1),
        state: Some(DeliveryState::Received { section_number: 1, section_offset: 2 }),
        resume: true,
        aborted: true,
        batchable: true,
    }));
    rt_perf(Performative::Disposition(Disposition {
        role: Role::Sender,
        first: 1,
        last: Some(9),
        settled: true,
        state: Some(DeliveryState::Accepted),
        batchable: true,
    }));
    rt_perf(Performative::Detach(Detach { handle: 1, closed: true, error: Some(Error::new("c", "d").with_info("k", Value::Int(1))) }));
    rt_perf(Performative::End(End { error: Some(Error::condition_only("amqp:session:errant-link")) }));
    rt_perf(Performative::Close(Close { error: None }));
}

#[test]
fn delivery_states_round_trip() {
    let states = vec![
        DeliveryState::Accepted,
        DeliveryState::Released,
        DeliveryState::Rejected { error: None },
        DeliveryState::Rejected {
            error: Some(Error::new("com.microsoft:dead-letter", "d").with_info("DeadLetterReason", Value::str("r"))),
        },
        DeliveryState::Modified { delivery_failed: true, undeliverable_here: false, message_annotations: None },
        DeliveryState::Modified {
            delivery_failed: false,
            undeliverable_here: true,
            message_annotations: Some(Value::Map(vec![(Value::sym("a"), Value::Int(1))])),
        },
        DeliveryState::Received { section_number: 3, section_offset: 100 },
        DeliveryState::Declared { txn_id: vec![1, 2] },
        DeliveryState::Transactional { txn_id: vec![9], outcome: Some(Box::new(DeliveryState::Accepted)) },
        DeliveryState::Transactional { txn_id: vec![9], outcome: None },
    ];
    for s in states {
        let enc = encode_to_vec(&s.to_value());
        let (v, _) = decode(&enc).unwrap();
        assert_eq!(DeliveryState::from_value(&v).unwrap(), s);
    }
    // accepted on the wire is the described empty list: 00 53 24 45
    assert_eq!(encode_to_vec(&DeliveryState::Accepted.to_value()), hex("00 53 24 45"));
    // symbol descriptors (older clients) decode too
    let v = Value::described(Value::sym("amqp:accepted:list"), Value::List(vec![]));
    assert_eq!(DeliveryState::from_value(&v).unwrap(), DeliveryState::Accepted);
}

#[test]
fn txn_control_round_trip() {
    for c in [
        TxnControl::Declare { global_id: None },
        TxnControl::Discharge { txn_id: vec![1], fail: true },
        TxnControl::Discharge { txn_id: vec![1], fail: false },
    ] {
        let (v, _) = decode(&encode_to_vec(&c.to_value())).unwrap();
        assert_eq!(TxnControl::from_value(&v).unwrap(), c);
    }
}

#[test]
fn sasl_frames() {
    let m = Sasl::Mechanisms { mechanisms: vec!["ANONYMOUS".into(), "PLAIN".into(), "EXTERNAL".into(), "MSSBCBS".into()] };
    let f = Frame { channel: 0, body: FrameBody::Sasl(m.clone()) };
    let bytes = frame_bytes(&f);
    assert_eq!(bytes[5], FRAME_TYPE_SASL);
    assert_eq!(parse(&bytes), f);
    for s in [
        Sasl::Init { mechanism: "PLAIN".into(), initial_response: Some(b"\0u\0p".to_vec()), hostname: Some("h".into()) },
        Sasl::Init { mechanism: "ANONYMOUS".into(), initial_response: None, hostname: None },
        Sasl::Challenge { challenge: vec![1] },
        Sasl::Response { response: vec![2] },
        Sasl::Outcome { code: 0, additional_data: None },
        Sasl::Outcome { code: 1, additional_data: Some(vec![3]) },
    ] {
        let f = Frame { channel: 0, body: FrameBody::Sasl(s) };
        assert_eq!(parse(&frame_bytes(&f)), f);
    }
    // sasl-init as the Python SDK sends it for MSSBCBS: mechanism symbol, no response
    let init = hex("00000020 02 01 0000 005341 c0 0b 02 a3 07 4d535342434253 40");
    let mut b = BytesMut::from(&init[..]);
    // fix the size to the real length
    let len = init.len() as u32;
    b[..4].copy_from_slice(&len.to_be_bytes());
    let f = decode_frame(&mut b, u32::MAX).unwrap().unwrap();
    assert!(matches!(f.body, FrameBody::Sasl(Sasl::Init { ref mechanism, .. }) if mechanism == "MSSBCBS"));
}

#[test]
fn partial_frames_wait_for_more_bytes() {
    let bytes = py(PY_TRANSFER);
    let mut buf = BytesMut::new();
    for (i, b) in bytes.iter().enumerate() {
        buf.extend_from_slice(&[*b]);
        let r = decode_frame(&mut buf, u32::MAX).unwrap();
        if i + 1 < bytes.len() {
            assert!(r.is_none(), "frame complete too early at {i}");
        } else {
            assert!(r.is_some());
        }
    }
}

#[test]
fn two_frames_in_one_buffer() {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&py(PY_EMPTY));
    buf.extend_from_slice(&py(PY_END));
    assert_eq!(decode_frame(&mut buf, 1000).unwrap().unwrap().body, FrameBody::Empty);
    assert!(matches!(decode_frame(&mut buf, 1000).unwrap().unwrap().body, FrameBody::Amqp(Performative::End(_), _)));
    assert!(decode_frame(&mut buf, 1000).unwrap().is_none());
}

#[test]
fn framing_errors() {
    // size smaller than header
    let mut b = BytesMut::from(&hex("00000004 02 00 0000")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Framing(_))));
    // size above the limit is rejected before the body arrives
    let mut b = BytesMut::from(&hex("00100000 02 00 0000")[..]);
    assert!(matches!(decode_frame(&mut b, 65536), Err(FrameError::Framing(_))));
    // doff < 2
    let mut b = BytesMut::from(&hex("00000008 01 00 0000")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Framing(_))));
    // doff beyond the frame
    let mut b = BytesMut::from(&hex("00000008 05 00 0000")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Framing(_))));
    // unknown frame type
    let mut b = BytesMut::from(&hex("00000009 02 07 0000 40")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Framing(_))));
    // garbage body
    let mut b = BytesMut::from(&hex("00000009 02 00 0000 ff")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Decode(_))));
    // not a performative
    let mut b = BytesMut::from(&hex("00000009 02 00 0000 41")[..]);
    assert!(matches!(decode_frame(&mut b, 1000), Err(FrameError::Decode(_))));
    // extended header (doff 3) is skipped
    let mut b = BytesMut::from(&hex("00000010 03 00 0000 aabbccdd 005317 4540")[..]);
    // size says 16: header 8 + ext 4 + body 4 (0053 17 45) ... plus a stray byte stays buffered
    let f = decode_frame(&mut b, 1000).unwrap().unwrap();
    assert!(matches!(f.body, FrameBody::Amqp(Performative::End(_), _)));
    assert_eq!(b.len(), 1);
}

#[test]
fn transfers_split_by_max_frame_size() {
    let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    let t = Transfer {
        handle: 2,
        delivery_id: Some(5),
        delivery_tag: Some(vec![0xaa; 16]),
        message_format: Some(0),
        settled: Some(false),
        ..Default::default()
    };
    let frames = split_transfer(1, t.clone(), &payload, 1024);
    assert!(frames.len() >= 5);
    let mut got = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let bytes = frame_bytes(f);
        assert!(bytes.len() <= 1024, "frame {i} is {} bytes", bytes.len());
        let (ch, p, pl) = perf(parse(&bytes));
        assert_eq!(ch, 1);
        let Performative::Transfer(tr) = p else { panic!() };
        assert_eq!(tr.more, i + 1 < frames.len(), "more flag on frame {i}");
        assert_eq!(tr.handle, 2);
        if i == 0 {
            assert_eq!(tr.delivery_id, Some(5));
            assert_eq!(tr.delivery_tag, Some(vec![0xaa; 16]));
        } else {
            assert_eq!(tr.delivery_id, None, "continuations carry no delivery-id");
        }
        got.extend_from_slice(&pl);
    }
    assert_eq!(got, payload);
    // a payload that fits stays one frame, even with the minimum frame size
    let one = split_transfer(0, t.clone(), b"small", 512);
    assert_eq!(one.len(), 1);
    // an empty payload is still one frame
    assert_eq!(split_transfer(0, t, b"", 512).len(), 1);
}

#[test]
fn batch_message_format() {
    let a = Message {
        properties: Some(Properties { message_id: Some(Value::str("1")), ..Default::default() }),
        body: Body::Data(vec![b"x".to_vec()]),
        ..Default::default()
    };
    let b = Message::data(b"yy".to_vec());
    let payload = Message::encode_batch(&[a.encode(), b.encode()]);
    let msgs = Message::decode_batch(&payload).unwrap();
    assert_eq!(msgs, vec![a, b]);
    assert_eq!(BATCH_MESSAGE_FORMAT, 0x80013700);
}

#[test]
fn message_body_kinds() {
    let m = Message { body: Body::Sequence(vec![vec![Value::Int(1)], vec![Value::str("a")]]), ..Default::default() };
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    let m = Message {
        body: Body::Value(Value::Map(vec![(Value::str("k"), Value::Uint(1))])),
        footer: Some(vec![(Value::sym("f"), Value::Int(2))]),
        delivery_annotations: Some(vec![(Value::sym("d"), Value::Int(3))]),
        ..Default::default()
    };
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    let m = Message { body: Body::Data(vec![vec![1], vec![2, 3]]), ..Default::default() };
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    // empty message
    assert_eq!(Message::decode(&[]).unwrap(), Message::default());
    // message ids of every allowed type
    for id in [Value::Ulong(5), Value::Uuid([1; 16]), Value::Binary(vec![1, 2]), Value::str("s")] {
        let m = Message {
            properties: Some(Properties { message_id: Some(id.clone()), correlation_id: Some(id), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    }
    // timestamps and group sequence
    let m = Message {
        properties: Some(Properties {
            absolute_expiry_time: Some(1_700_000_000_000),
            creation_time: Some(5),
            group_sequence: Some(3),
            reply_to_group_id: Some("g".into()),
            user_id: Some(vec![1]),
            content_encoding: Some("gzip".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(Message::decode(&m.encode()).unwrap(), m);
    // mixed body sections are refused
    let mut bad = Message::data(vec![1]).encode();
    bad.extend_from_slice(&encode_to_vec(&Value::described_code(0x77, Value::Null)));
    assert!(Message::decode(&bad).is_err());
}

#[test]
fn fuzz_frames_never_panic() {
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let seeds: Vec<Vec<u8>> =
        [PY_OPEN, PY_BEGIN, PY_ATTACH, PY_FLOW, PY_TRANSFER, PY_DISPOSITION, PY_DETACH, PY_END, PY_CLOSE].iter().map(|s| py(s)).collect();
    for _ in 0..30_000 {
        let mut b = seeds[(next() % seeds.len() as u64) as usize].clone();
        match next() % 3 {
            0 => b.truncate((next() as usize) % b.len()),
            1 => {
                let i = (next() as usize) % b.len();
                b[i] = next() as u8;
            }
            _ => {
                let i = (next() as usize) % b.len();
                b[i] ^= 1 << (next() % 8);
            }
        }
        let mut buf = BytesMut::from(&b[..]);
        let _ = decode_frame(&mut buf, 1 << 20);
    }
    // random garbage with a plausible header
    for _ in 0..20_000 {
        let len = 8 + (next() % 40) as usize;
        let mut b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        b[..4].copy_from_slice(&(len as u32).to_be_bytes());
        b[4] = 2;
        b[5] = (next() % 2) as u8;
        let mut buf = BytesMut::from(&b[..]);
        let _ = decode_frame(&mut buf, 1 << 20);
    }
}
