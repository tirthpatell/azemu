//! Effective partition keys: the hash of a partition key value, as the SDKs compute it
//! (MurmurHash3, port of the algorithm in the Cosmos SDKs). Used to answer requests that
//! carry an EPK range (feed ranges, hierarchical key prefixes) and for change feed filters.
//!
//! Strings are upper-case hex. The whole key space is `""` to `"FF"`.
//!
//! MurmurHash3 is by Austin Appleby (public domain). What is hashed, and how a value is written for hashing, follows the behaviour
//! of Microsoft's Cosmos DB SDKs (MIT, Copyright Microsoft), read from their source and checked against real answers. See NOTICE.

use crate::pk::{PartitionKeyDef, PkComp};

fn rotl64(v: u64, r: u32) -> u64 {
    v.rotate_left(r)
}

fn fmix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51afd7ed558ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ceb9fe1a85ec53);
    k ^= k >> 33;
    k
}

/// MurmurHash3 x64 128 with seed 0; returns (h1, h2).
pub fn murmur128(data: &[u8]) -> (u64, u64) {
    let c1: u64 = 0x87c37b91114253d5;
    let c2: u64 = 0x4cf5ad432745937f;
    let (mut h1, mut h2) = (0u64, 0u64);
    let (chunks, rest) = data.as_chunks::<16>();
    for ch in chunks {
        let mut k1 = u64::from_le_bytes(ch[0..8].try_into().unwrap_or([0; 8]));
        let mut k2 = u64::from_le_bytes(ch[8..16].try_into().unwrap_or([0; 8]));
        k1 = k1.wrapping_mul(c1);
        k1 = rotl64(k1, 31);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
        h1 = rotl64(h1, 27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);
        k2 = k2.wrapping_mul(c2);
        k2 = rotl64(k2, 33);
        k2 = k2.wrapping_mul(c1);
        h2 ^= k2;
        h2 = rotl64(h2, 31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
    }
    let tail = rest;
    let n = tail.len();
    let mut k1 = 0u64;
    let mut k2 = 0u64;
    for i in (8..n).rev() {
        k2 ^= (tail[i] as u64) << (8 * (i - 8));
    }
    if n > 8 {
        k2 = k2.wrapping_mul(c2);
        k2 = rotl64(k2, 33);
        k2 = k2.wrapping_mul(c1);
        h2 ^= k2;
    }
    for i in (0..n.min(8)).rev() {
        k1 ^= (tail[i] as u64) << (8 * i);
    }
    if n > 0 {
        k1 = k1.wrapping_mul(c1);
        k1 = rotl64(k1, 31);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u64;
    h2 ^= data.len() as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

/// MurmurHash3 x86 32 with seed 0.
pub fn murmur32(data: &[u8]) -> u32 {
    let c1: u32 = 0xcc9e2d51;
    let c2: u32 = 0x1b873593;
    let mut h1: u32 = 0;
    let (chunks, rest) = data.as_chunks::<4>();
    for ch in chunks {
        let mut k1 = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]);
        k1 = k1.wrapping_mul(c1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe6546b64);
    }
    let tail = rest;
    let mut k1 = 0u32;
    if tail.len() == 3 {
        k1 ^= (tail[2] as u32) << 16;
    }
    if tail.len() >= 2 {
        k1 ^= (tail[1] as u32) << 8;
    }
    if !tail.is_empty() {
        k1 ^= tail[0] as u32;
        k1 = k1.wrapping_mul(c1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(c2);
        h1 ^= k1;
    }
    h1 ^= data.len() as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85ebca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2ae35);
    h1 ^= h1 >> 16;
    h1
}

fn write_for_hashing(c: &PkComp, suffix: u8, out: &mut Vec<u8>) {
    match c {
        PkComp::Bool(true) => out.push(0x3),
        PkComp::Bool(false) => out.push(0x2),
        PkComp::Null => out.push(0x1),
        PkComp::Num(n) => {
            out.push(0x5);
            out.extend_from_slice(&n.to_le_bytes());
        }
        PkComp::Str(s) => {
            out.push(0x8);
            out.extend_from_slice(s.as_bytes());
            out.push(suffix);
        }
        PkComp::Undefined => out.push(0x0),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn hash_v2(buf: &[u8]) -> String {
    let (h1, h2) = murmur128(buf);
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&h1.to_le_bytes());
    bytes.extend_from_slice(&h2.to_le_bytes());
    bytes.reverse();
    bytes[0] &= 0x3F;
    hex(&bytes)
}

fn encode_double(v: f64, out: &mut Vec<u8>) {
    out.push(0x5);
    let bits = v.to_bits();
    let mask = 0x8000_0000_0000_0000u64;
    let mut payload = if bits < mask { bits ^ mask } else { (!bits).wrapping_add(1) };
    out.push((payload >> 56) as u8);
    payload <<= 8;
    let mut byte_to_write = 0u8;
    let mut first = true;
    while payload != 0 {
        if !first {
            out.push(byte_to_write);
        } else {
            first = false;
        }
        byte_to_write = ((payload >> 56) as u8) | 1;
        payload <<= 7;
    }
    out.push(byte_to_write & 0xFE);
}

fn binary_v1(c: &PkComp, out: &mut Vec<u8>) {
    match c {
        PkComp::Bool(true) => out.push(0x3),
        PkComp::Bool(false) => out.push(0x2),
        PkComp::Num(n) => encode_double(*n, out),
        PkComp::Str(s) => {
            out.push(0x8);
            let b = s.as_bytes();
            let short = b.len() <= 100;
            let n = if short { b.len() } else { 101.min(b.len()) };
            for &x in &b[..n] {
                out.push(x.wrapping_add(1));
            }
            if short {
                out.push(0);
            }
        }
        PkComp::Undefined => out.push(0x0),
        PkComp::Null => {}
    }
}

fn hash_v1(value: &[PkComp]) -> String {
    let comps: Vec<PkComp> = value
        .iter()
        .map(|c| match c {
            PkComp::Str(s) => PkComp::Str(s.chars().take(100).collect()),
            PkComp::Num(n) if n.fract() == 0.0 => PkComp::Num((*n as i64 as u32) as f64),
            other => other.clone(),
        })
        .collect();
    let mut buf = Vec::new();
    for c in &comps {
        write_for_hashing(c, 0, &mut buf);
    }
    let h = murmur32(&buf) as f64;
    let mut out = Vec::new();
    encode_double(h, &mut out);
    for c in &comps {
        binary_v1(c, &mut out);
    }
    hex(&out)
}

/// Effective partition key string of a (full or prefix) partition key value.
pub fn epk(def: &PartitionKeyDef, value: &[PkComp]) -> String {
    if value.is_empty() {
        return String::new();
    }
    if def.kind == "MultiHash" {
        let mut s = String::new();
        for c in value {
            let mut buf = Vec::new();
            write_for_hashing(c, 0xFF, &mut buf);
            s.push_str(&hash_v2(&buf));
        }
        return s;
    }
    if def.version == 1 {
        return hash_v1(value);
    }
    let mut buf = Vec::new();
    for c in value {
        write_for_hashing(c, 0xFF, &mut buf);
    }
    hash_v2(&buf)
}

/// `[min, max]` bounds from the EPK headers; absent headers mean the whole key space.
pub fn in_range(epk: &str, min: Option<&str>, max: Option<&str>) -> bool {
    if let Some(m) = min {
        if epk < m {
            return false;
        }
    }
    if let Some(m) = max {
        if !m.is_empty() && epk > m {
            return false;
        }
    }
    true
}
