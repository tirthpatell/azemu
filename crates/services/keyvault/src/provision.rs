//! Small public functions for the ARM control plane (`azemu-arm`). Additive only.

use crate::KeyVaultService;

impl KeyVaultService {
    /// Remove a vault and everything in it (secrets, keys, certificates, deleted items).
    /// Returns whether the vault held anything.
    pub fn delete_vault(&self, name: &str) -> bool {
        let mut state = self.state.lock();
        let removed = state.vaults.remove(&name.to_ascii_lowercase()).is_some();
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        removed
    }
}
