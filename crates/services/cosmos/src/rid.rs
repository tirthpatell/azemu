//! Resource ids in the real Cosmos layout: base64 of the bytes of a hierarchical id, in the
//! alphabet Cosmos uses (`-` stands in for `/`, so an id can sit in a URL path).
//!
//! The Java SDK parses every `_rid` (`com.azure.cosmos.implementation.ResourceId.tryParse`,
//! the same layout as the .NET `ResourceId`), so the bytes must follow it:
//!
//! | resource | bytes |
//! |---|---|
//! | database | 4: database number (big endian) |
//! | collection (container) | 8: database id, then 4 with the high bit of the first byte SET |
//! | user | 8: database id, then 4 with that high bit CLEAR |
//! | permission | 16: user id, then 8 |
//! | document | 16: collection id, then 8 whose last byte has 0x0 in its high four bits |
//! | stored procedure, trigger, UDF, conflict, partition key range | 16: collection id, then 8 whose last byte carries 0x8, 0x7, 0x6, 0x4, 0x5 in its high four bits |
//! | offer | 3 bytes (4 characters) |
//!
//! The counter that makes an id unique sits in the first bytes of the last eight (little endian),
//! so the type nibble in byte 15 never collides with it.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// The text of a resource id: standard base64 with `-` in place of `/`.
pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes).replace('/', "-")
}

/// What an id names below a container or user (the high four bits of the last byte).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Document,
    StoredProcedure,
    Trigger,
    Udf,
    /// No conflicts exist (one write region), kept so the whole layout is here.
    #[allow(dead_code)]
    Conflict,
    PartitionKeyRange,
    /// Under a user: eight bytes, no type nibble.
    Permission,
    /// Under a database: four bytes with the collection bit clear.
    User,
}

impl Kind {
    fn nibble(self) -> u8 {
        match self {
            Kind::Document | Kind::Permission | Kind::User => 0x0,
            Kind::StoredProcedure => 0x8,
            Kind::Trigger => 0x7,
            Kind::Udf => 0x6,
            Kind::Conflict => 0x4,
            Kind::PartitionKeyRange => 0x5,
        }
    }
}

/// Ids stay free of `+`: it is legal in a path, but some HTTP stacks turn it into a space.
fn clean(s: &str) -> bool {
    !s.contains('+')
}

/// The first counter value at or after `n` whose id text is clean.
fn pick(prefix: &[u8], n: u64, tag: impl Fn(&mut Vec<u8>, u64)) -> (Vec<u8>, u64) {
    let mut c = n;
    loop {
        let mut bytes = prefix.to_vec();
        tag(&mut bytes, c);
        if clean(&encode(&bytes)) {
            return (bytes, c);
        }
        c += 1;
    }
}

fn db_tag(b: &mut Vec<u8>, n: u64) {
    // three varying bytes and a zero, like real database ids ("kDBZAA==")
    let x = (n as u32).wrapping_mul(2654435761) & 0x00FF_FFFF | 0x0040_0000;
    b.extend_from_slice(&[(x >> 16) as u8, (x >> 8) as u8, x as u8, 0]);
}

/// Four bytes; `collection` sets the high bit of the first byte (the SDK's test for a collection).
fn word_tag(b: &mut Vec<u8>, n: u64, collection: bool) {
    let x = (n as u32).wrapping_mul(2246822519);
    let first = if collection { 0x80 | (x as u8 & 0x7F) } else { x as u8 & 0x7F };
    b.extend_from_slice(&[first, (x >> 8) as u8, (x >> 16) as u8, (x >> 24) as u8]);
}

/// New database id bytes for counter `n`; returns the bytes and the counter value used.
pub fn database(n: u64) -> (Vec<u8>, u64) {
    pick(&[], n, db_tag)
}

/// Container id under a database.
pub fn collection(db: &[u8], n: u64) -> (Vec<u8>, u64) {
    pick(db, n, |b, c| word_tag(b, c, true))
}

/// User id under a database.
pub fn user(db: &[u8], n: u64) -> (Vec<u8>, u64) {
    pick(db, n, |b, c| word_tag(b, c, false))
}

/// Document, script, partition key range, conflict or permission id under `parent`
/// (a container, or a user for a permission).
pub fn child_of(parent: &[u8], n: u64, kind: Kind) -> (Vec<u8>, u64) {
    if kind == Kind::User {
        return user(parent, n);
    }
    pick(parent, n, |b, c| {
        let mut eight = (c & 0x00FF_FFFF_FFFF_FFFF).to_le_bytes();
        eight[7] = kind.nibble() << 4;
        b.extend_from_slice(&eight);
    })
}

/// Document id under a container.
pub fn child(parent: &[u8], n: u64) -> (Vec<u8>, u64) {
    child_of(parent, n, Kind::Document)
}

/// Offer id bytes: three bytes, which the SDK reads as an offer (a database id has four).
pub fn offer(n: u64) -> (Vec<u8>, u64) {
    pick(&[], n, |b, c| {
        let x = (c as u32).wrapping_mul(40503) & 0x00FF_FFFF;
        b.extend_from_slice(&[(x >> 16) as u8, (x >> 8) as u8, x as u8]);
    })
}

/// Partition key range id bytes under a container (`n` is the range number).
pub fn pkrange(coll: &[u8], n: u64) -> Vec<u8> {
    let mut b = coll.to_vec();
    let mut eight = (n & 0x00FF_FFFF_FFFF_FFFF).to_le_bytes();
    eight[7] = Kind::PartitionKeyRange.nibble() << 4;
    b.extend_from_slice(&eight);
    b
}

/// A rid as it may appear in a URL: `-` or `/` (as a client may have decoded it) stands for the
/// 63rd symbol; both are the same id.
pub fn matches(rid: &str, seg: &str) -> bool {
    rid == seg || rid == seg.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules of the Java `ResourceId.tryParse`, written out again.
    fn java_kind(bytes: &[u8]) -> &'static str {
        if bytes.len() == 3 {
            return "offer";
        }
        assert!(bytes.len().is_multiple_of(4) && bytes.len() <= 20, "length {}", bytes.len());
        if bytes.len() == 4 {
            return "database";
        }
        let is_collection = bytes[4] & 0x80 != 0;
        if !is_collection {
            return match bytes.len() {
                8 => "user",
                16 => "permission",
                n => panic!("bad user length {n}"),
            };
        }
        match bytes.len() {
            8 => "collection",
            16 | 20 => {
                let t = bytes[15] as i8 >> 4;
                match t {
                    0 => "document",
                    -8 | 8 => "stored procedure",
                    7 => "trigger",
                    6 => "udf",
                    4 => "conflict",
                    5 => "partition key range",
                    other => panic!("bad type nibble {other}"),
                }
            }
            n => panic!("bad collection child length {n}"),
        }
    }

    #[test]
    fn every_kind_parses_like_the_java_sdk() {
        let (db, _) = database(1);
        let (coll, _) = collection(&db, 1);
        let (user_id, _) = user(&db, 1);
        assert_eq!(java_kind(&db), "database");
        assert_eq!(java_kind(&coll), "collection");
        assert_eq!(java_kind(&user_id), "user");
        assert_eq!(java_kind(&child_of(&user_id, 1, Kind::Permission).0), "permission");
        for (kind, name) in [
            (Kind::Document, "document"),
            (Kind::StoredProcedure, "stored procedure"),
            (Kind::Trigger, "trigger"),
            (Kind::Udf, "udf"),
            (Kind::Conflict, "conflict"),
            (Kind::PartitionKeyRange, "partition key range"),
        ] {
            for n in 1..200u64 {
                assert_eq!(java_kind(&child_of(&coll, n, kind).0), name);
            }
        }
        assert_eq!(java_kind(&pkrange(&coll, 0)), "partition key range");
        for n in 1..200u64 {
            assert_eq!(java_kind(&offer(n).0), "offer");
            assert_eq!(java_kind(&collection(&db, n).0), "collection");
            assert_eq!(java_kind(&user(&db, n).0), "user");
        }
    }

    #[test]
    fn ids_are_unique_and_use_the_cosmos_alphabet() {
        let (db, _) = database(1);
        let mut seen = std::collections::HashSet::new();
        let mut counter = 1;
        for _ in 0..3000 {
            let (b, used) = collection(&db, counter);
            counter = used + 1;
            let s = encode(&b);
            assert!(!s.contains('/') && !s.contains('+'));
            assert!(seen.insert(s.clone()));
            assert_eq!(STANDARD.decode(s.replace('-', "/")).ok().as_deref(), Some(&b[..]));
        }
        // a byte pattern that is "/" in standard base64 comes out as "-"
        assert_eq!(encode(&[0xFF, 0xFF, 0xFF]), "----");
        assert!(matches("a-b-", "a/b/"));
    }
}
