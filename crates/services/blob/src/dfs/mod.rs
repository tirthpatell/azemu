//! Azure Data Lake Storage Gen2 (`dfs` endpoint) on top of the blob data model.
//! See `docs/services/datalake.md` and `model.rs` in this directory.

use std::sync::Arc;

use azemu_core::{async_trait, Mount, Request, Response, Service};

use crate::Shared;

mod access;
pub mod acl;
mod auth;
mod deleted;
mod errors;
mod expiry;
mod filesystem;
mod list;
pub mod model;
pub(crate) mod ops;
mod path;
mod rename;
mod router;
mod update;
mod util;

pub(crate) use deleted::{list_deleted, undelete as undelete_path};
/// The Data Lake service: `http://{account}.dfs.core.localhost:{port}/{filesystem}/{path}`.
/// Created with `BlobService::datalake()`; it shares the Blob service's data.
pub(crate) use errors::into_response as error_response;
pub(crate) use expiry::set_expiry;
pub(crate) use rename::parse_source as rename_source;
pub(crate) use router::SERVER;

pub struct DataLakeService {
    pub(crate) shared: Arc<Shared>,
}

#[async_trait]
impl Service for DataLakeService {
    fn name(&self) -> &'static str {
        "dfs"
    }

    fn mounts(&self) -> Vec<Mount> {
        vec![Mount::AccountHost("dfs")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn default_account(&self) -> &'static str {
        "devstoreaccount1"
    }

    async fn handle(&self, req: Request) -> Response {
        let op = router::op_of(&req);
        // `x-ms-structured-body` on an append: take the message apart first (see `structured.rs`).
        let mut wrapped = false;
        let req = if matches!(op, ops::Op::Append) {
            match crate::structured::unwrap_upload(&req) {
                Ok(Some(plain)) => {
                    wrapped = true;
                    plain
                }
                Ok(None) => req,
                Err(e) => {
                    let resp = errors::into_response(e, &req.ctx);
                    return azemu_core::storage::finish(resp, &req, router::SERVER);
                }
            }
        } else {
            req
        };
        let pre = crate::pre::prepare_dfs(&req, op).await;
        let mut resp = self.shared.serve(&req, pre, |st, pre| router::run(st, &req, &self.shared.accounts, pre)).await;
        if wrapped {
            crate::structured::echo(&mut resp);
        }
        resp
    }

    fn reset(&self) {
        self.shared.reset();
    }

    /// The same account as the Blob service (they share it).
    fn reset_account(&self, account: &str) -> bool {
        self.shared.state.lock().accounts.remove(&account.to_ascii_lowercase());
        self.shared.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Reads only: a `POST` here is a query or a lease, and the two cannot be told apart from the path.
    fn abort_on_disconnect(&self, method: &azemu_core::http::Method, _path: &str) -> bool {
        *method == azemu_core::http::Method::GET
    }
}
