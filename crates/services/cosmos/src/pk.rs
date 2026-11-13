//! Partition key values: parsing the request header, extracting from documents,
//! ordering, and the partition key definition of a container.

use std::cmp::Ordering;

use serde_json::Value;

use crate::errors::{CosmosError, R};

/// Longest partition key value: 2,048 bytes (Microsoft Learn, "Service quotas and default limits", per-item limits: "2,048
/// bytes (101 bytes if large partition-key isn't enabled)"). Containers made with partition key version 1 are the ones
/// without large partition keys on Azure; the emulator gives every container the larger limit, so a key of 102 to 2,048
/// bytes is accepted in a version 1 container, where Azure would refuse it (listed in `docs/services/cosmos.md`). With
/// hierarchical keys the limit is read per component. Only strings can be long.
pub const MAX_PK_VALUE_BYTES: usize = 2048;

/// One component of a partition key value.
#[derive(Debug, Clone)]
pub enum PkComp {
    /// The path is missing from the document.
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl PkComp {
    fn rank(&self) -> u8 {
        match self {
            PkComp::Undefined => 0,
            PkComp::Null => 1,
            PkComp::Bool(_) => 2,
            PkComp::Num(_) => 3,
            PkComp::Str(_) => 4,
        }
    }

    /// The value as JSON; undefined is `{}` (the form the SDKs use for it).
    pub fn to_json(&self) -> Value {
        match self {
            PkComp::Undefined => Value::Object(Default::default()),
            PkComp::Null => Value::Null,
            PkComp::Bool(b) => Value::Bool(*b),
            PkComp::Num(n) => num_json(*n),
            PkComp::Str(s) => Value::String(s.clone()),
        }
    }

    pub fn from_json(v: &Value) -> R<PkComp> {
        Ok(match v {
            Value::Null => PkComp::Null,
            Value::Bool(b) => PkComp::Bool(*b),
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                PkComp::Num(if f == 0.0 { 0.0 } else { f })
            }
            Value::String(s) => {
                if s.len() > MAX_PK_VALUE_BYTES {
                    return Err(CosmosError::bad_request(format!("Partition key reached maximum size of {MAX_PK_VALUE_BYTES} bytes.")));
                }
                PkComp::Str(s.clone())
            }
            Value::Object(o) if o.is_empty() => PkComp::Undefined,
            _ => return Err(CosmosError::bad_request("Partition key value must be a string, a number, a boolean or null.")),
        })
    }
}

pub fn num_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

impl PartialEq for PkComp {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for PkComp {}
impl PartialOrd for PkComp {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for PkComp {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self, o) {
            (PkComp::Bool(a), PkComp::Bool(b)) => a.cmp(b),
            (PkComp::Num(a), PkComp::Num(b)) => a.total_cmp(b),
            (PkComp::Str(a), PkComp::Str(b)) => a.cmp(b),
            _ => self.rank().cmp(&o.rank()),
        }
    }
}

pub type PkValue = Vec<PkComp>;

/// Key of a document in a container: logical partition, then id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocKey {
    pub pk: PkValue,
    pub id: String,
}

/// The partition key definition of a container.
#[derive(Debug, Clone)]
pub struct PartitionKeyDef {
    pub paths: Vec<String>,
    pub kind: String,
    pub version: u8,
}

impl PartitionKeyDef {
    /// Parses the `partitionKey` member of a container body.
    pub fn from_json(v: &Value) -> R<PartitionKeyDef> {
        let paths: Vec<String> = match v.get("paths") {
            Some(Value::Array(a)) => a
                .iter()
                .map(|p| p.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| CosmosError::bad_request("Partition key paths must be strings."))?,
            _ => return Err(CosmosError::bad_request("The partition key definition needs at least one path.")),
        };
        if paths.is_empty() {
            return Err(CosmosError::bad_request("The partition key definition needs at least one path."));
        }
        for p in &paths {
            if !p.starts_with('/') || p.len() < 2 {
                return Err(CosmosError::bad_request(format!("Partition key {p} is invalid.")));
            }
        }
        let given_kind = v.get("kind").and_then(Value::as_str).unwrap_or(if paths.len() > 1 { "MultiHash" } else { "Hash" });
        // Any case is accepted; the definition keeps the documented spelling.
        let kind = ["Hash", "MultiHash", "Range"]
            .into_iter()
            .find(|k| k.eq_ignore_ascii_case(given_kind))
            .ok_or_else(|| CosmosError::bad_request(format!("Partition key kind {given_kind} is not supported.")))?
            .to_string();
        if kind == "MultiHash" && paths.len() > 3 {
            return Err(CosmosError::bad_request("Hierarchical partition keys support at most 3 levels of partition key paths."));
        }
        if kind != "MultiHash" && paths.len() > 1 {
            return Err(CosmosError::bad_request("Multiple partition key paths require kind MultiHash."));
        }
        let version = match v.get("version").and_then(Value::as_u64) {
            Some(1) => 1,
            Some(2) => 2,
            None => {
                if kind == "MultiHash" {
                    2
                } else {
                    1
                }
            }
            Some(_) => return Err(CosmosError::bad_request("Partition key version must be 1 or 2.")),
        };
        Ok(PartitionKeyDef { paths, kind, version })
    }

    pub fn hierarchical(&self) -> bool {
        self.paths.len() > 1
    }

    /// Value of each path in `doc`.
    pub fn extract(&self, doc: &Value) -> R<PkValue> {
        self.paths
            .iter()
            .map(|p| {
                let mut cur = doc;
                for seg in p.trim_start_matches('/').split('/') {
                    match cur.get(seg) {
                        Some(v) => cur = v,
                        None => return Ok(PkComp::Undefined),
                    }
                }
                PkComp::from_json(cur)
            })
            .collect()
    }
}

/// Parses `x-ms-documentdb-partitionkey`: a JSON array of values. `[{}]` is the undefined
/// partition, `[]` means no partition key.
pub fn parse_header(raw: &str) -> R<PkValue> {
    let v: Value = serde_json::from_str(raw).map_err(|_| CosmosError::bad_request("The partition key header is not valid JSON."))?;
    match v {
        Value::Array(a) => a.iter().map(PkComp::from_json).collect(),
        other => Ok(vec![PkComp::from_json(&other)?]),
    }
}
