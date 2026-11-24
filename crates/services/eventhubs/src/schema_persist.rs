//! The Schema Registry's file (`--data-dir`): `schemas.state` (kind `eventhubs-schemas`, version 1), next to the files of
//! `persist.rs`. It exists only while some namespace has a schema group; it is written again only when its content changed. A
//! directory from before the registry has no such file and loads as it did.

use std::io;
use std::path::Path;

use azemu_core::persist;
use azemu_statefile::read_state;
use serde::{Deserialize, Serialize};

use crate::model::key;
use crate::persist::{hash_bytes, Saved};
use crate::schema_registry::{stored_hash, Compatibility, SchemaEntry, SchemaGroup, SchemaType, SchemaVersion};
use crate::Shared;

pub const SCHEMAS_FILE: &str = "schemas.state";
pub const SCHEMAS_KIND: &str = "eventhubs-schemas";
pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct VersionD {
    version: u32,
    kind: String,
    id: String,
    content: String,
    created_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct SchemaD {
    name: String,
    versions: Vec<VersionD>,
}

#[derive(Serialize, Deserialize)]
struct GroupD {
    name: String,
    schema_type: String,
    compatibility: String,
    properties: Vec<(String, String)>,
    created_ms: i64,
    updated_ms: i64,
    etag: String,
    schemas: Vec<SchemaD>,
}

#[derive(Serialize, Deserialize)]
struct NsD {
    ns: String,
    groups: Vec<GroupD>,
}

#[derive(Serialize, Deserialize, Default)]
struct SchemasFile {
    namespaces: Vec<NsD>,
}

impl Shared {
    pub(crate) fn save_schemas(&self, dir: &Path, saved: &mut Saved) -> io::Result<()> {
        let file = {
            let st = self.state.lock();
            let mut f = SchemasFile::default();
            for (ns, n) in &st.namespaces {
                if n.schema_groups.is_empty() {
                    continue;
                }
                let groups = n
                    .schema_groups
                    .values()
                    .map(|g| GroupD {
                        name: g.name.clone(),
                        schema_type: g.schema_type.name().to_string(),
                        compatibility: g.compatibility.name().to_string(),
                        properties: g.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        created_ms: g.created_ms,
                        updated_ms: g.updated_ms,
                        etag: g.etag.clone(),
                        schemas: g
                            .schemas
                            .values()
                            .map(|s| SchemaD {
                                name: s.name.clone(),
                                versions: s
                                    .versions
                                    .iter()
                                    .map(|v| VersionD {
                                        version: v.version,
                                        kind: v.kind.name().to_string(),
                                        id: v.id.clone(),
                                        content: v.content.clone(),
                                        created_ms: v.created_ms,
                                    })
                                    .collect(),
                            })
                            .collect(),
                    })
                    .collect();
                f.namespaces.push(NsD { ns: ns.clone(), groups });
            }
            f
        };
        let path = dir.join(SCHEMAS_FILE);
        if file.namespaces.is_empty() {
            saved.schemas_hash = None;
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let bytes = persist::encode(SCHEMAS_KIND, VERSION, &file)?;
        let h = hash_bytes(&bytes);
        if saved.schemas_hash != Some(h) {
            persist::write_atomic(&path, &bytes)?;
            saved.schemas_hash = Some(h);
        }
        Ok(())
    }

    pub(crate) fn load_schemas(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join(SCHEMAS_FILE);
        let Some((_, file)) = read_state::<SchemasFile>(&path, SCHEMAS_KIND, VERSION)? else { return Ok(()) };
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {m}", path.display()));
        let mut st = self.state.lock();
        for nd in file.namespaces {
            let mut groups = std::collections::BTreeMap::new();
            for gd in nd.groups {
                let schema_type = SchemaType::parse(&gd.schema_type)
                    .ok_or_else(|| bad(format!("schema group {}: unknown type {}", gd.name, gd.schema_type)))?;
                let compatibility = Compatibility::parse(&gd.compatibility)
                    .ok_or_else(|| bad(format!("schema group {}: unknown compatibility {}", gd.name, gd.compatibility)))?;
                let mut group = SchemaGroup::new(&gd.name, schema_type, compatibility, gd.created_ms);
                group.updated_ms = gd.updated_ms;
                group.etag = gd.etag;
                group.properties = gd.properties.into_iter().collect();
                for sd in gd.schemas {
                    let mut versions = Vec::with_capacity(sd.versions.len());
                    for vd in sd.versions {
                        let kind =
                            SchemaType::parse(&vd.kind).ok_or_else(|| bad(format!("schema {}: unknown kind {}", sd.name, vd.kind)))?;
                        if let Some(h) = stored_hash(kind, &vd.content) {
                            group.hashes.insert(vd.id.clone(), h);
                        }
                        versions.push(SchemaVersion {
                            version: vd.version,
                            kind,
                            id: vd.id,
                            content: vd.content,
                            created_ms: vd.created_ms,
                        });
                    }
                    let entry = SchemaEntry { name: sd.name.clone(), versions };
                    if group.schemas.insert(key(&sd.name), entry).is_some() {
                        return Err(bad(format!("schema {} is listed twice in group {}", sd.name, gd.name)));
                    }
                }
                if groups.insert(key(&gd.name), group).is_some() {
                    return Err(bad(format!("schema group {} is listed twice in namespace {}", gd.name, nd.ns)));
                }
            }
            st.namespaces.entry(nd.ns).or_default().schema_groups = groups;
        }
        Ok(())
    }
}
