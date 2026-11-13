//! Saving the tables to a data directory (`--data-dir`) and reading them back.
//!
//! One file, `table.state` (kind "table", version 1): for each storage account its tables (name as created, every
//! entity with its properties, typed as stored, and its timestamp, from which the ETag is derived; the access
//! policies) and the service properties. Entities are written as a list in key order and put back in the ordered map
//! when read. Nothing else is stored.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use azemu_core::persist;
use serde::{Deserialize, Serialize};

use crate::props::ServiceProps;
use crate::state::{key, Account, Entity, SignedId, Table};
use crate::TableService;

const KIND: &str = "table";
const VERSION: u32 = 1;
const FILE: &str = "table.state";

#[derive(Serialize, Deserialize)]
struct SavedTable {
    name: String,
    entities: Vec<Entity>,
    acl: Vec<SignedId>,
}

#[derive(Serialize, Deserialize)]
struct SavedAccount {
    /// By lower-case table name.
    tables: BTreeMap<String, SavedTable>,
    props: ServiceProps,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    /// By account name.
    accounts: BTreeMap<String, SavedAccount>,
}

impl TableService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        // copy under the lock, encode and write after it
        let accounts: BTreeMap<String, SavedAccount> = {
            let state = self.state.lock();
            state
                .accounts
                .iter()
                .map(|(name, a)| {
                    let tables = a
                        .tables
                        .iter()
                        .map(|(k, t)| {
                            (
                                k.clone(),
                                SavedTable { name: t.name.clone(), entities: t.entities.values().cloned().collect(), acl: t.acl.clone() },
                            )
                        })
                        .collect();
                    (name.clone(), SavedAccount { tables, props: a.props.clone() })
                })
                .collect()
        };
        persist::save_file_if_changed(dir, FILE, KIND, VERSION, &Saved { accounts }, false)?;
        Ok(())
    }

    pub(crate) fn load_state(&self, dir: &Path) -> io::Result<()> {
        let Some((_, saved)) = persist::load_file::<Saved>(dir, FILE, KIND, VERSION)? else {
            return Ok(());
        };
        let mut accounts = std::collections::HashMap::new();
        for (name, a) in saved.accounts {
            let mut tables = BTreeMap::new();
            for (k, t) in a.tables {
                let mut entities = BTreeMap::new();
                for e in t.entities {
                    entities.insert(key(&e.pk, &e.rk), e);
                }
                tables.insert(k, Table { name: t.name, entities, acl: t.acl });
            }
            accounts.insert(name, Account { tables, props: a.props });
        }
        self.state.lock().accounts = accounts;
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
