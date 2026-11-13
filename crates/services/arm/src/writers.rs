//! Writes and deletes of a resource group must not overlap.
//!
//! What must hold: when a group delete answers 200 or 202, the group and everything in it are gone, and no create that was
//! accepted before can show up afterwards; a create that arrives while the group is being deleted fails with
//! `ResourceGroupBeingDeleted` (409, as Azure answers). How:
//! - every write into a group (a resource PUT or PATCH, an event subscription, a deployment record, a group PUT) takes a
//!   [`WriteGuard`] first, which checks that the group exists and is not being deleted, and counts the writer;
//! - a group delete first marks the group as deleting (so no new writer gets in), then waits until the writers that were
//!   already in have finished (their results are in the state), and only then reads the members and deletes them.

use std::time::Duration;

use crate::error::{ArmError, ArmResult};
use crate::state::group_key;
use crate::Inner;

/// How long a group delete waits for writes that are still running.
const WAIT_FOR_WRITERS: Duration = Duration::from_secs(60);

pub fn being_deleted(name: &str) -> ArmError {
    ArmError::conflict(
        "ResourceGroupBeingDeleted",
        format!("The resource group '{name}' is being deleted. Wait until the deletion is finished."),
    )
}

/// Held while a write into a group runs; dropping it lets a waiting group delete go on.
pub struct WriteGuard<'a> {
    inner: &'a Inner,
    key: String,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.inner.state.lock();
        if let Some(n) = st.writers.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.writers.remove(&self.key);
            }
        }
    }
}

impl Inner {
    /// Start a write into the group `rg` of `sub`. `ResourceGroupNotFound` when there is none, `ResourceGroupBeingDeleted` when a
    /// delete has started. A write outside any group (`rg` empty) needs no guard.
    pub(crate) fn begin_write(&self, sub: &str, rg: &str) -> ArmResult<Option<WriteGuard<'_>>> {
        if rg.is_empty() {
            return Ok(None);
        }
        let key = group_key(sub, rg);
        let mut st = self.state.lock();
        let name = st.groups.get(&key).map(|g| g.name.clone()).ok_or_else(|| ArmError::group_not_found(rg))?;
        if st.deleting.contains(&key) {
            return Err(being_deleted(&name));
        }
        *st.writers.entry(key.clone()).or_insert(0) += 1;
        Ok(Some(WriteGuard { inner: self, key }))
    }

    /// Mark the group as being deleted and wait for the writers that are already in. `Err` when it does not exist, is already being
    /// deleted, or a writer does not finish in time (then the mark is removed again).
    pub(crate) async fn begin_group_delete(&self, sub: &str, rg: &str) -> ArmResult<String> {
        let key = group_key(sub, rg);
        {
            let mut st = self.state.lock();
            let name = st.groups.get(&key).map(|g| g.name.clone()).ok_or_else(|| ArmError::group_not_found(rg))?;
            if !st.deleting.insert(key.clone()) {
                return Err(being_deleted(&name));
            }
        }
        let started = std::time::Instant::now();
        loop {
            if self.state.lock().writers.get(&key).copied().unwrap_or(0) == 0 {
                return Ok(key);
            }
            if started.elapsed() > WAIT_FOR_WRITERS {
                self.state.lock().deleting.remove(&key);
                return Err(ArmError::new(
                    409,
                    "ResourceGroupBusy",
                    format!(
                        "A write into the resource group '{rg}' did not finish in {} seconds; try the delete again.",
                        WAIT_FOR_WRITERS.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub(crate) fn end_group_delete(&self, key: &str) {
        self.state.lock().deleting.remove(key);
    }
}
