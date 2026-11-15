//! Query Blob Contents over HTTP, on the Blob and the Data Lake endpoint.
//!
//! The answer is an Avro container file; `Avro` below is a reader written from the Avro specification and
//! from what the real service sent (see `src/query/testdata/*.avro`), not from the emulator's writer.

mod common;

use std::sync::Arc;
use std::time::Duration;

use azemu_blob::{BlobService, QueryLimits};
use azemu_core::testing::{TestResponse, TestServer};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::*;
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Clone)]
enum Msg {
    Data(Vec<u8>),
    Error { fatal: bool, name: String, description: String, position: i64 },
    Progress(i64, i64),
    End(i64),
}

struct Avro {
    schema: String,
    msgs: Vec<Msg>,
}

fn long(b: &[u8], p: &mut usize) -> i64 {
    let (mut n, mut s) = (0u64, 0);
    loop {
        let c = b[*p];
        *p += 1;
        n |= u64::from(c & 0x7f) << s;
        s += 7;
        if c & 0x80 == 0 {
            return ((n >> 1) as i64) ^ -((n & 1) as i64);
        }
    }
}

fn bytes<'a>(b: &'a [u8], p: &mut usize) -> &'a [u8] {
    let n = long(b, p) as usize;
    *p += n;
    &b[*p - n..*p]
}

fn avro(b: &[u8]) -> Avro {
    assert_eq!(&b[..4], b"Obj\x01", "an Avro object container file");
    let mut p = 4;
    let mut schema = String::new();
    loop {
        let n = long(b, &mut p);
        if n == 0 {
            break;
        }
        for _ in 0..n {
            let k = bytes(b, &mut p).to_vec();
            let v = bytes(b, &mut p).to_vec();
            assert_eq!(k, b"avro.schema");
            schema = String::from_utf8(v).unwrap();
        }
    }
    let sync = b[p..p + 16].to_vec();
    p += 16;
    let mut msgs = vec![];
    while p < b.len() {
        assert_eq!(long(b, &mut p), 1);
        let size = long(b, &mut p) as usize;
        let rec = &b[p..p + size];
        assert_eq!(&b[p + size..p + size + 16], &sync[..]);
        p += size + 16;
        let mut q = 0;
        msgs.push(match long(rec, &mut q) {
            0 => Msg::Data(bytes(rec, &mut q).to_vec()),
            1 => {
                let fatal = rec[q] != 0;
                q += 1;
                let name = String::from_utf8(bytes(rec, &mut q).to_vec()).unwrap();
                let description = String::from_utf8(bytes(rec, &mut q).to_vec()).unwrap();
                Msg::Error { fatal, name, description, position: long(rec, &mut q) }
            }
            2 => {
                let a = long(rec, &mut q);
                Msg::Progress(a, long(rec, &mut q))
            }
            3 => Msg::End(long(rec, &mut q)),
            i => panic!("record type {i}"),
        });
    }
    Avro { schema, msgs }
}

impl Avro {
    fn data(&self) -> Vec<u8> {
        self.msgs.iter().filter_map(|m| if let Msg::Data(d) = m { Some(d.clone()) } else { None }).flatten().collect()
    }

    fn errors(&self) -> Vec<(bool, String, i64)> {
        self.msgs
            .iter()
            .filter_map(|m| if let Msg::Error { fatal, name, position, .. } = m { Some((*fatal, name.clone(), *position)) } else { None })
            .collect()
    }
}

fn body(expr: &str, extra: &str) -> String {
    format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<QueryRequest><QueryType>SQL</QueryType><Expression>{}</Expression>{extra}</QueryRequest>",
        expr.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    )
}

const CSV: &str = "id,name,price\r\n1,apple,0.5\r\n2,banana,0.25\r\n3,\"cherry, red\",4\r\n";

async fn q(env: &Env, path: &str, expr: &str, extra: &str, headers: &[(&str, &str)]) -> TestResponse {
    env.send("POST", &format!("{path}?comp=query"), headers, body(expr, extra)).await
}

async fn start_with_csv() -> Env {
    let env = start().await;
    env.create_container("cnt").await;
    assert_eq!(env.put_blob("cnt", "b.csv", CSV).await.status, 201);
    env
}

#[tokio::test]
async fn select_star_gives_the_blob_and_the_headers_the_service_sends() {
    let env = start_with_csv().await;
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some("avro/binary"));
    // what the recordings of the real service carry
    for h in [
        "etag",
        "last-modified",
        "accept-ranges",
        "x-ms-blob-type",
        "x-ms-creation-time",
        "x-ms-lease-state",
        "x-ms-lease-status",
        "x-ms-server-encrypted",
        "x-ms-request-id",
        "x-ms-version",
    ] {
        assert!(r.header(h).is_some(), "missing {h}: {:?}", r.headers);
    }
    assert_eq!(r.header("x-ms-blob-type"), Some("BlockBlob"));
    let a = avro(&r.body);
    assert!(a.schema.contains("com.microsoft.azure.storage.queryBlobContents.resultData"));
    assert_eq!(a.data(), CSV.replace("\r\n", "\n").as_bytes());
    let n = CSV.len() as i64;
    assert_eq!(&a.msgs[a.msgs.len() - 2..], &[Msg::Progress(n, n), Msg::End(n)]);
}

#[tokio::test]
async fn queries_with_a_header_row_where_and_projection() {
    let env = start_with_csv().await;
    let input = "<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><ColumnSeparator>,</ColumnSeparator><FieldQuote>\"</FieldQuote><RecordSeparator>\n</RecordSeparator><EscapeChar /><HasHeaders>true</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>";
    let r = q(&env, "/cnt/b.csv", "SELECT name, price * 2 AS double FROM BlobStorage WHERE id > 1", input, &[]).await;
    assert_eq!(avro(&r.body).data(), b"banana,0.5\n\"cherry, red\",8\n");
    let json_out = "<OutputSerialization><Format><Type>json</Type><JsonTextConfiguration><RecordSeparator>\n</RecordSeparator></JsonTextConfiguration></Format></OutputSerialization>";
    let r =
        q(&env, "/cnt/b.csv", "SELECT name, price * 2 AS double FROM BlobStorage WHERE id > 1", &format!("{input}{json_out}"), &[]).await;
    assert_eq!(avro(&r.body).data(), b"{\"name\":\"banana\",\"double\":0.5}\n{\"name\":\"cherry, red\",\"double\":8}\n");
    let r = q(&env, "/cnt/b.csv", "SELECT SUM(CAST(price AS FLOAT)) FROM BlobStorage", input, &[]).await;
    assert_eq!(avro(&r.body).data(), b"4.75\n");
}

#[tokio::test]
async fn errors_in_the_stream_and_in_the_request() {
    let env = start_with_csv().await;
    // a bad expression: 400 before any data, with the expression in the message (Azure, November 2026)
    let r = q(&env, "/cnt/b.csv", "SELECT FROM", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "ErrorProcessingQueryExpression"));
    assert!(r.text().contains("<Message>Syntax error in query: SELECT FROM\n"), "{}", r.text());
    assert_eq!(r.header("content-type"), Some("application/xml"));
    // a column that is not in the header row, or any name when there is no header row: 400 as well
    let input = "<InputSerialization><Format><Type>delimited</Type><DelimitedTextConfiguration><HasHeaders>true</HasHeaders></DelimitedTextConfiguration></Format></InputSerialization>";
    let r = q(&env, "/cnt/b.csv", "SELECT nope FROM BlobStorage", input, &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "ErrorProcessingQueryExpression"));
    assert!(r.text().contains("<Message>Column name(s) does not exist in the header row.\n"), "{}", r.text());
    let r = q(&env, "/cnt/b.csv", "SELECT id, _9 FROM BlobStorage", input, &[]).await;
    assert!(r.text().contains("<Message>Invalid column ordinal(s).\n"), "{}", r.text());
    let r = q(&env, "/cnt/b.csv", "SELECT name FROM BlobStorage", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "ErrorProcessingQueryExpression"));
    assert!(r.text().contains("Column names can be used only if &apos;hasHeader&apos; is set"), "{}", r.text());
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM Wrong", "", &[]).await;
    assert!(r.text().contains("Record paths are not valid for queries over CSV-formatted input."), "{}", r.text());
    // a body that is not the request: InvalidXmlDocument for a missing element (also an empty Expression)
    let r = env.send("POST", "/cnt/b.csv?comp=query", &[], "<QueryRequest><Expression>x</Expression></QueryRequest>").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"));
    let r = q(&env, "/cnt/b.csv", "", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"));
    // any QueryType is read as SQL
    let r = env
        .send(
            "POST",
            "/cnt/b.csv?comp=query",
            &[],
            "<QueryRequest><QueryType>NOSQL</QueryType><Expression>SELECT * FROM BlobStorage</Expression></QueryRequest>",
        )
        .await;
    assert_eq!(r.status, 200);
    let r = env.send("POST", "/cnt/b.csv?comp=query", &[], "not xml").await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidXmlDocument"));
    // Parquet input: Azure reads it; here a blob that is not Parquet is refused as Azure does, a real file is not built
    let parquet = "<InputSerialization><Format><Type>parquet</Type><ParquetTextConfiguration> </ParquetTextConfiguration></Format></InputSerialization>";
    let r = q(&env, "/cnt/b.csv", "select * from blobstorage where id < 1;", parquet, &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "ErrorProcessingQueryExpression"));
    assert!(r.text().contains("Invalid parquet file. Corrupt footer."), "{}", r.text());
    assert_eq!(env.put_blob("cnt", "p.parquet", "PAR1 anything PAR1").await.status, 201);
    let r = q(&env, "/cnt/p.parquet", "select * from blobstorage", parquet, &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (501, "NotImplemented"));
    assert!(r.text().contains("parquet"));
    let arrow = "<OutputSerialization><Format><Type>arrow</Type><ArrowConfiguration><Schema><Field><Type>decimal</Type><Name>abc</Name><Precision>4</Precision><Scale>2</Scale></Field></Schema></ArrowConfiguration></Format></OutputSerialization>";
    let r = q(&env, "/cnt/b.csv", "SELECT _2 from BlobStorage WHERE _1 > 250", arrow, &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (501, "NotImplemented"));
    assert!(r.text().contains("arrow"));
}

/// What is wrong in the data, not in the expression, stays in the stream: `200` and a fatal error at position 0 (Azure, November 2026).
#[tokio::test]
async fn errors_in_the_data_are_in_the_stream() {
    let env = start().await;
    env.create_container("cnt").await;
    assert_eq!(env.put_blob("cnt", "bad.json", "{\"a\":1}\n{bad\n{\"a\":3}\n").await.status, 201);
    let json = "<InputSerialization><Format><Type>json</Type></Format></InputSerialization>";
    let r = q(&env, "/cnt/bad.json", "SELECT * FROM BlobStorage[*]", json, &[]).await;
    assert_eq!(r.status, 200);
    let a = avro(&r.body);
    assert_eq!(a.data(), b"{\"a\":1}\n");
    assert_eq!(a.errors(), vec![(true, "ParseError".to_string(), 0)]);
    // the byte in the text counts from the start of the blob (the bad line starts at byte 8)
    assert!(
        matches!(&a.msgs[1], Msg::Error { description, .. } if description == "Unexpected token 'bad' at [byte: 9]. Expecting token '\"'. ")
    );
    // an unclosed quote in delimited text
    assert_eq!(env.put_blob("cnt", "q.csv", "x,y\n\"unclosed,2\n3,4\n").await.status, 201);
    let a = avro(&q(&env, "/cnt/q.csv", "SELECT * FROM BlobStorage", "", &[]).await.body);
    assert_eq!(a.data(), b"x,y\n");
    assert_eq!(a.errors(), vec![(true, "ParseError".to_string(), 0)]);
    // a row that cannot be read as a number is a row error, the query goes on
    assert_eq!(env.put_blob("cnt", "n.csv", "30\nx\n7\n").await.status, 201);
    let a = avro(&q(&env, "/cnt/n.csv", "SELECT _1 + 1 FROM BlobStorage", "", &[]).await.body);
    assert_eq!(a.data(), b"31\n8\n");
    assert_eq!(a.errors(), vec![(false, "InvalidTypeConversion".to_string(), 3)]);
    // division by zero ends the query
    let a = avro(&q(&env, "/cnt/n.csv", "SELECT 1/0 FROM BlobStorage", "", &[]).await.body);
    assert_eq!(a.errors(), vec![(true, "ClientError".to_string(), 0)]);
}

#[tokio::test]
async fn the_blob_is_found_checked_and_refused_like_a_read() {
    let env = start_with_csv().await;
    assert_eq!(code(&q(&env, "/nocnt/b.csv", "SELECT * FROM BlobStorage", "", &[]).await), "ContainerNotFound");
    assert_eq!(code(&q(&env, "/cnt/nope", "SELECT * FROM BlobStorage", "", &[]).await), "BlobNotFound");
    // conditions
    let etag = env.send("HEAD", "/cnt/b.csv", &[], "").await.header("etag").unwrap().to_string();
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("If-Match", "\"0x1\"")]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("If-None-Match", &etag)]).await;
    assert_eq!(r.status, 304);
    assert_eq!(q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("If-Match", &etag)]).await.status, 200);
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("x-ms-if-tags", "\"a\" = 'b'")]).await;
    assert_eq!((r.status, code(&r).as_str()), (412, "ConditionNotMet"));
    // a lease: reading needs no lease id, a wrong one is refused
    let r = env.send("PUT", "/cnt/b.csv?comp=lease", &[("x-ms-lease-action", "acquire"), ("x-ms-lease-duration", "-1")], "").await;
    let lease = r.header("x-ms-lease-id").unwrap().to_string();
    assert_eq!(q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[]).await.status, 200);
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("x-ms-lease-id", &lease)]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("x-ms-lease-state"), Some("leased"));
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[("x-ms-lease-id", "00000000-0000-0000-0000-000000000000")]).await;
    assert_eq!(r.status, 412);
    // no credentials at all
    let r = env.anon("POST", "/cnt/b.csv?comp=query", &[], body("SELECT * FROM BlobStorage", "")).await;
    assert_eq!(r.status, 401);
    // other blob types and an archived blob
    assert_eq!(env.send("PUT", "/cnt/app", &[("x-ms-blob-type", "AppendBlob")], "").await.status, 201);
    let r = q(&env, "/cnt/app", "SELECT * FROM BlobStorage", "", &[]).await;
    // Azure: 400 with the code as the message, also for a page blob
    assert_eq!((r.status, code(&r).as_str()), (400, "QueryBlobContentsNotSupportedForPageBlobsOrAppendBlobs"));
    assert_eq!(env.send("PUT", "/cnt/b.csv?comp=tier", &[("x-ms-access-tier", "Archive")], "").await.status, 200);
    let r = q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobArchived"));
}

#[tokio::test]
async fn snapshots_and_versions_can_be_queried() {
    let env = start_versioned().await;
    env.create_container("cnt").await;
    assert_eq!(env.put_blob("cnt", "b.csv", CSV).await.status, 201);
    let snap = env.send("PUT", "/cnt/b.csv?comp=snapshot", &[], "").await;
    let (snapshot, v1) = (snap.header("x-ms-snapshot").unwrap().to_string(), snap.header("x-ms-version-id").map(str::to_string));
    let r = env.put_blob("cnt", "b.csv", "new\n").await;
    assert_eq!(r.status, 201);
    let a = avro(&q(&env, "/cnt/b.csv", "SELECT * FROM BlobStorage", "", &[]).await.body);
    assert_eq!(a.data(), b"new\n");
    let r = env
        .send(
            "POST",
            &format!("/cnt/b.csv?comp=query&snapshot={}", snapshot.replace(':', "%3A")),
            &[],
            body("SELECT * FROM BlobStorage", ""),
        )
        .await;
    assert_eq!(avro(&r.body).data(), CSV.replace("\r\n", "\n").as_bytes());
    let v = v1.expect("versioning is on for this service");
    let r = env
        .send("POST", &format!("/cnt/b.csv?comp=query&versionid={}", v.replace(':', "%3A")), &[], body("SELECT * FROM BlobStorage", ""))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(avro(&r.body).data(), CSV.replace("\r\n", "\n").as_bytes());
    let r =
        env.send("POST", "/cnt/b.csv?comp=query&snapshot=2020-01-01T00:00:00.0000000Z", &[], body("SELECT * FROM BlobStorage", "")).await;
    assert_eq!(r.status, 404);
}

fn key(byte: u8) -> (String, String) {
    (B64.encode([byte; 32]), B64.encode(Sha256::digest([byte; 32])))
}

#[tokio::test]
async fn customer_provided_keys_are_checked() {
    let env = start().await;
    env.create_container("cnt").await;
    let k = key(3);
    let kh = [("x-ms-encryption-key", k.0.as_str()), ("x-ms-encryption-key-sha256", k.1.as_str()), ("x-ms-encryption-algorithm", "AES256")];
    let mut put = vec![("x-ms-blob-type", "BlockBlob")];
    put.extend_from_slice(&kh);
    assert_eq!(env.send("PUT", "/cnt/k", &put, "a,b\n").await.status, 201);
    let r = q(&env, "/cnt/k", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (409, "BlobUsesCustomerSpecifiedEncryption"));
    let r = q(&env, "/cnt/k", "SELECT * FROM BlobStorage", "", &kh).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-ms-encryption-key-sha256"), Some(k.1.as_str()));
    assert_eq!(avro(&r.body).data(), b"a,b\n");
    // the headers must agree with each other (checked for every request)
    let r =
        q(&env, "/cnt/k", "SELECT * FROM BlobStorage", "", &[("x-ms-encryption-key", &k.0), ("x-ms-encryption-algorithm", "AES256")]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"));
}

#[tokio::test]
async fn a_blob_of_many_blocks_is_read_across_them() {
    let env = start().await;
    env.create_container("cnt").await;
    let mut list = String::from("<BlockList>");
    let mut want = String::new();
    for i in 0..5 {
        let id = B64.encode(format!("block-{i}"));
        // the blocks cut records in the middle on purpose
        let chunk = format!(
            "{}{}",
            if i == 0 { String::new() } else { format!("{}\n", i * 100 - 1) },
            (i * 100..i * 100 + 40).map(|n| format!("{n},v{n}\n")).collect::<String>()
        );
        let chunk = chunk.trim_end_matches('\n').to_string() + if i == 4 { "\n" } else { "" };
        want.push_str(&chunk);
        let r = env.send("PUT", &format!("/cnt/m?comp=block&blockid={}", id.replace('=', "%3D")), &[], chunk).await;
        assert_eq!(r.status, 201);
        list.push_str(&format!("<Latest>{id}</Latest>"));
    }
    list.push_str("</BlockList>");
    assert_eq!(env.send("PUT", "/cnt/m?comp=blocklist", &[], list).await.status, 201);
    let all = env.send("GET", "/cnt/m", &[], "").await.body;
    assert_eq!(all, want.as_bytes());
    let r = q(&env, "/cnt/m", "SELECT _2 FROM BlobStorage WHERE _1 LIKE '4%'", "", &[]).await;
    let expect: String =
        want.lines().filter(|l| l.starts_with('4')).filter_map(|l| l.split(',').nth(1)).map(|v| format!("{v}\n")).collect();
    assert!(!expect.is_empty());
    assert_eq!(avro(&r.body).data(), expect.as_bytes());
}

#[tokio::test]
async fn a_big_result_comes_in_records_of_bounded_size() {
    let small = QueryLimits { data_chunk_bytes: 256 * 1024, ..QueryLimits::default() };
    let env = start_with(BlobService::with_query_limits(small)).await;
    env.create_container("cnt").await;
    let rows: String = (0..150_000).map(|i| format!("{i},value-{i}\n")).collect();
    assert!(rows.len() > 2_000_000);
    assert_eq!(env.put_blob("cnt", "big", &rows).await.status, 201);
    let r = q(&env, "/cnt/big", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!(r.status, 200);
    let a = avro(&r.body);
    assert_eq!(a.data(), rows.as_bytes());
    let datas: Vec<usize> = a.msgs.iter().filter_map(|m| if let Msg::Data(d) = m { Some(d.len()) } else { None }).collect();
    assert!(datas.len() > 5, "{} data records", datas.len());
    assert!(datas.iter().all(|n| *n <= 256 * 1024), "{datas:?}");
    let mut last = 0;
    let mut progress = 0;
    for m in &a.msgs {
        if let Msg::Progress(scanned, total) = m {
            assert_eq!(*total, rows.len() as i64);
            assert!(*scanned >= last);
            last = *scanned;
            progress += 1;
        }
    }
    assert!(progress > 5);
    assert_eq!(a.msgs.last(), Some(&Msg::End(rows.len() as i64)));
}

/// Azure sent a result of 1,020,000 bytes as one data record, one progress message and `end` (November 2026).
#[tokio::test]
async fn a_result_of_a_million_bytes_is_one_record() {
    let env = start().await;
    env.create_container("cnt").await;
    let rows: String = (0..20_000).map(|i| format!("row{i:06},{}\n", "x".repeat(40))).collect();
    assert_eq!(rows.len(), 1_020_000);
    assert_eq!(env.put_blob("cnt", "big", &rows).await.status, 201);
    let a = avro(&q(&env, "/cnt/big", "SELECT * FROM BlobStorage", "", &[]).await.body);
    let n = rows.len() as i64;
    assert_eq!(a.msgs, vec![Msg::Data(rows.into_bytes()), Msg::Progress(n, n), Msg::End(n)]);
}

#[tokio::test]
async fn the_limits_are_enforced() {
    let limits = QueryLimits { max_blob_bytes: 1000, max_concurrent: 0, queue_wait: Duration::from_millis(50), ..QueryLimits::default() };
    // too big
    let env = start_with(BlobService::with_query_limits(QueryLimits { max_concurrent: 4, ..limits })).await;
    env.create_container("cnt").await;
    assert_eq!(env.put_blob("cnt", "small", "a\n").await.status, 201);
    assert_eq!(env.put_blob("cnt", "big", &"x".repeat(1001)).await.status, 201);
    let r = q(&env, "/cnt/big", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "InvalidInput"));
    assert!(r.text().contains("1000"), "{}", r.text());
    assert_eq!(q(&env, "/cnt/small", "SELECT * FROM BlobStorage", "", &[]).await.status, 200);
    // no place to run in
    let env = start_with(BlobService::with_query_limits(limits)).await;
    env.create_container("cnt").await;
    env.put_blob("cnt", "small", "a\n").await;
    let r = q(&env, "/cnt/small", "SELECT * FROM BlobStorage", "", &[]).await;
    assert_eq!((r.status, code(&r).as_str()), (503, "ServerBusy"));
    // other requests are not affected
    assert_eq!(env.send("GET", "/cnt/small", &[], "").await.status, 200);
}

#[tokio::test]
async fn a_query_does_not_stop_other_requests() {
    let env = start().await;
    env.create_container("cnt").await;
    let rows: String = (0..400_000).map(|i| format!("{i},row-{i},x\n")).collect();
    assert_eq!(env.put_blob("cnt", "big", &rows).await.status, 201);
    assert_eq!(env.put_blob("cnt", "small", "hello").await.status, 201);
    let env = Arc::new(env);
    let e = env.clone();
    let query = tokio::spawn(async move {
        let t = std::time::Instant::now();
        let r =
            q(&e, "/cnt/big", "SELECT _2, UPPER(_3), CAST(_1 AS INT) * 2 FROM BlobStorage WHERE _1 LIKE '%7' OR _2 LIKE '%99%'", "", &[])
                .await;
        (r.status, t.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut slowest = Duration::ZERO;
    for _ in 0..20 {
        let t = std::time::Instant::now();
        let r = env.send("GET", "/cnt/small", &[], "").await;
        slowest = slowest.max(t.elapsed());
        assert_eq!(r.body.as_ref(), b"hello");
    }
    let (status, took) = query.await.unwrap();
    assert_eq!(status, 200);
    assert!(slowest < took / 2 || slowest < Duration::from_millis(50), "a GET took {slowest:?} while the query took {took:?}");
}

async fn lake() -> (TestServer, String, String) {
    let blob = BlobService::new();
    let dfs = blob.datalake();
    let srv = TestServer::start(vec![Arc::new(blob), Arc::new(dfs)]).await;
    let (d, b) = (srv.url("dfs", "acct"), srv.url("blob", "acct"));
    (srv, d, b)
}

#[tokio::test]
async fn a_data_lake_file_is_queried_through_the_blob_endpoint_only() {
    let (srv, dfs, blob) = lake().await;
    let s = |m: &'static str, u: String, b: String| {
        let srv = &srv;
        async move { srv.send(m, &u, &[AUTH], b).await }
    };
    let r = s("PUT", format!("{dfs}/fsq?resource=filesystem"), String::new()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(s("PUT", format!("{dfs}/fsq/dir/f.csv?resource=file"), String::new()).await.status, 201);
    assert_eq!(s("PATCH", format!("{dfs}/fsq/dir/f.csv?action=append&position=0"), "a,1\nb,2\n".into()).await.status, 202);
    assert_eq!(s("PATCH", format!("{dfs}/fsq/dir/f.csv?action=flush&position=8"), String::new()).await.status, 200);
    // the blob endpoint: the file is a block blob and says it is a file (as in the recorded answers)
    let r = s("POST", format!("{blob}/fsq/dir/f.csv?comp=query"), body("SELECT _1 FROM BlobStorage WHERE _2 > 1", "")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-ms-resource-type"), Some("file"));
    assert_eq!(avro(&r.body).data(), b"b\n");
    // the dfs host has no query: it reads a POST as a lease and wants `x-ms-lease-action` (Azure, November 2026)
    let r = s("POST", format!("{dfs}/fsq/dir/f.csv?comp=query"), body("SELECT * FROM BlobStorage", "")).await;
    assert_eq!((r.status, code(&r).as_str()), (400, "MissingRequiredHeader"), "{}", r.text());
    assert!(r.text().contains("\"HeaderName\":\"x-ms-lease-action\""), "{}", r.text());
}
