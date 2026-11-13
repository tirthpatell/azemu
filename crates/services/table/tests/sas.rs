mod common;
use azemu_core::storage::StorageAccounts;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use common::*;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn sign(account: &str, sts: &str) -> String {
    let key = B64.decode(StorageAccounts::new().key(account, 1)).unwrap();
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).unwrap();
    m.update(sts.as_bytes());
    B64.encode(m.finalize().into_bytes())
}

const ACCT: &str = "devstoreaccount1";
const FUTURE: &str = "2099-01-01T00:00:00Z";
const PAST: &str = "2001-01-01T00:00:00Z";

#[derive(Default, Clone)]
struct TableSas<'a> {
    table: &'a str,
    sp: &'a str,
    st: &'a str,
    se: &'a str,
    si: &'a str,
    spr: &'a str,
    range: [&'a str; 4],
}

impl TableSas<'_> {
    fn token(&self) -> String {
        let sts = [
            self.sp,
            self.st,
            self.se,
            &format!("/table/{ACCT}/{}", self.table.to_lowercase()),
            self.si,
            "",
            self.spr,
            "2019-02-02",
            self.range[0],
            self.range[1],
            self.range[2],
            self.range[3],
        ]
        .join("\n");
        let mut q = vec![];
        for (k, v) in [
            ("st", self.st),
            ("se", self.se),
            ("sp", self.sp),
            ("spr", self.spr),
            ("sv", "2019-02-02"),
            ("si", self.si),
            ("tn", self.table),
            ("spk", self.range[0]),
            ("srk", self.range[1]),
            ("epk", self.range[2]),
            ("erk", self.range[3]),
        ] {
            if !v.is_empty() {
                q.push(format!("{k}={}", pct(v)));
            }
        }
        q.push(format!("sig={}", pct(&sign(ACCT, &sts))));
        q.join("&")
    }
}

fn account_token(sp: &str, ss: &str, srt: &str, se: &str) -> String {
    let sts = format!("devstoreaccount1\n{sp}\n{ss}\n{srt}\n\n{se}\n\n\n2019-02-02\n");
    format!("sv=2019-02-02&ss={ss}&srt={srt}&sp={sp}&se={}&sig={}", pct(se), pct(&sign(ACCT, &sts)))
}

async fn seeded() -> Env {
    let mut e = env().await;
    e.base = e.srv.url("table", ACCT);
    e.create_table("tbl").await;
    for (p, r) in [("p", "r"), ("q", "r")] {
        e.insert("tbl", &format!(r#"{{"PartitionKey":"{p}","RowKey":"{r}","v":1}}"#)).await;
    }
    e
}

/// A request that carries only the SAS (no Authorization header).
async fn with_sas(e: &Env, method: &str, path: &str, token: &str, body: &str) -> azemu_core::testing::TestResponse {
    with_sas_h(e, method, path, token, &[], body).await
}

async fn with_sas_h(
    e: &Env,
    method: &str,
    path: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> azemu_core::testing::TestResponse {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut h = vec![JSON];
    h.extend_from_slice(headers);
    e.srv.send(method, &format!("{}{path}{sep}{token}", e.base), &h, body.to_string()).await
}

const P: &str = "/tbl(PartitionKey='p',RowKey='r')";

#[tokio::test]
async fn table_sas_permissions() {
    let e = seeded().await;
    let read = TableSas { table: "tbl", sp: "r", se: FUTURE, ..Default::default() }.token();
    assert_eq!(with_sas(&e, "GET", P, &read, "").await.status, 200);
    assert_eq!(with_sas(&e, "GET", "/tbl()", &read, "").await.json()["value"].as_array().unwrap().len(), 2);
    let r = with_sas(&e, "POST", "/tbl", &read, r#"{"PartitionKey":"p","RowKey":"n"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let r = with_sas(&e, "DELETE", P, &read, "").await;
    assert_eq!(r.status, 403);
    // table-level operations are not available to a table SAS
    assert_eq!(with_sas(&e, "GET", "/Tables", &read, "").await.status, 403);

    for (sp, method, path, body, ok) in [
        ("a", "POST", "/tbl", r#"{"PartitionKey":"p","RowKey":"n"}"#, 201),
        ("u", "PATCH", P, r#"{"v":2}"#, 204),
        ("u", "PUT", P, r#"{"v":2}"#, 204),
        ("d", "DELETE", P, "", 204),
    ] {
        let t = TableSas { table: "tbl", sp, se: FUTURE, ..Default::default() }.token();
        let r = with_sas_h(&e, method, path, &t, &[("If-Match", "*")], body).await;
        assert_eq!(r.status, ok, "{sp} {method}: {}", r.text());
    }
    e.insert("tbl", r#"{"PartitionKey":"p","RowKey":"r","v":1}"#).await;
    // upsert (PUT without If-Match) needs add and update
    let u = TableSas { table: "tbl", sp: "u", se: FUTURE, ..Default::default() }.token();
    assert_eq!(with_sas(&e, "PUT", P, &u, r#"{"v":3}"#).await.status, 403);
    let au = TableSas { table: "tbl", sp: "au", se: FUTURE, ..Default::default() }.token();
    assert_eq!(with_sas(&e, "PUT", P, &au, r#"{"v":3}"#).await.status, 204);
}

#[tokio::test]
async fn table_sas_validity_and_binding() {
    let e = seeded().await;
    let get = |t: String| {
        let e = &e;
        async move { with_sas(e, "GET", P, &t, "").await }
    };
    let expired = get(TableSas { table: "tbl", sp: "r", st: "2000-01-01T00:00:00Z", se: PAST, ..Default::default() }.token()).await;
    assert_eq!((expired.status, code(&expired).as_str()), (403, "AuthorizationFailure"));
    let early = get(TableSas { table: "tbl", sp: "r", st: "2098-01-01T00:00:00Z", se: FUTURE, ..Default::default() }.token()).await;
    assert_eq!((early.status, code(&early).as_str()), (403, "AuthorizationFailure"));
    let https_only = get(TableSas { table: "tbl", sp: "r", se: FUTURE, spr: "https", ..Default::default() }.token()).await;
    assert_eq!((https_only.status, code(&https_only).as_str()), (403, "AuthorizationProtocolMismatch"));
    // a SAS for another table
    e.create_table("other").await;
    let other = get(TableSas { table: "other", sp: "r", se: FUTURE, ..Default::default() }.token()).await;
    assert_eq!(other.status, 403);
    // the table name is signed in lower case and compared ignoring case
    let upper = get(TableSas { table: "TBL", sp: "r", se: FUTURE, ..Default::default() }.token()).await;
    assert_eq!(upper.status, 200);
    // tampered signature or widened permissions
    let good = TableSas { table: "tbl", sp: "r", se: FUTURE, ..Default::default() }.token();
    let bad_sig = format!("{}AAAAAA", &good[..good.len() - 6]);
    let r = get(bad_sig).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    let r = get(good.replace("sp=r", "sp=rau")).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    // a placeholder signature (as in recorded traffic) is not checked
    let r = get("sv=Sanitized&se=Sanitized&sp=r&tn=tbl&sig=Sanitized".to_string()).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn table_sas_key_ranges() {
    let e = seeded().await;
    let rng = TableSas { table: "tbl", sp: "ra", se: FUTURE, range: ["q", "", "q", ""], ..Default::default() }.token();
    assert_eq!(with_sas(&e, "GET", "/tbl(PartitionKey='q',RowKey='r')", &rng, "").await.status, 200);
    let r = with_sas(&e, "GET", P, &rng, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
    // queries only see the permitted partition
    let r = with_sas(&e, "GET", "/tbl()", &rng, "").await;
    let rows: Vec<String> = r.json()["value"].as_array().unwrap().iter().map(|x| x["PartitionKey"].as_str().unwrap().to_string()).collect();
    assert_eq!(rows, ["q"]);
    let r = with_sas(&e, "POST", "/tbl", &rng, r#"{"PartitionKey":"other","RowKey":"x"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
    assert_eq!(with_sas(&e, "POST", "/tbl", &rng, r#"{"PartitionKey":"q","RowKey":"x"}"#).await.status, 201);

    // a single-entity range
    let one = TableSas { table: "tbl", sp: "a", se: FUTURE, range: ["test", "test1", "test", "test1"], ..Default::default() }.token();
    assert_eq!(with_sas(&e, "POST", "/tbl", &one, r#"{"PartitionKey":"test","RowKey":"test1"}"#).await.status, 201);
    assert_eq!(with_sas(&e, "POST", "/tbl", &one, r#"{"PartitionKey":"test","RowKey":"test2"}"#).await.status, 403);
}

#[tokio::test]
async fn table_sas_from_a_stored_access_policy() {
    let e = seeded().await;
    let acl = "<SignedIdentifiers><SignedIdentifier><Id>readers</Id><AccessPolicy><Start>2001-01-01T00:00:00Z</Start><Expiry>2099-01-01T00:00:00Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier><SignedIdentifier><Id>old</Id><AccessPolicy><Start>2001-01-01T00:00:00Z</Start><Expiry>2002-01-01T00:00:00Z</Expiry><Permission>r</Permission></AccessPolicy></SignedIdentifier></SignedIdentifiers>";
    assert_eq!(e.send("PUT", "/tbl?comp=acl", &[("Content-Type", "application/xml")], acl).await.status, 204);
    let t = TableSas { table: "tbl", si: "readers", ..Default::default() }.token();
    assert_eq!(with_sas(&e, "GET", P, &t, "").await.status, 200);
    let r = with_sas(&e, "POST", "/tbl", &t, r#"{"PartitionKey":"p","RowKey":"n"}"#).await;
    assert_eq!(r.status, 403);
    let t = TableSas { table: "tbl", si: "old", ..Default::default() }.token();
    assert_eq!(with_sas(&e, "GET", P, &t, "").await.status, 403, "the policy has expired");
    let t = TableSas { table: "tbl", si: "missing", ..Default::default() }.token();
    assert_eq!(with_sas(&e, "GET", P, &t, "").await.status, 403);
}

#[tokio::test]
async fn account_sas() {
    let e = seeded().await;
    let all = account_token("rwdlacu", "bfqt", "sco", FUTURE);
    assert_eq!(with_sas(&e, "GET", P, &all, "").await.status, 200);
    assert_eq!(with_sas(&e, "GET", "/Tables", &all, "").await.status, 200);
    assert_eq!(with_sas(&e, "GET", "/?restype=service&comp=properties", &all, "").await.status, 200);
    assert_eq!(with_sas(&e, "POST", "/Tables", &all, r#"{"TableName":"viasas"}"#).await.status, 201);
    assert_eq!(with_sas(&e, "DELETE", "/Tables('viasas')", &all, "").await.status, 204);

    // resource type: entities need `o`, listing needs `s` or `c`
    let c = account_token("r", "t", "c", FUTURE);
    let r = with_sas(&e, "GET", "/tbl()", &c, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationResourceTypeMismatch"));
    let o = account_token("r", "t", "o", FUTURE);
    assert_eq!(with_sas(&e, "GET", "/tbl()", &o, "").await.status, 200);
    let r = with_sas(&e, "GET", "/Tables", &o, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationResourceTypeMismatch"));
    // service: the table service letter is `t`
    let blob_only = account_token("r", "b", "sco", FUTURE);
    let r = with_sas(&e, "GET", "/tbl()", &blob_only, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationServiceMismatch"));
    // permissions
    let r = with_sas(&e, "POST", "/tbl", &o, r#"{"PartitionKey":"p","RowKey":"n"}"#).await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationPermissionMismatch"));
    let au = account_token("au", "t", "o", FUTURE);
    assert_eq!(with_sas(&e, "PUT", P, &au, r#"{"v":9}"#).await.status, 204);
    // expiry and tampering
    let old = account_token("r", "t", "o", PAST);
    let r = with_sas(&e, "GET", "/tbl()", &old, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthorizationFailure"));
    let r = with_sas(&e, "GET", "/tbl()", &o.replace("sp=r", "sp=rau"), "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    // access policies are not available through an account SAS
    let r = with_sas(&e, "GET", "/tbl?comp=acl", &all, "").await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn sas_applies_to_the_operations_inside_a_batch() {
    let e = seeded().await;
    let (a, b) = (r#"{"PartitionKey":"p","RowKey":"n1"}"#, r#"{"PartitionKey":"p","RowKey":"n2"}"#);
    let body = batch(&[("POST", "/tbl", &[], a), ("POST", "/tbl", &[], b)]);
    let send = |t: String| {
        let e = &e;
        let body = body.clone();
        async move { e.srv.send("POST", &format!("{}/$batch?{t}", e.base), &[BATCH_CT], body).await }
    };
    let ro = TableSas { table: "tbl", sp: "r", se: FUTURE, ..Default::default() }.token();
    let r = send(ro).await;
    assert_eq!(r.status, 202);
    assert_eq!(parts(&r)[0].0, 403);
    assert_eq!(e.row_keys("tbl", "").await.len(), 2);
    let add = account_token("a", "t", "o", FUTURE);
    let r = send(add).await;
    assert_eq!(parts(&r).iter().map(|p| p.0).collect::<Vec<_>>(), [201, 201]);
}

#[tokio::test]
async fn signatures_are_only_checked_for_the_development_account() {
    let forged = format!("sv=2019-02-02&se={}&sp=r&tn=tbl&sig={}", pct(FUTURE), "A".repeat(43) + "%3D");
    // some other account: its key is made up by azemu, nobody can sign for it, the SAS is honoured as written
    let other = env().await;
    other.create_table("tbl").await;
    assert_eq!(with_sas(&other, "GET", "/tbl()", &forged, "").await.status, 200);
    // the development account: the well-known key is checked
    let dev = seeded().await;
    let r = with_sas(&dev, "GET", "/tbl()", &forged, "").await;
    assert_eq!((r.status, code(&r).as_str()), (403, "AuthenticationFailed"));
    // permissions and expiry are still honoured for other accounts
    let expired = format!("sv=2019-02-02&se={}&sp=r&tn=tbl&sig={}", pct(PAST), "A".repeat(43) + "%3D");
    assert_eq!(with_sas(&other, "GET", "/tbl()", &expired, "").await.status, 403);
    assert_eq!(with_sas(&other, "POST", "/tbl", &forged, r#"{"PartitionKey":"p","RowKey":"r"}"#).await.status, 403);
}
