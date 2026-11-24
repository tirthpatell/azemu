//! file: the Azure Files REST API (file shares, directories and files over HTTP).
//!
//! SMB and NFS are not here: a share cannot be mounted. This is the API the storage SDKs call (`azure-storage-file-share`,
//! `Azure.Storage.Files.Shares`, `azfile` for Go, ...).
//!
//! Reachable as `http://{account}.file.core.localhost:{port}/{share}/{path}` on the gateway and as
//! `http://127.0.0.1:{port}/{account}/{share}/{path}` on a pinned port (Azurite style). Any account name works without being
//! created first.
//!
//! Layout: `routing` picks the operation, `auth` checks what the credentials allow, `shares`, `dirs`, `files`, `ranges`, `copy`
//! and `rename` do the work on the `state`, `smb` reads the SMB properties, `leasing` and `lease` hold the leases, `props` has the
//! service properties, `xml` reads and writes the bodies and `errors` has every error Azure sends.

// Handlers return `Result<_, Response>` and the error is the finished HTTP response, which is large; boxing it would only add noise.
#![allow(clippy::result_large_err)]

mod auth;
mod copy;
mod dirs;
mod download;
mod errors;
mod files;
mod inspect;
mod lease;
mod leasing;
mod meta;
mod names;
mod persist;
mod props;
mod ranges;
mod rename;
mod routing;
mod shares;
mod smb;
mod state;
mod tree;
mod xml;

use std::sync::atomic::{AtomicU64, Ordering};

use azemu_core::http::Method;
use azemu_core::settings;
use azemu_core::storage;
use azemu_core::time::OffsetDateTime;
use azemu_core::{async_trait, Mount, Request, Response, Service};
use parking_lot::Mutex;

use crate::auth::Lookup;
use crate::routing::{route, Op, Routed, Unrouted};
use crate::state::{Account, State, MAX_PUT_RANGE};

/// The `Server` header real Azure Files sends.
pub const SERVER: &str = "Windows-Azure-File/1.0 Microsoft-HTTPAPI/2.0";

/// How many days a deleted share can be restored: a new storage account has soft delete for shares on, for 7 days.
const DEFAULT_SOFT_DELETE_DAYS: u64 = 7;

/// What a handler needs besides the account: the request, the time, and the setting that decides what a delete leaves behind.
pub(crate) struct Cx<'a> {
    pub req: &'a Request,
    pub now: OffsetDateTime,
    pub soft_delete_days: u32,
}

pub struct FileService {
    state: Mutex<State>,
    /// Account keys, for checking signatures (derived from the account name; `reset()` does not touch them).
    accounts: storage::StorageAccounts,
    /// Raised on every change to the state that a save would write (see `persist_generation`).
    generation: AtomicU64,
    soft_delete_days: u32,
}

impl Default for FileService {
    fn default() -> Self {
        Self::new()
    }
}

impl FileService {
    /// The soft delete retention comes from `AZEMU_FILE_SOFT_DELETE_DAYS`.
    pub fn new() -> Self {
        settings::describe(
            "AZEMU_FILE_SOFT_DELETE_DAYS",
            "7",
            "days a deleted file share can be restored (List Shares with include=deleted, then Restore Share); 0 deletes shares for good. On Azure this is a setting of the storage account (a new account has 7 days)",
        );
        let days = settings::number("AZEMU_FILE_SOFT_DELETE_DAYS", DEFAULT_SOFT_DELETE_DAYS).min(365) as u32;
        Self::with_soft_delete_days(days)
    }

    pub fn with_soft_delete_days(soft_delete_days: u32) -> Self {
        FileService {
            state: Mutex::new(State::default()),
            accounts: storage::StorageAccounts::new(),
            generation: AtomicU64::new(0),
            soft_delete_days,
        }
    }

    fn dispatch(&self, req: &Request) -> Response {
        let routed = match route(req) {
            Ok(r) => r,
            Err(u) => return unrouted(req, u),
        };
        if let Some(v) = storage::credentials::refused_version(req) {
            return errors::invalid_version(&req.ctx, v);
        }
        // The lock is taken and released inside this function, never across an .await.
        let mut state = self.state.lock();
        let key = req.account.to_ascii_lowercase();
        if let Some(a) = state.accounts.get_mut(&key) {
            a.purge_deleted(req.ctx.time, self.soft_delete_days);
        }
        if let Err(resp) = authorize(&state, &self.accounts, req, &routed, &key) {
            return resp;
        }
        // Azure checks every name of the request before it looks for anything
        if let Some(share) = routed.share.as_deref() {
            if let Err(e) = names::validate_share(share).and_then(|_| names::validate_path(&routed.path)) {
                return names::share_error(&req.ctx, e);
            }
        }
        let c = Cx { req, now: req.ctx.time, soft_delete_days: self.soft_delete_days };
        let share = routed.share.as_deref().unwrap_or("");
        let path = routed.path.as_slice();
        let resp = self.run(&mut state, &c, routed.op, &key, share, path);
        if changes(routed.op) && resp.status().is_success() {
            // inside the lock and after the change: a save that sees the new number also sees the change
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        resp
    }

    fn run(&self, state: &mut State, c: &Cx, op: Op, key: &str, share: &str, path: &[String]) -> Response {
        let req = c.req;
        // Operations that may reach into another account (a copy from a file of another account) get the whole state.
        match op {
            Op::PutRange => return ranges::put(state, c, share, path),
            Op::PutRangeFromUrl => return ranges::put_from_url(state, c, share, path),
            Op::ListRanges => return ranges::list(state, c, share, path),
            Op::CopyFile => return copy::copy_file(state, c, share, path),
            Op::AbortCopy => return copy::abort(state, c, share, path),
            Op::NotBuilt(name) => return errors::not_implemented(&req.ctx, name),
            Op::GetUserDelegationKey => return props::user_delegation_key(req, &self.accounts),
            Op::GetServiceProperties => return props::get_properties(state, req),
            Op::SetServiceProperties => return props::set_properties(state, req),
            _ => {}
        }
        let reads = matches!(
            op,
            Op::ListShares
                | Op::GetShareProperties
                | Op::GetShareMetadata
                | Op::GetShareAcl
                | Op::GetShareStats
                | Op::GetPermission
                | Op::GetDirectoryProperties
                | Op::GetDirectoryMetadata
                | Op::ListFilesAndDirectories
                | Op::ListHandles
                | Op::ForceCloseHandles
                | Op::GetFile
                | Op::GetFileMetadata
        );
        let empty = Account::default();
        let acct: &mut Account =
            if reads && !state.accounts.contains_key(key) { &mut { empty } } else { state.accounts.entry(key.to_string()).or_default() };
        match op {
            Op::ListShares => shares::list(acct, c),
            Op::CreateShare => shares::create(acct, c, share),
            Op::GetShareProperties => shares::get_properties(acct, c, share),
            Op::GetShareMetadata => shares::get_metadata(acct, c, share),
            Op::DeleteShare => shares::delete(acct, c, share),
            Op::SetShareProperties => shares::set_properties(acct, c, share),
            Op::SetShareMetadata => shares::set_metadata(acct, c, share),
            Op::GetShareAcl => shares::get_acl(acct, c, share),
            Op::SetShareAcl => shares::set_acl(acct, c, share),
            Op::GetShareStats => shares::stats(acct, c, share),
            Op::CreateShareSnapshot => shares::snapshot(acct, c, share),
            Op::LeaseShare => shares::lease(acct, c, share),
            Op::RestoreShare => shares::restore(acct, c, share),
            Op::CreatePermission => shares::create_permission(acct, c, share),
            Op::GetPermission => shares::get_permission(acct, c, share),
            Op::CreateDirectory => dirs::create(acct, c, share, path),
            Op::GetDirectoryProperties => dirs::get_properties(acct, c, share, path, false),
            Op::GetDirectoryMetadata => dirs::get_properties(acct, c, share, path, true),
            Op::DeleteDirectory => dirs::delete(acct, c, share, path),
            Op::SetDirectoryProperties => dirs::set_properties(acct, c, share, path),
            Op::SetDirectoryMetadata => dirs::set_metadata(acct, c, share, path),
            Op::ListFilesAndDirectories => dirs::list(acct, c, share, path),
            Op::RenameDirectory | Op::RenameFile => rename::rename(acct, c, share, path),
            Op::ListHandles => rename::list_handles(acct, c, share, path),
            Op::ForceCloseHandles => rename::force_close_handles(acct, c, share, path),
            Op::CreateFile => files::create(acct, c, share, path),
            Op::GetFile => files::get(acct, c, share, path),
            Op::GetFileMetadata => files::get_metadata(acct, c, share, path),
            Op::DeleteFile => files::delete(acct, c, share, path),
            Op::SetFileProperties => files::set_properties(acct, c, share, path),
            Op::SetFileMetadata => files::set_metadata(acct, c, share, path),
            Op::LeaseFile => files::lease(acct, c, share, path),
            // handled above
            _ => errors::not_implemented(&req.ctx, "this operation"),
        }
    }
}

/// Does the operation change what a save would write? (An answer that is not a success never does.)
fn changes(op: Op) -> bool {
    !matches!(
        op,
        Op::GetServiceProperties
            | Op::ListShares
            | Op::GetUserDelegationKey
            | Op::GetShareProperties
            | Op::GetShareMetadata
            | Op::GetShareAcl
            | Op::GetShareStats
            | Op::GetPermission
            | Op::GetDirectoryProperties
            | Op::GetDirectoryMetadata
            | Op::ListFilesAndDirectories
            | Op::ListHandles
            | Op::ForceCloseHandles
            | Op::GetFile
            | Op::GetFileMetadata
            | Op::ListRanges
            | Op::NotBuilt(_)
    )
}

fn authorize(state: &State, accounts: &storage::StorageAccounts, req: &Request, routed: &Routed, key: &str) -> Result<(), Response> {
    let share = routed.share.as_deref();
    let lookup = match (auth::policy_id(req), share) {
        (Some(id), Some(share)) => match state.accounts.get(key).and_then(|a| a.shares.get(share)) {
            None => Lookup::NoShare,
            Some(s) => match s.acl.iter().find(|p| p.id == id) {
                Some(policy) => Lookup::Found(policy),
                None => Lookup::Missing,
            },
        },
        _ => Lookup::NotUsed,
    };
    auth::authorize(req, routed.op, lookup, accounts, share, &routed.path)
}

fn unrouted(req: &Request, why: Unrouted) -> Response {
    match why {
        Unrouted::UnsupportedVerb => errors::unsupported_verb(&req.ctx),
        Unrouted::BadUri(path) => storage::error_with_details(
            &req.ctx,
            400,
            "InvalidUri",
            "The requested URI does not represent any resource on the server.",
            &[("UriPath", &path)],
        ),
        Unrouted::BadQuery(name, value) => errors::invalid_query(&req.ctx, name, &value, ""),
    }
}

#[async_trait]
impl Service for FileService {
    fn name(&self) -> &'static str {
        "file"
    }

    fn mounts(&self) -> Vec<Mount> {
        // http://{account}.file.core.localhost:4577
        vec![Mount::AccountHost("file")]
    }

    fn path_style_on_pinned(&self) -> bool {
        true
    }

    fn inspect(&self, req: &azemu_core::inspect::InspectRequest) -> azemu_core::inspect::InspectResult {
        inspect::node(&self.state.lock(), self.soft_delete_days, req)
    }

    fn default_account(&self) -> &'static str {
        storage::DEV_ACCOUNT
    }

    /// One Put Range takes 4 MiB; every other body of this API is small. Core refuses a bigger one before it is read.
    fn max_body_bytes(&self, _method: &Method, _path: &str) -> Option<usize> {
        Some(MAX_PUT_RANGE)
    }

    /// Azure: 413 `RequestBodyTooLarge` with `MaxLimit` for a Put Range over 4 MiB (tried in November 2026).
    fn body_too_large(&self, limit: usize) -> Option<Response> {
        let id = uuid::Uuid::new_v4().to_string();
        let body = format!(
            "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>RequestBodyTooLarge</Code><Message>The request body is too large and exceeds the maximum permissible limit.\nRequestId:{id}\nTime:{}</Message><MaxLimit>{limit}</MaxLimit></Error>",
            azemu_core::time::iso8601_7(OffsetDateTime::now_utc())
        );
        let resp = azemu_core::resp::Resp::new(413u16)
            .xml(body)
            .header("x-ms-error-code", "RequestBodyTooLarge")
            .header("x-ms-request-id", &id)
            .header("x-ms-version", storage::DEFAULT_VERSION)
            .header("Server", SERVER)
            .build();
        Some(resp)
    }

    fn fault_response(&self, req: &Request, answer: &azemu_core::FaultAnswer) -> Response {
        storage::fault_response(req, SERVER, answer)
    }

    async fn handle(&self, req: Request) -> Response {
        let mut resp = if req.method == Method::OPTIONS {
            // Preflight: no credentials, judged against the service's CORS rules.
            storage::cors::preflight(&req, &props::cors_rules(&self.state.lock(), &req.account))
        } else {
            self.dispatch(&req)
        };
        // the common headers first: `Access-Control-Expose-Headers` names the ones the answer has
        resp = storage::finish(resp, &req, SERVER);
        if req.method != Method::OPTIONS {
            storage::cors::apply(&mut resp, &req, &props::cors_rules(&self.state.lock(), &req.account));
        }
        resp
    }

    fn reset(&self) {
        let mut state = self.state.lock();
        *state = State::default();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Empty one storage account: its shares, deleted shares and service properties. Persistence sees a change.
    fn reset_account(&self, account: &str) -> bool {
        self.state.lock().accounts.retain(|k, _| !k.eq_ignore_ascii_case(account));
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }

    fn persist_generation(&self) -> Option<u64> {
        Some(self.generation.load(Ordering::SeqCst))
    }

    fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.save_state(dir)
    }

    fn load(&self, dir: &std::path::Path) -> std::io::Result<()> {
        self.load_state(dir)
    }
}
