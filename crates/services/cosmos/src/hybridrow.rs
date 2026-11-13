//! The binary body of a batch request, as the .NET SDK sends it (`TransactionalBatch`, and every item call of a
//! client with `AllowBulkExecution = true`), and the same format for the answer.
//!
//! This is HybridRow, Microsoft's row format, in the two fixed schemas of the batch API (`BatchOperation`
//! and `BatchResult`, from `Microsoft.Azure.Cosmos/src/Batch/HybridRowBatchSchemas.json` in the .NET SDK) inside
//! a "record IO" stream. Nothing else of HybridRow is built: no other schema is read or written, so the
//! module is only the layout of those two rows, worked out from the schema file and checked byte for byte
//! against what the real library wrote and read (the tests hold bytes captured from `Microsoft.Azure.Cosmos` 3.64.0
//! and written by `Microsoft.Azure.Cosmos.Serialization.HybridRow`).
//!
//! ```text
//! stream   = segment record*
//! segment  = 81 | F0 D8 FF 7F | 01 | 0A 00 00 00        a row of the "Segment" schema: its length is the row's own (10)
//! record   = 81 | F1 D8 FF 7F | length u32 | crc32 u32 | body[length]       a "Record" header row, then the body
//! body     = a row of the batch schema, below; crc32 is the usual CRC-32 (IEEE) of the body
//! row      = 81 | schema id u32 | fixed region (9 bytes) | variable columns | sparse fields
//! ```
//!
//! All numbers are little endian. The fixed region is one byte of "present" bits (bit n = column n of the layout
//! has a value) and two int32. Variable columns follow in layout order, only those whose bit is set, each a
//! length (7 bit groups, low group first, the high bit means "more") and the bytes. Sparse fields come last,
//! in any order: a type code, the field's number (its position in the schema, from 1) as a length like the above,
//! then the value (`Utf8` and `Binary`: a length and the bytes; `Int32`, `UInt32`: 4 bytes; `Float64`: 8; `Boolean`: the
//! type code is `Boolean` (3) for true and `BooleanFalse` (2) for false and there is nothing more).
//!
//! `BatchOperation` (schema id 2145473648, columns in order): operationType int32 (fixed), resourceType int32 (fixed),
//! partitionKey utf8, effectivePartitionKey binary, id utf8, binaryId binary, resourceBody binary (variable),
//! indexingDirective utf8, ifMatch utf8, ifNoneMatch utf8, timeToLiveInSeconds int32, minimalReturnPreference bool (sparse).
//! `BatchResult` (2145473649): statusCode int32, subStatusCode int32 (fixed), eTag utf8, resourceBody binary (variable),
//! retryAfterMilliseconds uint32, requestCharge float64 (sparse).

use serde_json::{json, Map, Value};

const ROW_V1: u8 = 0x81;
const SEGMENT_SCHEMA: u32 = 0x7FFF_D8F0;
const RECORD_SCHEMA: u32 = 0x7FFF_D8F1;
const OPERATION_SCHEMA: u32 = 2_145_473_648;
const RESULT_SCHEMA: u32 = 2_145_473_649;

const CODE_BOOL_FALSE: u8 = 0x02;
const CODE_BOOL: u8 = 0x03;
const CODE_INT32: u8 = 0x07;
const CODE_UINT32: u8 = 0x0B;
const CODE_FLOAT64: u8 = 0x10;
const CODE_UTF8: u8 = 0x14;

/// Size of a row header (version byte and schema id) and of the fixed region of both batch schemas.
const HEADER: usize = 5;
const FIXED: usize = 9;

/// Sparse field numbers of `BatchOperation` (position in the schema, from 1).
const F_INDEXING_DIRECTIVE: u64 = 8;
const F_IF_MATCH: u64 = 9;
const F_IF_NONE_MATCH: u64 = 10;
const F_TTL: u64 = 11;
const F_MINIMAL_RETURN: u64 = 12;
/// Sparse field numbers of `BatchResult`.
const F_RETRY_AFTER: u8 = 5;
const F_REQUEST_CHARGE: u8 = 6;

/// A batch request body in this format starts with the row version byte; a JSON body starts with `[`, a Cosmos binary
/// JSON body with `0x80`.
pub fn is_hybrid_row(body: &[u8]) -> bool {
    body.first() == Some(&ROW_V1)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

type Res<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }

    fn left(&self) -> usize {
        self.b.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        if n > self.left() {
            return Err("the body ends inside a record".into());
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Res<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Res<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn i32(&mut self) -> Res<i32> {
        Ok(self.u32()? as i32)
    }

    /// A length or field number: 7 bit groups, low group first.
    fn varint(&mut self) -> Res<u64> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = self.u8()?;
            v |= u64::from(b & 0x7F) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("a length is longer than 10 bytes".into())
    }

    fn sized(&mut self) -> Res<&'a [u8]> {
        let n = self.varint()?;
        let n = usize::try_from(n).map_err(|_| "a length is too large".to_string())?;
        self.take(n)
    }

    fn text(&mut self) -> Res<String> {
        String::from_utf8(self.sized()?.to_vec()).map_err(|_| "a string field is not UTF-8".to_string())
    }
}

fn crc32(data: &[u8]) -> u32 {
    // the usual CRC-32 (IEEE, reflected polynomial 0xEDB88320), bitwise: records are small
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// What the .NET SDK writes as `operationType`.
fn operation_name(n: i32) -> String {
    match n {
        0 => "create".into(),
        1 => "patch".into(),
        2 => "read".into(),
        4 => "delete".into(),
        5 => "replace".into(),
        20 => "upsert".into(),
        other => format!("operation {other}"),
    }
}

/// Reads a batch request body into the operations of the JSON form (`operationType` as a word, `id`,
/// `partitionKey` as a string holding a JSON array, `resourceBody` as JSON, `ifMatch`, `ifNoneMatch`,
/// `indexingDirective`, `minimalReturnPreference`), at most `max_ops` of them (the caller refuses a request that has
/// more). An error is the message for a 400.
pub fn decode_operations(body: &[u8], max_ops: usize) -> Res<Vec<Value>> {
    let mut r = Reader::new(body);
    // the segment: a row whose second field is its own length
    let start = r.pos;
    if r.u8()? != ROW_V1 || r.u32()? != SEGMENT_SCHEMA {
        return Err("the body is not a HybridRow record stream (no segment header)".into());
    }
    let _present = r.u8()?;
    let seg_len = r.u32()? as usize;
    if seg_len < r.pos - start || seg_len > r.left() + (r.pos - start) {
        return Err("the segment header has an invalid length".into());
    }
    r.pos = start + seg_len;
    let mut ops = Vec::new();
    while r.left() > 0 {
        if ops.len() > max_ops {
            break;
        }
        if r.u8()? != ROW_V1 || r.u32()? != RECORD_SCHEMA {
            return Err("a record header is not valid".into());
        }
        let len = r.u32()? as usize;
        let crc = r.u32()?;
        let rec = r.take(len)?;
        if crc32(rec) != crc {
            return Err("a record has a wrong checksum".into());
        }
        ops.push(decode_operation(rec)?);
    }
    Ok(ops)
}

fn decode_operation(rec: &[u8]) -> Res<Value> {
    let mut r = Reader::new(rec);
    if r.u8()? != ROW_V1 || r.u32()? != OPERATION_SCHEMA {
        return Err("a record is not a batch operation".into());
    }
    let present = r.u8()?;
    let op_type = r.i32()?;
    let _resource_type = r.i32()?;
    let mut m = Map::new();
    if present & 1 != 0 {
        m.insert("operationType".into(), json!(operation_name(op_type)));
    }
    // variable columns, in layout order: partitionKey, effectivePartitionKey, id, binaryId, resourceBody
    for bit in 2..=6u8 {
        if present & (1 << bit) == 0 {
            continue;
        }
        match bit {
            2 => {
                m.insert("partitionKey".into(), json!(r.text()?));
            }
            3 | 5 => {
                r.sized()?; // effectivePartitionKey and binaryId: not used
            }
            4 => {
                m.insert("id".into(), json!(r.text()?));
            }
            _ => {
                let raw = r.sized()?;
                let v: Value = serde_json::from_slice(raw).map_err(|_| "the resourceBody of an operation is not valid JSON".to_string())?;
                m.insert("resourceBody".into(), v);
            }
        }
    }
    // sparse fields
    while r.left() > 0 {
        let code = r.u8()?;
        let field = r.varint()?;
        let name = match (field, code) {
            (F_INDEXING_DIRECTIVE, CODE_UTF8) => "indexingDirective",
            (F_IF_MATCH, CODE_UTF8) => "ifMatch",
            (F_IF_NONE_MATCH, CODE_UTF8) => "ifNoneMatch",
            (F_TTL, CODE_INT32) => "timeToLiveInSeconds",
            (F_MINIMAL_RETURN, CODE_BOOL | CODE_BOOL_FALSE) => "minimalReturnPreference",
            _ => return Err(format!("an operation has an unknown field {field} of type {code}")),
        };
        let v = match code {
            CODE_UTF8 => json!(r.text()?),
            CODE_INT32 => json!(r.i32()?),
            _ => json!(code == CODE_BOOL),
        };
        m.insert(name.into(), v);
    }
    Ok(Value::Object(m))
}

fn put_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One `BatchResult` row from the JSON result object the batch code builds
/// (`statusCode`, `subStatusCode`, `eTag`, `resourceBody`, `requestCharge`, `retryAfterMilliseconds`).
fn encode_result(r: &Value) -> Vec<u8> {
    let status = r.get("statusCode").and_then(Value::as_i64).unwrap_or(0) as i32;
    let sub = r.get("subStatusCode").and_then(Value::as_i64).unwrap_or(0) as i32;
    let etag = r.get("eTag").and_then(Value::as_str);
    let body = r.get("resourceBody").and_then(|b| serde_json::to_vec(b).ok());
    let mut present = 0b11u8;
    if etag.is_some() {
        present |= 1 << 2;
    }
    if body.is_some() {
        present |= 1 << 3;
    }
    let mut row = vec![ROW_V1];
    row.extend_from_slice(&RESULT_SCHEMA.to_le_bytes());
    row.push(present);
    row.extend_from_slice(&status.to_le_bytes());
    row.extend_from_slice(&sub.to_le_bytes());
    if let Some(e) = etag {
        put_varint(&mut row, e.len());
        row.extend_from_slice(e.as_bytes());
    }
    if let Some(b) = &body {
        put_varint(&mut row, b.len());
        row.extend_from_slice(b);
    }
    if let Some(ms) = r.get("retryAfterMilliseconds").and_then(Value::as_u64) {
        row.push(CODE_UINT32);
        row.push(F_RETRY_AFTER);
        row.extend_from_slice(&(ms as u32).to_le_bytes());
    }
    if let Some(c) = r.get("requestCharge").and_then(Value::as_f64) {
        row.push(CODE_FLOAT64);
        row.push(F_REQUEST_CHARGE);
        row.extend_from_slice(&c.to_le_bytes());
    }
    debug_assert!(row.len() >= HEADER + FIXED);
    row
}

/// The answer body: a segment and one record per result.
pub fn encode_results(results: &[Value]) -> Vec<u8> {
    let mut out = vec![ROW_V1];
    out.extend_from_slice(&SEGMENT_SCHEMA.to_le_bytes());
    out.push(1);
    out.extend_from_slice(&10u32.to_le_bytes());
    for r in results {
        let row = encode_result(r);
        out.push(ROW_V1);
        out.extend_from_slice(&RECORD_SCHEMA.to_le_bytes());
        out.extend_from_slice(&(row.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32(&row).to_le_bytes());
        out.extend_from_slice(&row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bodies the .NET SDK (`Microsoft.Azure.Cosmos` 3.64.0, gateway mode) sent, captured on the wire.
    const BULK_CREATE: &str = concat!(
        "81f0d8ff7f010a00000081f1d8ff7f33000000e3883fc7817054e17f470000000002000000065b227062225d1d7b2269",
        "64223a226231222c22706b223a227062222c2276223a2278227d",
    );
    const BULK_DELETE: &str =
        concat!("81f0d8ff7f010a00000081f1d8ff7f1e0000006bb673c2817054e17f170400000002000000065b227062225d02623114", "0903227122",);
    const BATCH_ALL: &str = concat!(
        "81f0d8ff7f010a00000081f1d8ff7f2a0000004f980a40817054e17f4300000000020000001b7b226964223a22693122",
        "2c22706b223a227031222c226e223a317d81f1d8ff7f320000008ad61f5a817054e17f4314000000020000001b7b2269",
        "64223a226932222c22706b223a227031222c226e223a327d140905226162632281f1d8ff7f3f000000c7e27947817054",
        "e17f5305000000020000000269311b7b226964223a226931222c22706b223a227031222c226e223a357d140807457863",
        "6c756465140a052278797a2281f1d8ff7f110000000c787372817054e17f13020000000200000002693181f1d8ff7f11",
        "000000f5e3cf03817054e17f13040000000200000002693281f1d8ff7f69000000a53269cf817054e17f530100000002",
        "000000026931577b226f7065726174696f6e73223a5b7b226f70223a22736574222c2270617468223a222f6e222c2276",
        "616c7565223a397d2c7b226f70223a22696e6372222c2270617468223a222f6e222c2276616c7565223a317d5d7d81f1",
        "d8ff7f300000006310d33e817054e17f4300000000020000001f7b226964223a226933222c22706b223a227031222c22",
        "c3a9223a22c3bc227d030c",
    );
    const BULK_BIG: &str = concat!(
        "81f0d8ff7f010a00000081f1d8ff7f63010000ed9db8e0817054e17f470000000002000000065b227062225dcc027b22",
        "6964223a226269672dc3a9222c22706b223a227062222c2276223a227979797979797979797979797979797979797979",
        "797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979",
        "797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979",
        "797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979",
        "797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979",
        "797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979797979",
        "79797979797979797979797979797979797979797979797979797979797979797979797979797979227d",
    );
    const BULK_NO_CONTENT: &str = concat!(
        "81f0d8ff7f010a00000081f1d8ff7f2d0000008eff2b1e817054e17f471400000002000000065b227062225d157b2269",
        "64223a226232222c22706b223a227062227d030c",
    );
    /// Six `BatchResult` records written by `Microsoft.Azure.Cosmos.Serialization.HybridRow` (`RowWriter` and
    /// `WriteRecordIOAsync`) for the results listed in `golden_results` below.
    const GOLDEN_RESULTS: &str = concat!(
        "81f0d8ff7f010a00000081f1d8ff7f42000000825f9450817154e17f0fc9000000000000000822657461672d3122207b",
        "225f65746167223a225c22657461672d315c22222c226964223a226931227d1006d7a3703d0ad7164081f1d8ff7f1800",
        "0000a9a7d414817154e17f0394010000eb0300001006000000000000f03f81f1d8ff7f18000000e3c14e24817154e17f",
        "03a8010000000000001006000000000000000081f1d8ff7f14000000899e6150817154e17f03ad010000800c00000b05",
        "fa00000081f1d8ff7f4a010000c9329fab817154e17f0fc80000000000000003226522ac022278787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878787878",
        "221006000000000000f43f81f1d8ff7f1800000060e5bcea817154e17f03cc000000000000001006d7a3703d0ad71640",
    );

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn requests_from_the_dotnet_sdk_are_read() {
        // Bulk executor, one create: its own partition key, the body is the JSON text of the item.
        assert_eq!(
            decode_operations(&hex(BULK_CREATE), 100).unwrap(),
            vec![json!({"operationType": "create", "partitionKey": "[\"pb\"]", "resourceBody": {"id": "b1", "pk": "pb", "v": "x"}})]
        );
        // a delete with an ETag condition
        assert_eq!(
            decode_operations(&hex(BULK_DELETE), 100).unwrap(),
            vec![json!({"operationType": "delete", "partitionKey": "[\"pb\"]", "id": "b1", "ifMatch": "\"q\""})]
        );
        // an upsert with EnableContentResponseOnWrite = false: the sparse boolean
        assert_eq!(
            decode_operations(&hex(BULK_NO_CONTENT), 100).unwrap(),
            vec![
                json!({"operationType": "upsert", "partitionKey": "[\"pb\"]", "resourceBody": {"id": "b2", "pk": "pb"}, "minimalReturnPreference": true})
            ]
        );
        // a body longer than 127 bytes: a length of two groups, and a non ASCII id
        let big = decode_operations(&hex(BULK_BIG), 100).unwrap();
        assert_eq!(big[0]["resourceBody"]["id"], "big-\u{e9}");
        assert_eq!(big[0]["resourceBody"]["v"], "y".repeat(300));
        // a transactional batch with every operation type, conditions, an indexing directive and a patch
        let ops = decode_operations(&hex(BATCH_ALL), 100).unwrap();
        let types: Vec<&str> = ops.iter().map(|o| o["operationType"].as_str().unwrap()).collect();
        assert_eq!(types, ["create", "upsert", "replace", "read", "delete", "patch", "create"]);
        assert!(ops[0].get("partitionKey").is_none() && ops[0].get("id").is_none(), "a transactional batch names its key in a header");
        assert_eq!(ops[1]["ifMatch"], "\"abc\"");
        assert_eq!(
            ops[2],
            json!({"operationType": "replace", "id": "i1", "resourceBody": {"id": "i1", "pk": "p1", "n": 5}, "indexingDirective": "Exclude", "ifNoneMatch": "\"xyz\""})
        );
        assert_eq!(ops[3], json!({"operationType": "read", "id": "i1"}));
        assert_eq!(
            ops[5]["resourceBody"],
            json!({"operations": [{"op": "set", "path": "/n", "value": 9}, {"op": "incr", "path": "/n", "value": 1}]})
        );
        assert_eq!(ops[6]["resourceBody"]["\u{e9}"], "\u{fc}");
        assert_eq!(ops[6]["minimalReturnPreference"], true);
    }

    #[test]
    fn results_are_written_as_the_real_library_writes_them() {
        let results = vec![
            json!({"statusCode": 201, "subStatusCode": 0, "eTag": "\"etag-1\"", "resourceBody": {"_etag": "\"etag-1\"", "id": "i1"}, "requestCharge": 5.71}),
            json!({"statusCode": 404, "subStatusCode": 1003, "requestCharge": 1.0}),
            json!({"statusCode": 424, "requestCharge": 0.0}),
            json!({"statusCode": 429, "subStatusCode": 3200, "retryAfterMilliseconds": 250}),
            json!({"statusCode": 200, "eTag": "\"e\"", "resourceBody": "x".repeat(298), "requestCharge": 1.25}),
            json!({"statusCode": 204, "requestCharge": 5.71}),
        ];
        // the whole stream, segment header and six records, identical to the library's bytes
        assert_eq!(encode_results(&results), hex(GOLDEN_RESULTS));
    }

    #[test]
    fn bad_bodies_are_refused_with_a_message() {
        let good = hex(BULK_CREATE);
        assert!(decode_operations(&good, 100).is_ok());
        // truncated at every length: an error, never a panic
        for n in 0..good.len() {
            let r = decode_operations(&good[..n], 100);
            if n == 10 {
                assert_eq!(r.unwrap(), Vec::<Value>::new(), "the segment header alone is a stream with no records");
            } else {
                assert!(r.is_err(), "cut at {n}");
            }
        }
        // a flipped byte in the body is a checksum error
        let mut bad = good.clone();
        let last = bad.len() - 3;
        bad[last] ^= 1;
        assert!(decode_operations(&bad, 100).unwrap_err().contains("checksum"));
        // not a stream
        assert!(decode_operations(b"[]", 100).is_err());
        assert!(decode_operations(&[0x81, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 100).is_err());
        // lengths that run past the end, and a length that never ends
        let mut huge = good.clone();
        huge[10 + 5..10 + 9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_operations(&huge, 100).is_err());
        let mut forever = vec![ROW_V1];
        forever.extend_from_slice(&OPERATION_SCHEMA.to_le_bytes());
        forever.push(0b100);
        forever.extend_from_slice(&[0; 8]);
        forever.extend(std::iter::repeat_n(0xFF, 40));
        assert!(decode_operation(&forever).is_err());
        // a flipped bit anywhere in the stream must not panic
        for i in 0..good.len() {
            for bit in 0..8 {
                let mut b = good.clone();
                b[i] ^= 1 << bit;
                let _ = decode_operations(&b, 100);
            }
        }
    }

    #[test]
    fn at_most_max_ops_plus_one_records_are_read() {
        let ops = decode_operations(&hex(BATCH_ALL), 3).unwrap();
        assert_eq!(ops.len(), 4);
    }
}
