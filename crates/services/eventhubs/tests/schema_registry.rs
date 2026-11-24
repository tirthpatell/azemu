//! The Schema Registry REST API on the namespace host (`/$schemaGroups/...`). The texts and the headers are those of a Standard
//! namespace, checked in November 2026 (`docs/services/schema-registry.md`); a test that holds a guess says so.

mod common;

use azemu_core::testing::TestResponse;
use common::*;
use serde_json::{json, Value};

const AVRO: &str = "application/json; serialization=Avro";
const API: &str = "2023-07-01";

struct Reg {
    env: Env,
}

async fn reg() -> Reg {
    Reg { env: env_with(azemu_eventhubs::state::Shared::new(false, None)).await }
}

async fn lenient() -> Reg {
    Reg { env: env_with(azemu_eventhubs::state::Shared::new(true, None)).await }
}

impl Reg {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.env.srv.url("eventhub", "myns"))
    }

    async fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TestResponse {
        self.env.srv.send(method, &self.url(path), headers, body.to_string()).await
    }

    /// With a token and the api-version added to the path.
    async fn call(&self, method: &str, path: &str, ctype: Option<&str>, body: &str) -> TestResponse {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut h = vec![("authorization", "Bearer any-token")];
        if let Some(c) = ctype {
            h.push(("content-type", c));
        }
        self.raw(method, &format!("{path}{sep}api-version={API}"), &h, body).await
    }

    async fn get(&self, path: &str) -> TestResponse {
        self.call("GET", path, None, "").await
    }

    async fn put(&self, path: &str, body: &str) -> TestResponse {
        self.call("PUT", path, Some(AVRO), body).await
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
    if r.body.is_empty() {
        return String::new();
    }
    r.json()["error"]["message"].as_str().unwrap_or_default().to_string()
}

/// The message without the tracking suffix.
fn short(r: &TestResponse) -> String {
    let m = message(r);
    m.split(" TrackingId:").next().unwrap_or_default().to_string()
}

const ORDER1: &str = r#"{"type":"record","name":"Order","namespace":"azemu.test","fields":[{"name":"id","type":"string"}]}"#;
const ORDER2: &str =
    r#"{"type":"record","name":"Order","namespace":"azemu.test","fields":[{"name":"id","type":"string"},{"name":"qty","type":"int"}]}"#;

#[tokio::test]
async fn groups_are_listed_with_azures_paging_link() {
    let r = reg().await;
    let empty = r.get("/$schemaGroups").await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.header("content-type"), Some("application/json"));
    assert_eq!(empty.json()["Value"], json!([]));
    r.group("orders", "Forward").await;
    let v = r.get("/$schemaGroups").await.json();
    assert_eq!(v["Value"], json!(["orders"]));
    // Azure's link on a first page: skip is the number of the last item taken
    assert!(v["NextLink"].as_str().unwrap().ends_with("/$schemaGroups?api-version=2023-07-01&%24skip=0"), "{v}");
    assert!(v["NextLink"].as_str().unwrap().starts_with("http://myns.eventhub.localhost:"), "{v}");
    // with a $skip and no $top there is no link; the group's properties
    assert_eq!(r.get("/$schemaGroups?$skip=0").await.json()["NextLink"], Value::Null);
    let props = r.get("/$schemaGroups/orders").await.json();
    assert_eq!((props["SchemaCompatibility"].as_str(), props["SchemaType"].as_str()), (Some("Forward"), Some("Avro")));
    assert_eq!(props["GroupProperties"], json!({}));
    let missing = r.get("/$schemaGroups/nogroup").await;
    assert_eq!((missing.status, short(&missing)), (404, "Schema nogroup does not exist.".into()));
    // the path is not case sensitive
    assert_eq!(r.get("/$schemagroups").await.status, 200);
}

#[tokio::test]
async fn register_returns_the_same_id_for_the_same_content_and_a_new_version_for_changed_content() {
    let r = reg().await;
    r.group("orders", "None").await;
    let a = r.put("/$schemaGroups/orders/schemas/order", ORDER1).await;
    assert_eq!(a.status, 204, "{}", a.text());
    let id1 = a.header("schema-id").unwrap().to_string();
    assert_eq!(id1.len(), 32, "{id1}");
    assert!(id1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(a.header("schema-version"), Some("1"));
    assert_eq!(a.header("schema-group-name"), Some("orders"));
    assert_eq!(a.header("schema-name"), Some("order"));
    let host = "http://myns.eventhub.localhost:";
    assert!(a.header("location").unwrap().starts_with(host));
    assert!(a.header("location").unwrap().ends_with("/$schemagroups/orders/schemas/order/versions/1?api-version=2023-07-01"));
    assert!(a.header("schema-id-location").unwrap().ends_with(&format!("/$schemagroups/$schemas/{id1}?api-version=2023-07-01")));
    assert!(a.header("schema-versions-location").unwrap().ends_with("/$schemagroups/orders/schemas/order/versions?api-version=2023-07-01"));
    assert!(a.body.is_empty());

    // white space and the order of keys do not make a new schema
    let spaced = "{ \"type\": \"record\", \"name\": \"Order\", \"namespace\": \"azemu.test\",\n \"fields\": [ {\"type\": \"string\", \"name\": \"id\"} ] }";
    let b = r.put("/$schemaGroups/orders/schemas/order", spaced).await;
    assert_eq!((b.status, b.header("schema-id"), b.header("schema-version")), (204, Some(id1.as_str()), Some("1")));

    let c = r.put("/$schemaGroups/orders/schemas/order", ORDER2).await;
    assert_eq!((c.status, c.header("schema-version")), (204, Some("2")));
    let id2 = c.header("schema-id").unwrap().to_string();
    assert_ne!(id1, id2);
    // the same content under another name is another schema with its own id
    let d = r.put("/$schemaGroups/orders/schemas/copy", ORDER1).await;
    assert_eq!(d.header("schema-version"), Some("1"));
    assert_ne!(d.header("schema-id"), Some(id1.as_str()));

    // read it back: by version, the latest (0), by id (with and without dashes, any case)
    let v1 = r.get("/$schemaGroups/orders/schemas/order/versions/1").await;
    assert_eq!(v1.status, 200);
    assert_eq!(v1.header("content-type"), Some("application/json;serialization=Avro"));
    assert_eq!(v1.text(), ORDER1);
    assert_eq!(v1.header("schema-id"), Some(id1.as_str()));
    assert_eq!(r.get("/$schemaGroups/orders/schemas/order/versions/0").await.text(), ORDER2);
    let dashed = uuid::Uuid::parse_str(&id2).unwrap().to_string().to_uppercase();
    let by_id = r.get(&format!("/$schemaGroups/$schemas/{dashed}")).await;
    assert_eq!((by_id.status, by_id.text()), (200, ORDER2.to_string()));
    assert_eq!(by_id.header("schema-version"), Some("2"));
    assert_eq!(by_id.header("schema-id"), Some(id2.as_str()));
    // what Azure keeps and returns is the compact form
    let stored = r.put("/$schemaGroups/orders/schemas/spaced", "{ \"type\" : \"record\",\n \"name\":\"S\", \"fields\":[ ] }").await;
    assert_eq!(stored.status, 204);
    assert_eq!(r.get("/$schemaGroups/orders/schemas/spaced/versions/1").await.text(), r#"{"type":"record","name":"S","fields":[]}"#);

    // names are not case sensitive and are kept as they were first written
    let up = r.get("/$schemaGroups/ORDERS/schemas/ORDER/versions/2").await;
    assert_eq!((up.status, up.header("schema-name"), up.header("schema-group-name")), (200, Some("order"), Some("orders")));

    let list = r.get("/$schemaGroups/orders/schemas/order/versions").await.json();
    assert_eq!(list["Value"], json!([1, 2]));
    assert!(
        list["NextLink"].as_str().unwrap().ends_with("/$schemaGroups/orders/schemas/order/versions?api-version=2023-07-01&%24skip=1"),
        "{list}"
    );
    let names = r.get("/$schemaGroups/orders/schemas").await.json();
    assert_eq!(names["Value"], json!(["copy", "order", "spaced"]));
}

#[tokio::test]
async fn get_id_finds_the_version_that_has_this_content() {
    let r = reg().await;
    r.group("g", "None").await;
    let a = r.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    r.put("/$schemaGroups/g/schemas/s", ORDER2).await;
    for path in ["/$schemaGroups/g/schemas/s:get-id", "/$schemaGroups/g/schemas/s/:get-id"] {
        let f = r.call("POST", path, Some(AVRO), ORDER1).await;
        assert_eq!((f.status, f.header("schema-id"), f.header("schema-version")), (204, a.header("schema-id"), Some("1")), "{path}");
        assert!(f.body.is_empty());
    }
    // other content, an unknown schema, an unknown group, content that is not Avro: all "does not exist"
    let other = r.call("POST", "/$schemaGroups/g/schemas/s:get-id", Some(AVRO), r#"{"type":"record","name":"X","fields":[]}"#).await;
    assert_eq!((other.status, short(&other)), (404, "Schema g/s does not exist.".into()));
    assert!(message(&other).contains("SystemTracker:myns.servicebus.windows.net:$schemaGroups/g/schemas/s:get-id"));
    assert_eq!(other.header("x-ms-error-code"), Some("ItemNotFound"));
    assert_eq!(short(&r.call("POST", "/$schemaGroups/g/schemas/none:get-id", Some(AVRO), ORDER1).await), "Schema g/none does not exist.");
    assert_eq!(
        short(&r.call("POST", "/$schemaGroups/nogroup/schemas/s:get-id", Some(AVRO), ORDER1).await),
        "Schema nogroup/s does not exist."
    );
    assert_eq!(r.call("POST", "/$schemaGroups/g/schemas/s:get-id", Some(AVRO), r#"{"type":"foo"#).await.status, 404);
    assert_eq!(r.call("POST", "/$schemaGroups/g/schemas/s:get-id", Some("application/json; serialization=Json"), ORDER1).await.status, 404);
    let no_ct = r.call("POST", "/$schemaGroups/g/schemas/s:get-id", None, ORDER1).await;
    assert_eq!((no_ct.status, short(&no_ct)), (400, "Content-Type is required.".into()));
}

#[tokio::test]
async fn unknown_things_are_not_found_with_azures_texts() {
    let r = reg().await;
    r.group("g", "None").await;
    r.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    let nf = |what: TestResponse| (what.status, short(&what), what.header("x-ms-error-code").map(str::to_string));
    assert_eq!(
        nf(r.get("/$schemaGroups/g/schemas/none/versions/1").await),
        (404, "Schema g/none/ver.1 does not exist.".into(), Some("ItemNotFound".into()))
    );
    assert_eq!(nf(r.get("/$schemaGroups/g/schemas/s/versions/9").await).1, "Schema g/s/ver.9 does not exist.");
    assert_eq!(nf(r.get("/$schemaGroups/nogroup/schemas/s/versions/1").await).1, "Schema nogroup/s/ver.1 does not exist.");
    // an unknown schema or group has no versions, not an error; the link says -1
    let v = r.get("/$schemaGroups/g/schemas/none/versions").await.json();
    assert_eq!(v["Value"], json!([]));
    assert!(v["NextLink"].as_str().unwrap().ends_with("%24skip=-1"));
    assert_eq!(r.get("/$schemaGroups/nogroup/schemas/s/versions").await.json()["Value"], json!([]));
    assert_eq!(r.get("/$schemaGroups/nogroup/schemas").await.json()["Value"], json!([]));
    let unknown_id = r.get("/$schemaGroups/$schemas/258e22ae9c4d463abe61dbd251bdaab5").await;
    assert_eq!(nf(unknown_id).1, "Schema id 258e22ae9c4d463abe61dbd251bdaab5 does not exist.");
    let bad = r.get("/$schemaGroups/$schemas/notanid").await;
    assert_eq!((bad.status, short(&bad)), (400, "The request is invalid. Invalid schema Id format.".into()));
    let zero = r.get("/$schemaGroups/$schemas/00000000000000000000000000000000").await;
    assert_eq!((zero.status, short(&zero)), (400, "SubCode=40000, Missing schema id as input.".into()));
    let text = r.get("/$schemaGroups/g/schemas/s/versions/abc").await;
    assert_eq!((text.status, short(&text)), (400, "Schema version abc is not a valid version number.".into()));
    // no group, no registration (as long as nothing is auto-created)
    let put = r.put("/$schemaGroups/nogroup/schemas/s", ORDER1).await;
    assert_eq!(put.status, 404);
    assert_eq!(short(&put), "SubCode=40400, Schema group 'nogroup' does not exist while auto-create is turned off.");
    assert!(message(&put).contains("SystemTracker:NoSystemTracker"));
}

#[tokio::test]
async fn a_lenient_emulator_makes_the_group_that_is_named() {
    let r = lenient().await;
    let a = r.put("/$schemaGroups/made/schemas/s", ORDER1).await;
    assert_eq!(a.status, 204, "{}", a.text());
    let props = r.get("/$schemaGroups/made").await.json();
    assert_eq!((props["SchemaType"].as_str(), props["SchemaCompatibility"].as_str()), (Some("Avro"), Some("None")));
    assert_eq!(r.get("/$schemaGroups").await.json()["Value"], json!(["made"]));
    // reading a group that is not there is still not found
    assert_eq!(r.get("/$schemaGroups/other").await.status, 404);
}

#[tokio::test]
async fn avro_that_does_not_parse_is_refused_with_azures_text() {
    let r = reg().await;
    r.group("g", "None").await;
    let put = |body: &'static str| {
        let r = &r;
        async move {
            let x = r.put("/$schemaGroups/g/schemas/bad", body).await;
            (x.status, short(&x), x.header("x-ms-error-code").map(str::to_string))
        }
    };
    assert_eq!(
        put("{}").await,
        (400, "Avro schema validation failed: Property type is required at ''.".into(), Some("InvalidRequest".into()))
    );
    assert_eq!(put(r#"{"type":"record"}"#).await.1, "Avro schema validation failed: name cannot be null for record schema. at ''.");
    assert_eq!(put(r#"{"type":"record","name":"X"}"#).await.1, "Avro schema validation failed: 'fields' cannot be null for record at ''.");
    assert_eq!(
        put(r#"{"type":"record","name":"X","fields":[{"name":"a"}]}"#).await.1,
        "Avro schema validation failed: 'type' was not found for field: name at 'fields[0]'."
    );
    assert_eq!(put(r#"{"type":"enum","name":"E"}"#).await.1, "Avro schema validation failed: Enum has no symbols: E at ''.");
    assert_eq!(put(r#"{"type":"array"}"#).await.1, "Avro schema validation failed: Array does not have 'items' at ''.");
    assert!(put("[1]").await.1.starts_with("Avro schema validation failed: Error reading JObject from JsonReader."));
    assert!(put("hello").await.1.starts_with("Avro schema validation failed: Unexpected character encountered while parsing value: h."));
    // what Azure lets through
    assert_eq!(put(r#"{"type":"foo"}"#).await.0, 204);
    assert_eq!(put(r#"{"type":"record","name":"1bad","fields":[]}"#).await.0, 204);
    // an empty body
    let empty = r.put("/$schemaGroups/g/schemas/bad", "").await;
    assert_eq!(
        (empty.status, short(&empty)),
        (400, "Error when consuming request body: The body of the message cannot be read because it is empty.".into())
    );
}

#[tokio::test]
async fn content_types_as_azure_takes_them() {
    let r = reg().await;
    r.group("g", "None").await;
    let t = |ct: Option<&'static str>| {
        let r = &r;
        async move {
            let x = r.call("PUT", "/$schemaGroups/g/schemas/ct", ct, ORDER1).await;
            (x.status, short(&x), x.header("x-ms-error-code").map(str::to_string))
        }
    };
    assert_eq!(t(None).await, (400, "Content-Type is required.".into(), Some("InvalidRequest".into())));
    let missing = "Missing required header for PUT request. Schema/serialization type must be provided.";
    assert_eq!(t(Some("application/json")).await.1, missing);
    assert_eq!(
        t(Some("application/json; serialization=Thrift")).await,
        (415, "Invalid schema type for PUT request. 'thrift' is not supported.".into(), Some("InvalidSchemaType".into()))
    );
    // accepted spellings: any case, no space, a charset
    for ok in [
        "application/json; serialization=avro",
        "application/json;serialization=Avro",
        "application/json; serialization=Avro; charset=utf-8",
    ] {
        assert_eq!(t(Some(ok)).await.0, 204, "{ok}");
    }
    // XML in, XML out for the refusal of the type
    let xml = r.call("PUT", "/$schemaGroups/g/schemas/ct", Some("application/xml"), ORDER1).await;
    assert_eq!((xml.status, xml.header("content-type")), (400, Some("application/xml; charset=utf-8")));
    assert!(xml.text().starts_with("<Error><Code>400</Code><Detail>Missing required header for PUT request."), "{}", xml.text());
}

#[tokio::test]
async fn a_group_takes_only_its_own_kind() {
    let r = reg().await;
    r.group("avro", "None").await;
    let json_in_avro =
        r.call("PUT", "/$schemaGroups/avro/schemas/s", Some("application/json; serialization=Json"), r#"{"type":"object"}"#).await;
    assert_eq!(json_in_avro.status, 415);
    assert_eq!(json_in_avro.header("x-ms-error-code"), Some("InvalidSchemaType"));
    assert_eq!(
        short(&json_in_avro),
        "SubCode=41500, The messaging entity 'Schema of type Json cannot be created within group 'avro' that supports Avro schema type.' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions. ."
    );
    let custom = r.call("PUT", "/$schemaGroups/avro/schemas/s", Some("text/plain; charset=utf-8"), "hello").await;
    assert!(short(&custom).contains("Schema of type Custom cannot be created within group 'avro'"));
    let proto = r.call("PUT", "/$schemaGroups/avro/schemas/s", Some("text/vnd.ms.protobuf"), "syntax = \"proto3\";").await;
    assert!(short(&proto).contains("Schema of type Protobuf cannot be created"));
    // the content is judged first: broken JSON is a 400 even in a group of another kind
    let broken = r.call("PUT", "/$schemaGroups/avro/schemas/s", Some("application/json; serialization=Json"), r#"{"type":"#).await;
    assert_eq!(broken.status, 400);
    assert!(short(&broken).starts_with("Json schema validation failed: "), "{}", short(&broken));

    // groups of the other kinds (what Azure does with them was not tried: documented as a guess)
    for (ty, ct, body, back) in [
        ("Json", "application/json; serialization=Json", r#"{ "type": "object" }"#, "application/json;serialization=Json"),
        ("Custom", "text/plain; charset=utf-8", "any text at all", "text/plain; charset=utf-8"),
        ("Protobuf", "text/vnd.ms.protobuf", "syntax = \"proto3\";", "text/vnd.ms.protobuf"),
    ] {
        let name = ty.to_lowercase();
        let made = r
            .env
            .srv
            .send(
                "PUT",
                &format!("{}/_azemu/eventhubs/myns/schemagroups/{name}", r.env.srv.gateway_url()),
                &[("content-type", "application/json")],
                format!(r#"{{"schemaType":"{ty}"}}"#),
            )
            .await;
        assert_eq!(made.status, 201);
        let put = r.call("PUT", &format!("/$schemaGroups/{name}/schemas/s"), Some(ct), body).await;
        assert_eq!(put.status, 204, "{ty}: {}", put.text());
        let get = r.get(&format!("/$schemaGroups/{name}/schemas/s/versions/1")).await;
        assert_eq!(
            (get.header("content-type"), get.text()),
            (Some(back), if ty == "Json" { r#"{"type":"object"}"#.to_string() } else { body.to_string() })
        );
        // the same content again is the same version; other content is a new one (no compatibility rule outside Avro)
        assert_eq!(r.call("PUT", &format!("/$schemaGroups/{name}/schemas/s"), Some(ct), body).await.header("schema-version"), Some("1"));
        let other = r.call("PUT", &format!("/$schemaGroups/{name}/schemas/s"), Some(ct), &format!("{body} ")).await;
        let expect = if ty == "Json" { "1" } else { "2" }; // a Json schema is compared as JSON, white space does not count
        assert_eq!(other.header("schema-version"), Some(expect), "{ty}");
    }
}

#[tokio::test]
async fn compatibility_modes_check_the_latest_version() {
    let r = reg().await;
    r.group("fwd", "Forward").await;
    r.group("bwd", "Backward").await;
    r.group("none", "None").await;
    let rec = |fields: &str| format!(r#"{{"type":"record","name":"R","fields":[{fields}]}}"#);
    let (a, b) = (r#"{"name":"a","type":"string"}"#, r#"{"name":"b","type":"int"}"#);
    let bd = r#"{"name":"b","type":"int","default":0}"#;

    // Forward: the old schema reads what the new writes: a field may be added, one without a default may not go
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/s", &rec(&format!("{a},{b}"))).await.status, 204);
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/s", &rec(&format!("{a},{bd}"))).await.header("schema-version"), Some("2"));
    // version 3 is only checked against version 2 (b has a default there), version 1 would refuse it
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/s", &rec(a)).await.header("schema-version"), Some("3"));
    let refused = r.put("/$schemaGroups/fwd/schemas/s", &rec(b)).await;
    assert_eq!(refused.status, 400);
    let m = short(&refused);
    let id3 = r.get("/$schemaGroups/fwd/schemas/s/versions/3").await.header("schema-id").unwrap().to_string();
    let dashed = uuid::Uuid::parse_str(&id3).unwrap();
    assert_eq!(
        m,
        format!("Avro schema validation failed: Forward compatibility test failed when checking against schema s, version 3, id {dashed}.")
    );
    assert!(message(&refused).contains("SystemTracker:myns.servicebus.windows.net:$schemaGroups/fwd/schemas/s"), "{}", message(&refused));
    assert_eq!(r.get("/$schemaGroups/fwd/schemas/s/versions").await.json()["Value"], json!([1, 2, 3]));
    // the latest content again is the latest version, an older content that still fits is that older version
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/s", &rec(a)).await.header("schema-version"), Some("3"));
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/s", &rec(&format!("{a},{b}"))).await.header("schema-version"), Some("1"));

    // content that is an older version but does not fit the latest is refused, as Azure did
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/t", &rec(a)).await.status, 204);
    assert_eq!(r.put("/$schemaGroups/fwd/schemas/t", &rec(&format!("{a},{b}"))).await.status, 204);
    assert_eq!(
        r.put("/$schemaGroups/fwd/schemas/t", &rec(a)).await.status,
        400,
        "the content of version 1, but the latest version needs b"
    );

    // Backward: the new schema reads what the old wrote: a field may go, one may be added only with a default
    assert_eq!(r.put("/$schemaGroups/bwd/schemas/s", &rec(&format!("{a},{b}"))).await.status, 204);
    assert_eq!(r.put("/$schemaGroups/bwd/schemas/s", &rec(a)).await.header("schema-version"), Some("2"));
    let refused = r.put("/$schemaGroups/bwd/schemas/s", &rec(&format!("{a},{b}"))).await;
    assert_eq!(
        (
            refused.status,
            short(&refused).starts_with(
                "Avro schema validation failed: Backward compatibility test failed when checking against schema s, version 2, id "
            )
        ),
        (400, true)
    );
    assert_eq!(r.put("/$schemaGroups/bwd/schemas/s", &rec(&format!("{a},{bd}"))).await.header("schema-version"), Some("3"));

    // None: everything goes
    assert_eq!(r.put("/$schemaGroups/none/schemas/s", &rec(a)).await.status, 204);
    assert_eq!(r.put("/$schemaGroups/none/schemas/s", &rec(b)).await.header("schema-version"), Some("2"));
}

#[tokio::test]
async fn the_size_of_a_schema_is_limited_to_one_mebibyte() {
    let r = reg().await;
    r.group("g", "None").await;
    let sized = |n: usize| {
        let base = r#"{"type":"record","name":"Big","doc":"","fields":[]}"#;
        format!(r#"{{"type":"record","name":"Big","doc":"{}","fields":[]}}"#, "x".repeat(n - base.len()))
    };
    let ok = r.put("/$schemaGroups/g/schemas/big", &sized(1_048_576)).await;
    assert_eq!(ok.status, 204, "{}", ok.text());
    let over = r.put("/$schemaGroups/g/schemas/big2", &sized(1_048_577)).await;
    assert_eq!((over.status, over.header("x-ms-error-code")), (403, Some("NotAvaliable")));
    assert_eq!(
        short(&over),
        "SubCode=40300, Cannot create schema big2 because size of schema data cannot exceed 1048576 bytes. Right now big2 data is 1048577 bytes."
    );
}

#[tokio::test]
async fn names_are_checked_as_azure_checks_them() {
    let r = reg().await;
    r.group("g", "None").await;
    let get = |name: String| {
        let r = &r;
        async move {
            let x = r.get(&format!("/$schemaGroups/g/schemas/{name}/versions/1")).await;
            (x.status, short(&x))
        }
    };
    assert_eq!(get("a".repeat(101)).await, (400, "Name cannot exceed 100 characters.".into()));
    assert_eq!(get("a".repeat(100)).await.0, 404, "100 characters is the limit of Azure, the REST specification says 50");
    assert_eq!(get("a$b".into()).await, (400, "Invalid character '$' detected in name 'a$b'.".into()));
    assert_eq!(get("-a".into()).await, (400, "Name must start and end with letter or digit.".into()));
    assert_eq!(get("a-".into()).await, (400, "Name must start and end with letter or digit.".into()));
    assert_eq!(get("a_b".into()).await.0, 404);
    assert_eq!(get("a.b".into()).await.0, 404);
    let space = get("a%20b".into()).await;
    assert_eq!(space.0, 400);
    assert!(space.1.starts_with("Uri path '/$schemaGroups/g/schemas/a%20b/versions/1' contain invalid characters. Schema name and schema group name must consisted of letters, numbers, '-', and '.' only."));
    // a colon ends the name: `a:x` is the schema `a`
    assert_eq!(get("a:x".into()).await, (404, "Schema g/a/ver.1 does not exist.".into()));
}

#[tokio::test]
async fn credentials_and_the_api_version_are_checked() {
    let r = reg().await;
    r.group("g", "None").await;
    let t = |h: Vec<(&'static str, &'static str)>, path: &'static str| {
        let r = &r;
        async move {
            let x = r.raw("GET", path, &h, "").await;
            (x.status, short(&x), x.header("x-ms-error-code").map(str::to_string))
        }
    };
    let ok = "/$schemaGroups?api-version=2023-07-01";
    assert_eq!(t(vec![], ok).await, (401, "Missing authorization token.".into(), Some("ErrorInRequest".into())));
    assert_eq!(
        t(vec![("authorization", "Basic YTpi")], ok).await.1,
        "SubCode=40105, MalformedToken: Invalid authorization header: The request is missing WRAP authorization credentials."
    );
    // any token passes (the emulator knows no keys)
    assert_eq!(t(vec![("authorization", "Bearer anything")], ok).await.0, 200);
    let bearer = vec![("authorization", "Bearer x")];
    assert_eq!(
        t(bearer.clone(), "/$schemaGroups").await,
        (400, "No query parameter found. At least api-version should be provided.".into(), Some("InvalidRequest".into()))
    );
    assert_eq!(t(bearer.clone(), "/$schemaGroups?api-version=").await.1, "api-version should be provided.");
    assert_eq!(t(bearer.clone(), "/$schemaGroups?x=1").await.1, "api-version should be provided.");
    let unsupported = t(bearer.clone(), "/$schemaGroups?api-version=2099-01-01").await;
    assert_eq!(unsupported.0, 400);
    assert!(unsupported.1.starts_with(
        "SubCode=40000, The api-version in the query string is not supported. Either remove it from the Uri or use one of '2012-03,"
    ));
    assert!(unsupported.1.ends_with("2023-07-01,2024-05'."), "{}", unsupported.1);
    // the older versions of the SDKs, and the link carries the version that was asked for
    for v in ["2021-10", "2022-10", "2023-07-01", "2024-05"] {
        let x = r.raw("GET", &format!("/$schemaGroups?api-version={v}"), &bearer, "").await;
        assert_eq!(x.status, 200, "{v}");
        assert!(x.json()["NextLink"].as_str().unwrap().contains(&format!("api-version={v}&")), "{v}");
    }
    let put = r
        .raw("PUT", "/$schemaGroups/g/schemas/s?api-version=2021-10", &[("authorization", "Bearer x"), ("content-type", AVRO)], ORDER1)
        .await;
    assert!(put.header("location").unwrap().ends_with("/versions/1?api-version=2021-10"));
}

#[tokio::test]
async fn other_requests_are_refused_as_azure_refuses_them() {
    let r = reg().await;
    r.group("g", "None").await;
    let post = r.call("POST", "/$schemaGroups", Some(AVRO), "x").await;
    assert_eq!(
        (post.status, short(&post)),
        (400, "The given path $schemaGroups is not a valid path for SchemaRegistry protocol handler's Put or Post request.".into())
    );
    let put = r.call("PUT", "/$schemaGroups/g/schemas/s/versions/1", Some(AVRO), "x").await;
    assert_eq!(short(&put), "The given path $schemaGroups/g/schemas/s/versions/1 is not a valid path for SchemaRegistry protocol handler's Put or Post request.");
    assert_eq!(r.call("HEAD", "/$schemaGroups", None, "").await.status, 400);
    // something Azure has that this emulator has not: said so
    let patch = r.call("PATCH", "/$schemaGroups/g", None, "").await;
    assert_eq!(patch.status, 501);
}

#[tokio::test]
async fn schemas_and_versions_can_be_deleted_and_deleting_what_is_not_there_is_fine() {
    let r = reg().await;
    r.group("g", "None").await;
    r.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    let v2 = r.put("/$schemaGroups/g/schemas/s", ORDER2).await;
    let id1 = r.get("/$schemaGroups/g/schemas/s/versions/1").await.header("schema-id").unwrap().to_string();
    assert_eq!(r.call("DELETE", "/$schemaGroups/g/schemas/s/versions/1", None, "").await.status, 204);
    assert_eq!(r.get("/$schemaGroups/g/schemas/s/versions").await.json()["Value"], json!([2]));
    assert_eq!(r.get("/$schemaGroups/g/schemas/s/versions/1").await.status, 404);
    assert_eq!(r.get(&format!("/$schemaGroups/$schemas/{id1}")).await.status, 404);
    // a new version takes the number after the latest one
    let v3 = r.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    assert_eq!(v3.header("schema-version"), Some("3"));
    for path in [
        "/$schemaGroups/g/schemas/s/versions/1",
        "/$schemaGroups/g/schemas/s/versions/7",
        "/$schemaGroups/g/schemas/none",
        "/$schemaGroups/nogroup/schemas/none",
    ] {
        assert_eq!(r.call("DELETE", path, None, "").await.status, 204, "{path}");
    }
    let bad = r.call("DELETE", "/$schemaGroups/g/schemas/s/versions/x", None, "").await;
    assert_eq!((bad.status, short(&bad)), (400, "Schema version x is not a valid version number.".into()));
    // DELETE on the schema takes away its latest version only (as Azure does): 3, then 2
    assert_eq!(r.get("/$schemaGroups/g/schemas/s/versions").await.json()["Value"], json!([2, 3]));
    assert_eq!(r.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await.status, 204);
    assert_eq!(r.get("/$schemaGroups/g/schemas/s/versions").await.json()["Value"], json!([2]));
    // the number of the version that went is given again
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", ORDER1).await.header("schema-version"), Some("3"));
    assert_eq!(r.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await.status, 204);
    assert_eq!(r.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await.status, 204);
    // the last version takes the schema with it; a new one starts again at 1
    assert_eq!(r.get("/$schemaGroups/g/schemas").await.json()["Value"], json!([]));
    assert_eq!(r.get(&format!("/$schemaGroups/$schemas/{}", v2.header("schema-id").unwrap())).await.status, 404);
    assert_eq!(r.get("/$schemaGroups/g/schemas/s/versions/0").await.status, 404);
    assert_eq!(r.put("/$schemaGroups/g/schemas/s", ORDER2).await.header("schema-version"), Some("1"));
    // a reader's role is not checked here: anyone with a token may delete (docs: known differences)
}

#[tokio::test]
async fn lists_are_cut_by_skip_and_top_as_azure_cuts_them() {
    let r = reg().await;
    r.group("g", "None").await;
    for i in 0..3 {
        r.put("/$schemaGroups/g/schemas/s", &format!(r#"{{"type":"record","name":"R{i}","fields":[]}}"#)).await;
    }
    let l = |q: &'static str| {
        let r = &r;
        async move {
            let v = r.get(&format!("/$schemaGroups/g/schemas/s/versions{q}")).await.json();
            let link = v["NextLink"].as_str().map(|l| l.split("versions?").nth(1).unwrap_or_default().to_string());
            (v["Value"].clone(), link)
        }
    };
    assert_eq!(l("").await, (json!([1, 2, 3]), Some("api-version=2023-07-01&%24skip=2".into())));
    assert_eq!(l("?$skip=0").await, (json!([1, 2, 3]), None));
    assert_eq!(l("?$skip=2").await, (json!([3]), None));
    assert_eq!(l("?$skip=3").await, (json!([]), None));
    assert_eq!(l("?$top=2").await, (json!([1, 2]), Some("api-version=2023-07-01&%24top=2&%24skip=1".into())));
    assert_eq!(l("?$top=2&$skip=1").await, (json!([2, 3]), None));
    assert_eq!(l("?$top=1&$skip=1").await, (json!([2]), Some("api-version=2023-07-01&%24top=1&%24skip=2".into())));
    assert_eq!(l("?$top=0").await, (json!([]), Some("api-version=2023-07-01&%24top=0&%24skip=-1".into())));
    assert_eq!(l("?$top=x").await.0, json!([1, 2, 3]));
    assert_eq!(l("?$skip=-1").await.0, json!([1, 2, 3]));
}

#[tokio::test]
async fn groups_come_from_the_admin_api_the_config_file_and_go_with_reset() {
    let r = reg().await;
    let admin = |m: &'static str, p: &'static str, b: &'static str| {
        let r = &r;
        async move {
            let x = r
                .env
                .srv
                .send(
                    m,
                    &format!("{}/_azemu/eventhubs/myns/schemagroups{p}", r.env.srv.gateway_url()),
                    &[("content-type", "application/json")],
                    b.to_string(),
                )
                .await;
            (x.status, if x.body.is_empty() { Value::Null } else { x.json() })
        }
    };
    let (st, v) =
        admin("PUT", "/orders", r#"{"schemaType":"Avro","schemaCompatibility":"Backward","groupProperties":{"owner":"team"}}"#).await;
    assert_eq!((st, v["schemaCompatibility"].clone(), v["groupProperties"].clone()), (201, json!("Backward"), json!({"owner": "team"})));
    // again: 200; the mode changes; the kind cannot once there are schemas
    assert_eq!(admin("PUT", "/orders", r#"{"schemaCompatibility":"Forward"}"#).await.0, 200);
    r.put("/$schemaGroups/orders/schemas/s", ORDER1).await;
    let (st, v) = admin("PUT", "/orders", r#"{"schemaType":"Json"}"#).await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(admin("PUT", "/orders", r#"{"schemaType":"Thrift"}"#).await.0, 400);
    assert_eq!(admin("PUT", "/orders", r#"{"schemaCompatibility":"Sideways"}"#).await.0, 400);
    let (st, v) = admin("GET", "", "").await;
    assert_eq!((st, v["schemaGroups"][0]["schemas"][0]["name"].clone()), (200, json!("s")));
    assert_eq!(admin("GET", "/nope", "").await.0, 404);
    // the namespace exists for the shared AMQP port as soon as it has a group
    assert!(r.env.shared.knows_namespace("myns"));

    // per-namespace reset drops the groups; so does the whole reset
    assert!(r.env.shared.reset_namespace("myns"));
    assert_eq!(r.get("/$schemaGroups").await.json()["Value"], json!([]));
    admin("PUT", "/again", "").await;
    r.env.srv.reset();
    assert_eq!(r.get("/$schemaGroups").await.json()["Value"], json!([]));
    admin("PUT", "/last", "").await;
    assert_eq!(admin("DELETE", "/last", "").await.0, 204);
    assert_eq!(admin("DELETE", "/last", "").await.0, 404);
    admin("PUT", "/last", "").await;
    r.env.shared.delete_namespace("myns");
    assert_eq!(r.get("/$schemaGroups").await.json()["Value"], json!([]));
}

#[tokio::test]
async fn the_config_file_declares_groups() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-schema-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Config.json");
    std::fs::write(
        &path,
        r#"{"UserConfig":{"NamespaceConfig":[{"Name":"myns","Entities":[{"Name":"h","PartitionCount":1}],
            "SchemaGroups":[{"Name":"orders","SchemaType":"Avro","SchemaCompatibility":"Forward"},{"Name":"plain"}]}]}}"#,
    )
    .unwrap();
    let shared = azemu_eventhubs::state::Shared::new(false, Some(path.clone()));
    shared.load_config();
    let r = Reg { env: env_with(shared).await };
    let v = r.get("/$schemaGroups").await.json();
    assert_eq!(v["Value"], json!(["orders", "plain"]));
    assert_eq!(r.get("/$schemaGroups/orders").await.json()["SchemaCompatibility"], "Forward");
    assert_eq!(r.get("/$schemaGroups/plain").await.json()["SchemaCompatibility"], "None");
    // a registered schema goes with a reset, the groups come back from the file
    r.put("/$schemaGroups/orders/schemas/s", ORDER1).await;
    r.env.shared.reset();
    assert_eq!(r.get("/$schemaGroups").await.json()["Value"], json!(["orders", "plain"]));
    assert_eq!(r.get("/$schemaGroups/orders/schemas").await.json()["Value"], json!([]));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn the_registry_is_kept_between_runs() {
    let dir = std::env::temp_dir().join(format!("azemu-eh-schema-keep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = Reg { env: env_persistent(&dir).await };
    first.group("g", "Forward").await;
    let a = first.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    let b = first.put("/$schemaGroups/g/schemas/s", ORDER2).await;
    first.call("DELETE", "/$schemaGroups/g/schemas/s/versions/1", None, "").await;
    first.env.srv.shutdown().await.unwrap();
    assert!(dir.join("eventhubs").join("schemas.state").exists());

    let second = Reg { env: env_persistent(&dir).await };
    let props = second.get("/$schemaGroups/g").await.json();
    assert_eq!(props["SchemaCompatibility"], "Forward");
    let got = second.get("/$schemaGroups/g/schemas/s/versions/2").await;
    assert_eq!((got.text(), got.header("schema-id")), (ORDER2.to_string(), b.header("schema-id")));
    assert_eq!(second.get("/$schemaGroups/g/schemas/s/versions").await.json()["Value"], json!([2]));
    assert_eq!(second.get(&format!("/$schemaGroups/$schemas/{}", a.header("schema-id").unwrap())).await.status, 404);
    // the numbers go on where they were, and the mode still holds: version 1's content drops the field `qty` that version 2 has
    let refused = second.put("/$schemaGroups/g/schemas/s", ORDER1).await;
    assert_eq!(refused.status, 400, "{}", refused.text());
    let order3 = r#"{"type":"record","name":"Order","namespace":"azemu.test","fields":[{"name":"id","type":"string"},{"name":"qty","type":"int"},{"name":"note","type":"string"}]}"#;
    assert_eq!(second.put("/$schemaGroups/g/schemas/s", order3).await.header("schema-version"), Some("3"));
    // the latest version goes (3), then 2: the schema is gone and numbering starts again
    second.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await;
    second.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await;
    assert_eq!(second.put("/$schemaGroups/g/schemas/s", ORDER1).await.header("schema-version"), Some("1"));
    // a registry that is empty again leaves no file behind
    second.call("DELETE", "/$schemaGroups/g/schemas/s", None, "").await;
    second.env.srv.reset();
    second.env.srv.shutdown().await.unwrap();
    assert!(!dir.join("eventhubs").join("schemas.state").exists());
    std::fs::remove_dir_all(&dir).ok();
}
