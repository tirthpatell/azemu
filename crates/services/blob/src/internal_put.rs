//! Writing a block blob from inside the emulator, for Event Grid's dead letters (`docs/services/eventgrid.md`).
//!
//! The write goes straight into the account's state: no request, no authorisation, and no storage event. The last matters:
//! a subscription on the same storage account would otherwise see the blob of its own dead letter, whose delivery could fail
//! and be dead-lettered into one more blob, and so on without end.

use azemu_core::bytes::Bytes;
use azemu_core::time::OffsetDateTime;

use crate::state::{Blob, BlobKind};
use crate::BlobService;

impl BlobService {
    /// Put `body` as the block blob `name` of `container` in `account`, replacing a blob of that name. The container must
    /// exist (nothing is created); `Err` is the text of the reason.
    pub fn put_block_blob_silently(
        &self,
        account: &str,
        container: &str,
        name: &str,
        content_type: &str,
        body: Bytes,
    ) -> Result<(), String> {
        let now = OffsetDateTime::now_utc();
        let mut blob = Blob::new(BlobKind::Block, now);
        blob.props.content_type = content_type.to_string();
        blob.set_content(body);
        let acct = account.to_ascii_lowercase();
        let mut st = self.shared.state.lock();
        let (containers, mut ctl) = st.account(&acct).split();
        let c = containers
            .get_mut(container)
            .ok_or_else(|| format!("ContainerNotFound: the container '{container}' of the account '{acct}' does not exist"))?;
        let entry = c.blobs.entry(name.to_string()).or_default();
        crate::store::install(&mut ctl, entry, blob, now);
        st.changed = true;
        self.shared.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_blob_is_there_and_a_missing_container_is_an_error() {
        let svc = BlobService::new();
        let e = svc.put_block_blob_silently("acct", "nope", "a/b.json", "application/json", Bytes::from_static(b"[]")).unwrap_err();
        assert!(e.contains("ContainerNotFound"), "{e}");
        {
            let mut st = svc.shared.state.lock();
            let now = OffsetDateTime::now_utc();
            st.account("acct").containers.insert("c".into(), crate::state::Container::new("c", now, "etag".into()));
        }
        svc.put_block_blob_silently("ACCT", "c", "a/b.json", "application/json", Bytes::from_static(b"[1]")).unwrap();
        let st = svc.shared.state.lock();
        let b = st.accounts.get("acct").unwrap().containers["c"].blobs["a/b.json"].live().unwrap();
        assert_eq!(b.size, 3);
        assert_eq!(b.props.content_type, "application/json");
    }
}
