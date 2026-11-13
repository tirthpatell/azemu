//! Small public functions for the ARM control plane (`azemu-arm`). Additive only.

use crate::AppConfigService;

impl AppConfigService {
    /// Remove a configuration store and everything in it. Returns whether the store held anything.
    pub fn delete_store(&self, name: &str) -> bool {
        let mut state = self.state.lock();
        let removed = state.stores.remove(&name.to_ascii_lowercase()).is_some();
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        removed
    }
}
