//! Saving the shares to a data directory (`--data-dir`) and reading them back.
//!
//! One file, `file.state` (kind "file", version 1): for each storage account its service properties, shares (properties, metadata,
//! access policies, leases, snapshots and the whole tree of directories and files with their content), deleted shares and
//! permissions. Times are absolute, so a lease or a soft-deleted share runs out on schedule across a restart.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use azemu_core::persist;
use serde::{Deserialize, Serialize};

use crate::state::Account;
use crate::FileService;

const KIND: &str = "file";
const VERSION: u32 = 1;
const FILE: &str = "file.state";

#[derive(Serialize, Deserialize)]
struct Saved {
    /// By lower-case account name.
    accounts: BTreeMap<String, Account>,
}

impl FileService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        // copy under the lock (file content is shared, not copied), encode and write after it
        let accounts: BTreeMap<String, Account> = {
            let state = self.state.lock();
            state.accounts.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        persist::save_file_if_changed(dir, FILE, KIND, VERSION, &Saved { accounts }, false)?;
        Ok(())
    }

    pub(crate) fn load_state(&self, dir: &Path) -> io::Result<()> {
        let Some((_, saved)) = persist::load_file::<Saved>(dir, FILE, KIND, VERSION)? else {
            return Ok(());
        };
        let mut state = self.state.lock();
        state.accounts = saved.accounts.into_iter().collect();
        drop(state);
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
