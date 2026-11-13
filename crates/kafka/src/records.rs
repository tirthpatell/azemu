//! Record batch format v2 (magic 2): the batch header, CRC-32C, records with varints and headers, the attributes
//! word, and gzip. Other compression codecs are refused (`Unsupported`), as Event Hubs refuses them.
//!
//! Limits: a batch is at most as long as the bytes at hand, the record count and every length are checked against
//! the bytes left before anything is allocated, and the inflated size of a compressed batch is capped.

use std::io::Read;

use bytes::Bytes;

use crate::crc32c::crc32c;
use crate::wire::{Reader, Writer};

/// Bytes of the batch header from `baseOffset` to the record count: 8+4+4+1+4+2+4+8+8+8+2+4+4.
pub const BATCH_OVERHEAD: usize = 61;
/// `batchLength` counts from `partitionLeaderEpoch` on, so it is at least this much.
const MIN_BATCH_LENGTH: usize = BATCH_OVERHEAD - 12;

pub const COMPRESSION_MASK: i16 = 0x07;
pub const TIMESTAMP_TYPE: i16 = 0x08;
pub const TRANSACTIONAL: i16 = 0x10;
pub const CONTROL: i16 = 0x20;

#[derive(Debug, Clone, PartialEq)]
pub enum RecordsError {
    /// The bytes are not a valid batch (bad length, bad CRC, bad varint, ...): `CORRUPT_MESSAGE`.
    Corrupt(&'static str),
    /// A magic other than 2: `UNSUPPORTED_FOR_MESSAGE_FORMAT`.
    UnsupportedMagic(i8),
    /// A compression codec other than none and gzip: `UNSUPPORTED_FOR_MESSAGE_FORMAT`, like Event Hubs.
    UnsupportedCompression(i16),
    /// One batch is over a limit: its inflated bytes, or its record count (`MESSAGE_TOO_LARGE`).
    TooLarge,
    /// The batches of the whole request together are over the request's budget (`RECORD_LIST_TOO_LARGE`).
    RequestTooLarge,
}

/// What a request may cost, shared by every batch it carries. A batch is refused before its records are made, and
/// a compressed batch stops inflating as soon as its output passes the smaller of its own limit and what is left
/// of the request's, so the memory a request takes is bounded by these numbers, not by what the client claims.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Most bytes one batch's records may take once inflated.
    pub max_batch_bytes: usize,
    /// Most records one batch may hold.
    pub max_batch_records: usize,
    /// Bytes that all the batches of the request may still take, counting every record as `record_overhead` more.
    pub left: usize,
    /// What a stored record costs besides its bytes (the memory of the event made from it).
    pub record_overhead: usize,
}

impl Budget {
    pub fn new(max_batch_bytes: usize, max_batch_records: usize, request_total: usize, record_overhead: usize) -> Budget {
        Budget { max_batch_bytes, max_batch_records, left: request_total, record_overhead }
    }
    /// A budget that only caps the bytes one batch inflates to (no record or request limits).
    pub fn bytes_only(max_batch_bytes: usize) -> Budget {
        Budget { max_batch_bytes, max_batch_records: usize::MAX, left: usize::MAX, record_overhead: 0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub key: String,
    pub value: Option<Bytes>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Record {
    pub offset_delta: i32,
    pub timestamp_delta: i64,
    pub key: Option<Bytes>,
    pub value: Option<Bytes>,
    pub headers: Vec<Header>,
}

impl Record {
    /// Bytes the record's key, value and headers take: what counts against a message size limit.
    pub fn payload_size(&self) -> usize {
        self.key.as_ref().map_or(0, |k| k.len())
            + self.value.as_ref().map_or(0, |v| v.len())
            + self.headers.iter().map(|h| h.key.len() + h.value.as_ref().map_or(0, |v| v.len())).sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    pub base_offset: i64,
    pub partition_leader_epoch: i32,
    pub attributes: i16,
    pub last_offset_delta: i32,
    pub first_timestamp: i64,
    pub max_timestamp: i64,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub base_sequence: i32,
    pub records: Vec<Record>,
    /// Bytes the batch took on the wire.
    pub wire_len: usize,
}

impl Batch {
    pub fn compression(&self) -> i16 {
        self.attributes & COMPRESSION_MASK
    }
    pub fn is_transactional(&self) -> bool {
        self.attributes & TRANSACTIONAL != 0
    }
    pub fn is_control(&self) -> bool {
        self.attributes & CONTROL != 0
    }
}

/// Decode every batch in `data` (a Produce partition's records), charging each to `budget`. An incomplete batch at
/// the end is an error (a Produce request carries whole batches).
pub fn decode_batches(data: &[u8], budget: &mut Budget) -> Result<Vec<Batch>, RecordsError> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let (b, used) = decode_batch_with(&data[pos..], budget)?;
        out.push(b);
        pos += used;
    }
    Ok(out)
}

/// Decode the first batch of `data`; returns it and the bytes it took. `max_inflated` caps the bytes a compressed
/// batch may inflate to (no other limit).
pub fn decode_batch(data: &[u8], max_inflated: usize) -> Result<(Batch, usize), RecordsError> {
    decode_batch_with(data, &mut Budget::bytes_only(max_inflated))
}

/// [`decode_batch`] with a request budget.
pub fn decode_batch_with(data: &[u8], budget: &mut Budget) -> Result<(Batch, usize), RecordsError> {
    let mut r = Reader::new(data);
    let c = |_| RecordsError::Corrupt("batch header is cut short");
    let base_offset = r.i64().map_err(c)?;
    let batch_length = r.i32().map_err(c)?;
    if batch_length < 0 || (batch_length as usize) < MIN_BATCH_LENGTH {
        return Err(RecordsError::Corrupt("batch length is too small"));
    }
    let batch_length = batch_length as usize;
    if batch_length > r.remaining() {
        return Err(RecordsError::Corrupt("batch is longer than the bytes given"));
    }
    let wire_len = 12 + batch_length;
    let body = &data[12..wire_len];
    let mut r = Reader::new(body);
    let partition_leader_epoch = r.i32().map_err(c)?;
    let magic = r.i8().map_err(c)?;
    if magic != 2 {
        return Err(RecordsError::UnsupportedMagic(magic));
    }
    let crc = r.u32().map_err(c)?;
    if crc32c(&body[9..]) != crc {
        return Err(RecordsError::Corrupt("CRC-32C does not match"));
    }
    let attributes = r.i16().map_err(c)?;
    let last_offset_delta = r.i32().map_err(c)?;
    let first_timestamp = r.i64().map_err(c)?;
    let max_timestamp = r.i64().map_err(c)?;
    let producer_id = r.i64().map_err(c)?;
    let producer_epoch = r.i16().map_err(c)?;
    let base_sequence = r.i32().map_err(c)?;
    let count = r.i32().map_err(c)?;
    if count < 0 {
        return Err(RecordsError::Corrupt("negative record count"));
    }
    let compressed = r.rest();
    // the record count is checked before anything is inflated or allocated: a claim of a million records is refused
    // for what it would cost, whatever the bytes say
    let count_us = count as usize;
    if attributes & COMPRESSION_MASK == 0 && count_us > compressed.len() / 7 + 1 {
        return Err(RecordsError::Corrupt("record count over the bytes given"));
    }
    if count_us > budget.max_batch_records {
        return Err(RecordsError::TooLarge);
    }
    let count_cost = count_us.saturating_mul(budget.record_overhead);
    if count_cost > budget.left {
        return Err(RecordsError::RequestTooLarge);
    }
    // what the batch may inflate to: its own limit, and what the request has left once its records are paid for
    let room = (budget.left - count_cost).min(budget.max_batch_bytes);
    let inflated;
    let records_bytes: &[u8] = match attributes & COMPRESSION_MASK {
        0 => {
            if compressed.len() > room {
                return Err(if compressed.len() > budget.max_batch_bytes { RecordsError::TooLarge } else { RecordsError::RequestTooLarge });
            }
            compressed
        }
        1 => {
            inflated = gunzip(compressed, room, room < budget.max_batch_bytes)?;
            &inflated
        }
        other => return Err(RecordsError::UnsupportedCompression(other)),
    };
    // a record is at least 7 bytes (length, attributes, two deltas, key length, value length, header count)
    if count_us > records_bytes.len() / 7 + 1 {
        return Err(RecordsError::Corrupt("record count over the bytes given"));
    }
    budget.left -= count_cost + records_bytes.len();
    let mut rr = Reader::new(records_bytes);
    let mut records = Vec::with_capacity(count as usize);
    for _ in 0..count {
        records.push(decode_record(&mut rr)?);
    }
    if !rr.is_empty() {
        return Err(RecordsError::Corrupt("bytes left after the last record"));
    }
    Ok((
        Batch {
            base_offset,
            partition_leader_epoch,
            attributes,
            last_offset_delta,
            first_timestamp,
            max_timestamp,
            producer_id,
            producer_epoch,
            base_sequence,
            records,
            wire_len,
        },
        wire_len,
    ))
}

/// Inflate at most `max` bytes: the decoder is read through `take(max + 1)`, so it stops as soon as the output passes
/// the limit and a gzip bomb never grows past it. `request_limit` says which error the excess is.
fn gunzip(data: &[u8], max: usize, request_limit: bool) -> Result<Vec<u8>, RecordsError> {
    let mut out = Vec::with_capacity(data.len().saturating_mul(4).min(max).min(1 << 20));
    let mut d = flate2::read::GzDecoder::new(data).take(max as u64 + 1);
    d.read_to_end(&mut out).map_err(|_| RecordsError::Corrupt("gzip data is damaged"))?;
    if out.len() > max {
        return Err(if request_limit { RecordsError::RequestTooLarge } else { RecordsError::TooLarge });
    }
    Ok(out)
}

fn decode_record(r: &mut Reader<'_>) -> Result<Record, RecordsError> {
    let c = |_| RecordsError::Corrupt("record is cut short");
    let len = r.varint().map_err(c)?;
    if len < 0 || len as usize > r.remaining() {
        return Err(RecordsError::Corrupt("record length is wrong"));
    }
    let mut rr = Reader::new(r.take(len as usize).map_err(c)?);
    let _attributes = rr.i8().map_err(c)?;
    let timestamp_delta = rr.varlong().map_err(c)?;
    let offset_delta = rr.varint().map_err(c)?;
    let key = var_bytes(&mut rr)?;
    let value = var_bytes(&mut rr)?;
    let n = rr.varint().map_err(c)?;
    // a header is at least 2 bytes (two lengths)
    if n < 0 || (n as usize) > rr.remaining() / 2 {
        return Err(RecordsError::Corrupt("header count is wrong"));
    }
    let mut headers = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let kl = rr.varint().map_err(c)?;
        if kl < 0 {
            return Err(RecordsError::Corrupt("header key length is negative"));
        }
        let key = std::str::from_utf8(rr.take(kl as usize).map_err(c)?)
            .map_err(|_| RecordsError::Corrupt("header key is not UTF-8"))?
            .to_string();
        let value = var_bytes(&mut rr)?;
        headers.push(Header { key, value });
    }
    if !rr.is_empty() {
        return Err(RecordsError::Corrupt("bytes left in a record"));
    }
    Ok(Record { offset_delta, timestamp_delta, key, value, headers })
}

fn var_bytes(r: &mut Reader<'_>) -> Result<Option<Bytes>, RecordsError> {
    let c = |_| RecordsError::Corrupt("record is cut short");
    let n = r.varint().map_err(c)?;
    if n == -1 {
        return Ok(None);
    }
    if n < 0 {
        return Err(RecordsError::Corrupt("negative length in a record"));
    }
    Ok(Some(Bytes::copy_from_slice(r.take(n as usize).map_err(c)?)))
}

fn put_var_bytes(w: &mut Writer, b: Option<&Bytes>) {
    match b {
        None => w.varint(-1),
        Some(b) => {
            w.varint(b.len() as i32);
            w.raw(b);
        }
    }
}

/// One record in the wire format (length prefix included).
pub fn encode_record(rec: &Record) -> Vec<u8> {
    let mut body = Writer::with_capacity(32 + rec.payload_size());
    body.i8(0);
    body.varlong(rec.timestamp_delta);
    body.varint(rec.offset_delta);
    put_var_bytes(&mut body, rec.key.as_ref());
    put_var_bytes(&mut body, rec.value.as_ref());
    body.varint(rec.headers.len() as i32);
    for h in &rec.headers {
        body.varint(h.key.len() as i32);
        body.raw(h.key.as_bytes());
        put_var_bytes(&mut body, h.value.as_ref());
    }
    let mut out = Writer::with_capacity(body.len() + 5);
    out.varint(body.len() as i32);
    out.raw(&body.buf);
    out.into_vec()
}

/// Write a batch (no compression unless `gzip`), with the CRC. Offsets and timestamps in `b` are used as given;
/// `b.last_offset_delta`, `first_timestamp`, `max_timestamp` are taken from `b`, not computed.
pub fn encode_batch(b: &Batch, gzip: bool) -> Vec<u8> {
    let mut recs = Vec::new();
    for r in &b.records {
        recs.extend_from_slice(&encode_record(r));
    }
    encode_batch_raw(b, b.records.len() as i32, recs, gzip)
}

/// [`encode_batch`] for records that are already encoded (`recs`, `count` of them): the header fields come from `b`
/// (its own records are ignored). For tests that need a batch with more records than should be held in memory as structs.
pub fn encode_batch_raw(b: &Batch, count: i32, mut recs: Vec<u8>, gzip: bool) -> Vec<u8> {
    let mut attributes = b.attributes & !COMPRESSION_MASK;
    if gzip {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = e.write_all(&recs);
        recs = e.finish().unwrap_or_default();
        attributes |= 1;
    }
    let mut after_crc = Writer::with_capacity(recs.len() + 40);
    after_crc.i16(attributes);
    after_crc.i32(b.last_offset_delta);
    after_crc.i64(b.first_timestamp);
    after_crc.i64(b.max_timestamp);
    after_crc.i64(b.producer_id);
    after_crc.i16(b.producer_epoch);
    after_crc.i32(b.base_sequence);
    after_crc.i32(count);
    after_crc.raw(&recs);
    let mut w = Writer::with_capacity(after_crc.len() + 21);
    w.i64(b.base_offset);
    w.i32((after_crc.len() + 9) as i32); // partitionLeaderEpoch + magic + crc + the rest
    w.i32(b.partition_leader_epoch);
    w.i8(2);
    w.u32(crc32c(&after_crc.buf));
    w.raw(&after_crc.buf);
    w.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: i32, key: Option<&str>, value: Option<&str>, headers: &[(&str, Option<&str>)]) -> Record {
        Record {
            offset_delta: i,
            timestamp_delta: i as i64 * 3,
            key: key.map(|k| Bytes::copy_from_slice(k.as_bytes())),
            value: value.map(|k| Bytes::copy_from_slice(k.as_bytes())),
            headers: headers
                .iter()
                .map(|(k, v)| Header { key: k.to_string(), value: v.map(|v| Bytes::copy_from_slice(v.as_bytes())) })
                .collect(),
        }
    }

    fn batch() -> Batch {
        Batch {
            base_offset: 42,
            partition_leader_epoch: -1,
            attributes: 0,
            last_offset_delta: 2,
            first_timestamp: 1_700_000_000_000,
            max_timestamp: 1_700_000_000_006,
            producer_id: 7,
            producer_epoch: 1,
            base_sequence: 10,
            records: vec![
                rec(0, Some("k"), Some("hello"), &[("h1", Some("v1")), ("h2", None)]),
                rec(1, None, Some("world"), &[]),
                rec(2, Some("k"), None, &[]),
            ],
            wire_len: 0,
        }
    }

    #[test]
    fn round_trip_plain_and_gzip() {
        for gz in [false, true] {
            let wire = encode_batch(&batch(), gz);
            let (b, used) = decode_batch(&wire, 1 << 20).unwrap();
            assert_eq!(used, wire.len());
            let mut want = batch();
            want.wire_len = wire.len();
            want.attributes = if gz { 1 } else { 0 };
            assert_eq!(b, want);
        }
    }

    /// A batch of one record, written out by hand from the format description (KIP-98 / the Kafka protocol guide):
    /// baseOffset 0, length, epoch -1, magic 2, crc, attributes 0, lastOffsetDelta 0, timestamps 0, producer -1/-1/-1,
    /// one record: key null, value "a", no headers.
    #[test]
    fn hand_built_batch() {
        let rec_body = [0u8, 0, 0, 1, 2, b'a', 0]; // attributes, tsDelta 0, offsetDelta 0, key -1, value len 1, 'a', headers 0
        let mut record = vec![(rec_body.len() as u8) << 1];
        record.extend_from_slice(&rec_body);
        let mut after_crc = Vec::new();
        after_crc.extend_from_slice(&0i16.to_be_bytes());
        after_crc.extend_from_slice(&0i32.to_be_bytes());
        after_crc.extend_from_slice(&0i64.to_be_bytes());
        after_crc.extend_from_slice(&0i64.to_be_bytes());
        after_crc.extend_from_slice(&(-1i64).to_be_bytes());
        after_crc.extend_from_slice(&(-1i16).to_be_bytes());
        after_crc.extend_from_slice(&(-1i32).to_be_bytes());
        after_crc.extend_from_slice(&1i32.to_be_bytes());
        after_crc.extend_from_slice(&record);
        let mut wire = Vec::new();
        wire.extend_from_slice(&0i64.to_be_bytes());
        wire.extend_from_slice(&((after_crc.len() + 9) as i32).to_be_bytes());
        wire.extend_from_slice(&(-1i32).to_be_bytes());
        wire.push(2);
        wire.extend_from_slice(&crc32c(&after_crc).to_be_bytes());
        wire.extend_from_slice(&after_crc);
        let (b, _) = decode_batch(&wire, 1024).unwrap();
        assert_eq!(b.records.len(), 1);
        assert_eq!(b.records[0].key, None);
        assert_eq!(b.records[0].value.as_deref(), Some(&b"a"[..]));
        assert_eq!(b.producer_id, -1);
        assert_eq!(encode_batch(&b, false), wire);
    }

    #[test]
    fn corruption_is_caught() {
        let wire = encode_batch(&batch(), false);
        let mut bad = wire.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(decode_batch(&bad, 1 << 20).unwrap_err(), RecordsError::Corrupt("CRC-32C does not match"));
        assert!(matches!(decode_batch(&wire[..wire.len() - 1], 1 << 20), Err(RecordsError::Corrupt(_))));
        assert!(matches!(decode_batches(&wire[..10], &mut Budget::bytes_only(1 << 20)), Err(RecordsError::Corrupt(_))));
        let mut magic1 = wire.clone();
        magic1[16] = 1;
        assert_eq!(decode_batch(&magic1, 1 << 20).unwrap_err(), RecordsError::UnsupportedMagic(1));
    }

    #[test]
    fn other_codecs_are_refused() {
        let mut b = batch();
        b.attributes = 2; // snappy: the bytes are not snappy, but the codec is refused first
        let wire = encode_batch(&b, false);
        // encode_batch clears the codec bits; set them in place and fix the CRC
        let mut w = wire.clone();
        w[21 + 1] |= 2;
        let crc = crc32c(&w[21..]);
        w[17..21].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode_batch(&w, 1 << 20).unwrap_err(), RecordsError::UnsupportedCompression(2));
    }

    #[test]
    fn gzip_bomb_is_capped() {
        let mut b = batch();
        b.records = vec![Record { value: Some(Bytes::from(vec![0u8; 4 << 20])), ..Default::default() }];
        let wire = encode_batch(&b, true);
        assert!(wire.len() < 20_000, "zeros compress well: {}", wire.len());
        assert_eq!(decode_batch(&wire, 1 << 20).unwrap_err(), RecordsError::TooLarge);
        assert!(decode_batch(&wire, 8 << 20).is_ok());
    }

    fn tiny_records(n: usize) -> Batch {
        Batch { records: (0..n).map(|i| Record { offset_delta: i as i32, ..Default::default() }).collect(), ..batch() }
    }

    #[test]
    fn a_gzip_batch_stops_inflating_at_the_limit_not_after_it() {
        // 2 million empty records: 14 MB inflated, a few KB on the wire. With a 1 MiB limit the decoder must give up
        // after about 1 MiB (the `take`), which the error alone cannot show; the count check refuses it even earlier.
        let wire = encode_batch_raw(&batch(), 2_000_000, encode_record(&Record::default()).repeat(2_000_000), true);
        assert!(wire.len() < 100_000, "{}", wire.len());
        let mut budget = Budget::new(1 << 20, 50_000, 16 << 20, 256);
        assert_eq!(decode_batch_with(&wire, &mut budget).unwrap_err(), RecordsError::TooLarge);
        assert_eq!(budget.left, 16 << 20, "a refused batch is not charged");
        // without a record limit the byte limit does it
        let mut budget = Budget::new(1 << 20, usize::MAX, usize::MAX, 0);
        assert_eq!(decode_batch_with(&wire, &mut budget).unwrap_err(), RecordsError::TooLarge);
    }

    #[test]
    fn records_are_charged_their_overhead_and_the_request_has_one_budget() {
        let wire = encode_batch(&tiny_records(1000), true);
        // each batch costs 1000 * 256 + 7000: three fit in 800 KB, the fourth does not
        let mut budget = Budget::new(1 << 20, 50_000, 800_000, 256);
        for _ in 0..3 {
            assert!(decode_batch_with(&wire, &mut budget).is_ok());
        }
        assert_eq!(decode_batch_with(&wire, &mut budget).unwrap_err(), RecordsError::RequestTooLarge);
        assert!(budget.left < 800_000 - 3 * 256_000);
        // a plain (not compressed) batch is charged the same
        let wire = encode_batch(&tiny_records(1000), false);
        let mut budget = Budget::new(1 << 20, 50_000, 300_000, 256);
        assert!(decode_batch_with(&wire, &mut budget).is_ok());
        assert_eq!(decode_batch_with(&wire, &mut budget).unwrap_err(), RecordsError::RequestTooLarge);
    }

    #[test]
    fn a_batch_over_the_record_limit_is_too_large() {
        let wire = encode_batch(&tiny_records(101), false);
        assert_eq!(decode_batch_with(&wire, &mut Budget::new(1 << 20, 100, 1 << 30, 0)).unwrap_err(), RecordsError::TooLarge);
        assert!(decode_batch_with(&wire, &mut Budget::new(1 << 20, 101, 1 << 30, 0)).is_ok());
    }

    #[test]
    fn counts_and_lengths_are_checked_before_allocating() {
        let wire = encode_batch(&batch(), false);
        // record count claims 2 billion records
        let mut w = wire.clone();
        w[57..61].copy_from_slice(&i32::MAX.to_be_bytes());
        let crc = crc32c(&w[21..]);
        w[17..21].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(decode_batch(&w, 1 << 20), Err(RecordsError::Corrupt(_))));
        // batch length claims far more than the bytes
        let mut w = wire.clone();
        w[8..12].copy_from_slice(&i32::MAX.to_be_bytes());
        assert!(matches!(decode_batch(&w, 1 << 20), Err(RecordsError::Corrupt(_))));
    }
}
