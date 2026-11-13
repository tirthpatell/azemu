//! Saving the stores to a data directory (`--data-dir`) and reading them back.
//!
//! One file, `appconfig.state` (kind "appconfig", version 1): for each store (by name) every kept revision of every
//! key-value (with its etag, time, lock flag and the time it was replaced or deleted), which revision is current for
//! each (key, label), the revision counter, and the snapshots with the settings they hold. The indexes are built
//! again when the file is read. The limits (`AZEMU_APPCONFIG_*`) are settings of the run, not state: they come
//! from the new run, so a lowered limit takes effect on the next write. The `Sync-Token` counter is not stored.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use azemu_core::persist;
use serde::{Deserialize, Serialize};

use crate::state::{SavedStore, Store};
use crate::AppConfigService;

const KIND: &str = "appconfig";
const VERSION: u32 = 1;
const FILE: &str = "appconfig.state";

#[derive(Serialize, Deserialize)]
struct Saved {
    stores: BTreeMap<String, SavedStore>,
}

impl AppConfigService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        // copy under the lock (settings are shared, not copied), encode and write after it
        let stores: BTreeMap<String, SavedStore> = {
            let state = self.state.lock();
            state.stores.iter().map(|(k, s)| (k.clone(), s.to_saved())).collect()
        };
        persist::save_file_if_changed(dir, FILE, KIND, VERSION, &Saved { stores }, false)?;
        Ok(())
    }

    pub(crate) fn load_state(&self, dir: &Path) -> io::Result<()> {
        let Some((_, saved)) = persist::load_file::<Saved>(dir, FILE, KIND, VERSION)? else {
            return Ok(());
        };
        let path = dir.join(FILE);
        let mut state = self.state.lock();
        let limits = state.limits;
        let mut stores = std::collections::HashMap::new();
        for (name, s) in saved.stores {
            let store = Store::from_saved(limits, s).map_err(|why| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}: the store '{name}' is inconsistent: {why}", path.display()))
            })?;
            stores.insert(name, store);
        }
        state.stores = stores;
        drop(state);
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
