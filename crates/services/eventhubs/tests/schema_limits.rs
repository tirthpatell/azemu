//! The Schema Registry under hostile or very large schemas: the compatibility check must neither end the process nor take
//! exponential time nor keep the namespace's state locked, and the size of the registry grows in step with what is put into it.
//! These limits are the emulator's own (Azure's code is not available); `docs/services/schema-registry.md` names them.

mod common;

use std::time::{Duration, Instant};

use azemu_core::testing::TestResponse;
use common::*;
use serde_json::Value;

const AVRO: &str = "application/json; serialization=Avro";
const API: &str = "2023-07-01";

struct Reg {
    env: Env,
}

async fn reg() -> Reg {
    Reg { env: env_with(azemu_eventhubs::state::Shared::new(false, None)).await }
}

impl Reg {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.env.srv.url("eventhub", "myns"))
    }

    async fn put(&self, path: &str, body: &str) -> TestResponse {
        let h = [("authorization", "Bearer any-token"), ("content-type", AVRO)];
        self.env.srv.send("PUT", &self.url(&format!("{path}?api-version={API}")), &h, body.to_string()).await
    }

    async fn group(&self, name: &str, mode: &str) {
        let r = self
            .env
            .srv
            .send(
                "PUT",
                &format!("{}/_azemu/eventhubs/myns/schemagroups/{name}", self.env.srv.gateway_url()),
                &[("content-type", "application/json")],
                format!(r#"{{"schemaCompatibility":"{mode}"}}"#),
            )
            .await;
        assert!(r.status == 201 || r.status == 200, "{}", r.text());
    }
}

fn message(r: &TestResponse) -> String {
    r.json()["error"]["message"].as_str().unwrap_or_default().split(" TrackingId:").next().unwrap_or_default().to_string()
}

/// A root record whose fields hold records `A0..A{n}` side by side, each with one field typed by the name of the next
/// one: following the names goes `n` levels deep although the JSON is shallow. `extra` is one more field of the root.
fn chain(n: usize, extra: bool) -> String {
    let mut fields = Vec::new();
    for i in 0..=n {
        let next = if i < n { format!("\"A{}\"", i + 1) } else { "\"int\"".to_string() };
        fields.push(format!(r#"{{"name":"d{i}","type":{{"type":"record","name":"A{i}","fields":[{{"name":"n","type":{next}}}]}}}}"#));
    }
    if extra {
        fields.push(r#"{"name":"extra","type":"int","default":0}"#.to_string());
    }
    format!(r#"{{"type":"record","name":"Root","fields":[{}]}}"#, fields.join(","))
}

/// Level `k` has four fields of the type of level `k-1`: comparing it without remembering what was compared takes 4^levels steps.
fn fan(levels: usize, extra: bool) -> String {
    let mut fields = vec![r#"{"name":"L0","type":{"type":"record","name":"T0","fields":[{"name":"x","type":"int"}]}}"#.to_string()];
    for k in 1..=levels {
        let four: Vec<String> = (1..=4).map(|j| format!(r#"{{"name":"f{j}","type":"T{}"}}"#, k - 1)).collect();
        fields.push(format!(r#"{{"name":"L{k}","type":{{"type":"record","name":"T{k}","fields":[{}]}}}}"#, four.join(",")));
    }
    if extra {
        fields.push(r#"{"name":"extra","type":"int","default":0}"#.to_string());
    }
    format!(r#"{{"type":"record","name":"Root","fields":[{}]}}"#, fields.join(","))
}

#[tokio::test]
async fn names_that_lead_nine_thousand_levels_deep_are_refused_not_a_crash() {
    for mode in ["Backward", "Forward"] {
        let r = reg().await;
        let g = format!("g{mode}");
        r.group(&g, mode).await;
        let (v1, v2) = (chain(9000, false), chain(9000, true));
        assert!(v1.len() < 1_048_576, "{}", v1.len());
        assert_eq!(r.put(&format!("/$schemaGroups/{g}/schemas/s"), &v1).await.status, 204);
        let t = Instant::now();
        let second = r.put(&format!("/$schemaGroups/{g}/schemas/s"), &v2).await;
        assert!(t.elapsed() < Duration::from_secs(10), "took {:?}", t.elapsed());
        assert_eq!(second.status, 400, "{}", second.text());
        let m = message(&second);
        assert!(m.contains("compatibility check was stopped") && m.contains("64 levels"), "{m}");
        // the process is alive and the first version is still the latest
        let again = r.put(&format!("/$schemaGroups/{g}/schemas/s"), &v1).await;
        assert_eq!(again.header("schema-version"), Some("1"));
    }
}

#[tokio::test]
async fn a_schema_that_would_take_exponential_time_to_compare_is_answered_at_once() {
    let r = reg().await;
    r.group("g", "Backward").await;
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", &fan(12, false)).await.status, 204);
    let t = Instant::now();
    // a field with a default is added: compatible in both directions, and the answer is found by remembering pairs
    let second = r.put("/$schemaGroups/g/schemas/s", &fan(12, true)).await;
    assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    assert_eq!(second.status, 204, "{}", second.text());
    assert_eq!(second.header("schema-version"), Some("2"));
}

/// 150 fields whose type is a union of the same 150 records (written out in the first one, named in the others): every pair of
/// branches is a step, and the number of steps is far over the budget however the pairs are remembered.
fn wide(extra: bool) -> String {
    let n = 150;
    let names: Vec<String> = (0..n).map(|i| format!("\"W{i}\"")).collect();
    let inline: Vec<String> =
        (0..n).map(|i| format!(r#"{{"type":"record","name":"W{i}","fields":[{{"name":"x","type":"int"}}]}}"#)).collect();
    let mut fields = vec![format!(r#"{{"name":"d0","type":[{}]}}"#, inline.join(","))];
    for i in 1..n {
        fields.push(format!(r#"{{"name":"d{i}","type":[{}]}}"#, names.join(",")));
    }
    if extra {
        fields.push(r#"{"name":"extra","type":"int","default":0}"#.to_string());
    }
    format!(r#"{{"type":"record","name":"Root","fields":[{}]}}"#, fields.join(","))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_check_over_the_step_budget_is_a_clear_400_and_does_not_hold_other_requests_up() {
    let r = std::sync::Arc::new(reg().await);
    r.group("g", "Forward").await;
    let (w1, w2) = (wide(false), wide(true));
    assert!(w1.len() < 1_048_576, "{}", w1.len());
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", &w1).await.status, 204);

    let heavy = {
        let r = r.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            let resp = r.put("/$schemaGroups/g/schemas/s", &w2).await;
            (resp, t.elapsed())
        })
    };
    // meanwhile another request to the same namespace (the admin API takes the same lock): the slowest answer is what counts
    let mut slowest = Duration::ZERO;
    while !heavy.is_finished() {
        let t = Instant::now();
        let list = r.env.srv.send("GET", &format!("{}/_azemu/eventhubs/myns/schemagroups", r.env.srv.gateway_url()), &[], "").await;
        slowest = slowest.max(t.elapsed());
        assert_eq!(list.status, 200, "{}", list.text());
        tokio::task::yield_now().await;
    }
    let (resp, took) = heavy.await.unwrap();
    assert_eq!(resp.status, 400, "{}", resp.text());
    let m = message(&resp);
    assert!(m.contains("compatibility check was stopped") && m.contains("steps"), "{m}");
    eprintln!("the check took {took:?}; the slowest other request took {slowest:?}");
    if took > Duration::from_millis(100) {
        assert!(slowest < took / 2, "another request waited {slowest:?} while the check took {took:?}");
    }
}

#[tokio::test]
async fn json_nested_ten_thousand_levels_is_refused_by_the_parser() {
    let r = reg().await;
    r.group("g", "Backward").await;
    let arrays = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let a = r.put("/$schemaGroups/g/schemas/s", &arrays).await;
    assert_eq!(a.status, 400, "{}", a.text());
    // records inside records, written anonymously
    let mut rec = r#""int""#.to_string();
    for i in 0..10_000 {
        rec = format!(r#"{{"type":"record","name":"R{i}","fields":[{{"name":"f","type":{rec}}}]}}"#);
    }
    let b = r.put("/$schemaGroups/g/schemas/s", &rec).await;
    assert_eq!(b.status, 400, "{}", b.text());
    let m = message(&b);
    assert!(m.contains("nested") && m.contains("128"), "{m}");
    // and as a union of unions
    let unions =
        format!(r#"{{"type":"record","name":"U","fields":[{{"name":"f","type":{}"int"{}}}]}}"#, "[".repeat(10_000), "]".repeat(10_000));
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", &unions).await.status, 400);
    // a schema that is just under the parser's limit works
    let mut ok = r#""int""#.to_string();
    for _ in 0..30 {
        ok = format!(r#"{{"type":"array","items":{ok}}}"#);
    }
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", &ok).await.status, 204);
}

#[tokio::test]
async fn a_stored_schema_that_cannot_be_parsed_does_not_stop_a_new_version() {
    use azemu_eventhubs::schema_registry::{Compatibility, SchemaEntry, SchemaGroup, SchemaType, SchemaVersion};
    let r = reg().await;
    {
        let mut st = r.env.shared.state.lock();
        let mut g = SchemaGroup::new("g", SchemaType::Avro, Compatibility::Forward, 1);
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        let v = SchemaVersion { version: 1, kind: SchemaType::Avro, id: "a".repeat(32), content: deep, created_ms: 1 };
        g.schemas.insert("s".into(), SchemaEntry { name: "s".into(), versions: vec![v] });
        st.namespaces.entry("myns".into()).or_default().schema_groups.insert("g".into(), g);
    }
    let resp = r.put("/$schemaGroups/g/schemas/s", r#"{"type":"record","name":"R","fields":[]}"#).await;
    assert_eq!(resp.status, 204, "{}", resp.text());
    assert_eq!(resp.header("schema-version"), Some("2"));
}

#[tokio::test]
async fn registering_many_versions_stays_linear_and_has_a_cap() {
    let r = reg().await;
    r.group("g", "None").await;
    let rec = |i: usize| format!(r#"{{"type":"record","name":"R","fields":[{{"name":"f{i}","type":"int"}}]}}"#);
    let t = Instant::now();
    for i in 0..1000 {
        let resp = r.put("/$schemaGroups/g/schemas/s", &rec(i)).await;
        assert_eq!(resp.status, 204, "version {i}: {}", resp.text());
    }
    assert!(t.elapsed() < Duration::from_secs(60), "1000 versions took {:?}", t.elapsed());
    // the same content as an old version is that version, at any depth of the history, also with other white space and key order
    let old = r.put("/$schemaGroups/g/schemas/s", r#"{ "fields":[{"type":"int","name":"f7"}], "name":"R", "type":"record" }"#).await;
    assert_eq!((old.status, old.header("schema-version")), (204, Some("8")));
    let over = r.put("/$schemaGroups/g/schemas/s", &rec(1000)).await;
    assert_eq!((over.status, over.header("x-ms-error-code")), (403, Some("NotAvaliable")));
    let m = message(&over);
    assert!(m.contains("1000 versions") && m.contains("limit of the emulator"), "{m}");
    // another schema of the group is not affected
    assert_eq!(r.put("/$schemaGroups/g/schemas/other", &rec(1)).await.status, 204);
    let _ = Value::Null;
}

#[tokio::test]
async fn a_damaged_registry_file_is_an_error_that_names_the_file() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-schema-damaged-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = Reg { env: env_persistent(&dir).await };
    first.group("g", "None").await;
    assert_eq!(first.put("/$schemaGroups/g/schemas/s", r#"{"type":"record","name":"R","fields":[]}"#).await.status, 204);
    first.env.srv.shutdown().await.unwrap();
    let file = dir.join("eventhubs").join("schemas.state");
    let bytes = std::fs::read(&file).unwrap();
    for damaged in [bytes[..bytes.len() / 2].to_vec(), vec![0xc1; 64], Vec::new()] {
        std::fs::write(&file, &damaged).unwrap();
        let (svc, _amqp) = azemu_eventhubs::pair_with(azemu_eventhubs::state::Shared::new(true, None));
        let started = azemu_core::testing::TestServer::try_start_persistent(vec![svc], &dir, |b| b).await;
        match started {
            Ok(_) if damaged.is_empty() => {}
            Ok(_) => panic!("a damaged file was accepted: {} bytes", damaged.len()),
            Err(e) => assert!(format!("{e:#}").contains("schemas.state"), "{e:#}"),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
