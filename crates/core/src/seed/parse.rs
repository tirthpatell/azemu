//! Reading the seed file: JSON in, a checked list of things to create out. Nothing is sent to a service here; every
//! complaint names the place in the file and says why, and the first one stops the reading.

use serde_json::{Map, Value};

/// One thing to create.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Item {
    BlobContainer {
        account: String,
        name: String,
        public_access: Option<String>,
        metadata: Vec<(String, String)>,
    },
    Queue {
        account: String,
        name: String,
    },
    Table {
        account: String,
        name: String,
    },
    CosmosDatabase {
        account: String,
        id: String,
    },
    /// `body` is what a client posts: `id`, `partitionKey` (with `paths`, `kind`, `version`) and the other properties as given.
    CosmosContainer {
        account: String,
        database: String,
        body: Map<String, Value>,
    },
    Secret {
        vault: String,
        name: String,
        value: String,
    },
    KeyValue {
        store: String,
        key: String,
        label: Option<String>,
        value: String,
        content_type: Option<String>,
    },
}

impl Item {
    /// The name of the service that creates it.
    pub(super) fn service(&self) -> &'static str {
        match self {
            Item::BlobContainer { .. } => "blob",
            Item::Queue { .. } => "queue",
            Item::Table { .. } => "table",
            Item::CosmosDatabase { .. } | Item::CosmosContainer { .. } => "cosmos",
            Item::Secret { .. } => "keyvault",
            Item::KeyValue { .. } => "appconfig",
        }
    }

    /// The storage account, Cosmos account, vault or store it belongs to.
    pub(super) fn account(&self) -> &str {
        match self {
            Item::BlobContainer { account, .. }
            | Item::Queue { account, .. }
            | Item::Table { account, .. }
            | Item::CosmosDatabase { account, .. }
            | Item::CosmosContainer { account, .. } => account,
            Item::Secret { vault, .. } => vault,
            Item::KeyValue { store, .. } => store,
        }
    }
}

/// An item and where it is in the file (`blob.myaccount.containers[1]`).
#[derive(Debug, Clone)]
pub(super) struct Entry {
    pub place: String,
    pub item: Item,
}

/// The services whose content has a file of its own. Named so that the complaint can say where it goes.
const OWN_FILE: &[(&str, &str)] =
    &[("servicebus", "AZEMU_SERVICEBUS_CONFIG"), ("eventhubs", "AZEMU_EVENTHUBS_CONFIG"), ("eventgrid", "AZEMU_EVENTGRID_CONFIG")];

const SERVICES: &[&str] = &["blob", "queue", "table", "cosmos", "keyvault", "appconfig"];

/// A complaint: `<place>: <reason>`.
type Res<T> = Result<T, String>;

fn fail<T>(place: &str, reason: impl AsRef<str>) -> Res<T> {
    Err(format!("{place}: {}", reason.as_ref()))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn object<'a>(v: &'a Value, place: &str) -> Res<&'a Map<String, Value>> {
    v.as_object().ok_or_else(|| format!("{place}: must be an object, found {}", kind(v)))
}

fn array<'a>(v: &'a Value, place: &str) -> Res<&'a Vec<Value>> {
    v.as_array().ok_or_else(|| format!("{place}: must be a list, found {}", kind(v)))
}

fn text<'a>(v: &'a Value, place: &str) -> Res<&'a str> {
    v.as_str().ok_or_else(|| format!("{place}: must be a string, found {}", kind(v)))
}

fn name<'a>(v: &'a Value, place: &str) -> Res<&'a str> {
    let s = text(v, place)?;
    if s.is_empty() {
        return fail(place, "must not be empty");
    }
    Ok(s)
}

/// Every key of `m` must be one of `known`.
fn only_known(m: &Map<String, Value>, place: &str, known: &[&str]) -> Res<()> {
    match m.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => fail(&format!("{place}.{k}"), format!("unknown property (known here: {})", known.join(", "))),
        None => Ok(()),
    }
}

fn required<'a>(m: &'a Map<String, Value>, place: &str, key: &str) -> Res<&'a Value> {
    m.get(key).ok_or_else(|| format!("{place}: the property '{key}' is missing"))
}

/// Parse and check the whole file. Items keep the order of the file (databases come before their containers).
pub(super) fn parse(bytes: &[u8]) -> Res<Vec<Entry>> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not valid UTF-8 (at byte {})", e.valid_up_to()))?;
    // a byte order mark is what some editors put at the start of a file
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let top = doc.as_object().ok_or_else(|| format!("top level: must be an object, found {}", kind(&doc)))?;
    let mut out = Vec::new();
    for key in top.keys() {
        if let Some((_, var)) = OWN_FILE.iter().find(|(s, _)| s == key) {
            return fail(
                key,
                format!("the {key} service has a file of its own that this file does not carry; name it with {var} (see docs/seed.md)"),
            );
        }
        if !SERVICES.contains(&key.as_str()) {
            return fail(key, format!("unknown top-level property (known: {})", SERVICES.join(", ")));
        }
    }
    for service in SERVICES {
        let Some(v) = top.get(*service) else { continue };
        for (account, body) in object(v, service)? {
            let place = format!("{service}.{account}");
            if account.is_empty() {
                return fail(service, "an account name must not be empty");
            }
            let body = object(body, &place)?;
            match *service {
                "blob" => blob(account, body, &place, &mut out)?,
                "queue" => names(account, body, &place, "queues", &mut out, |account, name| Item::Queue { account, name })?,
                "table" => names(account, body, &place, "tables", &mut out, |account, name| Item::Table { account, name })?,
                "cosmos" => cosmos(account, body, &place, &mut out)?,
                "keyvault" => keyvault(account, body, &place, &mut out)?,
                _ => appconfig(account, body, &place, &mut out)?,
            }
        }
    }
    Ok(out)
}

fn blob(account: &str, body: &Map<String, Value>, place: &str, out: &mut Vec<Entry>) -> Res<()> {
    only_known(body, place, &["containers"])?;
    let Some(list) = body.get("containers") else { return Ok(()) };
    for (i, c) in array(list, &format!("{place}.containers"))?.iter().enumerate() {
        let at = format!("{place}.containers[{i}]");
        let (cname, public_access, metadata) = match c {
            Value::String(_) => (name(c, &at)?.to_string(), None, Vec::new()),
            Value::Object(m) => {
                only_known(m, &at, &["name", "publicAccess", "metadata"])?;
                let n = name(required(m, &at, "name")?, &format!("{at}.name"))?.to_string();
                let access = match m.get("publicAccess") {
                    None => None,
                    Some(v) => match text(v, &format!("{at}.publicAccess"))? {
                        a @ ("blob" | "container") => Some(a.to_string()),
                        other => {
                            return fail(&format!("{at}.publicAccess"), format!("must be \"blob\" or \"container\", found \"{other}\""))
                        }
                    },
                };
                let mut meta = Vec::new();
                if let Some(v) = m.get("metadata") {
                    let mplace = format!("{at}.metadata");
                    for (k, v) in object(v, &mplace)? {
                        let value = text(v, &format!("{mplace}.{k}"))?;
                        if http::HeaderName::from_bytes(format!("x-ms-meta-{k}").as_bytes()).is_err() || k.is_empty() {
                            return fail(
                                &format!("{mplace}.{k}"),
                                "this cannot be a metadata name (it is sent as the header x-ms-meta-<name>)",
                            );
                        }
                        if http::HeaderValue::from_str(value).is_err() {
                            return fail(&format!("{mplace}.{k}"), "this value has a character that cannot be sent in a header");
                        }
                        meta.push((k.clone(), value.to_string()));
                    }
                }
                (n, access, meta)
            }
            other => return fail(&at, format!("must be a container name or an object with a 'name', found {}", kind(other))),
        };
        out.push(Entry { place: at, item: Item::BlobContainer { account: account.to_string(), name: cname, public_access, metadata } });
    }
    Ok(())
}

/// Queues and tables: a list of names under one property.
fn names(
    account: &str,
    body: &Map<String, Value>,
    place: &str,
    prop: &str,
    out: &mut Vec<Entry>,
    make: impl Fn(String, String) -> Item,
) -> Res<()> {
    only_known(body, place, &[prop])?;
    let Some(list) = body.get(prop) else { return Ok(()) };
    for (i, n) in array(list, &format!("{place}.{prop}"))?.iter().enumerate() {
        let at = format!("{place}.{prop}[{i}]");
        out.push(Entry { item: make(account.to_string(), name(n, &at)?.to_string()), place: at });
    }
    Ok(())
}

fn partition_key(v: Option<&Value>, at: &str) -> Res<Value> {
    let place = format!("{at}.partitionKey");
    let Some(v) = v else {
        return fail(at, "the property 'partitionKey' is missing (a path such as \"/tenantId\", or a list of up to three paths)");
    };
    let paths: Vec<String> = match v {
        Value::String(_) => vec![text(v, &place)?.to_string()],
        Value::Array(a) => {
            let mut p = Vec::new();
            for (i, x) in a.iter().enumerate() {
                p.push(text(x, &format!("{place}[{i}]"))?.to_string());
            }
            p
        }
        other => return fail(&place, format!("must be a path or a list of paths, found {}", kind(other))),
    };
    if paths.is_empty() || paths.len() > 3 {
        return fail(&place, "needs one path, or up to three for a hierarchical key");
    }
    if let Some(p) = paths.iter().find(|p| !p.starts_with('/') || p.len() < 2) {
        return fail(&place, format!("a path starts with '/' and names a property, found \"{p}\""));
    }
    let kind = if paths.len() > 1 { "MultiHash" } else { "Hash" };
    Ok(serde_json::json!({"paths": paths, "kind": kind, "version": 2}))
}

/// What a container may carry besides `id` and `partitionKey`: the properties the service reads, passed on as given.
const CONTAINER_PROPERTIES: &[&str] = &[
    "id",
    "partitionKey",
    "uniqueKeyPolicy",
    "defaultTtl",
    "indexingPolicy",
    "conflictResolutionPolicy",
    "geospatialConfig",
    "computedProperties",
    "analyticalStorageTtl",
    "changeFeedPolicy",
    "vectorEmbeddingPolicy",
    "fullTextPolicy",
    "clientEncryptionPolicy",
];

fn cosmos(account: &str, body: &Map<String, Value>, place: &str, out: &mut Vec<Entry>) -> Res<()> {
    only_known(body, place, &["databases"])?;
    let Some(list) = body.get("databases") else { return Ok(()) };
    for (i, d) in array(list, &format!("{place}.databases"))?.iter().enumerate() {
        let at = format!("{place}.databases[{i}]");
        let (id, containers) = match d {
            Value::String(_) => (name(d, &at)?.to_string(), None),
            Value::Object(m) => {
                only_known(m, &at, &["id", "containers"])?;
                (name(required(m, &at, "id")?, &format!("{at}.id"))?.to_string(), m.get("containers"))
            }
            other => return fail(&at, format!("must be a database id or an object with an 'id', found {}", kind(other))),
        };
        out.push(Entry { place: at.clone(), item: Item::CosmosDatabase { account: account.to_string(), id: id.clone() } });
        let Some(containers) = containers else { continue };
        for (j, c) in array(containers, &format!("{at}.containers"))?.iter().enumerate() {
            let cat = format!("{at}.containers[{j}]");
            let m = object(c, &cat)?;
            only_known(m, &cat, CONTAINER_PROPERTIES)?;
            let cid = name(required(m, &cat, "id")?, &format!("{cat}.id"))?;
            let mut b = m.clone();
            b.insert("id".into(), Value::String(cid.to_string()));
            b.insert("partitionKey".into(), partition_key(m.get("partitionKey"), &cat)?);
            out.push(Entry { place: cat, item: Item::CosmosContainer { account: account.to_string(), database: id.clone(), body: b } });
        }
    }
    Ok(())
}

fn keyvault(vault: &str, body: &Map<String, Value>, place: &str, out: &mut Vec<Entry>) -> Res<()> {
    only_known(body, place, &["secrets"])?;
    let Some(list) = body.get("secrets") else { return Ok(()) };
    let sp = format!("{place}.secrets");
    for (n, v) in object(list, &sp)? {
        let at = format!("{sp}.{n}");
        if n.is_empty() {
            return fail(&sp, "a secret name must not be empty");
        }
        out.push(Entry { item: Item::Secret { vault: vault.to_string(), name: n.clone(), value: text(v, &at)?.to_string() }, place: at });
    }
    Ok(())
}

fn appconfig(store: &str, body: &Map<String, Value>, place: &str, out: &mut Vec<Entry>) -> Res<()> {
    only_known(body, place, &["keyValues"])?;
    let Some(list) = body.get("keyValues") else { return Ok(()) };
    for (i, kv) in array(list, &format!("{place}.keyValues"))?.iter().enumerate() {
        let at = format!("{place}.keyValues[{i}]");
        let m = object(kv, &at)?;
        only_known(m, &at, &["key", "label", "value", "contentType"])?;
        let key = name(required(m, &at, "key")?, &format!("{at}.key"))?.to_string();
        let value = text(required(m, &at, "value")?, &format!("{at}.value"))?.to_string();
        let opt = |prop: &str| -> Res<Option<String>> {
            match m.get(prop) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => Ok(Some(text(v, &format!("{at}.{prop}"))?.to_string())),
            }
        };
        // an empty label is no label
        let label = opt("label")?.filter(|l| !l.is_empty());
        let content_type = opt("contentType")?;
        out.push(Entry { place: at, item: Item::KeyValue { store: store.to_string(), key, label, value, content_type } });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(json: &str) -> Vec<Item> {
        parse(json.as_bytes()).unwrap().into_iter().map(|e| e.item).collect()
    }

    fn complaint(json: &str) -> String {
        parse(json.as_bytes()).unwrap_err()
    }

    #[test]
    fn a_complete_file_gives_every_kind_of_item_in_file_order() {
        let got = parse(
            br#"{
              "blob": {"acct": {"containers": ["a", {"name": "b", "publicAccess": "blob", "metadata": {"team": "x"}}]}},
              "queue": {"acct": {"queues": ["q1"]}},
              "table": {"acct": {"tables": ["T1"]}},
              "cosmos": {"acct": {"databases": ["empty", {"id": "app", "containers": [
                  {"id": "items", "partitionKey": "/tenant", "defaultTtl": -1},
                  {"id": "h", "partitionKey": ["/a", "/b"], "uniqueKeyPolicy": {"uniqueKeys": [{"paths": ["/x"]}]}}]}]}},
              "keyvault": {"v": {"secrets": {"db": "pw"}}},
              "appconfig": {"s": {"keyValues": [{"key": "K", "value": "1"}, {"key": "K", "label": "dev", "value": "2", "contentType": "text/plain"}]}}
            }"#,
        )
        .unwrap();
        let places: Vec<&str> = got.iter().map(|e| e.place.as_str()).collect();
        assert_eq!(
            places,
            [
                "blob.acct.containers[0]",
                "blob.acct.containers[1]",
                "queue.acct.queues[0]",
                "table.acct.tables[0]",
                "cosmos.acct.databases[0]",
                "cosmos.acct.databases[1]",
                "cosmos.acct.databases[1].containers[0]",
                "cosmos.acct.databases[1].containers[1]",
                "keyvault.v.secrets.db",
                "appconfig.s.keyValues[0]",
                "appconfig.s.keyValues[1]"
            ]
        );
        assert_eq!(
            got[1].item,
            Item::BlobContainer {
                account: "acct".into(),
                name: "b".into(),
                public_access: Some("blob".into()),
                metadata: vec![("team".into(), "x".into())]
            }
        );
        let Item::CosmosContainer { body, database, .. } = &got[6].item else { panic!("{:?}", got[6].item) };
        assert_eq!(database, "app");
        assert_eq!(body["partitionKey"], serde_json::json!({"paths": ["/tenant"], "kind": "Hash", "version": 2}));
        assert_eq!(body["defaultTtl"], -1);
        let Item::CosmosContainer { body, .. } = &got[7].item else { panic!() };
        assert_eq!(body["partitionKey"]["kind"], "MultiHash");
        assert_eq!(body["uniqueKeyPolicy"]["uniqueKeys"][0]["paths"][0], "/x");
    }

    #[test]
    fn an_empty_object_and_a_file_with_a_byte_order_mark_are_fine() {
        assert!(items("{}").is_empty());
        assert_eq!(items("\u{feff}{\"queue\": {\"a\": {\"queues\": [\"q\"]}}}").len(), 1);
        assert!(items(r#"{"blob": {}, "queue": {"a": {}}}"#).is_empty());
    }

    #[test]
    fn an_empty_label_is_no_label() {
        let i = items(r#"{"appconfig": {"s": {"keyValues": [{"key": "k", "label": "", "value": "v"}]}}}"#);
        assert!(matches!(&i[0], Item::KeyValue { label: None, .. }));
    }

    #[test]
    fn every_complaint_names_the_place_and_the_reason() {
        let cases: &[(&str, &[&str])] = &[
            ("{", &["not valid JSON", "line 1"]),
            ("[]", &["top level", "must be an object", "a list"]),
            (r#"{"bolb": {}}"#, &["bolb", "unknown top-level property", "blob, queue, table"]),
            (r#"{"servicebus": {}}"#, &["servicebus", "AZEMU_SERVICEBUS_CONFIG"]),
            (r#"{"eventhubs": {}}"#, &["eventhubs", "AZEMU_EVENTHUBS_CONFIG"]),
            (r#"{"eventgrid": {}}"#, &["eventgrid", "AZEMU_EVENTGRID_CONFIG"]),
            (r#"{"blob": []}"#, &["blob", "must be an object"]),
            (r#"{"blob": {"a": {"container": []}}}"#, &["blob.a.container", "unknown property", "containers"]),
            (r#"{"blob": {"a": {"containers": "x"}}}"#, &["blob.a.containers", "must be a list"]),
            (r#"{"blob": {"a": {"containers": [3]}}}"#, &["blob.a.containers[0]", "found a number"]),
            (r#"{"blob": {"a": {"containers": [""]}}}"#, &["blob.a.containers[0]", "must not be empty"]),
            (
                r#"{"blob": {"a": {"containers": [{"nam": "x"}]}}}"#,
                &["blob.a.containers[0].nam", "unknown property", "name, publicAccess, metadata"],
            ),
            (r#"{"blob": {"a": {"containers": [{"publicAccess": "blob"}]}}}"#, &["blob.a.containers[0]", "'name' is missing"]),
            (
                r#"{"blob": {"a": {"containers": [{"name": "x", "publicAccess": "all"}]}}}"#,
                &["publicAccess", "\"blob\" or \"container\"", "\"all\""],
            ),
            (r#"{"blob": {"a": {"containers": [{"name": "x", "metadata": {"k": 1}}]}}}"#, &["metadata.k", "must be a string"]),
            (r#"{"blob": {"a": {"containers": [{"name": "x", "metadata": {"bad name": "v"}}]}}}"#, &["metadata.bad name", "x-ms-meta-"]),
            (r#"{"queue": {"a": {"queues": [null]}}}"#, &["queue.a.queues[0]", "found null"]),
            (r#"{"table": {"a": {"tables": {}}}}"#, &["table.a.tables", "must be a list"]),
            (r#"{"cosmos": {"a": {"databases": [{"containers": []}]}}}"#, &["cosmos.a.databases[0]", "'id' is missing"]),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c"}]}]}}}"#,
                &["cosmos.a.databases[0].containers[0]", "'partitionKey' is missing"],
            ),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": "pk"}]}]}}}"#,
                &["containers[0].partitionKey", "starts with '/'"],
            ),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": []}]}]}}}"#,
                &["partitionKey", "up to three"],
            ),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": ["/a","/b","/c","/d"]}]}]}}}"#,
                &["partitionKey", "up to three"],
            ),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": 4}]}]}}}"#,
                &["partitionKey", "found a number"],
            ),
            (
                r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": [{"id": "c", "partitionKey": "/p", "ttl": 3}]}]}}}"#,
                &["containers[0].ttl", "unknown property", "defaultTtl"],
            ),
            (r#"{"cosmos": {"a": {"databases": [{"id": "d", "containers": {}}]}}}"#, &["databases[0].containers", "must be a list"]),
            (r#"{"keyvault": {"v": {"secrets": []}}}"#, &["keyvault.v.secrets", "must be an object"]),
            (r#"{"keyvault": {"v": {"secrets": {"s": 5}}}}"#, &["keyvault.v.secrets.s", "must be a string"]),
            (r#"{"keyvault": {"v": {"keys": {}}}}"#, &["keyvault.v.keys", "unknown property", "secrets"]),
            (r#"{"appconfig": {"s": {"keyValues": [{"value": "v"}]}}}"#, &["appconfig.s.keyValues[0]", "'key' is missing"]),
            (r#"{"appconfig": {"s": {"keyValues": [{"key": "k"}]}}}"#, &["keyValues[0]", "'value' is missing"]),
            (
                r#"{"appconfig": {"s": {"keyValues": [{"key": "k", "value": "v", "tags": {}}]}}}"#,
                &["keyValues[0].tags", "unknown property", "contentType"],
            ),
            (
                r#"{"appconfig": {"s": {"keyValues": [{"key": "k", "value": "v", "label": 1}]}}}"#,
                &["keyValues[0].label", "must be a string"],
            ),
        ];
        for (json, wants) in cases {
            let m = complaint(json);
            for w in *wants {
                assert!(m.contains(w), "{json}\n  wanted {w:?} in: {m}");
            }
        }
    }

    #[test]
    fn bytes_that_are_not_text_are_refused() {
        let m = parse(&[b'{', 0xff, b'}']).unwrap_err();
        assert!(m.contains("UTF-8") && m.contains("byte 1"), "{m}");
    }
}
