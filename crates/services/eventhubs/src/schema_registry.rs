//! The Schema Registry of a namespace: schema groups, the schemas in them and their versions. The REST API is in
//! `schema_http.rs`, the Avro rules in `schema_avro.rs`, the files in `schema_persist.rs`.
//!
//! A group is made outside this API (on Azure by Resource Manager or the portal; here by the admin API, the config file, Resource
//! Manager, or `--lenient`). A schema belongs to a group and has numbered versions; every version has an id that is unique in the
//! namespace. Names are compared without regard to case and kept as they were first written.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::model::{key, next_revision};
use crate::schema_avro;
use crate::state::{now_ms, Shared, State};

/// What Azure documents for a schema: 1 MB.
pub const MAX_SCHEMA_BYTES: usize = 1_048_576;

/// How many versions one schema may have. Azure's own limit is 25 versions across all groups of a Standard namespace (Microsoft
/// Learn, "Event Hubs quotas and limits"), which is not enforced here so that a test suite may register as many as it likes;
/// this is the emulator's own limit, so that a registry cannot grow without end.
pub const MAX_VERSIONS_PER_SCHEMA: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchemaType {
    Avro,
    Json,
    Custom,
    Protobuf,
    /// A group that takes every kind (what Resource Manager calls `Unknown`).
    Unknown,
}

impl SchemaType {
    pub fn parse(s: &str) -> Option<SchemaType> {
        match s.to_ascii_lowercase().as_str() {
            "avro" => Some(SchemaType::Avro),
            "json" => Some(SchemaType::Json),
            "custom" => Some(SchemaType::Custom),
            "protobuf" => Some(SchemaType::Protobuf),
            "unknown" => Some(SchemaType::Unknown),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SchemaType::Avro => "Avro",
            SchemaType::Json => "Json",
            SchemaType::Custom => "Custom",
            SchemaType::Protobuf => "Protobuf",
            SchemaType::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compatibility {
    None,
    Backward,
    Forward,
}

impl Compatibility {
    pub fn parse(s: &str) -> Option<Compatibility> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(Compatibility::None),
            "backward" => Some(Compatibility::Backward),
            "forward" => Some(Compatibility::Forward),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Compatibility::None => "None",
            Compatibility::Backward => "Backward",
            Compatibility::Forward => "Forward",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SchemaVersion {
    pub version: u32,
    /// What the content is (the group may take any kind)
    pub kind: SchemaType,
    /// 32 hex digits
    pub id: String,
    /// As stored: for Avro and Json the compact JSON, for the others the text as it came.
    pub content: String,
    pub created_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SchemaEntry {
    pub name: String,
    pub versions: Vec<SchemaVersion>,
}

#[derive(Clone, Debug)]
pub struct SchemaGroup {
    pub name: String,
    pub schema_type: SchemaType,
    pub compatibility: Compatibility,
    pub properties: BTreeMap<String, String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub etag: String,
    pub schemas: BTreeMap<String, SchemaEntry>,
    /// For the versions of Avro and Json content: [`schema_avro::canonical_hash`] of the content by version id, so that the
    /// version that has some content is found without reading every stored version again. A version that is not here (a state
    /// made by hand) is read when it is looked for.
    pub hashes: HashMap<String, u64>,
}

impl SchemaGroup {
    pub fn new(name: &str, schema_type: SchemaType, compatibility: Compatibility, now: i64) -> SchemaGroup {
        SchemaGroup {
            name: name.to_string(),
            schema_type,
            compatibility,
            properties: BTreeMap::new(),
            created_ms: now,
            updated_ms: now,
            etag: uuid::Uuid::new_v4().to_string(),
            schemas: BTreeMap::new(),
            hashes: HashMap::new(),
        }
    }
}

/// An error the REST API answers: status, Azure's error code and the text (without the tracking suffix).
#[derive(Debug, Clone)]
pub struct SchemaError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    /// Azure's gateway errors name the request (`SystemTracker:<namespace>:<path>`, tracking id ending `_G..`); the ones from
    /// the entity layer say `NoSystemTracker` (tracking id ending `_D45`).
    pub gateway: bool,
}

impl SchemaError {
    pub fn invalid(message: impl Into<String>) -> SchemaError {
        SchemaError { status: 400, code: "InvalidRequest", message: message.into(), gateway: true }
    }

    pub fn not_found(message: impl Into<String>) -> SchemaError {
        SchemaError { status: 404, code: "ItemNotFound", message: message.into(), gateway: true }
    }

    fn entity(status: u16, code: &'static str, message: String) -> SchemaError {
        SchemaError { status, code, message, gateway: false }
    }
}

/// What a `PUT` or a `POST ...:get-id` found or made: the version with the names of its schema and group.
#[derive(Clone, Debug)]
pub struct Found {
    pub group: String,
    pub schema: String,
    pub version: SchemaVersion,
}

fn dashed(id: &str) -> String {
    match uuid::Uuid::parse_str(id) {
        Ok(u) => u.to_string(),
        Err(_) => id.to_string(),
    }
}

/// The ids Azure takes: 32 hex digits, or the same with dashes. `None` for anything else.
pub fn parse_id(s: &str) -> Option<String> {
    uuid::Uuid::parse_str(s).ok().map(|u| u.simple().to_string())
}

impl State {
    pub fn schema_group(&self, ns: &str, group: &str) -> Option<&SchemaGroup> {
        self.namespaces.get(&self.resolve(ns))?.schema_groups.get(&key(group))
    }

    pub fn schema_group_mut(&mut self, ns: &str, group: &str) -> Option<&mut SchemaGroup> {
        let n = self.resolve(ns);
        self.namespaces.get_mut(&n)?.schema_groups.get_mut(&key(group))
    }

    /// Make a group, or change the mode and the properties of one that exists (the kind cannot change once it holds schemas).
    /// `true` when it was made.
    pub fn put_schema_group(
        &mut self,
        ns: &str,
        name: &str,
        schema_type: SchemaType,
        compatibility: Compatibility,
        properties: Option<BTreeMap<String, String>>,
    ) -> Result<bool, SchemaError> {
        if !valid_group_name(name) {
            return Err(SchemaError::invalid(format!("'{name}' is not a valid schema group name.")));
        }
        let now = now_ms();
        let nsk = self.resolve(ns);
        let n = self.namespaces.entry(nsk).or_default();
        let created = match n.schema_groups.get_mut(&key(name)) {
            Some(g) => {
                if g.schema_type != schema_type && !g.schemas.is_empty() {
                    return Err(SchemaError::invalid("The type of a schema group that holds schemas cannot change."));
                }
                g.schema_type = schema_type;
                g.compatibility = compatibility;
                if let Some(p) = properties {
                    g.properties = p;
                }
                g.updated_ms = now;
                g.etag = uuid::Uuid::new_v4().to_string();
                false
            }
            None => {
                let mut g = SchemaGroup::new(name, schema_type, compatibility, now);
                if let Some(p) = properties {
                    g.properties = p;
                }
                n.schema_groups.insert(key(name), g);
                true
            }
        };
        next_revision();
        Ok(created)
    }

    pub fn delete_schema_group(&mut self, ns: &str, name: &str) -> bool {
        let n = self.resolve(ns);
        let gone = self.namespaces.get_mut(&n).map(|n| n.schema_groups.remove(&key(name)).is_some()).unwrap_or(false);
        if gone {
            next_revision();
        }
        gone
    }

    /// The version that has this id, in any group of the namespace.
    pub fn schema_by_id(&self, ns: &str, id: &str) -> Option<Found> {
        let n = self.namespaces.get(&self.resolve(ns))?;
        for g in n.schema_groups.values() {
            for s in g.schemas.values() {
                if let Some(v) = s.versions.iter().find(|v| v.id == id) {
                    return Some(Found { group: g.name.clone(), schema: s.name.clone(), version: v.clone() });
                }
            }
        }
        None
    }

    /// One version of a schema; `version` 0 is the latest (as Azure answers it).
    pub fn schema_version(&self, ns: &str, group: &str, schema: &str, version: i64) -> Option<Found> {
        let g = self.schema_group(ns, group)?;
        let s = g.schemas.get(&key(schema))?;
        let v = if version == 0 { s.versions.last()? } else { s.versions.iter().find(|v| i64::from(v.version) == version)? };
        Some(Found { group: g.name.clone(), schema: s.name.clone(), version: v.clone() })
    }

    pub fn delete_schema_version(&mut self, ns: &str, group: &str, schema: &str, version: u32) {
        if let Some(s) = self.schema_group_mut(ns, group).and_then(|g| g.schemas.get_mut(&key(schema))) {
            let before = s.versions.len();
            let gone: Vec<String> = s.versions.iter().filter(|v| v.version == version).map(|v| v.id.clone()).collect();
            s.versions.retain(|v| v.version != version);
            if s.versions.len() != before {
                let empty = s.versions.is_empty();
                if let Some(g) = self.schema_group_mut(ns, group) {
                    for id in &gone {
                        g.hashes.remove(id);
                    }
                    if empty {
                        g.schemas.remove(&key(schema));
                    }
                }
                next_revision();
            }
        }
    }

    /// `DELETE` on the schema itself: Azure removes its latest version only (and the schema with its last version).
    pub fn delete_latest_schema_version(&mut self, ns: &str, group: &str, schema: &str) {
        let latest =
            self.schema_group(ns, group).and_then(|g| g.schemas.get(&key(schema))).and_then(|s| s.versions.last()).map(|v| v.version);
        if let Some(v) = latest {
            self.delete_schema_version(ns, group, schema, v);
        }
    }

    /// First step of a registration, with the state locked: the checks Azure makes before the compatibility check, in its order
    /// (the group, the kind the group takes, the size), and what the second step needs.
    #[allow(clippy::too_many_arguments)]
    fn register_prepare(
        &mut self,
        ns: &str,
        group: &str,
        schema: &str,
        kind: SchemaType,
        content: &Content,
        autocreate: bool,
        raw_len: usize,
    ) -> Result<Prepared, SchemaError> {
        if self.schema_group(ns, group).is_none() {
            if !autocreate {
                return Err(SchemaError::entity(
                    404,
                    "ItemNotFound",
                    format!("SubCode=40400, Schema group '{group}' does not exist while auto-create is turned off."),
                ));
            }
            self.put_schema_group(ns, group, kind, Compatibility::None, None)?;
        }
        let g = self.schema_group_mut(ns, group).ok_or_else(|| SchemaError::not_found("The schema group disappeared."))?;
        if g.schema_type != SchemaType::Unknown && g.schema_type != kind {
            return Err(SchemaError::entity(
                415,
                "InvalidSchemaType",
                format!(
                    "SubCode=41500, The messaging entity 'Schema of type {} cannot be created within group '{}' that supports {} schema type.' could not be found. To know more visit https://aka.ms/sbResourceMgrExceptions. .",
                    kind.name(),
                    g.name,
                    g.schema_type.name()
                ),
            ));
        }
        if raw_len > MAX_SCHEMA_BYTES {
            return Err(SchemaError::entity(
                403,
                "NotAvaliable", // sic: Azure's spelling
                format!(
                    "SubCode=40300, Cannot create schema {schema} because size of schema data cannot exceed {MAX_SCHEMA_BYTES} bytes. Right now {schema} data is {raw_len} bytes."
                ),
            ));
        }
        let mode = g.compatibility;
        let entry = g.schemas.get(&key(schema));
        let last = entry.and_then(|e| e.versions.last());
        // Azure checks against the latest version only
        let latest = match (content, last, mode != Compatibility::None) {
            (Content::Avro(..), Some(v), true) => Some(Latest { version: v.version, id: v.id.clone(), content: v.content.clone() }),
            _ => None,
        };
        Ok(Prepared { mode, entry_name: entry.map(|e| e.name.clone()).unwrap_or_default(), latest, latest_id: last.map(|v| v.id.clone()) })
    }

    /// Last step of a registration, with the state locked again: the version of this content, made if it is new. `None` when the
    /// latest version of the schema is not the one the check was made against (somebody registered in between): the caller
    /// starts again.
    fn register_commit(
        &mut self,
        ns: &str,
        group: &str,
        schema: &str,
        kind: SchemaType,
        content: &Content,
        prepared: &Prepared,
    ) -> Result<Option<Found>, SchemaError> {
        let now = now_ms();
        let g = self.schema_group_mut(ns, group).ok_or_else(|| SchemaError::not_found("The schema group disappeared."))?;
        let group_name = g.name.clone();
        let current = g.schemas.get(&key(schema)).and_then(|e| e.versions.last()).map(|v| v.id.as_str());
        if current != prepared.latest_id.as_deref() {
            return Ok(None);
        }
        let hashes = &g.hashes;
        if let Some(entry) = g.schemas.get(&key(schema)) {
            if let Some(v) = entry.versions.iter().find(|v| v.kind == kind && content.is_stored_as(v, hashes)) {
                return Ok(Some(Found { group: group_name, schema: entry.name.clone(), version: v.clone() }));
            }
            if entry.versions.len() >= MAX_VERSIONS_PER_SCHEMA {
                return Err(SchemaError::entity(
                    403,
                    "NotAvaliable",
                    format!(
                        "SubCode=40300, Cannot create schema {schema} in group {group_name} because no more than {MAX_VERSIONS_PER_SCHEMA} versions of a schema are allowed (a limit of the emulator: Azure's own limit is 25 versions across all groups of a Standard namespace, and it is not enforced here)."
                    ),
                ));
            }
        }
        let entry = g.schemas.entry(key(schema)).or_insert_with(|| SchemaEntry { name: schema.to_string(), versions: vec![] });
        let version = SchemaVersion {
            // the number after the latest one: after the latest version was deleted its number is given again
            version: entry.versions.last().map(|v| v.version + 1).unwrap_or(1),
            kind,
            id: uuid::Uuid::new_v4().simple().to_string(),
            content: content.stored().to_string(),
            created_ms: now,
        };
        entry.versions.push(version.clone());
        let found = Found { group: group_name, schema: entry.name.clone(), version };
        if let Some(h) = content.hash() {
            g.hashes.insert(found.version.id.clone(), h);
        }
        next_revision();
        Ok(Some(found))
    }

    /// `POST ...:get-id`: the version that has this content. Azure answers "does not exist" for a schema without that content.
    pub fn schema_id_for_content(&self, ns: &str, group: &str, schema: &str, kind: SchemaType, text: &str) -> Result<Found, SchemaError> {
        let missing = || SchemaError::not_found(format!("Schema {group}/{schema} does not exist."));
        let g = self.schema_group(ns, group).ok_or_else(missing)?;
        let s = g.schemas.get(&key(schema)).ok_or_else(missing)?;
        let content = Content::new(kind, text).map_err(|_| missing())?;
        let v = s.versions.iter().find(|v| v.kind == kind && content.is_stored_as(v, &g.hashes)).ok_or_else(missing)?;
        Ok(Found { group: g.name.clone(), schema: s.name.clone(), version: v.clone() })
    }
}

/// How often a registration starts again because another one got in between before it answers that it is busy.
const REGISTER_ATTEMPTS: usize = 16;

/// What the first step of a registration found out.
struct Prepared {
    mode: Compatibility,
    /// the name of the schema as it was first written (the text of a refusal has it)
    entry_name: String,
    /// the version the content has to be compatible with: Avro content, a group with a mode, a schema that has a version
    latest: Option<Latest>,
    /// the latest version of the schema whatever the content (to notice that it changed)
    latest_id: Option<String>,
}

struct Latest {
    version: u32,
    id: String,
    content: String,
}

/// `PUT`: the version of this content (made if it is new). The order of the checks is Azure's: the content, the group, the
/// kind the group takes, the size, the compatibility mode.
///
/// The state is locked for the short steps only. Reading the content and comparing it with the latest version can take a long
/// time for a big schema (up to the limits of `schema_avro::compatible`) and run with the state unlocked, so that no other
/// request of the namespace waits for them.
#[allow(clippy::too_many_arguments)]
pub fn register_schema(
    shared: &Shared,
    ns: &str,
    group: &str,
    schema: &str,
    kind: SchemaType,
    text: &str,
    raw_len: usize,
) -> Result<Found, SchemaError> {
    let content = Content::new(kind, text)?;
    for _ in 0..REGISTER_ATTEMPTS {
        let prepared = shared.state.lock().register_prepare(ns, group, schema, kind, &content, shared.autocreate, raw_len)?;
        check_compatibility(&content, &prepared)?;
        if let Some(found) = shared.state.lock().register_commit(ns, group, schema, kind, &content, &prepared)? {
            return Ok(found);
        }
    }
    Err(SchemaError::entity(
        409,
        "Conflict",
        format!("SubCode=40900, The schema {schema} was changed by other requests while it was checked. Try again."),
    ))
}

/// The compatibility check of the group's mode against the latest version. Azure returns the latest version itself when the
/// content is the same, so equal content is not checked.
fn check_compatibility(content: &Content, p: &Prepared) -> Result<(), SchemaError> {
    let (Content::Avro(new, _), Some(latest)) = (content, &p.latest) else { return Ok(()) };
    // a stored version that cannot be read (a damaged state file) is not checked against
    let Ok(old) = schema_avro::parse(&latest.content) else { return Ok(()) };
    if old.value == new.value {
        return Ok(());
    }
    let verdict = match p.mode {
        Compatibility::Forward => schema_avro::compatible(&old.value, &new.value),
        _ => schema_avro::compatible(&new.value, &old.value),
    };
    match verdict {
        Ok(true) => Ok(()),
        Ok(false) => Err(SchemaError::invalid(format!(
            "Avro schema validation failed: {} compatibility test failed when checking against schema {}, version {}, id {}.",
            p.mode.name(),
            p.entry_name,
            latest.version,
            dashed(&latest.id)
        ))),
        Err(limit) => Err(SchemaError::invalid(format!("Avro schema validation failed: {limit}"))),
    }
}

/// A group name as Resource Manager takes it: letters, digits, `-`, `_` and `.`, up to 50 characters.
pub fn valid_group_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 50 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Content after its kind's checks.
enum Content {
    Avro(schema_avro::Parsed, u64),
    Json(Value, String, u64),
    Plain(String),
}

/// [`schema_avro::canonical_hash`] of stored Avro or Json content; `None` for other kinds and for text that does not parse.
pub fn stored_hash(kind: SchemaType, content: &str) -> Option<u64> {
    match kind {
        SchemaType::Avro => schema_avro::parse(content).ok().map(|p| schema_avro::canonical_hash(&p.value)),
        SchemaType::Json => serde_json::from_str::<Value>(content).ok().map(|v| schema_avro::canonical_hash(&v)),
        _ => None,
    }
}

impl Content {
    fn new(kind: SchemaType, text: &str) -> Result<Content, SchemaError> {
        match kind {
            SchemaType::Avro => schema_avro::parse(text)
                .map(|p| {
                    let h = schema_avro::canonical_hash(&p.value);
                    Content::Avro(p, h)
                })
                .map_err(|m| SchemaError::invalid(format!("Avro schema validation failed: {m}"))),
            SchemaType::Json => match serde_json::from_str::<Value>(text) {
                Ok(v) => {
                    let compact = serde_json::to_string(&v).unwrap_or_else(|_| text.to_string());
                    let h = schema_avro::canonical_hash(&v);
                    Ok(Content::Json(v, compact, h))
                }
                Err(e) => Err(SchemaError::invalid(format!("Json schema validation failed: {e}."))),
            },
            _ => Ok(Content::Plain(text.to_string())),
        }
    }

    fn stored(&self) -> &str {
        match self {
            Content::Avro(p, _) => &p.compact,
            Content::Json(_, c, _) => c,
            Content::Plain(t) => t,
        }
    }

    fn hash(&self) -> Option<u64> {
        match self {
            Content::Avro(_, h) | Content::Json(_, _, h) => Some(*h),
            Content::Plain(_) => None,
        }
    }

    /// Is the stored text of this version this schema? A version whose hash is known and different is not, and is not read; one
    /// whose hash is the same is read once to be sure.
    fn is_stored_as(&self, v: &SchemaVersion, hashes: &HashMap<String, u64>) -> bool {
        let differs = |own: u64| hashes.get(&v.id).is_some_and(|h| *h != own);
        match self {
            Content::Avro(p, h) => !differs(*h) && schema_avro::parse(&v.content).map(|o| o.value == p.value).unwrap_or(false),
            Content::Json(j, _, h) => !differs(*h) && serde_json::from_str::<Value>(&v.content).map(|o| &o == j).unwrap_or(false),
            Content::Plain(t) => *t == v.content,
        }
    }
}
