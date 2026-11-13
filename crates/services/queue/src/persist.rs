//! Saving the queues to a data directory (`--data-dir`) and reading them back.
//!
//! One file, `queue.state` (kind "queue", version 1): for each storage account its queues (metadata, access
//! policies and every message with its text, insert/expiry/visibility times as absolute times, dequeue count and
//! pop receipt) and the service properties. A message that was handed out and not deleted stays hidden until the
//! time it was hidden to, as before the restart. Nothing else is stored: there are no connections here.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use azemu_core::persist;
use serde::{Deserialize, Serialize};

use crate::state::Account;
use crate::QueueService;

const KIND: &str = "queue";
const VERSION: u32 = 1;
const FILE: &str = "queue.state";

#[derive(Serialize, Deserialize)]
struct Saved {
    /// By lower-case account name.
    accounts: BTreeMap<String, Account>,
}

impl QueueService {
    pub(crate) fn save_state(&self, dir: &Path) -> io::Result<()> {
        // copy under the lock, encode and write after it
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
