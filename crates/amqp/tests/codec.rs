//! Codec tests: every type and every encoding, plus malformed input.

use azemu_amqp::codec::{decode, decode_exact, encode_to_vec, DecodeError};
use azemu_amqp::types::Value;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn rt(v: Value) {
    let enc = encode_to_vec(&v);
    let back = decode_exact(&enc).unwrap_or_else(|e| panic!("{v:?} -> {enc:02x?}: {e}"));
    assert_eq!(back, v, "round trip of {v:?} via {enc:02x?}");
}

/// Decoding `bytes` gives `v`; encoding `v` gives `bytes` (the smallest encoding).
fn exact(bytes: &str, v: Value) {
    let b = hex(bytes);
    assert_eq!(decode_exact(&b).unwrap_or_else(|e| panic!("decode {bytes}: {e}")), v, "decode {bytes}");
    assert_eq!(encode_to_vec(&v), b, "encode {v:?}");
}

/// Only decoding: a non-canonical encoding of the same value.
fn decodes(bytes: &str, v: Value) {
    assert_eq!(decode_exact(&hex(bytes)).unwrap_or_else(|e| panic!("decode {bytes}: {e}")), v, "decode {bytes}");
}

#[test]
fn null_and_booleans() {
    exact("40", Value::Null);
    exact("41", Value::Bool(true));
    exact("42", Value::Bool(false));
    decodes("5601", Value::Bool(true));
    decodes("5600", Value::Bool(false));
    assert!(decode_exact(&hex("5602")).is_err());
}

#[test]
fn unsigned_integers() {
    exact("5000", Value::Ubyte(0));
    exact("50ff", Value::Ubyte(255));
    exact("600102", Value::Ushort(0x0102));
    exact("43", Value::Uint(0));
    exact("5201", Value::Uint(1));
    exact("52ff", Value::Uint(255));
    exact("7000000100", Value::Uint(256));
    exact("70ffffffff", Value::Uint(u32::MAX));
    decodes("7000000000", Value::Uint(0));
    decodes("700000000a", Value::Uint(10));
    exact("44", Value::Ulong(0));
    exact("5301", Value::Ulong(1));
    exact("53ff", Value::Ulong(255));
    exact("800000000000000100", Value::Ulong(256));
    exact("80ffffffffffffffff", Value::Ulong(u64::MAX));
    decodes("800000000000000001", Value::Ulong(1));
}

#[test]
fn signed_integers() {
    exact("51ff", Value::Byte(-1));
    exact("6180 00", Value::Short(-32768));
    exact("54ff", Value::Int(-1));
    exact("547f", Value::Int(127));
    exact("5480", Value::Int(-128));
    exact("7100000080", Value::Int(128));
    exact("7180000000", Value::Int(i32::MIN));
    exact("55ff", Value::Long(-1));
    exact("5500", Value::Long(0));
    exact("810000000000000080", Value::Long(128));
    exact("818000000000000000", Value::Long(i64::MIN));
    decodes("71ffffffff", Value::Int(-1));
    decodes("81ffffffffffffffff", Value::Long(-1));
}

#[test]
fn floats_decimals_char_timestamp_uuid() {
    exact("723fc00000", Value::Float(1.5));
    exact("82bff0000000000000", Value::Double(-1.0));
    exact("7400000001", Value::Decimal32([0, 0, 0, 1]));
    exact("840000000000000002", Value::Decimal64([0, 0, 0, 0, 0, 0, 0, 2]));
    exact("9400000000000000000000000000000003", Value::Decimal128([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]));
    exact("7300000041", Value::Char('A'));
    exact("73 0001f600", Value::Char('\u{1f600}'));
    exact("830000018bcfe56800", Value::Timestamp(0x18bcfe56800));
    exact("83ffffffffffffffff", Value::Timestamp(-1));
    exact("98 00112233445566778899aabbccddeeff", Value::Uuid(hex("00112233445566778899aabbccddeeff").try_into().unwrap()));
    assert!(decode_exact(&hex("73 00110000")).is_err(), "surrogate/invalid char");
    for v in [f32::NAN, f32::INFINITY, -0.0] {
        let b = encode_to_vec(&Value::Float(v));
        match decode_exact(&b).unwrap() {
            Value::Float(x) => assert!(x.to_bits() == v.to_bits()),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn binary_string_symbol() {
    exact("a000", Value::Binary(vec![]));
    exact("a003010203", Value::Binary(vec![1, 2, 3]));
    exact("a1026869", Value::String("hi".into()));
    exact("a30161", Value::Symbol("a".into()));
    decodes("b00000000201ff", Value::Binary(vec![1, 255]));
    decodes("b1000000026869", Value::String("hi".into()));
    decodes("b3000000026869", Value::Symbol("hi".into()));
    // multi-byte utf-8
    exact("a106 c3a9 e282ac 61".replace(' ', "").as_str(), Value::String("é€a".into()));
    // large: 256 bytes needs the 32 bit forms
    let big = vec![7u8; 256];
    let enc = encode_to_vec(&Value::Binary(big.clone()));
    assert_eq!(&enc[..5], &[0xb0, 0, 0, 1, 0]);
    assert_eq!(decode_exact(&enc).unwrap(), Value::Binary(big));
    let s = "x".repeat(300);
    rt(Value::String(s.clone()));
    rt(Value::Symbol(s));
    assert!(decode_exact(&hex("a102c328")).is_err(), "invalid utf-8");
    assert_eq!(decode_exact(&hex("a003 0102")), Err(DecodeError::Truncated));
}

#[test]
fn lists() {
    exact("45", Value::List(vec![]));
    exact("c0 02 01 41", Value::List(vec![Value::Bool(true)]));
    exact("c0 04 02 41 52 05", Value::List(vec![Value::Bool(true), Value::Uint(5)]));
    decodes("c0 01 00", Value::List(vec![]));
    decodes("d0 00000005 00000001 41", Value::List(vec![Value::Bool(true)]));
    decodes("d0 00000004 00000000", Value::List(vec![]));
    // 300 elements need list32
    let v = Value::List((0..300).map(|_| Value::Bool(true)).collect());
    let enc = encode_to_vec(&v);
    assert_eq!(enc[0], 0xd0);
    assert_eq!(decode_exact(&enc).unwrap(), v);
    // nested
    rt(Value::List(vec![Value::List(vec![Value::Null, Value::String("x".into())]), Value::Map(vec![])]));
}

#[test]
fn maps() {
    exact("c1 06 02 a1 01 61 52 01", Value::Map(vec![(Value::String("a".into()), Value::Uint(1))]));
    decodes("d1 00000009 00000002 a1 01 61 52 01", Value::Map(vec![(Value::String("a".into()), Value::Uint(1))]));
    exact("c1 01 00", Value::Map(vec![]));
    // order is preserved and duplicate keys survive
    rt(Value::Map(vec![(Value::sym("b"), Value::Int(1)), (Value::sym("a"), Value::Int(2)), (Value::sym("b"), Value::Null)]));
    assert!(decode_exact(&hex("c1 03 01 41")).is_err(), "odd element count");
}

#[test]
fn arrays() {
    // array8 of uint (fixed width 4): count 2, constructor 0x70
    exact("e0 0a 02 70 00000001 00000002", Value::Array(vec![Value::Uint(1), Value::Uint(2)]));
    // array of symbols (what `multiple` fields use)
    let syms = Value::Array(vec![Value::sym("a"), Value::sym("bc")]);
    let enc = encode_to_vec(&syms);
    assert_eq!(enc[0], 0xe0);
    assert_eq!(&enc[3..4], &[0xb3], "array elements share the sym32 constructor");
    assert_eq!(decode_exact(&enc).unwrap(), syms);
    decodes("e0 06 02 a3 01 61 01 62", Value::Array(vec![Value::sym("a"), Value::sym("b")]));
    decodes("f0 00000006 00000001 50 07", Value::Array(vec![Value::Ubyte(7)]));
    rt(Value::Array(vec![]));
    rt(Value::Array(vec![Value::Bool(true), Value::Bool(false)]));
    rt(Value::Array(vec![Value::Int(-1), Value::Int(100000)]));
    rt(Value::Array(vec![Value::Binary(vec![1]), Value::Binary(vec![])]));
    rt(Value::Array(vec![Value::Array(vec![Value::Ulong(1)]), Value::Array(vec![])]));
    // described array: every element carries the same descriptor
    rt(Value::Array(vec![Value::described_code(0x70, Value::Uint(1)), Value::described_code(0x70, Value::Uint(2))]));
    // zero width elements
    decodes("e0 02 03 41", Value::Array(vec![Value::Bool(true); 3]));
    assert!(decode_exact(&hex("f0 00000004 ffffffff 41")).is_err(), "huge zero-width array");
}

#[test]
fn described_values() {
    exact("00 53 70 40", Value::described_code(0x70, Value::Null));
    exact("00 a3 03 61 62 63 52 07", Value::described(Value::sym("abc"), Value::Uint(7)));
    rt(Value::described(Value::sym("x"), Value::described_code(1, Value::List(vec![Value::Uint(300)]))));
}

#[test]
fn mixed_array_degrades_to_list_but_still_decodes() {
    let enc = encode_to_vec(&Value::Array(vec![Value::Uint(1), Value::String("x".into())]));
    assert!(matches!(decode_exact(&enc).unwrap(), Value::List(_)));
}

#[test]
fn decode_reports_consumed_bytes() {
    let (v, n) = decode(&hex("41 42 43")).unwrap();
    assert_eq!((v, n), (Value::Bool(true), 1));
    assert!(decode_exact(&hex("41 42")).is_err());
}

#[test]
fn invalid_constructors_and_depth() {
    assert_eq!(decode_exact(&hex("ff")), Err(DecodeError::InvalidConstructor(0xff)));
    assert_eq!(decode_exact(&[]), Err(DecodeError::Truncated));
    // nesting 100 deep fails instead of overflowing the stack
    let mut b = Vec::new();
    for _ in 0..100 {
        b.extend_from_slice(&[0xc0, 0xff, 0x01]);
    }
    assert!(decode(&b).is_err());
    let mut d = Vec::new();
    for _ in 0..200 {
        d.extend_from_slice(&[0x00, 0x53, 0x01]);
    }
    d.push(0x40);
    assert_eq!(decode_exact(&d), Err(DecodeError::TooDeep));
}

/// An array whose only element is an array whose only element is an array ... `levels` deep, in
/// the 32-bit form (9 bytes per level, so 100,000 levels fit in one frame).
fn nested_arrays(levels: usize) -> Vec<u8> {
    let mut b = vec![0xf0];
    for k in (1..=levels).rev() {
        b.extend_from_slice(&((9 * k + 5) as u32).to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes());
        b.push(0xf0);
    }
    b.extend_from_slice(&5u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.push(0x40);
    b
}

#[test]
fn arrays_nested_100000_deep_are_refused_not_a_stack_overflow() {
    assert_eq!(decode(&nested_arrays(100_000)), Err(DecodeError::TooDeep));
    // the same limit as lists, maps and described values: 64 levels are fine, 100 are not
    assert!(decode(&nested_arrays(40)).is_ok());
    assert_eq!(decode(&nested_arrays(100)), Err(DecodeError::TooDeep));
}

#[test]
fn lying_counts_do_not_allocate() {
    // list32 claiming 4 billion elements in a 4 byte body
    assert!(decode(&hex("d0 00000004 ffffffff")).is_err());
    assert!(decode(&hex("d1 00000004 fffffffe")).is_err());
    assert!(decode(&hex("f0 00000005 ffffffff 50")).is_err());
    // size larger than the data
    assert_eq!(decode(&hex("d0 ffffffff 00000001 41")), Err(DecodeError::Truncated));
    assert_eq!(decode(&hex("b0 ffffffff 41")), Err(DecodeError::Truncated));
    // size smaller than its own count field
    assert!(decode(&hex("d0 00000002 00000001 41")).is_err());
    // body contains more than count says
    assert!(decode(&hex("c0 03 01 41 41")).is_err());
}

/// xorshift: deterministic pseudo random bytes without extra dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn fuzz_random_bytes_never_panic() {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let ctors = [
        0x00u8, 0x40, 0x41, 0x43, 0x45, 0x52, 0x53, 0x70, 0x80, 0xa0, 0xa1, 0xa3, 0xb0, 0xb1, 0xb3, 0xc0, 0xc1, 0xd0, 0xd1, 0xe0, 0xf0,
        0x98, 0x83, 0x73,
    ];
    for _ in 0..20_000 {
        let len = (rng.next() % 64) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // bias the first byte toward real constructors so the interesting paths run
        if let Some(b) = buf.first_mut() {
            *b = ctors[(rng.next() % ctors.len() as u64) as usize];
        }
        let _ = decode(&buf);
        let _ = azemu_amqp::message::Message::decode(&buf);
    }
}

#[test]
fn fuzz_truncations_and_bit_flips_of_valid_values_never_panic() {
    let v = Value::described_code(
        0x12,
        Value::List(vec![
            Value::String("link".into()),
            Value::Uint(1),
            Value::Map(vec![(Value::sym("k"), Value::Array(vec![Value::sym("a"), Value::sym("b")]))]),
            Value::Binary(vec![9; 300]),
            Value::List(vec![Value::Long(-5), Value::Double(2.5), Value::Timestamp(1)]),
        ]),
    );
    let enc = encode_to_vec(&v);
    assert_eq!(decode_exact(&enc).unwrap(), v);
    for cut in 0..enc.len() {
        assert!(decode_exact(&enc[..cut]).is_err(), "truncated at {cut} must fail");
    }
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let mut b = enc.clone();
        for _ in 0..(1 + rng.next() % 3) {
            let i = (rng.next() as usize) % b.len();
            b[i] ^= 1 << (rng.next() % 8);
        }
        let _ = decode(&b);
    }
}

// ---- the budget of values per decode ----

fn zero_width_array(count: u32) -> Vec<u8> {
    let mut v = vec![0xf0];
    v.extend_from_slice(&5u32.to_be_bytes());
    v.extend_from_slice(&count.to_be_bytes());
    v.push(0x40);
    v
}

/// A list holding `n` arrays of `per` zero-width elements each: 9 bytes per array, `n * per` values.
fn list_of_zero_width_arrays(n: usize, per: u32) -> Vec<u8> {
    let items: Vec<u8> = (0..n).flat_map(|_| zero_width_array(per)).collect();
    let mut v = vec![0xd0];
    v.extend_from_slice(&((items.len() + 4) as u32).to_be_bytes());
    v.extend_from_slice(&(n as u32).to_be_bytes());
    v.extend_from_slice(&items);
    v
}

#[test]
fn many_zero_width_arrays_in_one_frame_are_refused() {
    // The review's frame: 150 arrays of 65,535 nulls in 1.5 KB asked for 640 MB.
    let frame = list_of_zero_width_arrays(150, 65_535);
    assert!(frame.len() < 1600);
    assert!(matches!(decode(&frame), Err(DecodeError::Invalid(_))));
    // a whole 64 KiB frame of them
    let frame = list_of_zero_width_arrays(7_000, 65_535);
    assert!(matches!(decode(&frame), Err(DecodeError::Invalid(_))));
    // the same arrays nested one level deeper, in a map
    let inner = list_of_zero_width_arrays(40, 65_535);
    let mut wide = vec![0xd1];
    wide.extend_from_slice(&((4 + 3 + inner.len()) as u32).to_be_bytes());
    wide.extend_from_slice(&2u32.to_be_bytes());
    wide.extend_from_slice(&[0xa3, 1, b'a']);
    wide.extend_from_slice(&inner);
    assert!(matches!(decode(&wide), Err(DecodeError::Invalid(_))));
}

#[test]
fn zero_width_arrays_within_the_budget_still_decode() {
    // 9 bytes of input allow a thousand elements (the allowance covers it) ...
    match decode(&zero_width_array(1000)).unwrap().0 {
        Value::Array(a) => assert_eq!(a.len(), 1000),
        other => panic!("{other:?}"),
    }
    // ... but not tens of thousands
    assert!(decode(&zero_width_array(60_000)).is_err());
    // an array of 60,000 booleans that really are in the input is fine
    let mut real = vec![0xf0];
    real.extend_from_slice(&(60_000u32 + 5).to_be_bytes());
    real.extend_from_slice(&60_000u32.to_be_bytes());
    real.push(0x56);
    real.extend(std::iter::repeat_n(1u8, 60_000));
    assert!(decode(&real).is_ok());
    // a list of 10,000 nulls costs 10,000 bytes and is fine
    let mut list = vec![0xd0];
    list.extend_from_slice(&(4 + 10_000u32).to_be_bytes());
    list.extend_from_slice(&10_000u32.to_be_bytes());
    list.extend(std::iter::repeat_n(0x40u8, 10_000));
    assert!(decode(&list).is_ok());
}

#[test]
fn the_budget_is_shared_by_decodes_that_share_it() {
    use azemu_amqp::codec::{decode_budgeted, value_budget};
    let a = zero_width_array(900);
    let mut budget = value_budget(a.len() * 2);
    assert!(decode_budgeted(&a, &mut budget).is_ok());
    assert!(budget < value_budget(0), "900 values were charged");
    // the second one no longer fits
    assert!(decode_budgeted(&a, &mut budget).is_err());
}

#[test]
fn an_array_descriptor_is_a_short_symbol_or_a_ulong() {
    // f0 size count 00 <descriptor> 40: elements are `described(descriptor, null)`.
    fn array_with_descriptor(descriptor: &[u8], count: u32) -> Vec<u8> {
        let mut v = vec![0xf0];
        v.extend_from_slice(&((4 + 1 + descriptor.len() + 1) as u32).to_be_bytes());
        v.extend_from_slice(&count.to_be_bytes());
        v.push(0x00);
        v.extend_from_slice(descriptor);
        v.push(0x40);
        v
    }
    assert!(decode(&array_with_descriptor(&[0x53, 0x12], 3)).is_ok());
    assert!(decode(&array_with_descriptor(&[0xa3, 3, b'a', b':', b'b'], 3)).is_ok());
    // a 300 KB descriptor repeated on 700,000 elements would be 200 GB
    let mut big = vec![0xb3];
    big.extend_from_slice(&300_000u32.to_be_bytes());
    big.extend(std::iter::repeat_n(b'x', 300_000));
    assert!(decode(&array_with_descriptor(&big, 3)).is_err());
    // a list as descriptor is no descriptor
    assert!(decode(&array_with_descriptor(&[0x45], 3)).is_err());
}

#[test]
fn a_message_with_many_sections_shares_one_budget() {
    use azemu_amqp::message::Message;
    // each section is an amqp-value holding 900 zero-width array elements; each alone would pass
    let one: Vec<u8> = [&[0x00u8, 0x53, 0x77][..], &zero_width_array(900)].concat();
    let mut payload = Vec::new();
    for _ in 0..50 {
        payload.extend_from_slice(&one);
    }
    // 50 amqp-value sections are also "mixed body" in the end, so look at the error text: the
    // budget runs out at the second or third section, before that is noticed
    let err = Message::decode(&payload).unwrap_err();
    assert!(err.to_string().contains("too many values") || err.to_string().contains("mixed"), "{err}");
    // use footers (maps of arrays) so that only the budget can refuse
    let mut footers = Vec::new();
    for _ in 0..50 {
        let mut f = vec![0x00u8, 0x53, 0x78, 0xc1];
        let arr = zero_width_array(900);
        f.push((1 + 3 + arr.len()) as u8); // size: the count byte and the two items
        f.push(2);
        f.extend_from_slice(&[0xa3, 1, b'a']);
        f.extend_from_slice(&arr);
        footers.push(f);
    }
    let payload: Vec<u8> = footers.concat();
    let err = Message::decode(&payload).unwrap_err();
    assert!(err.to_string().contains("too many values"), "{err}");
}
