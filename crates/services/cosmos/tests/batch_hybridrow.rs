//! Transactional batch and bulk from the .NET SDK: the body is a HybridRow record stream, and so is the answer
//! (see `docs/sdk-notes.md`, .NET, Cosmos DB). The request bodies below were sent by `Microsoft.Azure.Cosmos` 3.64.0
//! (gateway mode, no content type) and captured on the wire; the answers are decoded here by a reader written for the test.

mod common;

use common::{start, H};
use serde_json::{json, Value};

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

const BULK_NO_CONTENT: &str = concat!(
    "81f0d8ff7f010a00000081f1d8ff7f2d0000008eff2b1e817054e17f471400000002000000065b227062225d157b2269",
    "64223a226232222c22706b223a227062227d030c",
);

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// One decoded `BatchResult` record.
#[derive(Debug)]
struct Res {
    status: i32,
    sub: i32,
    etag: Option<String>,
    body: Option<Value>,
    charge: Option<f64>,
}

fn varint(b: &[u8], pos: &mut usize) -> usize {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let x = b[*pos];
        *pos += 1;
        v |= ((x & 0x7F) as usize) << shift;
        if x & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn decode(body: &[u8]) -> Vec<Res> {
    assert_eq!(body[0], 0x81, "a HybridRow answer starts with the row version byte");
    assert_eq!(&body[1..5], &[0xF0, 0xD8, 0xFF, 0x7F], "segment schema");
    let seg = u32::from_le_bytes(body[6..10].try_into().unwrap()) as usize;
    let mut pos = seg;
    let mut out = Vec::new();
    while pos < body.len() {
        assert_eq!(&body[pos..pos + 5], &[0x81, 0xF1, 0xD8, 0xFF, 0x7F], "record header");
        let len = u32::from_le_bytes(body[pos + 5..pos + 9].try_into().unwrap()) as usize;
        let rec = &body[pos + 13..pos + 13 + len];
        pos += 13 + len;
        assert_eq!(&rec[..5], &[0x81, 0x71, 0x54, 0xE1, 0x7F], "BatchResult schema");
        let present = rec[5];
        let status = i32::from_le_bytes(rec[6..10].try_into().unwrap());
        let sub = i32::from_le_bytes(rec[10..14].try_into().unwrap());
        let mut p = 14;
        let mut etag = None;
        let mut body = None;
        if present & 4 != 0 {
            let n = varint(rec, &mut p);
            etag = Some(String::from_utf8(rec[p..p + n].to_vec()).unwrap());
            p += n;
        }
        if present & 8 != 0 {
            let n = varint(rec, &mut p);
            body = Some(serde_json::from_slice(&rec[p..p + n]).unwrap());
            p += n;
        }
        let mut charge = None;
        while p < rec.len() {
            let (code, field) = (rec[p], rec[p + 1]);
            p += 2;
            match (code, field) {
                (0x10, 6) => {
                    charge = Some(f64::from_le_bytes(rec[p..p + 8].try_into().unwrap()));
                    p += 8;
                }
                (0x0B, 5) => p += 4,
                other => panic!("unexpected sparse field {other:?}"),
            }
        }
        out.push(Res { status, sub, etag, body, charge });
    }
    out
}

async fn setup() -> H {
    let h = start().await;
    h.db("d").await;
    h.coll("d", "c", "/pk").await;
    h
}

async fn send(h: &H, headers: &[(&str, &str)], body: &[u8]) -> (u16, Option<String>, Vec<u8>) {
    // the SDK sends no content type; the test server takes a `String` body, so the bytes go through the raw client of the test server
    let r = h.raw("POST", "dbs/d/colls/c/docs", headers, body.to_vec()).await;
    (r.status, r.header("content-type").map(str::to_string), r.body.to_vec())
}

const TXN: [(&str, &str); 4] = [
    ("x-ms-documentdb-partitionkey", "[\"p1\"]"),
    ("x-ms-cosmos-batch-atomic", "True"),
    ("x-ms-cosmos-batch-ordered", "True"),
    ("x-ms-cosmos-is-batch-request", "True"),
];

#[tokio::test]
async fn a_transactional_batch_from_the_dotnet_sdk_runs_and_is_answered_in_hybridrow() {
    let h = setup().await;
    let (status, _, body) = send(&h, &TXN, &hex(BATCH_ALL)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let res = decode(&body);
    let codes: Vec<i32> = res.iter().map(|r| r.status).collect();
    assert_eq!(codes, [201, 201, 200, 200, 204, 200, 201], "{res:?}");
    assert!(res.iter().all(|r| r.sub == 0 && r.charge.is_some()));
    assert_eq!(res[0].body.as_ref().unwrap()["id"], "i1");
    assert_eq!(res[0].etag.as_deref(), res[0].body.as_ref().unwrap()["_etag"].as_str());
    assert_eq!(res[2].body.as_ref().unwrap()["n"], 5);
    assert_eq!(res[3].body.as_ref().unwrap()["n"], 5, "the read sees the replace");
    assert!(res[4].body.is_none(), "a delete has no body");
    assert_eq!(res[5].body.as_ref().unwrap()["n"], 10, "set to 9 then incremented");
    assert!(res[6].body.is_none() && res[6].etag.is_some(), "minimalReturnPreference: no body, the etag stays");
    // the effects are real
    let r = h.req("GET", "dbs/d/colls/c/docs/i1", &[("x-ms-documentdb-partitionkey", "[\"p1\"]")], "").await;
    assert_eq!(r.json()["n"], 10);
    let r = h.req("GET", "dbs/d/colls/c/docs/i2", &[("x-ms-documentdb-partitionkey", "[\"p1\"]")], "").await;
    assert_eq!(r.status, 404);
    let r = h.req("GET", "dbs/d/colls/c/docs/i3", &[("x-ms-documentdb-partitionkey", "[\"p1\"]")], "").await;
    assert_eq!(r.json()["\u{e9}"], "\u{fc}");
}

#[tokio::test]
async fn a_failing_transactional_batch_is_rolled_back_and_answers_207() {
    let h = setup().await;
    h.doc("d", "c", &json!("p1"), &json!({"id": "i1", "pk": "p1", "n": 0})).await;
    // the first operation creates i1, which exists: 409 and everything else is 424
    let (status, _, body) = send(&h, &TXN, &hex(BATCH_ALL)).await;
    assert_eq!(status, 207);
    let codes: Vec<i32> = decode(&body).iter().map(|r| r.status).collect();
    assert_eq!(codes, [409, 424, 424, 424, 424, 424, 424]);
    let r = h.req("GET", "dbs/d/colls/c/docs/i3", &[("x-ms-documentdb-partitionkey", "[\"p1\"]")], "").await;
    assert_eq!(r.status, 404, "nothing of the batch is kept");
    let r = h.req("GET", "dbs/d/colls/c/docs/i1", &[("x-ms-documentdb-partitionkey", "[\"p1\"]")], "").await;
    assert_eq!(r.json()["n"], 0);
}

const BULK: [(&str, &str); 4] = [
    ("x-ms-documentdb-partitionkeyrangeid", "0"),
    ("x-ms-cosmos-batch-atomic", "False"),
    ("x-ms-cosmos-batch-continue-on-error", "True"),
    ("x-ms-cosmos-is-batch-request", "True"),
];

#[tokio::test]
async fn bulk_operations_from_the_dotnet_sdk_each_stand_alone() {
    let h = setup().await;
    // create b1 in partition "pb"
    let (status, ct, body) = send(&h, &BULK, &hex(BULK_CREATE)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_ne!(ct.as_deref(), Some("application/json"), "the answer is not JSON");
    let res = decode(&body);
    assert_eq!((res.len(), res[0].status), (1, 201));
    assert_eq!(res[0].body.as_ref().unwrap()["v"], "x");
    // delete b1 with the wrong etag: 412, and the answer for the single operation is a 207 batch
    let (status, _, body) = send(&h, &BULK, &hex(BULK_DELETE)).await;
    assert_eq!(status, 207);
    assert_eq!(decode(&body)[0].status, 412);
    // an upsert without content in the answer
    let (status, _, body) = send(&h, &BULK, &hex(BULK_NO_CONTENT)).await;
    assert_eq!(status, 200);
    let res = decode(&body);
    assert_eq!(res[0].status, 201);
    assert!(res[0].body.is_none() && res[0].etag.is_some());
    let r = h.req("GET", "dbs/d/colls/c/docs/b2", &[("x-ms-documentdb-partitionkey", "[\"pb\"]")], "").await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn a_damaged_hybridrow_body_is_a_400_that_says_what_is_wrong() {
    let h = setup().await;
    let mut body = hex(BULK_CREATE);
    let last = body.len() - 2;
    body[last] ^= 0x40;
    let (status, ct, text) = send(&h, &BULK, &body).await;
    assert_eq!(status, 400);
    assert_eq!(ct.as_deref(), Some("application/json"));
    let text = String::from_utf8_lossy(&text).to_string();
    assert!(text.contains("HybridRow") && text.contains("checksum"), "{text}");
    // cut short
    let (status, _, text) = send(&h, &BULK, &hex(BULK_CREATE)[..30]).await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&text));
    // a body of bytes that is neither JSON nor HybridRow keeps its old message
    let (status, _, text) = send(&h, &BULK, &[0x80, 1, 2, 3]).await;
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&text).contains("binary JSON"));
}
