//! Known-answer vectors. Generated with the Python SDK (azure-storage-blob 12.31.1 and
//! azure-storage-queue 12.18.1) for everything it signs, with Microsoft's Azurite signing code
//! for the older `sv` layouts the SDK no longer produces, and by hand from the REST documentation
//! for Shared Key Lite and the 2012-02-12 / 2013-08-15 service SAS layouts. Account key is the
//! well-known `devstoreaccount1` key; user delegation keys are re-derived (see `udk.rs`).

use super::*;
use crate::request::{Endpoint, Query, RequestCtx};
use crate::time::OffsetDateTime;
use bytes::Bytes;

const ACCT: &str = "devstoreaccount1";

fn req(method: &str, path: &str, query: &str, headers: &[(&str, &str)], path_style: bool) -> Request {
    let mut h = http::HeaderMap::new();
    for (k, v) in headers {
        h.append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    Request {
        method: method.parse().unwrap(),
        path: path.into(),
        query: Query::parse(query),
        headers: h,
        body: Bytes::new(),
        account: ACCT.into(),
        ctx: RequestCtx {
            request_id: "r".into(),
            time: OffsetDateTime::UNIX_EPOCH,
            endpoint: Endpoint { scheme: "http", authority: "h".into(), path_style, account: ACCT.into() },
        },
    }
}

struct Sk {
    name: &'static str,
    method: &'static str,
    path: &'static str,
    query: &'static str,
    headers: &'static [(&'static str, &'static str)],
    path_style: bool,
}

fn check_sk(vectors: &[Sk], auth_of: &[&str]) {
    let accounts = StorageAccounts::new();
    for (v, auth) in vectors.iter().zip(auth_of) {
        let mut h: Vec<(&str, &str)> = v.headers.to_vec();
        h.push(("authorization", auth));
        let ok = req(v.method, v.path, v.query, &h, v.path_style);
        assert_eq!(verify_shared_key(&ok, StorageService::Blob, &accounts), Check::Valid, "{}", v.name);
        // the same signature on a different path, or the wrong endpoint style, is rejected
        let moved = req(v.method, &format!("{}x", v.path), v.query, &h, v.path_style);
        assert!(matches!(verify_shared_key(&moved, StorageService::Blob, &accounts), Check::Invalid(_)), "{} moved", v.name);
        let flipped = req(v.method, v.path, v.query, &h, !v.path_style);
        assert!(matches!(verify_shared_key(&flipped, StorageService::Blob, &accounts), Check::Invalid(_)), "{} flipped", v.name);
    }
}

const SK: &[Sk] = &[
    Sk {
        name: "list_blobs_host",
        method: "GET",
        path: "/cont",
        query: "restype=container&comp=list&prefix=a%2Fb&maxresults=5",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2026-10-06")],
        path_style: false,
    },
    Sk {
        name: "put_blob_unicode_host",
        method: "PUT",
        path: "/cont/dir%2Fhello%20w%C3%B6rld.txt",
        query: "",
        headers: &[
            ("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"),
            ("x-ms-version", "2026-10-06"),
            ("x-ms-blob-type", "BlockBlob"),
            ("Content-Type", "text/plain"),
            ("Content-Length", "4"),
            ("x-ms-meta-b", "2"),
            ("x-ms-meta-a-b", "x"),
            ("x-ms-meta-ab", "y"),
            ("If-None-Match", "*"),
        ],
        path_style: false,
    },
    Sk {
        name: "create_container_empty_body_host",
        method: "PUT",
        path: "/cont",
        query: "restype=container",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2026-10-06"), ("Content-Length", "0")],
        path_style: false,
    },
    Sk {
        name: "list_containers_path",
        method: "GET",
        path: "/",
        query: "comp=list",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2026-10-06")],
        path_style: true,
    },
    Sk {
        name: "get_blob_range_path",
        method: "GET",
        path: "/cont/dir%2Fhello%20w%C3%B6rld.txt",
        query: "",
        headers: &[
            ("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"),
            ("x-ms-version", "2026-10-06"),
            ("x-ms-range", "bytes=0-3"),
            ("If-Match", "\"0x8DAB86A4C0A1B2C\""),
        ],
        path_style: true,
    },
    Sk {
        name: "put_message_path_queue",
        method: "POST",
        path: "/myqueue/messages",
        query: "visibilitytimeout=5&messagettl=60",
        headers: &[
            ("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"),
            ("x-ms-version", "2026-10-06"),
            ("Content-Type", "application/xml; charset=utf-8"),
            ("Content-Length", "93"),
        ],
        path_style: true,
    },
    Sk {
        name: "get_messages_host_queue",
        method: "GET",
        path: "/myqueue/messages",
        query: "numofmessages=3",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2026-10-06")],
        path_style: false,
    },
    Sk {
        name: "lite_list_host",
        method: "GET",
        path: "/cont",
        query: "restype=container&comp=list",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2023-11-03")],
        path_style: false,
    },
    Sk {
        name: "lite_put_path",
        method: "PUT",
        path: "/cont/a%20b",
        query: "comp=metadata",
        headers: &[
            ("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"),
            ("x-ms-version", "2023-11-03"),
            ("x-ms-meta-k", "v"),
            ("Content-Type", "text/plain"),
        ],
        path_style: true,
    },
    Sk {
        name: "lite_queue_path",
        method: "GET",
        path: "/q1/messages",
        query: "peekonly=true&comp=x",
        headers: &[("Date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2023-11-03")],
        path_style: true,
    },
    Sk {
        name: "old_version_content_length_zero",
        method: "PUT",
        path: "/cont",
        query: "restype=container",
        headers: &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2014-02-14"), ("Content-Length", "0")],
        path_style: false,
    },
];

const SK_AUTH: &[&str] = &[
    "SharedKey devstoreaccount1:HEljY3NJJdYZJSDr66zlFbo0QMKu5gnfSChyMxSPlSE=",
    "SharedKey devstoreaccount1:uo45DtEr+6DxCDgFrwg5lWuCI8yjoc8OfGhrR0YYGOY=",
    "SharedKey devstoreaccount1:2t4EUYSkoroNkY5/OruMyMvZEtFkx5wJkdnIO1kmJs4=",
    "SharedKey devstoreaccount1:muDlz9RcQZoaInfX3mbakwPJHAqlbxTqMSt8EwBKpJY=",
    "SharedKey devstoreaccount1:UBhnh9Mi/LHDYX5+3IPKU438wwsxCc1ASwbRVpevRPA=",
    "SharedKey devstoreaccount1:s6nLfRVkEKTrtzE4cMVcVZoIcjNUEDyU92faCNk36Lc=",
    "SharedKey devstoreaccount1:JRTgJ5ROS8tRUFc7yvdSNKuC2sgRWse4UYAz79HCflk=",
    "SharedKeyLite devstoreaccount1:uiLVAWswU2otQK6IxRwlsaLSo5VkOjHgdA6GpY0U0ws=",
    "SharedKeyLite devstoreaccount1:hiO8kvYG8rlJEiaGI25l1EZ5SXatL/vk/skO6840BeE=",
    "SharedKeyLite devstoreaccount1:su5ny5MvKgfupBnMAPJmwkmpd9qwPo/yBcn2nhz+oHo=",
    "SharedKey devstoreaccount1:IeLG38f7AUvXuAAP4sYjhDQy98XyMRwjys3j6mH63WI=",
];

#[test]
fn shared_key_known_answers_host_and_path_style() {
    check_sk(SK, SK_AUTH);
}

#[test]
fn shared_key_placeholders_and_wrong_account_are_not_checked() {
    let accounts = StorageAccounts::new();
    let r = req("GET", "/c", "", &[("authorization", "SharedKey devstoreaccount1:replay")], false);
    assert_eq!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Skipped(Skip::Placeholder));
    let r = req("GET", "/c", "", &[("authorization", "SharedKey devstoreaccount1:Sanitized")], false);
    assert_eq!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Skipped(Skip::Placeholder));
    // 32 bytes of base64 that do not match
    let bad = B64.encode([7u8; 32]);
    let r = req("GET", "/c", "", &[("authorization", &format!("SharedKey devstoreaccount1:{bad}"))], false);
    assert!(matches!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Invalid(_)));
    // another account: key is invented by azemu, anything goes
    let mut r = req("GET", "/c", "", &[("authorization", &format!("SharedKey other:{bad}"))], false);
    r.account = "other".into();
    assert_eq!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Skipped(Skip::UnknownAccount));
}

struct Sas {
    name: &'static str,
    kind: &'static str,
    service: &'static str,
    query: &'static str,
    sts: &'static str,
    container: &'static str,
    blob: Option<&'static str>,
    extra_query: &'static str,
    udk: bool,
}
const SAS: &[Sas] = &[
    Sas { name: "account_latest", kind: "account", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=rwdlacup&sv=2026-10-06&ss=bfqt&srt=sco&sig=wzzxECNaEPxgOQHMZmJ0D9uIPp98PrDYM4hsrG93Lfo%3D", sts: "devstoreaccount1\nrwdlacup\nbfqt\nsco\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n\n\n2026-10-06\n\n", container: "", blob: None, extra_query: "", udk: false },
    Sas { name: "account_latest_ses", kind: "account", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=rwdlacup&spr=https%2Chttp&sv=2026-10-06&ss=bfqt&srt=sco&ses=scope1&sig=3mOVGRb0Jtfie5rkvuhlQex2G2vLEy2oPZb1aazVyio%3D", sts: "devstoreaccount1\nrwdlacup\nbfqt\nsco\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n\nhttps,http\n2026-10-06\nscope1\n", container: "", blob: None, extra_query: "", udk: false },
    Sas { name: "blob_latest", kind: "service", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=b&sig=IaggAxsM/y0MXs9kCTf%2BT8e8YCkyL3XT/hdnnlWz8%2Bw%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n\n\n\n2026-10-06\nb\n\n\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_snapshot_latest", kind: "service", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=bs&sig=Xp8JFfe7jV1VSni%2BsQAI%2BnCiywo6So%2B3iOTRc0cb2LI%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n\n\n\n2026-10-06\nbs\n2030-01-01T00:00:00.0000000Z\n\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "snapshot=2030-01-01T00:00:00.0000000Z", udk: false },
    Sas { name: "blob_version_latest", kind: "service", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=bv&sig=WQlSCZ4pJ7w10WwpM9X1rqmHo6K7gsiHLO5kaN%2BPF/Y%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n\n\n\n2026-10-06\nbv\n2030-01-01T00:00:00.0000000Z\n\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "versionid=2030-01-01T00:00:00.0000000Z", udk: false },
    Sas { name: "container_latest_headers", kind: "service", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sip=1.2.3.4&spr=https&sv=2026-10-06&sr=c&rscc=no-cache&rscd=attachment%3B%20filename%3Da%20b.txt&rsce=gzip&rscl=en-US&rsct=text/plain&sig=DOffDfnq4FhN6MxAQsVYWULQdqMh/iNX8H/qJv/RwUY%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n\n1.2.3.4\nhttps\n2026-10-06\nc\n\n\nno-cache\nattachment; filename=a b.txt\ngzip\nen-US\ntext/plain", container: "cont", blob: None, extra_query: "", udk: false },
    Sas { name: "queue_latest", kind: "service", service: "queue", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=rau&sip=1.2.3.4&spr=https&sv=2026-10-06&sig=nlaY7z49nKUndmLOnRu7KI20XGUVF677MdDT04w0tzc%3D", sts: "rau\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/queue/devstoreaccount1/myqueue\n\n1.2.3.4\nhttps\n2026-10-06", container: "myqueue", blob: None, extra_query: "", udk: false },
    Sas { name: "udk_blob_latest", kind: "udk", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=b&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2026-10-06&sig=KR0A4Hw8al2QuHOYEofBR82HR7XeDvsnejE8bTYE5dM%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2026-10-06\n\n\n\n\n\n\n\n2026-10-06\nb\n\n\n\n\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_latest", kind: "udk", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=c&rsct=text/plain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2026-10-06&sig=dlazG/MyY7REVqcvV4e5oaACUEc94QzDAgJVpqoC4Jk%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2026-10-06\n\n\n\n\n\n\n\n2026-10-06\nc\n\n\n\n\n\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "udk_blob_srh_srq", kind: "udk", service: "blob", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=r&sv=2026-10-06&sr=b&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2026-10-06&srh=foo,bar&srq=operation,identifier&sig=iWEk9tZrCV7yjgnSkncz7t11G1O3JCHtnujAh%2BRH2VM%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2026-10-06\n\n\n\n\n\n\n\n2026-10-06\nb\n\n\nfoo:123\nbar:456\n\n\noperation:update\nidentifier:abcd\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_queue_latest", kind: "udk", service: "queue", query: "st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sp=rau&sv=2026-10-06&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=q&skv=2026-10-06&sig=O83cxUqjkUI5fyAkW84dlISmGarMym25j84P3BjbsYE%3D", sts: "rau\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/queue/devstoreaccount1/myqueue\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nq\n2026-10-06\n\n\n\n\n2026-10-06", container: "myqueue", blob: None, extra_query: "", udk: true },
    Sas { name: "blob_blob_2015-04-05", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&sv=2015-04-05&sr=b&sig=Iu06j%2Fol9Tto8EOfuRtFcEc30BhHLq3Pcoj3FgoVRJ8%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n1.2.3.4\nhttps\n2015-04-05\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_container_2015-04-05", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&sv=2015-04-05&sr=c&sig=7hkxDTsEulTmxMH5rApal0WB1c13vB%2FFZQNrXNRvshg%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\npol1\n1.2.3.4\nhttps\n2015-04-05\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: false },
    Sas { name: "blob_blob_2018-11-09", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&sv=2018-11-09&sr=b&sig=b8Lo6AA76PVKU%2B2VRNt%2F21j94klLtTgIUCV1zaBpx84%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n1.2.3.4\nhttps\n2018-11-09\nb\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_container_2018-11-09", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&sv=2018-11-09&sr=c&sig=nrE31j0F2CFv%2Fc53XkZ3lUiR5UY5OvyiFRgEjAqsxrA%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\npol1\n1.2.3.4\nhttps\n2018-11-09\nc\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: false },
    Sas { name: "blob_snapshot_2018-11-09", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&sv=2018-11-09&sr=bs&sig=G130%2Bw7KtBPvgccqtGl7VKwhG%2BwaayRE7DBubrBZpcM%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n1.2.3.4\nhttps\n2018-11-09\nbs\n2030-01-01T00:00:00.0000000Z\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "snapshot=2030-01-01T00:00:00.0000000Z", udk: false },
    Sas { name: "blob_blob_2020-12-06", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&ses=scope1&sv=2020-12-06&sr=b&sig=kEQjfXMvcJsgJWXtihWmwCRCLvQA5NV0A4zZgL2QEW4%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n1.2.3.4\nhttps\n2020-12-06\nb\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_container_2020-12-06", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&ses=scope1&sv=2020-12-06&sr=c&sig=%2FktyrkcgrEsJxreVz9tRVd%2FrLde7TkWCze%2BG07KWypo%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\npol1\n1.2.3.4\nhttps\n2020-12-06\nc\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: false },
    Sas { name: "blob_snapshot_2020-12-06", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&ses=scope1&sv=2020-12-06&sr=bs&sig=nqUpiD2QDvR6EhD5X%2BOhgStBu2O%2BPLb6E3BeRRDQ3zo%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n1.2.3.4\nhttps\n2020-12-06\nbs\n2030-01-01T00:00:00.0000000Z\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "snapshot=2030-01-01T00:00:00.0000000Z", udk: false },
    Sas { name: "udk_blob_2018-11-09", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&sv=2018-11-09&sr=b&sig=VIU6A7PqDIBnKWSJdrSOJHFgP3zHPEBjijxoYv4l2Aw%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n1.2.3.4\nhttps\n2018-11-09\nb\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_2018-11-09", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&sv=2018-11-09&sr=c&sig=YmHjoW%2FjISLWZmU5nGpLnl8o9jhY4WV2zLPhVPejDoo%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n1.2.3.4\nhttps\n2018-11-09\nc\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "udk_blob_2020-02-10", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&sv=2020-02-10&sr=b&sig=erGTvuMJ77D%2FUZOvFAaX8PaM8vNhCHb622E%2FJaEyVHU%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n1.2.3.4\nhttps\n2020-02-10\nb\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_2020-02-10", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&sv=2020-02-10&sr=c&sig=2LVbsaI3yyQGQxh%2FNZWMQlU7UpGO%2BgbH2vx9Mp71hPc%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n1.2.3.4\nhttps\n2020-02-10\nc\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "udk_blob_2020-12-06", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2020-12-06&sr=b&sig=Daqnng2Yu2AlP1u3kyrAXyNddRWQge9kVzuU1Da1wB8%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n1.2.3.4\nhttps\n2020-12-06\nb\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_2020-12-06", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2020-12-06&sr=c&sig=7Wu1cfBKMq3COFFHI0H9hFwAWmNjYtIBq5go43SERaU%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n1.2.3.4\nhttps\n2020-12-06\nc\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "udk_blob_2025-07-05", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2025-07-05&sr=b&sig=R0zPw1cooiQRiWbuqhLylDid8FCSslb5tBNJvAW6jtg%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n\n\n1.2.3.4\nhttps\n2025-07-05\nb\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_2025-07-05", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2025-07-05&sr=c&sig=I%2Fi7eo8yUMPUwOnmZ0w4gfdR0T6GLXuQloLACHDM1ag%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n\n\n1.2.3.4\nhttps\n2025-07-05\nc\n\nscope1\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "udk_blob_2026-04-06", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2026-04-06&sr=b&sig=q%2BCkLihDD6xVzNlP%2Fe4SNOl6vJXc9zMhDLEW8f5CkXA%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n\n\n1.2.3.4\nhttps\n2026-04-06\nb\n\nscope1\n\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: true },
    Sas { name: "udk_container_2026-04-06", kind: "udk", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&rscc=no-cache&rsct=text%2Fplain&skoid=11111111-2222-3333-4444-555555555555&sktid=66666666-7777-8888-9999-000000000000&skt=2030-01-01T00%3A00%3A00Z&ske=2030-01-03T00%3A00%3A00Z&sks=b&skv=2023-11-03&ses=scope1&sv=2026-04-06&sr=c&sig=jAim2jJCskNJidEzsrSDoU3JeKH0cKOMpibF%2B9ZoX6U%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont\n11111111-2222-3333-4444-555555555555\n66666666-7777-8888-9999-000000000000\n2030-01-01T00:00:00Z\n2030-01-03T00:00:00Z\nb\n2023-11-03\n\n\n\n\n\n1.2.3.4\nhttps\n2026-04-06\nc\n\nscope1\n\n\nno-cache\n\n\n\ntext/plain", container: "cont", blob: None, extra_query: "", udk: true },
    Sas { name: "account_2015-04-05", kind: "account", service: "blob", query: "sp=rwdlacup&ss=bfqt&srt=sco&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&sv=2015-04-05&sig=WXsJsym0ssGYBWoOnqc2F4ha0O%2FBlV0R%2Fdllht84gt0%3D", sts: "devstoreaccount1\nrwdlacup\nbfqt\nsco\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n1.2.3.4\nhttps\n2015-04-05\n", container: "", blob: None, extra_query: "", udk: false },
    Sas { name: "account_2020-12-06", kind: "account", service: "blob", query: "sp=rwdlacup&ss=bfqt&srt=sco&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&sip=1.2.3.4&spr=https&ses=scope1&sv=2020-12-06&sig=a6ZUTzMnEVNnLspyoQjYXvpvRLvD35yz0SE4YausSKA%3D", sts: "devstoreaccount1\nrwdlacup\nbfqt\nsco\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n1.2.3.4\nhttps\n2020-12-06\nscope1\n", container: "", blob: None, extra_query: "", udk: false },
    Sas { name: "queue_2019-02-02", kind: "service", service: "queue", query: "sp=rau&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sip=1.2.3.4&spr=https&sv=2019-02-02&sig=5q%2FS5KIqpapT4JAg52JOrtrjDHhy%2FA0MKGGfziP0THM%3D", sts: "rau\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/queue/devstoreaccount1/myqueue\npol1\n1.2.3.4\nhttps\n2019-02-02", container: "myqueue", blob: None, extra_query: "", udk: false },
    Sas { name: "blob_2012-02-12_docs", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sv=2012-02-12&sr=b&sig=mFNuQ7YmsvDrhdaW32ejFcpcge2Au0raSdgHRczCs1I%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n2012-02-12", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_2013-08-15_docs", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sv=2013-08-15&rscc=no-cache&rsct=text%2Fplain&sr=b&sig=cfTDr5kc1QPIO3t%2FBmQ8VkLJiFTISBLiTPOQq50MbBA%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n2013-08-15\nno-cache\n\n\n\ntext/plain", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
    Sas { name: "blob_2015-02-21_docs", kind: "service", service: "blob", query: "sp=r&st=2030-01-01T00%3A00%3A00Z&se=2030-01-02T03%3A04%3A05Z&si=pol1&sv=2015-02-21&sr=b&sig=RkyjBWkqNciVxbcfbWLkC5CXDd%2B8aLv7zLcbSWuJc7c%3D", sts: "r\n2030-01-01T00:00:00Z\n2030-01-02T03:04:05Z\n/blob/devstoreaccount1/cont/dir/hello wörld.txt\npol1\n2015-02-21\n\n\n\n\n", container: "cont", blob: Some("dir/hello wörld.txt"), extra_query: "", udk: false },
];

fn sas_request(v: &Sas, query: &str) -> Request {
    let path = match (v.service, v.blob) {
        ("queue", _) => format!("/{}/messages", v.container),
        (_, Some(b)) => format!("/{}/{}", v.container, utf8_percent_encode(b, NON_ALPHANUMERIC).to_string().replace("%2F", "/")),
        _ => format!("/{}", v.container),
    };
    let q = if v.extra_query.is_empty() { query.to_string() } else { format!("{query}&{}", v.extra_query) };
    if query.contains("srh=") {
        // the request has to carry what the SAS names
        let q = format!("{q}&operation=update&identifier=abcd");
        return req("GET", &path, &q, &[("foo", "123"), ("bar", "456")], false);
    }
    req("GET", &path, &q, &[], false)
}

fn target<'a>(v: &'a Sas) -> SasTarget<'a> {
    if v.service == "queue" {
        SasTarget::Queue { queue: Some(v.container) }
    } else {
        SasTarget::Blob { container: v.container, blob: v.blob }
    }
}

fn service_of(v: &Sas) -> StorageService {
    if v.service == "queue" {
        StorageService::Queue
    } else {
        StorageService::Blob
    }
}

/// Resource path in the string to sign, and the snapshot / version time.
fn path_and_time(v: &Sas, f: &SasFields) -> (String, String) {
    let path = match (v.service, v.blob, f.sr.as_str()) {
        ("queue", _, _) => v.container.to_string(),
        (_, Some(b), "b" | "bs" | "bv" | "") => format!("{}/{b}", v.container),
        _ => v.container.to_string(),
    };
    let time = v.extra_query.split_once('=').map(|(_, t)| t.to_string()).unwrap_or_default();
    (path, time)
}

#[test]
fn string_to_sign_matches_every_vector() {
    for v in SAS {
        let r = sas_request(v, v.query);
        let f = SasFields::from_query(&r.query);
        let (path, time) = path_and_time(v, &f);
        let got = match v.kind {
            "account" => account_sas_string_to_sign(ACCT, &f),
            "service" => service_sas_string_to_sign(service_of(v), ACCT, &f, &path, &time, None),
            _ => {
                let (all, _) = user_delegation_sas_strings(service_of(v), ACCT, &f, &path, &time, &r);
                assert!(all.iter().any(|s| s == v.sts), "{}: none of {all:?} equals {:?}", v.name, v.sts);
                continue;
            }
        };
        assert_eq!(got, v.sts, "{}", v.name);
    }
}

#[test]
fn signatures_verify_and_tampering_is_rejected() {
    let accounts = StorageAccounts::new();
    for v in SAS {
        let mut query = v.query.to_string();
        if v.udk {
            // sign the independently produced string with the key azemu derives from the fields
            let r = sas_request(v, v.query);
            let f = SasFields::from_query(&r.query);
            let key = user_delegation_key(&accounts, ACCT, &f.skoid, &f.sktid, &f.skt, &f.ske, &f.sks, &f.skv);
            let sig = B64.encode(hmac_sha256(&key, v.sts.as_bytes()));
            let head = v.query.rsplit_once("sig=").map(|(h, _)| h).unwrap_or(v.query);
            query = format!("{head}sig={}", utf8_percent_encode(&sig, NON_ALPHANUMERIC));
        }
        let r = sas_request(v, &query);
        assert_eq!(verify_sas(&r, service_of(v), &accounts, &target(v)), Check::Valid, "{}", v.name);

        // one flipped signature character: well formed, wrong
        let sig = r.query.get("sig").unwrap().to_string();
        let mut bytes = B64.decode(&sig).unwrap();
        bytes[0] ^= 1;
        let bad = format!("{}sig={}", query.rsplit_once("sig=").unwrap().0, utf8_percent_encode(&B64.encode(bytes), NON_ALPHANUMERIC));
        let r = sas_request(v, &bad);
        let c = verify_sas(&r, service_of(v), &accounts, &target(v));
        let layout_unknown = c == Check::Skipped(Skip::Unverifiable);
        assert!(matches!(c, Check::Invalid(_)) || layout_unknown, "{}: {c:?}", v.name);
        if !layout_unknown {
            // a SAS for one resource does not open another
            if v.kind == "service" && v.service == "blob" && v.blob.is_some() {
                let other = SasTarget::Blob { container: v.container, blob: Some("other") };
                let r = sas_request(v, &query);
                assert!(matches!(verify_sas(&r, service_of(v), &accounts, &other), Check::Invalid(_)), "{} other blob", v.name);
            }
        }
    }
}

#[test]
fn sas_for_another_container_is_rejected() {
    let accounts = StorageAccounts::new();
    let v = SAS.iter().find(|v| v.name == "container_latest_headers").unwrap();
    let r = sas_request(v, v.query);
    let other = SasTarget::Blob { container: "other", blob: None };
    assert!(matches!(verify_sas(&r, StorageService::Blob, &accounts, &other), Check::Invalid(_)));
}

#[test]
fn newer_version_than_any_known_is_checked_with_the_newest_layout_and_refused() {
    let accounts = StorageAccounts::new();
    let sig = B64.encode([9u8; 32]);
    let q = format!("sv=2031-01-01&sr=b&sp=r&se=2030-01-01T00:00:00Z&sig={}", utf8_percent_encode(&sig, NON_ALPHANUMERIC));
    let r = req("GET", "/c/b", &q, &[], false);
    let t = SasTarget::Blob { container: "c", blob: Some("b") };
    match verify_sas(&r, StorageService::Blob, &accounts, &t) {
        Check::Invalid(d) => assert!(d.contains("sv=2031-01-01") && d.contains("AZEMU_LOOSE_AUTH"), "{d}"),
        other => panic!("a wrong signature with a too new sv must be refused, got {other:?}"),
    }
    // the same SAS with a correct signature for the newest layout is accepted, whatever the sv
    let key = B64.decode(accounts.key(ACCT, 1)).unwrap();
    let sts = format!("r\n\n2030-01-01T00:00:00Z\n/blob/{ACCT}/c/b\n\n\n\n2031-01-01\nb\n\n\n\n\n\n\n");
    let good = B64.encode(hmac_sha256(&key, sts.as_bytes()));
    let q = format!("sv=2031-01-01&sr=b&sp=r&se=2030-01-01T00:00:00Z&sig={}", utf8_percent_encode(&good, NON_ALPHANUMERIC));
    let r = req("GET", "/c/b", &q, &[], false);
    assert_eq!(verify_sas(&r, StorageService::Blob, &accounts, &t), Check::Valid);
    // the Go SDK's version is known: a wrong signature is refused without the too-new note
    let q = format!("sv=2026-12-06&sr=b&sp=r&se=2030-01-01T00:00:00Z&sig={}", utf8_percent_encode(&sig, NON_ALPHANUMERIC));
    let r = req("GET", "/c/b", &q, &[], false);
    match verify_sas(&r, StorageService::Blob, &accounts, &t) {
        Check::Invalid(d) => assert!(!d.contains("AZEMU_LOOSE_AUTH"), "{d}"),
        other => panic!("{other:?}"),
    }
    // an unknown account is never checked, nor is a placeholder
    let q = "sv=2023-11-03&sr=b&sp=r&se=2030-01-01T00:00:00Z&sig=Sanitized";
    let r = req("GET", "/c/b", q, &[], false);
    assert_eq!(verify_sas(&r, StorageService::Blob, &accounts, &t), Check::Skipped(Skip::Placeholder));
    let q = format!("sv=2023-11-03&sr=b&sp=r&se=2030-01-01T00:00:00Z&sig={}", utf8_percent_encode(&sig, NON_ALPHANUMERIC));
    let mut r = req("GET", "/c/b", &q, &[], false);
    r.account = "other".into();
    assert_eq!(verify_sas(&r, StorageService::Blob, &accounts, &t), Check::Skipped(Skip::UnknownAccount));
}

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

/// A header value with bytes above 0x7F. The Python and JavaScript SDKs sign the Unicode string
/// and send one Latin-1 byte per character; the Go SDK sends UTF-8. Both sign the UTF-8 bytes of the
/// characters, so both must verify. (The string to sign below is written out from the REST
/// documentation, "Authorize with Shared Key".)
#[test]
fn shared_key_non_ascii_header_value_latin1_and_utf8_on_the_wire() {
    let accounts = StorageAccounts::new();
    let key = B64.decode(accounts.key(ACCT, 1)).unwrap();
    let sts = format!(
        "PUT{}x-ms-date:Fri, 27 Mar 2026 01:00:51 GMT\nx-ms-meta-a:caf\u{e9}\nx-ms-version:2026-10-06\n/{ACCT}/cont\nrestype:container",
        "\n".repeat(12)
    );
    let sig = B64.encode(hmac_sha256(&key, sts.as_bytes()));
    for (label, value) in [("latin1", b"caf\xe9".to_vec()), ("utf8", "caf\u{e9}".as_bytes().to_vec())] {
        let mut r = req(
            "PUT",
            "/cont",
            "restype=container",
            &[("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"), ("x-ms-version", "2026-10-06"), ("Content-Length", "0")],
            false,
        );
        r.headers.insert("x-ms-meta-a", http::HeaderValue::from_bytes(&value).unwrap());
        r.headers.insert("authorization", format!("SharedKey {ACCT}:{sig}").parse().unwrap());
        assert_eq!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Valid, "{label}");
        // a different value is still rejected
        r.headers.insert("x-ms-meta-a", http::HeaderValue::from_bytes(b"caf\xe8").unwrap());
        assert!(matches!(verify_shared_key(&r, StorageService::Blob, &accounts), Check::Invalid(_)), "{label} changed");
    }
}

/// Table's Shared Key string to sign is `VERB, Content-MD5, Content-Type, Date, resource` (no
/// canonicalized headers, Date holds `x-ms-date` when there is one); Lite is `Date, resource`; the
/// resource carries `?comp=` and no other query parameter. Strings written from the REST documentation.
#[test]
fn table_shared_key_and_lite() {
    let accounts = StorageAccounts::new();
    let key = B64.decode(accounts.key(ACCT, 1)).unwrap();
    let date = "Fri, 27 Mar 2026 01:00:51 GMT";
    let sign = |s: &str| B64.encode(hmac_sha256(&key, s.as_bytes()));
    let with_auth = |mut r: Request, scheme: &str, sig: &str| {
        r.headers.insert("authorization", format!("{scheme} {ACCT}:{sig}").parse().unwrap());
        r
    };
    // Shared Key, x-ms-date and Content-Type, an unsigned query parameter and `comp`
    let r = req(
        "GET",
        "/mytable",
        "comp=acl&timeout=30",
        &[("x-ms-date", date), ("x-ms-version", "2019-02-02"), ("Content-Type", "application/json")],
        false,
    );
    let sts = format!("GET\n\napplication/json\n{date}\n/{ACCT}/mytable?comp=acl");
    assert_eq!(verify_shared_key_table(&with_auth(r.clone(), "SharedKey", &sign(&sts)), &accounts), Check::Valid);
    // Blob's string to sign (empty Date line, canonicalized headers) does not verify
    let blob_sts = format!(
        "GET\n\n\n\n\napplication/json\n\n\n\n\n\n\nx-ms-date:{date}\nx-ms-version:2019-02-02\n/{ACCT}/mytable\ncomp:acl\ntimeout:30"
    );
    assert!(matches!(verify_shared_key_table(&with_auth(r.clone(), "SharedKey", &sign(&blob_sts)), &accounts), Check::Invalid(_)));
    // Shared Key Lite
    let sts = format!("{date}\n/{ACCT}/mytable?comp=acl");
    assert_eq!(verify_shared_key_table(&with_auth(r.clone(), "SharedKeyLite", &sign(&sts)), &accounts), Check::Valid);
    // Date header only
    let r = req("GET", "/Tables", "", &[("Date", date)], false);
    let sts = format!("GET\n\n\n{date}\n/{ACCT}/Tables");
    assert_eq!(verify_shared_key_table(&with_auth(r, "SharedKey", &sign(&sts)), &accounts), Check::Valid);
    // path style: the account appears twice
    let r = req("GET", "/Tables", "", &[("x-ms-date", date)], true);
    let sts = format!("GET\n\n\n{date}\n/{ACCT}/{ACCT}/Tables");
    assert_eq!(verify_shared_key_table(&with_auth(r, "SharedKey", &sign(&sts)), &accounts), Check::Valid);
}

// ---- `+` in the query string: a space (`Query::parse`), a plus only as `%2B` ------------------------

#[test]
fn sas_signature_with_an_unencoded_plus_is_rejected_and_the_encoded_one_passes() {
    let accounts = StorageAccounts::new();
    let v = SAS.iter().find(|v| v.name == "blob_blob_2018-11-09").unwrap();
    assert!(v.query.contains("%2B"), "this vector's signature contains a plus");
    let ok = sas_request(v, v.query);
    assert_eq!(verify_sas(&ok, service_of(v), &accounts, &target(v)), Check::Valid);
    // percent-encoded in lower case is the same thing
    let lower = sas_request(v, &v.query.replace("%2B", "%2b"));
    assert_eq!(verify_sas(&lower, service_of(v), &accounts, &target(v)), Check::Valid);
    // a client that does not encode the plus sends a space as far as the service can tell: the signature does not match
    let raw = sas_request(v, &v.query.replace("%2B", "+"));
    assert!(raw.query.get("sig").unwrap().contains(' '));
    assert!(
        matches!(verify_sas(&raw, service_of(v), &accounts, &target(v)), Check::Invalid(_)),
        "an unencoded plus in sig must be refused"
    );
}

fn signed_list(query: &str, signed_value: &str) -> Request {
    // GET /cont?restype=container&comp=list&prefix=<signed_value>, Shared Key by the well-known key
    let key = B64.decode(accounts_key()).unwrap();
    let sts = format!(
        "GET\n\n\n\n\n\n\n\n\n\n\n\nx-ms-date:Fri, 27 Mar 2026 01:00:51 GMT\nx-ms-version:2026-10-06\n/{ACCT}/cont\ncomp:list\nprefix:{signed_value}\nrestype:container"
    );
    let sig = B64.encode(hmac_sha256(&key, sts.as_bytes()));
    req(
        "GET",
        "/cont",
        query,
        &[
            ("x-ms-date", "Fri, 27 Mar 2026 01:00:51 GMT"),
            ("x-ms-version", "2026-10-06"),
            ("authorization", &format!("SharedKey {ACCT}:{sig}")),
        ],
        false,
    )
}

fn accounts_key() -> String {
    StorageAccounts::new().key(ACCT, 1)
}

#[test]
fn shared_key_with_plus_space_and_percent_2b_in_a_query_value() {
    let accounts = StorageAccounts::new();
    let check = |r: &Request| verify_shared_key(r, StorageService::Blob, &accounts);
    // the Rust SDKs send a space as `+` and sign the space
    assert_eq!(check(&signed_list("restype=container&comp=list&prefix=dir+with+space", "dir with space")), Check::Valid);
    // `%20` and the signed space
    assert_eq!(check(&signed_list("restype=container&comp=list&prefix=dir%20with%20space", "dir with space")), Check::Valid);
    // a literal plus sent as `%2B` and signed as a plus (Python, JavaScript, Go)
    assert_eq!(check(&signed_list("restype=container&comp=list&prefix=a%2Bb", "a+b")), Check::Valid);
    // a client that sends a raw `+` and signed it as a plus is still accepted (Azure would not)
    assert_eq!(check(&signed_list("restype=container&comp=list&prefix=a+b", "a+b")), Check::Valid);
    // `%2B` signed as a space does not match
    assert!(matches!(check(&signed_list("restype=container&comp=list&prefix=a%2Bb", "a b")), Check::Invalid(_)));
}
